//! One sealed page of a producer's ring: its header, the engine-context markers inside it, the
//! syscall records, and the per-producer bookkeeping that survives from page to page.

use super::super::format::{
    Control, ENGINE_CONTEXT_BYTES, EngineContext, FLAG_CONTEXT_CONTROL, FileHeader,
    KIND_ENGINE_CONTEXT, KIND_PAGE, KIND_PRODUCER_END, KIND_SESSION_END, KIND_SYSCALL_ENTER,
    KIND_SYSCALL_EXIT, PAGE_HEADER_BYTES, PageHeader, SYSCALL_ENTER_BYTES, SYSCALL_EXIT_BYTES,
    SyscallEnter, SyscallExit, SyscallSemantics,
};
use super::{
    DecodedEvent, DecodedKind, EventContext, FileReport, ParseFailure, merge_report_delta,
};
use std::collections::BTreeMap;
use std::io;

#[derive(Clone, Copy, Debug)]
pub(super) struct OpenEnter {
    sequence: u64,
    semantics: SyscallSemantics,
}

#[derive(Clone, Debug, Default)]
pub(super) struct ProducerState {
    pub(super) identity: Option<(u32, u32)>,
    pub(super) next_page_ordinal: u64,
    pub(super) next_sequence: Option<u64>,
    pub(super) records: u64,
    pub(super) pages: u64,
    pub(super) sequence_gaps: u64,
    pub(super) open: Vec<OpenEnter>,
    pub(super) ended: bool,
}

pub(super) fn parse_page(
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

pub(super) fn add_gap(
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

pub(super) fn clear_open(state: &mut ProducerState, report: &mut FileReport) {
    report.incomplete_enters = report
        .incomplete_enters
        .saturating_add(state.open.len() as u64);
    state.open.clear();
}

pub(super) fn clear_all_open(
    producers: &mut BTreeMap<u64, ProducerState>,
    report: &mut FileReport,
) {
    for producer in producers.values_mut() {
        clear_open(producer, report);
    }
}
