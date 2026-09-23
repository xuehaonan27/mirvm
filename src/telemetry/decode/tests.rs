//! Decoder tests: golden files, commit/truncation boundaries, and the v0
//! non-nested syscall pairing contract.

use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};

use super::*;
use crate::telemetry::format::{
    CLOCK_NONE, ChunkFooter, ChunkHeader, Control, EngineContext, FLAG_CONTEXT_CONTROL, FileHeader,
    PAGE_BYTES_4K, PAGE_HEADER_BYTES, PRODUCER_END_BYTES, PageHeader, ProducerEnd,
    SESSION_END_BYTES, SYSCALL_ENTER_BYTES, SYSCALL_EXIT_BYTES, SessionEnd, SyscallEnter,
    SyscallExit, SyscallSemantics,
};

static NEXT_TEST_FILE: AtomicU64 = AtomicU64::new(0);

fn temp_file(name: &str, bytes: &[u8]) -> PathBuf {
    let id = NEXT_TEST_FILE.fetch_add(1, Ordering::Relaxed);
    let path =
        std::env::temp_dir().join(format!("mirvm-log-{name}-{}-{id}.mlog", std::process::id()));
    fs::write(&path, bytes).unwrap();
    path
}

fn set_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn recommit_only_chunk(bytes: &mut [u8]) {
    let chunk_start = FILE_HEADER_BYTES;
    let payload_start = chunk_start + CHUNK_HEADER_BYTES;
    let encoded_chunk: [u8; CHUNK_HEADER_BYTES] =
        bytes[chunk_start..payload_start].try_into().unwrap();
    let chunk = ChunkHeader::decode(&encoded_chunk).unwrap();
    let payload_end = payload_start + chunk.payload_bytes as usize;
    let footer =
        ChunkFooter::for_slices(&chunk, &encoded_chunk, [&bytes[payload_start..payload_end]])
            .unwrap()
            .to_le_bytes();
    bytes[payload_end..payload_end + CHUNK_FOOTER_BYTES].copy_from_slice(&footer);
}

fn clean_file() -> Vec<u8> {
    let enter = SyscallEnter {
        semantics: SyscallSemantics::Libc,
        nr: 1,
        args: [2, 3, 4, 5, 6, 7],
    }
    .to_le_bytes()
    .to_vec();
    let exit = SyscallExit {
        semantics: SyscallSemantics::Libc,
        result: 8,
        errno: None,
    }
    .to_le_bytes()
    .unwrap()
    .to_vec();
    file_with_records(&[enter, exit], 5)
}

fn file_with_records(records: &[Vec<u8>], initial_engine_id: u64) -> Vec<u8> {
    let file_header = FileHeader {
        pointer_width: 8,
        clock_kind: CLOCK_NONE,
        pid: 42,
        session_id: [0x11; 16],
        build_id: 0x0102_0304_0506_0708,
        process_generation: 3,
        segment_number: 0,
        monotonic_anchor: 0,
        wall_unix_ns: 0,
        clock_frequency_num: 0,
        clock_frequency_den: 0,
    };
    let record_bytes: usize = records.iter().map(Vec::len).sum();
    let record_count = records.len() as u64;
    let page_header = PageHeader {
        producer_id: 9,
        first_sequence: 0,
        next_sequence: record_count,
        initial_engine_id,
        page_ordinal: 0,
        thread_generation: 1,
        os_tid: 43,
        page_bytes: PAGE_BYTES_4K,
        used_bytes: record_bytes as u32,
    }
    .to_le_bytes()
    .unwrap();
    let producer_end = ProducerEnd {
        producer_id: 9,
        next_sequence: record_count,
        attempted: record_count,
        encoded: record_count,
        committed: record_count,
        drop_capacity: 0,
        drop_context: 0,
        drop_recursive: 0,
        sink_loss: 0,
        page_count: 1,
        last_page_ordinal: 0,
        thread_generation: 1,
        os_tid: 43,
        flags: 0,
    }
    .to_le_bytes()
    .unwrap();
    let payload_bytes = PAGE_HEADER_BYTES + record_bytes + PRODUCER_END_BYTES + SESSION_END_BYTES;
    let file_bytes = FILE_HEADER_BYTES + CHUNK_HEADER_BYTES + payload_bytes + CHUNK_FOOTER_BYTES;
    let session_end = SessionEnd {
        producer_count: 1,
        attempted: record_count,
        encoded: record_count,
        committed: record_count,
        drop_capacity: 0,
        drop_context: 0,
        drop_recursive: 0,
        sink_loss: 0,
        chunks_committed: 1,
        pages_committed: 1,
        bytes_committed: file_bytes as u64,
        write_error_count: 0,
        first_write_errno: 0,
        last_write_errno: 0,
        flags: 0,
    }
    .to_le_bytes()
    .unwrap();
    let mut payload = Vec::with_capacity(payload_bytes);
    payload.extend_from_slice(&page_header);
    for record in records {
        payload.extend_from_slice(record);
    }
    payload.extend_from_slice(&producer_end);
    payload.extend_from_slice(&session_end);
    let chunk = ChunkHeader::new(0, payload.len() as u64, 3, 1).unwrap();
    let encoded_chunk = chunk.to_le_bytes();
    let footer = ChunkFooter::for_slices(&chunk, &encoded_chunk, [&payload[..]])
        .unwrap()
        .to_le_bytes();
    let mut file = Vec::with_capacity(file_bytes);
    file.extend_from_slice(&file_header.to_le_bytes());
    file.extend_from_slice(&encoded_chunk);
    file.extend_from_slice(&payload);
    file.extend_from_slice(&footer);
    file
}

#[test]
fn golden_file_decodes_and_pairs_records() {
    let bytes = clean_file();
    // This independent digest makes an accidental wire-layout change fail even if the
    // encoder and decoder change together.
    assert_eq!(
        blake3::hash(&bytes).to_hex().as_str(),
        "cd2ef612e2ea7fe2dc5768da34a43530225cc4d6dd34a51d73cab7d2f57ea8cf"
    );
    let path = temp_file("golden", &bytes);
    let mut events = Vec::new();
    let outcome = decode_file(&path, &mut |event| {
        events.push(event.clone());
        Ok(())
    })
    .unwrap();
    fs::remove_file(path).unwrap();
    assert_eq!(outcome.health, Health::Clean);
    assert!(outcome.issues.is_empty());
    assert_eq!(outcome.report.records, 2);
    assert_eq!(events.len(), 2);
    assert!(matches!(events[0].kind, DecodedKind::SyscallEnter(_)));
    assert!(matches!(
        events[1].kind,
        DecodedKind::SyscallExit {
            enter_sequence: Some(0),
            ..
        }
    ));
}

#[test]
fn every_truncated_chunk_boundary_preserves_only_committed_prefix() {
    let bytes = clean_file();
    for length in [
        FILE_HEADER_BYTES,
        FILE_HEADER_BYTES + 1,
        FILE_HEADER_BYTES + CHUNK_HEADER_BYTES - 1,
        FILE_HEADER_BYTES + CHUNK_HEADER_BYTES,
        bytes.len() - CHUNK_FOOTER_BYTES,
        bytes.len() - 1,
    ] {
        let path = temp_file("truncated", &bytes[..length]);
        let outcome = decode_file(&path, &mut |_| Ok(())).unwrap();
        fs::remove_file(path).unwrap();
        assert_eq!(outcome.health, Health::Unclean, "length={length}");
        assert_eq!(outcome.report.committed_chunks, 0, "length={length}");
        assert_eq!(outcome.report.records, 0, "length={length}");
    }
}

#[test]
fn committed_checksum_corruption_is_not_treated_as_truncation() {
    let mut bytes = clean_file();
    bytes[FILE_HEADER_BYTES + CHUNK_HEADER_BYTES + PAGE_HEADER_BYTES + 8] ^= 1;
    let path = temp_file("corrupt", &bytes);
    let outcome = decode_file(&path, &mut |_| Ok(())).unwrap();
    fs::remove_file(path).unwrap();
    assert_eq!(outcome.health, Health::Corrupt);
    assert_eq!(outcome.report.committed_chunks, 0);
    assert!(outcome.issues[0].message.contains("checksum"));
}

#[test]
fn unknown_versions_skip_and_unknown_context_poison_lasts_until_next_page() {
    let context = EngineContext { engine_id: 7 }.to_le_bytes().to_vec();
    let unknown_context = Control::new(0x7f01, FLAG_CONTEXT_CONTROL, 8)
        .to_le_bytes()
        .to_vec();
    let ignored_context = EngineContext { engine_id: 9 }.to_le_bytes().to_vec();
    let mut future_enter = SyscallEnter {
        semantics: SyscallSemantics::Raw,
        nr: 1,
        args: [0; 6],
    }
    .to_le_bytes()
    .to_vec();
    future_enter[2] = 1;
    let bytes = file_with_records(
        &[context, unknown_context, ignored_context, future_enter],
        5,
    );
    let path = temp_file("unknown", &bytes);
    let mut events = Vec::new();
    let outcome = decode_file(&path, &mut |event| {
        events.push(event.clone());
        Ok(())
    })
    .unwrap();
    fs::remove_file(path).unwrap();
    assert_eq!(outcome.health, Health::Clean);
    assert_eq!(outcome.report.unknown_records, 2);
    assert_eq!(events[0].context.engine_id, Some(7));
    assert_eq!(events[1].context.engine_id, None);
    assert_eq!(events[2].context.engine_id, None);
    assert_eq!(events[3].context.engine_id, None);
    assert!(matches!(
        events[3].kind,
        DecodedKind::Unknown { version: 1, .. }
    ));
}

#[test]
fn engine_context_closes_an_open_enter_and_marks_contract_violation() {
    let enter = SyscallEnter {
        semantics: SyscallSemantics::Libc,
        nr: 1,
        args: [0; 6],
    }
    .to_le_bytes()
    .to_vec();
    let context = EngineContext { engine_id: 7 }.to_le_bytes().to_vec();
    let bytes = file_with_records(&[enter, context], 5);
    let path = temp_file("context-inside-syscall", &bytes);
    let outcome = decode_file(&path, &mut |_| Ok(())).unwrap();
    fs::remove_file(path).unwrap();

    assert_eq!(outcome.health, Health::Corrupt);
    assert_eq!(outcome.report.incomplete_enters, 1);
    assert_eq!(outcome.report.contract_violations, 1);
}

#[test]
fn unknown_context_control_also_closes_an_open_enter() {
    let enter = SyscallEnter {
        semantics: SyscallSemantics::Libc,
        nr: 1,
        args: [0; 6],
    }
    .to_le_bytes()
    .to_vec();
    let unknown_context = Control::new(0x7f01, FLAG_CONTEXT_CONTROL, 8)
        .to_le_bytes()
        .to_vec();
    let bytes = file_with_records(&[enter, unknown_context], 5);
    let path = temp_file("unknown-context-inside-syscall", &bytes);
    let outcome = decode_file(&path, &mut |_| Ok(())).unwrap();
    fs::remove_file(path).unwrap();

    assert_eq!(outcome.health, Health::Corrupt);
    assert_eq!(outcome.report.incomplete_enters, 1);
    assert_eq!(outcome.report.contract_violations, 1);
}

#[test]
fn producer_end_requires_next_sequence_to_equal_attempted() {
    let mut bytes = clean_file();
    let producer_end = FILE_HEADER_BYTES
        + CHUNK_HEADER_BYTES
        + PAGE_HEADER_BYTES
        + SYSCALL_ENTER_BYTES
        + SYSCALL_EXIT_BYTES;
    set_u64(&mut bytes, producer_end + 16, 3);
    recommit_only_chunk(&mut bytes);

    let path = temp_file("end-sequence", &bytes);
    let outcome = decode_file(&path, &mut |_| Ok(())).unwrap();
    fs::remove_file(path).unwrap();

    assert_eq!(outcome.health, Health::Corrupt);
    assert!(
        outcome
            .issues
            .iter()
            .any(|issue| issue.message.contains("next_sequence=3 but attempted=2"))
    );
}

#[test]
fn producer_end_closes_sequence_gaps_against_loss_ledger() {
    let mut bytes = clean_file();
    let page = FILE_HEADER_BYTES + CHUNK_HEADER_BYTES;
    let producer_end = page + PAGE_HEADER_BYTES + SYSCALL_ENTER_BYTES + SYSCALL_EXIT_BYTES;
    let session_end = producer_end + PRODUCER_END_BYTES;

    // One capacity drop happened before the first committed page.
    set_u64(&mut bytes, page + 16, 1);
    set_u64(&mut bytes, page + 24, 3);
    set_u64(&mut bytes, producer_end + 16, 3);
    set_u64(&mut bytes, producer_end + 24, 3);
    set_u64(&mut bytes, producer_end + 48, 1);
    set_u64(&mut bytes, session_end + 16, 3);
    set_u64(&mut bytes, session_end + 40, 1);
    recommit_only_chunk(&mut bytes);

    let path = temp_file("loss-ledger", &bytes);
    let outcome = decode_file(&path, &mut |_| Ok(())).unwrap();
    fs::remove_file(path).unwrap();

    assert_eq!(outcome.health, Health::Clean);
    assert_eq!(outcome.report.sequence_gaps, 1);
}

#[test]
fn producer_end_rejects_sink_loss_without_a_missing_page() {
    let mut bytes = clean_file();
    let page = FILE_HEADER_BYTES + CHUNK_HEADER_BYTES;
    let producer_end = page + PAGE_HEADER_BYTES + SYSCALL_ENTER_BYTES + SYSCALL_EXIT_BYTES;
    let session_end = producer_end + PRODUCER_END_BYTES;

    set_u64(&mut bytes, producer_end + 16, 3);
    set_u64(&mut bytes, producer_end + 24, 3);
    set_u64(&mut bytes, producer_end + 32, 3);
    set_u64(&mut bytes, producer_end + 72, 1);
    set_u64(&mut bytes, session_end + 16, 3);
    set_u64(&mut bytes, session_end + 24, 3);
    set_u64(&mut bytes, session_end + 64, 1);
    recommit_only_chunk(&mut bytes);

    let path = temp_file("sink-ledger", &bytes);
    let outcome = decode_file(&path, &mut |_| Ok(())).unwrap();
    fs::remove_file(path).unwrap();

    assert_eq!(outcome.health, Health::Corrupt);
    assert!(outcome.issues.iter().any(|issue| {
        issue
            .message
            .contains("sink-lost records but no missing page")
    }));
}

#[test]
fn corrupt_page_tail_emits_no_events_from_that_page() {
    let mut bytes = clean_file();
    let page = FILE_HEADER_BYTES + CHUNK_HEADER_BYTES;
    set_u64(&mut bytes, page + 24, 1);
    recommit_only_chunk(&mut bytes);

    let path = temp_file("page-tail", &bytes);
    let mut events = Vec::new();
    let outcome = decode_file(&path, &mut |event| {
        events.push(event.clone());
        Ok(())
    })
    .unwrap();
    fs::remove_file(path).unwrap();

    assert_eq!(outcome.health, Health::Corrupt);
    assert_eq!(outcome.report.records, 0);
    assert!(events.is_empty());
}
