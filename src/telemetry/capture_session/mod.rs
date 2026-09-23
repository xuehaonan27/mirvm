//! Capture session lifecycle: one process-wide session, its per-thread activation, the fork
//! rebuild, and the process generation it claims.
//!
//! The producer primitives and the process-wide state live in the sibling `capture` module. Here
//! live the session object a caller holds ([`activation`] and the two module-private halves it
//! reaches): [`activation`] is what a guest thread does to join and leave a session, [`rebuild`]
//! is what a `fork` child does to publish and reopen one, and [`generation`] is the process-wide
//! counters a session claims so that a parent and its children never share them.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::capture::{
    ACTIVE, CaptureSummary, EnterDisposition, HotEnter, PHASE_ARMED, PHASE_FINISHED,
    PHASE_STOPPING, PagePool, Producer, START_LOCK, SessionCore, TLS_ACTIVATION_DEPTH,
    TLS_ACTIVE_PRODUCER, TLS_CACHED_PRODUCER, TLS_CACHED_SESSION, WRITER_AWAKE, open_page,
    record_syscall_enter, record_syscall_enter_inline, record_syscall_exit, seal_page, set_engine,
};
use super::capture_writer::{partial_path, write_file_header, writer_main};

mod activation;
mod generation;
mod rebuild;

pub(crate) use activation::{
    ActivationToken, activation_enter, activation_exit, current_producer, host_syscall,
    host_syscall_pinned, is_armed, retire_current_thread,
};
pub(crate) use generation::{NEXT_SESSION_ID, claim_process_generation};
pub(crate) use rebuild::{
    after_fork_child, arm_lingering_writer, build_session_core, clear_rebuild_recipe,
    fork_child_guard, rebuild_on_boundary,
};
// The recipe is published and read inside the rebuild that owns it; only a test reaches the
// address or the pending value directly.
#[cfg(test)]
pub(crate) use rebuild::{
    REBUILD_RECIPE, RebuildRecipe, pending_rebuild_recipe, publish_rebuild_recipe,
};

/// Internal construction options. The byte cap is a hard process budget, not
/// a correctness switch; a producer that cannot obtain pages remains attached
/// and automatically retries when the writer returns pages to the pool.
pub(crate) struct StartOptions {
    pub(crate) output: PathBuf,
    pub(crate) page_budget_bytes: usize,
    /// Fork generation to record. `None` claims the next one for this process,
    /// which is what a caller that does not name its file after the generation
    /// wants; callers that do name the file must pass the same value they used
    /// for the name.
    pub(crate) process_generation: Option<u64>,
}

impl StartOptions {
    pub(crate) fn new(output: impl Into<PathBuf>, page_budget_bytes: usize) -> Self {
        Self {
            output: output.into(),
            page_budget_bytes,
            process_generation: None,
        }
    }

    pub(crate) fn with_process_generation(mut self, generation: u64) -> Self {
        self.process_generation = Some(generation);
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FinishStatus {
    Finished(CaptureSummary),
    TimedOut,
}

pub(crate) struct CaptureSession {
    pub(super) core: &'static SessionCore,
    writer: Option<JoinHandle<io::Result<CaptureSummary>>>,
    owner_pid: i32,
    /// A `fork` child's own session outlives every handle in its process. Its
    /// `Drop` only stops the session and hands the bounded drain to the exit
    /// hook; the creator's `Drop` would otherwise look like the owner taking the
    /// writer down.
    lingering: bool,
}

impl CaptureSession {
    pub(crate) fn start(options: StartOptions) -> io::Result<Self> {
        let _start = START_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        if !ACTIVE.load(Ordering::Acquire).is_null() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "a capture session is already active or draining",
            ));
        }
        match std::fs::symlink_metadata(&options.output) {
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "the final capture file already exists",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let partial = partial_path(&options.output);
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&partial)?;
        let process_generation = match options.process_generation {
            Some(generation) => generation,
            None => claim_process_generation(),
        };
        let (core, writer) = build_session_core(
            file,
            &partial,
            options.output,
            options.page_budget_bytes,
            process_generation,
        )?;
        Ok(Self {
            core,
            writer: Some(writer),
            owner_pid: crate::os::process::getpid(),
            lingering: false,
        })
    }

    pub(crate) fn request_stop(&self) {
        if crate::os::process::getpid() != self.owner_pid {
            return;
        }
        // No new child should expect a rebuild once the owner has stopped.
        clear_rebuild_recipe();
        let _ = self.core.phase.compare_exchange(
            PHASE_ARMED,
            PHASE_STOPPING,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        self.core.wake_writer();
    }

    pub(crate) fn finish(&mut self, timeout: Duration) -> io::Result<FinishStatus> {
        if crate::os::process::getpid() != self.owner_pid {
            return Ok(FinishStatus::TimedOut);
        }
        self.request_stop();
        let started = Instant::now();
        while !self.core.writer_done.load(Ordering::Acquire) {
            if started.elapsed() >= timeout {
                return Ok(FinishStatus::TimedOut);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let Some(writer) = self.writer.take() else {
            return Err(io::Error::other("capture writer was already joined"));
        };
        let summary = writer
            .join()
            .map_err(|_| io::Error::other("capture writer panicked"))??;
        Ok(FinishStatus::Finished(summary))
    }
}

impl Drop for CaptureSession {
    fn drop(&mut self) {
        self.request_stop();
        if !self.lingering {
            return;
        }
        // Nobody will call `finish` for a forked child's session; arrange for the
        // process's exit hook to drain and publish it instead, without relying on TLS
        // destructors or on a caller remembering to finish.
        arm_lingering_writer(self.core);
    }
}
