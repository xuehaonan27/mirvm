//! Streaming verifier and decoder for committed v0 event chunks.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::{Path, PathBuf};

use super::format::{
    CHUNK_FOOTER_BYTES, CHUNK_HEADER_BYTES, ChunkFooter, ChunkHeader, Control,
    ENGINE_CONTEXT_BYTES, EngineContext, FILE_HEADER_BYTES, FLAG_CONTEXT_CONTROL, FileHeader,
    KIND_ENGINE_CONTEXT, KIND_PAGE, KIND_PRODUCER_END, KIND_SESSION_END, KIND_SYSCALL_ENTER,
    KIND_SYSCALL_EXIT, PAGE_HEADER_BYTES, PageHeader, ProducerEnd, SYSCALL_ENTER_BYTES,
    SYSCALL_EXIT_BYTES, SessionEnd, SyscallEnter, SyscallExit, SyscallSemantics,
};

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

#[derive(Clone, Copy, Debug)]
struct OpenEnter {
    sequence: u64,
    semantics: SyscallSemantics,
}

#[derive(Clone, Debug, Default)]
struct ProducerState {
    identity: Option<(u32, u32)>,
    next_page_ordinal: u64,
    next_sequence: Option<u64>,
    records: u64,
    pages: u64,
    sequence_gaps: u64,
    open: Vec<OpenEnter>,
    ended: bool,
}

#[derive(Debug)]
enum ParseFailure {
    Format(String),
    Visitor(io::Error),
}

impl From<super::format::WireError> for ParseFailure {
    fn from(value: super::format::WireError) -> Self {
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
            && let Err(message) = validate_session_end(
                &outcome.report,
                &producers,
                &producer_ends,
                expected_chunk + 1,
            )
        {
            add_issue(&mut outcome, Health::Corrupt, chunk_offset, message);
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

fn parse_page(
    file: &FileHeader,
    block: &[u8],
    report: &mut FileReport,
    producers: &mut BTreeMap<u64, ProducerState>,
    visitor: &mut dyn FnMut(&DecodedEvent) -> io::Result<()>,
) -> Result<(), ParseFailure> {
    let encoded_header = block
        .get(..PAGE_HEADER_BYTES)
        .ok_or_else(|| ParseFailure::Format("truncated Page header".into()))?;
    let page = PageHeader::decode(encoded_header)?;
    if PAGE_HEADER_BYTES + page.used_bytes as usize != block.len() {
        return Err(ParseFailure::Format(
            "Page block length disagrees with used bytes".into(),
        ));
    }
    // A page is the smallest trusted event unit. Decode it against scratch
    // state so a bad tail cannot publish earlier records to the visitor or
    // retain partial accounting.
    let mut state = producers
        .get(&page.producer_id)
        .cloned()
        .unwrap_or_default();
    let mut page_report = FileReport::default();
    if !state.open.is_empty() {
        clear_open(&mut state, &mut page_report);
        page_report.contract_violations += 1;
    }
    if state.ended {
        return Err(ParseFailure::Format(format!(
            "producer {} publishes a page after ProducerEnd",
            page.producer_id
        )));
    }
    match state.identity {
        None => state.identity = Some((page.thread_generation, page.os_tid)),
        Some(identity) if identity == (page.thread_generation, page.os_tid) => {}
        Some(identity) => {
            return Err(ParseFailure::Format(format!(
                "producer {} identity changed from {identity:?} to ({}, {})",
                page.producer_id, page.thread_generation, page.os_tid
            )));
        }
    }
    if page.page_ordinal != state.next_page_ordinal {
        return Err(ParseFailure::Format(format!(
            "producer {} page ordinal is {}, expected {}",
            page.producer_id, page.page_ordinal, state.next_page_ordinal
        )));
    }
    state.next_page_ordinal = state
        .next_page_ordinal
        .checked_add(1)
        .ok_or_else(|| ParseFailure::Format("page ordinal overflow".into()))?;
    state.pages = state
        .pages
        .checked_add(1)
        .ok_or_else(|| ParseFailure::Format("producer page count overflow".into()))?;
    let expected = state.next_sequence.unwrap_or(0);
    if page.first_sequence < expected {
        return Err(ParseFailure::Format(format!(
            "producer {} sequence overlaps: page starts {}, expected at least {expected}",
            page.producer_id, page.first_sequence
        )));
    }
    if page.first_sequence > expected {
        add_gap(&mut state, &mut page_report, page.first_sequence - expected)?;
    }

    page_report.producers.insert(page.producer_id);
    page_report
        .threads
        .insert((page.producer_id, page.thread_generation, page.os_tid));
    let mut engine = Some(page.initial_engine_id);
    let mut context_poisoned = false;
    let mut position = PAGE_HEADER_BYTES;
    let mut record_index = 0_u64;
    let mut events = Vec::new();
    while position < block.len() {
        let control = Control::decode(&block[position..])?;
        let record_len = control.byte_len();
        let end = position
            .checked_add(record_len)
            .ok_or_else(|| ParseFailure::Format("page record length overflow".into()))?;
        let record_bytes = block.get(position..end).ok_or_else(|| {
            ParseFailure::Format(format!(
                "page record kind=0x{:04x} extends past used bytes",
                control.kind
            ))
        })?;
        let sequence = page
            .first_sequence
            .checked_add(record_index)
            .ok_or_else(|| ParseFailure::Format("event sequence overflow".into()))?;
        let kind = match (control.kind, control.version) {
            (KIND_SYSCALL_ENTER, 0) => {
                if record_len != SYSCALL_ENTER_BYTES {
                    return Err(ParseFailure::Format(
                        "SyscallEnter has non-v0 length".into(),
                    ));
                }
                let enter = SyscallEnter::decode(record_bytes)?;
                if !state.open.is_empty() {
                    page_report.incomplete_enters += state.open.len() as u64;
                    state.open.clear();
                    page_report.contract_violations += 1;
                }
                state.open.push(OpenEnter {
                    sequence,
                    semantics: enter.semantics,
                });
                DecodedKind::SyscallEnter(enter)
            }
            (KIND_SYSCALL_EXIT, 0) => {
                if record_len != SYSCALL_EXIT_BYTES {
                    return Err(ParseFailure::Format("SyscallExit has non-v0 length".into()));
                }
                let exit = SyscallExit::decode(record_bytes)?;
                let enter_sequence = match state.open.pop() {
                    Some(open) if open.semantics == exit.semantics => Some(open.sequence),
                    Some(_) => {
                        page_report.incomplete_enters += 1;
                        page_report.orphan_exits += 1;
                        page_report.contract_violations += 1;
                        None
                    }
                    None => {
                        page_report.orphan_exits += 1;
                        None
                    }
                };
                DecodedKind::SyscallExit {
                    record: exit,
                    enter_sequence,
                }
            }
            (KIND_ENGINE_CONTEXT, 0) => {
                if record_len != ENGINE_CONTEXT_BYTES {
                    return Err(ParseFailure::Format(
                        "EngineContext has non-v0 length".into(),
                    ));
                }
                let context = EngineContext::decode(record_bytes)?;
                if !state.open.is_empty() {
                    clear_open(&mut state, &mut page_report);
                    page_report.contract_violations += 1;
                }
                if !context_poisoned {
                    engine = Some(context.engine_id);
                }
                DecodedKind::EngineContext(context)
            }
            (KIND_PAGE | KIND_PRODUCER_END | KIND_SESSION_END, 0) => {
                return Err(ParseFailure::Format(format!(
                    "top-level block kind 0x{:04x} appears inside a Page",
                    control.kind
                )));
            }
            _ => {
                if control.flags & FLAG_CONTEXT_CONTROL != 0 {
                    if !state.open.is_empty() {
                        clear_open(&mut state, &mut page_report);
                        page_report.contract_violations += 1;
                    }
                    context_poisoned = true;
                    engine = None;
                }
                page_report.unknown_records += 1;
                DecodedKind::Unknown {
                    kind: control.kind,
                    version: control.version,
                    flags: control.flags,
                    bytes: record_bytes.to_vec(),
                }
            }
        };
        page_report.records = page_report
            .records
            .checked_add(1)
            .ok_or_else(|| ParseFailure::Format("record count overflow".into()))?;
        state.records = state
            .records
            .checked_add(1)
            .ok_or_else(|| ParseFailure::Format("producer record count overflow".into()))?;
        *page_report.kind_counts.entry(kind.kind_id()).or_default() += 1;
        *page_report.engine_counts.entry(engine).or_default() += 1;
        let event = DecodedEvent {
            context: EventContext {
                session_id: file.session_id,
                pid: file.pid,
                process_generation: file.process_generation,
                producer_id: page.producer_id,
                thread_generation: page.thread_generation,
                os_tid: page.os_tid,
                sequence,
                engine_id: engine,
            },
            kind,
        };
        events.push(event);
        record_index += 1;
        position = end;
    }
    let minimum_next = page
        .first_sequence
        .checked_add(record_index)
        .ok_or_else(|| ParseFailure::Format("page sequence range overflow".into()))?;
    if page.next_sequence != minimum_next {
        return Err(ParseFailure::Format(format!(
            "producer {} page next_sequence {} does not equal encoded next sequence {minimum_next}",
            page.producer_id, page.next_sequence
        )));
    }
    state.next_sequence = Some(page.next_sequence);

    merge_report_delta(report, page_report)?;
    producers.insert(page.producer_id, state);
    for event in events {
        visitor(&event).map_err(ParseFailure::Visitor)?;
    }
    Ok(())
}

fn parse_producer_end(
    end: ProducerEnd,
    report: &mut FileReport,
    producers: &mut BTreeMap<u64, ProducerState>,
    producer_ends: &mut BTreeMap<u64, ProducerEnd>,
) -> Result<(), ParseFailure> {
    if producer_ends.contains_key(&end.producer_id) {
        return Err(ParseFailure::Format(format!(
            "duplicate ProducerEnd for producer {}",
            end.producer_id
        )));
    }
    if end.next_sequence != end.attempted {
        return Err(ParseFailure::Format(format!(
            "ProducerEnd next_sequence={} but attempted={}",
            end.next_sequence, end.attempted
        )));
    }
    let mut state = producers.get(&end.producer_id).cloned().unwrap_or_default();
    let mut end_report = FileReport::default();
    if let Some(identity) = state.identity {
        if identity != (end.thread_generation, end.os_tid) {
            return Err(ParseFailure::Format(format!(
                "ProducerEnd identity for {} does not match its pages",
                end.producer_id
            )));
        }
    } else {
        state.identity = Some((end.thread_generation, end.os_tid));
    }
    let expected = state.next_sequence.unwrap_or(0);
    if end.next_sequence < expected {
        return Err(ParseFailure::Format(format!(
            "ProducerEnd sequence {} precedes page sequence {expected}",
            end.next_sequence
        )));
    }
    if end.next_sequence > expected {
        add_gap(&mut state, &mut end_report, end.next_sequence - expected)?;
    }
    if end.committed != state.records {
        return Err(ParseFailure::Format(format!(
            "ProducerEnd committed={} but file contains {} records for producer {}",
            end.committed, state.records, end.producer_id
        )));
    }
    let declared_missing = end
        .drop_capacity
        .checked_add(end.drop_context)
        .and_then(|count| count.checked_add(end.drop_recursive))
        .and_then(|count| count.checked_add(end.sink_loss))
        .ok_or_else(|| ParseFailure::Format("ProducerEnd loss total overflow".into()))?;
    if state.sequence_gaps != declared_missing {
        return Err(ParseFailure::Format(format!(
            "ProducerEnd declares {declared_missing} dropped or sink-lost records, but sequence gaps contain {}",
            state.sequence_gaps
        )));
    }
    if end.page_count < state.pages {
        return Err(ParseFailure::Format(format!(
            "ProducerEnd page_count={} is below {} committed pages for producer {}",
            end.page_count, state.pages, end.producer_id
        )));
    }
    let missing_pages = end.page_count - state.pages;
    if end.sink_loss != 0 && missing_pages == 0 {
        return Err(ParseFailure::Format(format!(
            "ProducerEnd declares {} sink-lost records but no missing page for producer {}",
            end.sink_loss, end.producer_id
        )));
    }
    if missing_pages > end.sink_loss {
        return Err(ParseFailure::Format(format!(
            "ProducerEnd is missing {missing_pages} pages but declares only {} sink-lost records for producer {}",
            end.sink_loss, end.producer_id
        )));
    }
    clear_open(&mut state, &mut end_report);
    state.next_sequence = Some(end.next_sequence);
    state.ended = true;
    end_report.producers.insert(end.producer_id);
    end_report
        .threads
        .insert((end.producer_id, end.thread_generation, end.os_tid));
    end_report.producer_ends += 1;
    merge_report_delta(report, end_report)?;
    producers.insert(end.producer_id, state);
    producer_ends.insert(end.producer_id, end);
    Ok(())
}

fn validate_session_end(
    report: &FileReport,
    producers: &BTreeMap<u64, ProducerState>,
    producer_ends: &BTreeMap<u64, ProducerEnd>,
    committed_chunks: u64,
) -> Result<(), String> {
    let end = report
        .session_end
        .as_ref()
        .ok_or_else(|| "SessionEnd marker was not retained".to_string())?;
    if producers.values().any(|producer| !producer.ended) {
        return Err("SessionEnd appears before every observed producer has ProducerEnd".into());
    }
    if end.producer_count != producer_ends.len() as u64 {
        return Err(format!(
            "SessionEnd producer_count={} but {} ProducerEnd blocks were committed",
            end.producer_count,
            producer_ends.len()
        ));
    }
    let mut attempted = 0_u64;
    let mut encoded = 0_u64;
    let mut committed = 0_u64;
    let mut drop_capacity = 0_u64;
    let mut drop_context = 0_u64;
    let mut drop_recursive = 0_u64;
    let mut sink_loss = 0_u64;
    for producer in producer_ends.values() {
        attempted = checked_sum(attempted, producer.attempted, "attempted")?;
        encoded = checked_sum(encoded, producer.encoded, "encoded")?;
        committed = checked_sum(committed, producer.committed, "committed")?;
        drop_capacity = checked_sum(drop_capacity, producer.drop_capacity, "capacity drop")?;
        drop_context = checked_sum(drop_context, producer.drop_context, "context drop")?;
        drop_recursive = checked_sum(drop_recursive, producer.drop_recursive, "recursive drop")?;
        sink_loss = checked_sum(sink_loss, producer.sink_loss, "sink loss")?;
    }
    for (name, declared, observed) in [
        ("attempted", end.attempted, attempted),
        ("encoded", end.encoded, encoded),
        ("committed", end.committed, committed),
        ("drop_capacity", end.drop_capacity, drop_capacity),
        ("drop_context", end.drop_context, drop_context),
        ("drop_recursive", end.drop_recursive, drop_recursive),
        ("sink_loss", end.sink_loss, sink_loss),
    ] {
        if declared != observed {
            return Err(format!(
                "SessionEnd {name}={declared}, ProducerEnd sum is {observed}"
            ));
        }
    }
    if end.committed != report.records {
        return Err(format!(
            "SessionEnd committed={} but decoder observed {} records",
            end.committed, report.records
        ));
    }
    if end.chunks_committed != committed_chunks {
        return Err(format!(
            "SessionEnd chunks_committed={} but decoder observed {committed_chunks}",
            end.chunks_committed
        ));
    }
    if end.pages_committed != report.committed_pages {
        return Err(format!(
            "SessionEnd pages_committed={} but decoder observed {}",
            end.pages_committed, report.committed_pages
        ));
    }
    if end.bytes_committed != report.committed_bytes {
        return Err(format!(
            "SessionEnd bytes_committed={} but decoder observed {}",
            end.bytes_committed, report.committed_bytes
        ));
    }
    Ok(())
}

fn checked_sum(left: u64, right: u64, what: &str) -> Result<u64, String> {
    left.checked_add(right)
        .ok_or_else(|| format!("SessionEnd {what} sum overflow"))
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

fn add_gap(
    state: &mut ProducerState,
    report: &mut FileReport,
    gap: u64,
) -> Result<(), ParseFailure> {
    state.sequence_gaps = state
        .sequence_gaps
        .checked_add(gap)
        .ok_or_else(|| ParseFailure::Format("producer sequence gap count overflow".into()))?;
    report.sequence_gaps = report
        .sequence_gaps
        .checked_add(gap)
        .ok_or_else(|| ParseFailure::Format("sequence gap count overflow".into()))?;
    clear_open(state, report);
    Ok(())
}

fn clear_open(state: &mut ProducerState, report: &mut FileReport) {
    report.incomplete_enters = report
        .incomplete_enters
        .saturating_add(state.open.len() as u64);
    state.open.clear();
}

fn clear_all_open(producers: &mut BTreeMap<u64, ProducerState>, report: &mut FileReport) {
    for producer in producers.values_mut() {
        clear_open(producer, report);
    }
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
