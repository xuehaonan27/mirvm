//! Capture writer thread: seals published pages into committed chunks, writes
//! the file header and the end ledger, and publishes the finished file.
//!
//! The writer owns a published page until its complete chunk has reached the
//! kernel; it is the only place that performs capture file I/O.

use std::fs::File;
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::Ordering;

use super::capture::{
    ACTIVE, CaptureSummary, PHASE_ARMED, PHASE_FINISHED, PHASE_SINK_FAILED, Page, Producer,
    SessionCore, WRITER_AWAKE, WRITER_SLEEPING,
};
use super::capture_session::NEXT_SESSION_ID;
use crate::telemetry::format::{
    CHUNK_FOOTER_BYTES, CHUNK_HEADER_BYTES, CLOCK_NONE, ChunkFooter, ChunkHeader, FileHeader,
    PAGE_HEADER_BYTES, PRODUCER_END_BYTES, ProducerEnd, SESSION_END_BYTES, SessionEnd, WireError,
};

pub(super) fn writer_main(
    core: &'static SessionCore,
    file: File,
    mut offset: i64,
    partial_path: &Path,
    final_path: &Path,
) -> io::Result<CaptureSummary> {
    // The guest cannot have created this thread, so the fork guard must not
    // count it as a guest pthread. Registration happens
    // before the thread is reachable, and the guard rebuilds the baseline after
    // fork.
    let _service = crate::os::thread::ServiceThreadGuard::register();
    let fd = file.as_raw_fd();
    let mut sink_error: Option<io::Error> = None;
    let mut stats = WriterStats::default();
    loop {
        let mut did_work = false;
        let producers = core
            .active_producers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        for producer_addr in producers {
            let producer = unsafe { &*(producer_addr as *const Producer) };
            if writer_has_page(producer) {
                did_work = true;
                let page = writer_page(producer);
                let bytes = sealed_page_bytes(page);
                let record_count = page_record_count(bytes);
                if sink_error.is_none() {
                    match pwrite_chunk(fd, &mut offset, stats.chunk_ordinal, bytes, 1, 1) {
                        Ok(()) => {
                            unsafe {
                                *producer.writer.committed_records.get() =
                                    (*producer.writer.committed_records.get())
                                        .wrapping_add(record_count);
                            }
                            stats.chunk_ordinal = stats.chunk_ordinal.wrapping_add(1);
                            stats.chunks_committed = stats.chunks_committed.wrapping_add(1);
                            stats.pages_committed = stats.pages_committed.wrapping_add(1);
                        }
                        Err(error) => {
                            core.phase.store(PHASE_SINK_FAILED, Ordering::Release);
                            note_sink_loss(core, producer, record_count);
                            sink_error = Some(error);
                        }
                    }
                } else {
                    note_sink_loss(core, producer, record_count);
                }
                writer_return_page(producer);
            }
            if producer.pages.load(Ordering::Acquire).is_null()
                && writer_offer_starter(core, producer)
            {
                did_work = true;
            }
        }
        if writer_reap_retired(core) {
            did_work = true;
        }

        let stopping = core.phase.load(Ordering::Acquire) != PHASE_ARMED;
        if stopping && core.active_roots.load(Ordering::Acquire) == 0 && !any_published(core) {
            break;
        }
        if did_work {
            continue;
        }

        core.writer_state.swap(WRITER_SLEEPING, Ordering::AcqRel);
        if any_published(core)
            || (core.phase.load(Ordering::Acquire) != PHASE_ARMED
                && core.active_roots.load(Ordering::Acquire) == 0)
        {
            core.writer_state.store(WRITER_AWAKE, Ordering::Release);
            continue;
        }
        let _ = crate::os::thread::futex_wait_raw(core.writer_state.as_ptr(), WRITER_SLEEPING);
        core.writer_state.store(WRITER_AWAKE, Ordering::Release);
    }

    reclaim_session_pages(core);
    let summary = summarize(core);
    let result = if let Some(error) = sink_error {
        Err(error)
    } else {
        write_end_chunk(core, fd, &mut offset, &stats, summary)
            .and_then(|()| {
                drop(file);
                publish_without_replace(partial_path, final_path)
            })
            .map(|()| summary)
    };
    core.phase.store(PHASE_FINISHED, Ordering::Release);
    let core_ptr = core as *const SessionCore as *mut SessionCore;
    let _ = ACTIVE.compare_exchange(
        core_ptr,
        ptr::null_mut(),
        Ordering::AcqRel,
        Ordering::Acquire,
    );
    core.writer_done.store(true, Ordering::Release);
    result
}

pub(super) fn reclaim_session_pages(core: &SessionCore) {
    debug_assert_eq!(core.active_roots.load(Ordering::Acquire), 0);
    let producers = core
        .producers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    for addr in producers {
        let producer = unsafe { &*(addr as *const Producer) };
        debug_assert!(!unsafe { (&*producer.cold.get()).has_active });
        debug_assert!(!writer_has_page(producer));
        let pages = producer.pages.swap(ptr::null_mut(), Ordering::AcqRel);
        core.page_pool.return_starter(pages);
    }
    core.active_producers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    unsafe { core.page_pool.release_all() };
}

#[derive(Default)]
struct WriterStats {
    chunk_ordinal: u64,
    chunks_committed: u64,
    pages_committed: u64,
}

fn note_sink_loss(core: &SessionCore, producer: &Producer, records: u64) {
    core.sink_loss.fetch_add(records, Ordering::Relaxed);
    unsafe {
        *producer.writer.sink_loss.get() = (*producer.writer.sink_loss.get()).wrapping_add(records);
    }
}

fn writer_has_page(producer: &Producer) -> bool {
    let head = unsafe { *producer.writer.head.get() };
    head != producer.published.tail.load(Ordering::Acquire)
}

fn writer_page(producer: &Producer) -> &Page {
    let head = unsafe { *producer.writer.head.get() };
    unsafe { producer.page((head & 1) as usize) }
}

fn writer_return_page(producer: &Producer) {
    let head = unsafe { &mut *producer.writer.head.get() };
    *head = head.wrapping_add(1);
    producer.returned.head.store(*head, Ordering::Release);
}

pub(super) fn writer_offer_starter(core: &SessionCore, producer: &Producer) -> bool {
    if producer.retired.retired.load(Ordering::Acquire) {
        return false;
    }
    let pages = core.page_pool.take_starter();
    if pages.is_null() {
        return false;
    }
    if producer.retired.retired.load(Ordering::Acquire) {
        core.page_pool.return_starter(pages);
        return false;
    }
    if producer
        .pages
        .compare_exchange(ptr::null_mut(), pages, Ordering::Release, Ordering::Acquire)
        .is_err()
    {
        core.page_pool.return_starter(pages);
        return false;
    }
    true
}

pub(super) fn writer_reap_retired(core: &SessionCore) -> bool {
    let mut removed = false;
    core.active_producers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|addr| {
            let producer = unsafe { &*(*addr as *const Producer) };
            if !producer.retired.retired.load(Ordering::Acquire) || writer_has_page(producer) {
                return true;
            }
            let pages = producer.pages.swap(ptr::null_mut(), Ordering::AcqRel);
            core.page_pool.return_starter(pages);
            removed = true;
            false
        });
    removed
}

fn any_published(core: &SessionCore) -> bool {
    core.active_producers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .copied()
        .any(|addr| writer_has_page(unsafe { &*(addr as *const Producer) }))
}

pub(super) fn sealed_page_bytes(page: &Page) -> &[u8] {
    let bytes = unsafe { &*page.bytes.get() };
    let used = u32::from_le_bytes(bytes[60..64].try_into().unwrap()) as usize;
    &bytes[..PAGE_HEADER_BYTES + used]
}

fn page_record_count(bytes: &[u8]) -> u64 {
    if bytes.len() < PAGE_HEADER_BYTES {
        return 0;
    }
    let first = u64::from_le_bytes(bytes[16..24].try_into().unwrap());
    let next = u64::from_le_bytes(bytes[24..32].try_into().unwrap());
    next.wrapping_sub(first)
}

fn summarize(core: &SessionCore) -> CaptureSummary {
    let producers = core
        .producers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let mut summary = CaptureSummary {
        producers: producers.len() as u64,
        sink_loss: core.sink_loss.load(Ordering::Relaxed),
        ..CaptureSummary::default()
    };
    for addr in producers {
        let producer = unsafe { &*(addr as *const Producer) };
        // No trace root remains, so the producer-only ledger is quiescent.
        let cold = unsafe { &*producer.cold.get() };
        let drops = cold
            .capacity_drops
            .wrapping_add(cold.context_drops)
            .wrapping_add(cold.recursive_drops);
        summary.producer_drops = summary.producer_drops.wrapping_add(drops);
        summary.encoded_records = summary
            .encoded_records
            .wrapping_add(cold.next_sequence.wrapping_sub(drops));
    }
    summary
}

fn pwrite_chunk(
    fd: i32,
    offset: &mut i64,
    chunk_ordinal: u64,
    payload: &[u8],
    block_count: u32,
    page_count: u32,
) -> io::Result<()> {
    let header = ChunkHeader::new(chunk_ordinal, payload.len() as u64, block_count, page_count)
        .map_err(wire_io)?;
    let mut encoded_header = header.to_le_bytes();
    let footer = ChunkFooter::for_slices(&header, &encoded_header, [payload]).map_err(wire_io)?;
    let mut encoded_footer = footer.to_le_bytes();
    let mut iov = [
        libc::iovec {
            iov_base: encoded_header.as_mut_ptr().cast(),
            iov_len: CHUNK_HEADER_BYTES,
        },
        libc::iovec {
            iov_base: payload.as_ptr() as *mut libc::c_void,
            iov_len: payload.len(),
        },
        libc::iovec {
            iov_base: encoded_footer.as_mut_ptr().cast(),
            iov_len: CHUNK_FOOTER_BYTES,
        },
    ];
    pwritev_all(fd, &mut iov, offset)
}

fn pwritev_all(fd: i32, iov: &mut [libc::iovec], offset: &mut i64) -> io::Result<()> {
    let mut first = 0;
    while first < iov.len() {
        let count = iov.len() - first;
        let written = unsafe { libc::pwritev(fd, iov[first..].as_ptr(), count as i32, *offset) };
        if written < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "capture pwritev made no progress",
            ));
        }
        *offset = offset
            .checked_add(written as i64)
            .ok_or_else(|| io::Error::other("capture file offset overflow"))?;
        let mut remaining = written as usize;
        while remaining != 0 {
            if remaining >= iov[first].iov_len {
                remaining -= iov[first].iov_len;
                first += 1;
            } else {
                iov[first].iov_base = unsafe {
                    (iov[first].iov_base as *mut u8)
                        .add(remaining)
                        .cast::<libc::c_void>()
                };
                iov[first].iov_len -= remaining;
                remaining = 0;
            }
        }
    }
    Ok(())
}

pub(super) fn partial_path(final_path: &Path) -> PathBuf {
    let mut name = final_path.as_os_str().to_os_string();
    name.push(".partial");
    PathBuf::from(name)
}

fn publish_without_replace(partial_path: &Path, final_path: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let partial = CString::new(partial_path.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "partial capture path contains a NUL byte",
        )
    })?;
    let final_path = CString::new(final_path.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "final capture path contains a NUL byte",
        )
    })?;
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            partial.as_ptr(),
            libc::AT_FDCWD,
            final_path.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

pub(super) fn write_file_header(
    file: &mut File,
    pid: libc::pid_t,
    process_generation: u64,
) -> io::Result<i64> {
    let sequence = NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed);
    let mut session_id = [0_u8; 16];
    session_id[..8].copy_from_slice(&(pid as u64).to_le_bytes());
    session_id[8..].copy_from_slice(&sequence.to_le_bytes());
    let build_id = u64::from_str_radix(crate::options::build::BUILD_ID, 16)
        .map_err(|error| io::Error::other(format!("invalid MIRVM_BUILD_ID: {error}")))?;
    let bytes = FileHeader {
        pointer_width: std::mem::size_of::<usize>() as u8,
        clock_kind: CLOCK_NONE,
        pid: pid as u32,
        session_id,
        build_id,
        process_generation,
        segment_number: 0,
        monotonic_anchor: 0,
        wall_unix_ns: 0,
        clock_frequency_num: 0,
        clock_frequency_den: 0,
    }
    .to_le_bytes();
    file.write_all(&bytes)?;
    Ok(bytes.len() as i64)
}

fn write_end_chunk(
    core: &SessionCore,
    fd: i32,
    offset: &mut i64,
    stats: &WriterStats,
    summary: CaptureSummary,
) -> io::Result<()> {
    let producers = core
        .producers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let payload_len = producers
        .len()
        .checked_mul(PRODUCER_END_BYTES)
        .and_then(|bytes| bytes.checked_add(SESSION_END_BYTES))
        .ok_or_else(|| io::Error::other("capture end ledger length overflow"))?;
    let mut payload = Vec::with_capacity(payload_len);
    let mut drop_capacity = 0_u64;
    let mut drop_context = 0_u64;
    let mut drop_recursive = 0_u64;
    for addr in &producers {
        let producer = unsafe { &*(*addr as *const Producer) };
        let cold = unsafe { &*producer.cold.get() };
        let committed = unsafe { *producer.writer.committed_records.get() };
        let sink_loss = unsafe { *producer.writer.sink_loss.get() };
        let drops = cold
            .capacity_drops
            .checked_add(cold.context_drops)
            .and_then(|n| n.checked_add(cold.recursive_drops))
            .ok_or_else(|| io::Error::other("producer drop count overflow"))?;
        let encoded = cold
            .next_sequence
            .checked_sub(drops)
            .ok_or_else(|| io::Error::other("producer sequence ledger underflow"))?;
        let page_count = cold.page_ordinal;
        let encoded_end = ProducerEnd {
            producer_id: producer.producer_id,
            next_sequence: cold.next_sequence,
            attempted: cold.next_sequence,
            encoded,
            committed,
            drop_capacity: cold.capacity_drops,
            drop_context: cold.context_drops,
            drop_recursive: cold.recursive_drops,
            sink_loss,
            page_count,
            last_page_ordinal: page_count.checked_sub(1).unwrap_or(u64::MAX),
            thread_generation: producer.thread_generation,
            os_tid: producer.tid,
            flags: 0,
        }
        .to_le_bytes()
        .map_err(wire_io)?;
        payload.extend_from_slice(&encoded_end);
        drop_capacity = drop_capacity.wrapping_add(cold.capacity_drops);
        drop_context = drop_context.wrapping_add(cold.context_drops);
        drop_recursive = drop_recursive.wrapping_add(cold.recursive_drops);
    }

    let committed = summary
        .encoded_records
        .checked_sub(summary.sink_loss)
        .ok_or_else(|| io::Error::other("session sink ledger underflow"))?;
    let encoded_session_end = SessionEnd {
        producer_count: producers.len() as u64,
        attempted: summary
            .encoded_records
            .checked_add(summary.producer_drops)
            .ok_or_else(|| io::Error::other("session attempt count overflow"))?,
        encoded: summary.encoded_records,
        committed,
        drop_capacity,
        drop_context,
        drop_recursive,
        sink_loss: summary.sink_loss,
        chunks_committed: stats.chunks_committed.wrapping_add(1),
        pages_committed: stats.pages_committed,
        bytes_committed: (*offset as u64)
            .checked_add(CHUNK_HEADER_BYTES as u64)
            .and_then(|n| n.checked_add(payload_len as u64))
            .and_then(|n| n.checked_add(CHUNK_FOOTER_BYTES as u64))
            .ok_or_else(|| io::Error::other("session committed byte count overflow"))?,
        write_error_count: 0,
        first_write_errno: 0,
        last_write_errno: 0,
        flags: 0,
    }
    .to_le_bytes()
    .map_err(wire_io)?;
    payload.extend_from_slice(&encoded_session_end);
    debug_assert_eq!(payload.len(), payload_len);
    pwrite_chunk(
        fd,
        offset,
        stats.chunk_ordinal,
        &payload,
        (producers.len() + 1) as u32,
        0,
    )
}

fn wire_io(error: WireError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}
