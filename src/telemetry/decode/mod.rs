//! Streaming verifier and decoder for committed v0 event chunks.
//!
//! The file is walked once, in order, and every level is checked against what the level above it
//! claimed. The result vocabulary the caller reads and the failure vocabulary the walker raises
//! live here with the two drivers: the file walk itself, and the dispatch over one chunk's payload
//! records. [`page`] parses what one producer sealed into a page; [`ledger`] parses the two
//! ledgers and checks them against the pages.

mod ledger;
mod page;

use super::format::{
    CHUNK_FOOTER_BYTES, CHUNK_HEADER_BYTES, ChunkFooter, ChunkHeader, Control, EngineContext,
    FILE_HEADER_BYTES, FileHeader, KIND_ENGINE_CONTEXT, KIND_PAGE, KIND_PRODUCER_END,
    KIND_SESSION_END, KIND_SYSCALL_ENTER, KIND_SYSCALL_EXIT, ProducerEnd, SessionEnd, SyscallEnter,
    SyscallExit, WireError,
};
use ledger::{parse_producer_end, validate_session_end};
use page::{ProducerState, clear_all_open, parse_page};
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs::File;
use std::io;
use std::io::BufReader;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum Health {
    Clean,
    Unclean,
    Corrupt,
}

impl Health {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::Unclean => "unclean",
            Self::Corrupt => "corrupt",
        }
    }

    fn worsen(&mut self, other: Self) {
        *self = (*self).max(other);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DecodeIssue {
    pub(crate) offset: u64,
    pub(crate) message: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct FileReport {
    pub(crate) committed_chunks: u64,
    pub(crate) committed_pages: u64,
    pub(crate) committed_bytes: u64,
    pub(crate) records: u64,
    pub(crate) producer_ends: u64,
    pub(crate) unknown_records: u64,
    pub(crate) sequence_gaps: u64,
    pub(crate) incomplete_enters: u64,
    pub(crate) orphan_exits: u64,
    pub(crate) contract_violations: u64,
    pub(crate) kind_counts: BTreeMap<u16, u64>,
    pub(crate) engine_counts: BTreeMap<Option<u64>, u64>,
    pub(crate) producers: BTreeSet<u64>,
    pub(crate) threads: BTreeSet<(u64, u32, u32)>,
    pub(crate) session_end: Option<SessionEnd>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DecodeOutcome {
    pub(crate) path: PathBuf,
    pub(crate) header: FileHeader,
    pub(crate) health: Health,
    pub(crate) issues: Vec<DecodeIssue>,
    pub(crate) report: FileReport,
}

/// A stream that violates the v0 contract, before the path that identifies it is attached.
///
/// The decoder has exactly one way to fail — the bytes do not satisfy the format — so the
/// class is the reason: [`DecodeError`] adds the path at the boundary that knows it.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct Malformed(String);

impl Malformed {
    fn new(detail: impl Into<String>) -> Self {
        Self(detail.into())
    }
}

impl From<Malformed> for String {
    fn from(reason: Malformed) -> Self {
        reason.0
    }
}

/// Why one event stream could not be decoded, with the path that identifies it.
#[derive(Debug, thiserror::Error)]
#[error("{}: {message}", path.display())]
pub(crate) struct DecodeError {
    path: PathBuf,
    message: String,
}

impl DecodeError {
    fn new(path: &Path, message: impl Into<String>) -> Self {
        Self {
            path: path.to_path_buf(),
            message: message.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EventContext {
    pub(crate) session_id: [u8; 16],
    pub(crate) pid: u32,
    pub(crate) process_generation: u64,
    pub(crate) producer_id: u64,
    pub(crate) thread_generation: u32,
    pub(crate) os_tid: u32,
    pub(crate) sequence: u64,
    pub(crate) engine_id: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum DecodedKind {
    SyscallEnter(SyscallEnter),
    SyscallExit {
        record: SyscallExit,
        enter_sequence: Option<u64>,
    },
    EngineContext(EngineContext),
    Unknown {
        kind: u16,
        version: u8,
        flags: u8,
        bytes: Vec<u8>,
    },
}

impl DecodedKind {
    pub(crate) const fn kind_id(&self) -> u16 {
        match self {
            Self::SyscallEnter(_) => KIND_SYSCALL_ENTER,
            Self::SyscallExit { .. } => KIND_SYSCALL_EXIT,
            Self::EngineContext(_) => KIND_ENGINE_CONTEXT,
            Self::Unknown { kind, .. } => *kind,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DecodedEvent {
    pub(crate) context: EventContext,
    pub(crate) kind: DecodedKind,
}

#[derive(Debug)]
enum ParseFailure {
    Format(String),
    Visitor(io::Error),
}

impl From<WireError> for ParseFailure {
    fn from(value: WireError) -> Self {
        Self::Format(value.to_string())
    }
}

pub(crate) fn decode_file(
    path: &Path,
    visitor: &mut dyn FnMut(&DecodedEvent) -> io::Result<()>,
) -> Result<DecodeOutcome, DecodeError> {
    let file = File::open(path)
        .map_err(|error| DecodeError::new(path, format!("cannot open event file: {error}")))?;
    let mut reader = BufReader::new(file);
    let mut header_bytes = [0_u8; FILE_HEADER_BYTES];
    let header_read = read_up_to(&mut reader, &mut header_bytes)
        .map_err(|error| DecodeError::new(path, format!("cannot read file header: {error}")))?;
    if header_read != FILE_HEADER_BYTES {
        return Err(DecodeError::new(
            path,
            format!("truncated file header ({header_read}/{FILE_HEADER_BYTES} bytes)"),
        ));
    }
    let header = FileHeader::decode(&header_bytes)
        .map_err(|error| DecodeError::new(path, format!("invalid file header: {error}")))?;

    let mut outcome = DecodeOutcome {
        path: path.to_path_buf(),
        header,
        health: Health::Clean,
        issues: Vec::new(),
        report: FileReport {
            committed_bytes: FILE_HEADER_BYTES as u64,
            ..FileReport::default()
        },
    };
    let mut producers: BTreeMap<u64, ProducerState> = BTreeMap::new();
    let mut producer_ends: BTreeMap<u64, ProducerEnd> = BTreeMap::new();
    let mut expected_chunk = 0_u64;
    let mut offset = FILE_HEADER_BYTES as u64;
    let mut saw_session_end = false;

    loop {
        let chunk_offset = offset;
        let mut encoded_header = [0_u8; CHUNK_HEADER_BYTES];
        let read = read_up_to(&mut reader, &mut encoded_header).map_err(|error| {
            DecodeError::new(
                path,
                format!("cannot read chunk header at {offset}: {error}"),
            )
        })?;
        offset = offset.saturating_add(read as u64);
        if read == 0 {
            break;
        }
        if saw_session_end {
            add_issue(
                &mut outcome,
                Health::Corrupt,
                chunk_offset,
                "bytes appear after SessionEnd",
            );
            break;
        }
        if read != CHUNK_HEADER_BYTES {
            add_issue(
                &mut outcome,
                Health::Unclean,
                chunk_offset,
                format!("truncated chunk header ({read}/{CHUNK_HEADER_BYTES} bytes); tail ignored"),
            );
            break;
        }
        if &encoded_header[..8] != super::format::CHUNK_MAGIC {
            add_issue(
                &mut outcome,
                Health::Unclean,
                chunk_offset,
                "uncommitted tail does not begin with a chunk header; tail ignored",
            );
            break;
        }
        let chunk = match ChunkHeader::decode(&encoded_header) {
            Ok(chunk) => chunk,
            Err(error) => {
                add_issue(
                    &mut outcome,
                    Health::Corrupt,
                    chunk_offset,
                    format!("invalid chunk header: {error}"),
                );
                break;
            }
        };
        if chunk.chunk_ordinal != expected_chunk {
            add_issue(
                &mut outcome,
                Health::Corrupt,
                chunk_offset,
                format!(
                    "chunk ordinal is {}, expected {expected_chunk}",
                    chunk.chunk_ordinal
                ),
            );
            break;
        }
        let payload_len = usize::try_from(chunk.payload_bytes)
            .expect("v0 chunk maximum always fits the supported host");
        let mut payload = vec![0_u8; payload_len];
        let payload_read = read_up_to(&mut reader, &mut payload).map_err(|error| {
            DecodeError::new(
                path,
                format!("cannot read chunk payload at {offset}: {error}"),
            )
        })?;
        offset = offset.saturating_add(payload_read as u64);
        if payload_read != payload_len {
            add_issue(
                &mut outcome,
                Health::Unclean,
                chunk_offset,
                format!(
                    "truncated chunk payload ({payload_read}/{payload_len} bytes); tail ignored"
                ),
            );
            break;
        }
        let footer_offset = offset;
        let mut encoded_footer = [0_u8; CHUNK_FOOTER_BYTES];
        let footer_read = read_up_to(&mut reader, &mut encoded_footer).map_err(|error| {
            DecodeError::new(
                path,
                format!("cannot read chunk footer at {offset}: {error}"),
            )
        })?;
        offset = offset.saturating_add(footer_read as u64);
        if footer_read != CHUNK_FOOTER_BYTES {
            add_issue(
                &mut outcome,
                Health::Unclean,
                footer_offset,
                format!(
                    "truncated chunk footer ({footer_read}/{CHUNK_FOOTER_BYTES} bytes); tail ignored"
                ),
            );
            break;
        }
        if &encoded_footer[..8] != super::format::COMMIT_MAGIC {
            add_issue(
                &mut outcome,
                Health::Unclean,
                footer_offset,
                "chunk has no complete commit footer; tail ignored",
            );
            break;
        }
        let footer = match ChunkFooter::decode(&encoded_footer) {
            Ok(footer) => footer,
            Err(error) => {
                add_issue(
                    &mut outcome,
                    Health::Corrupt,
                    footer_offset,
                    format!("invalid chunk footer: {error}"),
                );
                break;
            }
        };
        if let Err(error) = footer.verify(&chunk, &encoded_header, &payload) {
            add_issue(
                &mut outcome,
                Health::Corrupt,
                footer_offset,
                error.to_string(),
            );
            break;
        }

        outcome.report.committed_chunks += 1;
        outcome.report.committed_bytes = outcome
            .report
            .committed_bytes
            .checked_add(
                CHUNK_HEADER_BYTES as u64 + chunk.payload_bytes + CHUNK_FOOTER_BYTES as u64,
            )
            .ok_or_else(|| DecodeError::new(path, "committed byte count overflow"))?;
        let violations_before = outcome.report.contract_violations;
        match parse_payload(
            &outcome.header,
            &chunk,
            &payload,
            &mut outcome.report,
            &mut producers,
            &mut producer_ends,
            visitor,
        ) {
            Ok(found_end) => {
                saw_session_end |= found_end;
            }
            Err(ParseFailure::Format(message)) => {
                add_issue(
                    &mut outcome,
                    Health::Corrupt,
                    chunk_offset,
                    format!("invalid committed chunk: {message}"),
                );
                break;
            }
            Err(ParseFailure::Visitor(error)) => {
                return Err(DecodeError::new(
                    path,
                    format!("cannot emit decoded event: {error}"),
                ));
            }
        }
        if outcome.report.contract_violations != violations_before {
            add_issue(
                &mut outcome,
                Health::Corrupt,
                chunk_offset,
                "committed records violate the v0 non-nested syscall pairing contract",
            );
        }
        if saw_session_end
            && let Err(reason) = validate_session_end(
                &outcome.report,
                &producers,
                &producer_ends,
                expected_chunk + 1,
            )
        {
            add_issue(&mut outcome, Health::Corrupt, chunk_offset, reason);
            break;
        }
        expected_chunk += 1;
    }

    if outcome.health != Health::Corrupt && !saw_session_end {
        clear_all_open(&mut producers, &mut outcome.report);
        add_issue(
            &mut outcome,
            Health::Unclean,
            offset,
            "SessionEnd is absent; only committed chunks are trusted",
        );
    }
    Ok(outcome)
}

#[allow(clippy::too_many_arguments)]
fn parse_payload(
    file: &FileHeader,
    chunk: &ChunkHeader,
    payload: &[u8],
    report: &mut FileReport,
    producers: &mut BTreeMap<u64, ProducerState>,
    producer_ends: &mut BTreeMap<u64, ProducerEnd>,
    visitor: &mut dyn FnMut(&DecodedEvent) -> io::Result<()>,
) -> Result<bool, ParseFailure> {
    let mut position = 0_usize;
    let mut blocks = 0_u32;
    let mut pages = 0_u32;
    let mut found_session_end = false;
    while position < payload.len() {
        if blocks >= chunk.block_count {
            return Err(ParseFailure::Format(
                "payload contains more blocks than its chunk header".into(),
            ));
        }
        let control = Control::decode(&payload[position..])?;
        let block_len = control.byte_len();
        let end = position
            .checked_add(block_len)
            .ok_or_else(|| ParseFailure::Format("block length overflow".into()))?;
        let block = payload.get(position..end).ok_or_else(|| {
            ParseFailure::Format(format!(
                "block kind=0x{:04x} extends past chunk payload",
                control.kind
            ))
        })?;
        blocks += 1;
        match (control.kind, control.version) {
            (KIND_PAGE, 0) => {
                if found_session_end {
                    return Err(ParseFailure::Format("Page appears after SessionEnd".into()));
                }
                parse_page(file, block, report, producers, visitor)?;
                pages += 1;
            }
            (KIND_PRODUCER_END, 0) => {
                if found_session_end {
                    return Err(ParseFailure::Format(
                        "ProducerEnd appears after SessionEnd".into(),
                    ));
                }
                let end = ProducerEnd::decode(block)?;
                parse_producer_end(end, report, producers, producer_ends)?;
            }
            (KIND_SESSION_END, 0) => {
                if found_session_end || blocks != chunk.block_count || end != payload.len() {
                    return Err(ParseFailure::Format(
                        "SessionEnd must be the final block of its final chunk".into(),
                    ));
                }
                let session_end = SessionEnd::decode(block)?;
                report.session_end = Some(session_end);
                found_session_end = true;
            }
            _ => {
                // Top-level blocks have no inherited Engine context. Their control length is
                // sufficient to skip a future version without guessing its contents.
                if control.kind == KIND_PAGE {
                    pages += 1;
                }
                report.unknown_records += 1;
            }
        }
        position = end;
    }
    if blocks != chunk.block_count {
        return Err(ParseFailure::Format(format!(
            "decoded {blocks} blocks, chunk header declares {}",
            chunk.block_count
        )));
    }
    if pages != chunk.page_count {
        return Err(ParseFailure::Format(format!(
            "decoded {pages} pages, chunk header declares {}",
            chunk.page_count
        )));
    }
    report.committed_pages = report
        .committed_pages
        .checked_add(u64::from(pages))
        .ok_or_else(|| ParseFailure::Format("committed page count overflow".into()))?;
    Ok(found_session_end)
}

fn checked_sum(left: u64, right: u64, what: &str) -> Result<u64, Malformed> {
    left.checked_add(right)
        .ok_or_else(|| Malformed::new(format!("SessionEnd {what} sum overflow")))
}

fn merge_report_delta(report: &mut FileReport, delta: FileReport) -> Result<(), ParseFailure> {
    macro_rules! merge_count {
        ($field:ident) => {
            report.$field = report.$field.checked_add(delta.$field).ok_or_else(|| {
                ParseFailure::Format(concat!(stringify!($field), " count overflow").into())
            })?;
        };
    }
    merge_count!(records);
    merge_count!(producer_ends);
    merge_count!(unknown_records);
    merge_count!(sequence_gaps);
    merge_count!(incomplete_enters);
    merge_count!(orphan_exits);
    merge_count!(contract_violations);
    for (kind, count) in delta.kind_counts {
        let total = report.kind_counts.entry(kind).or_default();
        *total = total
            .checked_add(count)
            .ok_or_else(|| ParseFailure::Format("record kind count overflow".into()))?;
    }
    for (engine, count) in delta.engine_counts {
        let total = report.engine_counts.entry(engine).or_default();
        *total = total
            .checked_add(count)
            .ok_or_else(|| ParseFailure::Format("Engine record count overflow".into()))?;
    }
    report.producers.extend(delta.producers);
    report.threads.extend(delta.threads);
    Ok(())
}

fn add_issue(outcome: &mut DecodeOutcome, health: Health, offset: u64, message: impl Into<String>) {
    outcome.health.worsen(health);
    outcome.issues.push(DecodeIssue {
        offset,
        message: message.into(),
    });
}

fn read_up_to(reader: &mut impl Read, buffer: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..])? {
            0 => break,
            read => filled += read,
        }
    }
    Ok(filled)
}

#[cfg(test)]
mod tests;
