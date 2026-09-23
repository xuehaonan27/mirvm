//! The two ledgers: what a producer reports when it retires and what the session reports when it
//! closes. Both are checked against what the pages claimed, because a ledger that disagrees with
//! them is the corruption worth reporting.

use super::super::format::ProducerEnd;
use super::page::{ProducerState, add_gap, clear_open};
use super::{FileReport, Malformed, ParseFailure, checked_sum, merge_report_delta};
use std::collections::BTreeMap;

pub(super) fn parse_producer_end(
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

pub(super) fn validate_session_end(
    report: &FileReport,
    producers: &BTreeMap<u64, ProducerState>,
    producer_ends: &BTreeMap<u64, ProducerEnd>,
    committed_chunks: u64,
) -> Result<(), Malformed> {
    let end = report
        .session_end
        .as_ref()
        .ok_or_else(|| Malformed::new("SessionEnd marker was not retained"))?;
    if producers.values().any(|producer| !producer.ended) {
        return Err(Malformed::new(
            "SessionEnd appears before every observed producer has ProducerEnd",
        ));
    }
    if end.producer_count != producer_ends.len() as u64 {
        return Err(Malformed::new(format!(
            "SessionEnd producer_count={} but {} ProducerEnd blocks were committed",
            end.producer_count,
            producer_ends.len()
        )));
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
            return Err(Malformed::new(format!(
                "SessionEnd {name}={declared}, ProducerEnd sum is {observed}"
            )));
        }
    }
    if end.committed != report.records {
        return Err(Malformed::new(format!(
            "SessionEnd committed={} but decoder observed {} records",
            end.committed, report.records
        )));
    }
    if end.chunks_committed != committed_chunks {
        return Err(Malformed::new(format!(
            "SessionEnd chunks_committed={} but decoder observed {committed_chunks}",
            end.chunks_committed
        )));
    }
    if end.pages_committed != report.committed_pages {
        return Err(Malformed::new(format!(
            "SessionEnd pages_committed={} but decoder observed {}",
            end.pages_committed, report.committed_pages
        )));
    }
    if end.bytes_committed != report.committed_bytes {
        return Err(Malformed::new(format!(
            "SessionEnd bytes_committed={} but decoder observed {}",
            end.bytes_committed, report.committed_bytes
        )));
    }
    Ok(())
}
