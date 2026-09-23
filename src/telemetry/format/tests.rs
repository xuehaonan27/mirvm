//! Wire-format tests: every record's byte layout is fixed, and every reader refuses what it does
//! not understand.

use super::*;

#[test]
fn fixed_record_bytes_are_stable_and_round_trip() {
    let enter = SyscallEnter {
        semantics: SyscallSemantics::Raw,
        nr: 0x0102_0304,
        args: [1, 2, 3, 4, 5, u64::MAX],
    };
    let encoded = enter.to_le_bytes();
    assert_eq!(
        &encoded[..16],
        &[
            0x01, 0x01, 0x00, FLAG_RAW, 0x08, 0x00, 0x00, 0x00, 0x04, 0x03, 0x02, 0x01, 0x00, 0x00,
            0x00, 0x00,
        ]
    );
    assert_eq!(SyscallEnter::decode(&encoded).unwrap(), enter);

    let exit = SyscallExit {
        semantics: SyscallSemantics::Libc,
        result: -1,
        errno: Some(13),
    };
    let encoded = exit.to_le_bytes().unwrap();
    assert_eq!(
        &encoded[..8],
        &[0x02, 0x01, 0x00, FLAG_LIBC, 0x03, 0x00, 0x00, 0x00]
    );
    assert_eq!(&encoded[16..], &[13, 0, 0, 0, 1, 0, 0, 0]);
    assert_eq!(SyscallExit::decode(&encoded).unwrap(), exit);

    let context = EngineContext {
        engine_id: 0x0102_0304_0506_0708,
    };
    let encoded = context.to_le_bytes();
    assert_eq!(
        &encoded[..8],
        &[
            0x01,
            0x02,
            0x00,
            FLAG_CONTEXT_CONTROL,
            0x02,
            0x00,
            0x00,
            0x00,
        ]
    );
    assert_eq!(EngineContext::decode(&encoded).unwrap(), context);
}

#[test]
fn file_chunk_and_page_headers_round_trip() {
    let file = FileHeader {
        pointer_width: 8,
        clock_kind: CLOCK_NONE,
        pid: 123,
        session_id: [0x5a; 16],
        build_id: 0x0102_0304_0506_0708,
        process_generation: 9,
        segment_number: 0,
        monotonic_anchor: 0,
        wall_unix_ns: 0,
        clock_frequency_num: 0,
        clock_frequency_den: 0,
    };
    let encoded = file.to_le_bytes();
    assert_eq!(&encoded[..8], FILE_MAGIC);
    assert_eq!(FileHeader::decode(&encoded).unwrap(), file);

    let page = PageHeader {
        producer_id: 7,
        first_sequence: 10,
        next_sequence: 12,
        initial_engine_id: 3,
        page_ordinal: 4,
        thread_generation: 5,
        os_tid: 123,
        page_bytes: PAGE_BYTES_4K,
        used_bytes: SYSCALL_PAIR_BYTES as u32,
    };
    let encoded = page.to_le_bytes().unwrap();
    assert_eq!(PageHeader::decode(&encoded).unwrap(), page);

    let chunk = ChunkHeader::new(2, 152, 1, 1).unwrap();
    let encoded_header = chunk.to_le_bytes();
    let payload = [0xa5; 152];
    let footer = ChunkFooter::for_slices(&chunk, &encoded_header, [&payload[..]]).unwrap();
    let encoded_footer = footer.to_le_bytes();
    let decoded_footer = ChunkFooter::decode(&encoded_footer).unwrap();
    decoded_footer
        .verify(&chunk, &encoded_header, &payload)
        .unwrap();
}

#[test]
fn reserved_bits_and_invalid_errno_semantics_fail_loudly() {
    let mut enter = SyscallEnter {
        semantics: SyscallSemantics::Raw,
        nr: 1,
        args: [0; 6],
    }
    .to_le_bytes();
    enter[6] = 1;
    assert!(SyscallEnter::decode(&enter).is_err());

    let raw_errno = SyscallExit {
        semantics: SyscallSemantics::Raw,
        result: -1,
        errno: Some(1),
    };
    assert!(raw_errno.to_le_bytes().is_err());

    let mut success = SyscallExit {
        semantics: SyscallSemantics::Libc,
        result: 4,
        errno: None,
    }
    .to_le_bytes()
    .unwrap();
    success[16] = 2;
    assert!(SyscallExit::decode(&success).is_err());
}

#[test]
fn checksum_detects_header_and_payload_corruption() {
    let file = FileHeader {
        pointer_width: 8,
        clock_kind: CLOCK_NONE,
        pid: 1,
        session_id: [1; 16],
        build_id: 2,
        process_generation: 0,
        segment_number: 0,
        monotonic_anchor: 0,
        wall_unix_ns: 0,
        clock_frequency_num: 0,
        clock_frequency_den: 0,
    };
    let mut encoded = file.to_le_bytes();
    encoded[48] ^= 1;
    assert!(FileHeader::decode(&encoded).is_err());

    let chunk = ChunkHeader::new(0, 8, 1, 0).unwrap();
    let encoded_header = chunk.to_le_bytes();
    let payload = [1_u8; 8];
    let footer = ChunkFooter::for_slices(&chunk, &encoded_header, [&payload[..]]).unwrap();
    let mut corrupt = payload;
    corrupt[3] ^= 1;
    assert!(footer.verify(&chunk, &encoded_header, &corrupt).is_err());
}
