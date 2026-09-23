//! Capture tests, split by what each group drives: the pinned and inline hot paths, the fork
//! hooks, the session lifecycle, the page ring's accounting, and the writer's per-round schedule.
//!
//! The fork, path and writer-scan helpers are shared, so they live here rather than in whichever
//! group uses them most. A test that re-runs this binary by name passes its own module path to
//! `--exact`, so the module a group lives in is part of that test's contract.

use super::*;
use crate::os::process::{EDOM, ENOSYS, SYS_EXIT, SYS_FORK, SYS_GETPID, SYS_GETPPID};
use crate::telemetry::capture_session::{
    REBUILD_RECIPE, RebuildRecipe, clear_rebuild_recipe, pending_rebuild_recipe,
    publish_rebuild_recipe,
};
use crate::telemetry::capture_writer::{
    partial_path, reclaim_session_pages, sealed_page_bytes, write_file_header, writer_main,
    writer_offer_starter, writer_reap_retired,
};
use crate::telemetry::decode::{DecodedKind, Health, decode_file};
use crate::telemetry::format::{EngineContext, PageHeader};
use std::ffi::c_void;
use std::fs::OpenOptions;
use std::io;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

mod fork;
mod hot_path;
mod ring;
mod session;
mod writer;

static NEXT_PATH: AtomicU64 = AtomicU64::new(1);

fn test_path() -> PathBuf {
    let id = NEXT_PATH.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("mirvm-capture-{}-{id}.mlog", std::process::id()))
}

/// Fork through the product path and hand the parent the `N` bytes the child reports.
///
/// `report` runs in the child between `fork` and `_exit`, where the allocator lock may be held by a
/// thread the fork did not duplicate, so it may only read and compute.
fn forked_report<const N: usize>(report: impl FnOnce() -> [u8; N]) -> [u8; N] {
    let [read_end, write_end] = crate::os::fs::pipe().expect("pipe");
    // The product fork path, so the child hook runs (a raw libc fork does not).
    let pid = host_syscall(SYS_FORK, &[]);
    assert!(pid >= 0, "fork failed");
    if pid == 0 {
        crate::os::fs::close_fd(read_end);
        let payload = report();
        let written = crate::os::fs::write_fd(write_end, payload.as_ptr() as u64, payload.len());
        crate::os::process::exit_now(if written == payload.len() as i64 {
            0
        } else {
            3
        });
    }
    crate::os::fs::close_fd(write_end);
    let mut payload = [0_u8; N];
    let mut read = 0;
    while read < N {
        let got = crate::os::fs::read_fd(read_end, payload[read..].as_mut_ptr() as u64, N - read);
        if got <= 0 {
            break;
        }
        read += got as usize;
    }
    crate::os::fs::close_fd(read_end);
    let status = crate::os::process::wait(pid as i32).expect("waitpid");
    assert!(status.exited(), "child did not exit normally");
    assert_eq!(status.code(), 0, "child failed to report");
    assert_eq!(read, N, "short read from the child");
    payload
}

/// Fork through the product path, run `child` in the child, and require a clean exit.
///
/// Same restriction as [`forked_report`]: `child` runs after `fork` and before `_exit`.
fn fork_and_wait(child: impl FnOnce()) {
    let pid = host_syscall(SYS_FORK, &[]);
    assert!(pid >= 0, "fork failed");
    if pid == 0 {
        child();
        crate::os::process::exit_now(0);
    }
    let status = crate::os::process::wait(pid as i32).expect("waitpid");
    assert!(status.exited(), "child did not exit normally");
    assert_eq!(status.code(), 0, "child failed to report");
}

fn writer_active_scan_len(core: &SessionCore) -> usize {
    core.active_producers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .len()
}
