//! Capture session lifecycle: one process-wide session, its per-thread
//! activation, the fork rebuild, and the process generation it claims.
//!
//! The producer primitives and the process-wide state live in the parent
//! module; this module owns everything that starts, activates, retires or
//! rebuilds a session.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::writer::{partial_path, write_file_header, writer_main};
use super::{
    ACTIVE, CaptureSummary, EnterDisposition, HotEnter, PHASE_ARMED, PHASE_FINISHED,
    PHASE_STOPPING, PagePool, Producer, START_LOCK, SessionCore, TLS_ACTIVATION_DEPTH,
    TLS_ACTIVE_PRODUCER, TLS_CACHED_PRODUCER, TLS_CACHED_SESSION, WRITER_AWAKE, open_page,
    record_syscall_enter, record_syscall_enter_inline, record_syscall_exit, seal_page, set_engine,
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
    owner_pid: libc::pid_t,
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
            owner_pid: unsafe { libc::getpid() },
            lingering: false,
        })
    }

    pub(crate) fn request_stop(&self) {
        if unsafe { libc::getpid() } != self.owner_pid {
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
        if unsafe { libc::getpid() } != self.owner_pid {
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
        // process's exit hook to drain and publish it instead (design §6.2: no
        // reliance on TLS destructors or on a caller remembering to finish).
        arm_lingering_writer(self.core);
    }
}

/// A `fork` child may be inside a trace activation when it forks, so the rebuild
/// is deferred to the activation boundary rather than run from the kernel-side
/// hook, which may not open files or spawn threads.
static CHILD_NEEDS_REBUILD: AtomicBool = AtomicBool::new(false);

/// Session a `fork` child started for itself, waiting for the exit hook to drain
/// it. 0 when there is nothing pending.
static LINGERING_WRITER: AtomicUsize = AtomicUsize::new(0);

/// Process-exit hook: stop a child's own session and wait (bounded) for its
/// writer, so the file is published with a normal `End` instead of being left
/// half-written. `_exit`/signal exits never run this, and the file then stays as
/// `.partial`, which the decoder already repairs.
extern "C" fn drain_lingering_writer() {
    let address = LINGERING_WRITER.swap(0, Ordering::AcqRel);
    if address == 0 {
        return;
    }
    // SAFETY: the session core is leaked for the process lifetime, so the
    // address stays valid here.
    let core = unsafe { &*(address as *const SessionCore) };
    // A fork child has no owner that will call `finish`, so nobody has sealed
    // its producers' active pages. Without this the writer sees no published
    // page and the child's file keeps its records in memory instead of writing
    // chunks (L2). `seal_page` publishes and wakes the writer itself.
    let producers: Vec<usize> = core
        .active_producers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    for producer in producers {
        let producer = producer as *const Producer;
        // SAFETY: producers are leaked for the process lifetime, and the writer
        // only touches a page after `seal_page` publishes it.
        unsafe { seal_page(&*producer) };
    }
    let _ = core.phase.compare_exchange(
        PHASE_ARMED,
        PHASE_STOPPING,
        Ordering::AcqRel,
        Ordering::Acquire,
    );
    core.wake_writer();
    let _ = wait_for_writer_publish(core, Duration::from_secs(5));
}

/// Hand a lingering session to the process exit hook.
fn arm_lingering_writer(core: &'static SessionCore) {
    static HOOK: std::sync::Once = std::sync::Once::new();
    HOOK.call_once(|| unsafe {
        libc::atexit(drain_lingering_writer);
    });
    LINGERING_WRITER.store(core as *const SessionCore as usize, Ordering::Release);
}

/// Ordinary-boundary hook for every host builtin. Cheap when no fork happened:
/// one relaxed-ish atomic load.
pub(crate) fn rebuild_on_boundary() {
    if !CHILD_NEEDS_REBUILD.load(Ordering::Acquire) {
        return;
    }
    // A fork child keeps running inside the Engine it forked in, so it never
    // re-enters `activation_enter` and never gets a producer for its own
    // session. Without one, `host_syscall` sees a null producer and passes the
    // syscall through without recording, leaving the child's file empty. Attach
    // the producer here, where allocation and locking are allowed.
    let engine_id = {
        let inherited = TLS_ACTIVE_PRODUCER.load(Ordering::Relaxed);
        if inherited.is_null() {
            0
        } else {
            unsafe { (*(*inherited).cold_ptr()).current_engine }
        }
    };
    if !rebuild_session_from_recipe() {
        return;
    }
    let core = ACTIVE.load(Ordering::Acquire);
    if core.is_null() {
        return;
    }
    let producer = producer_for_session(core, engine_id);
    TLS_ACTIVE_PRODUCER.store(producer, Ordering::Relaxed);
    TLS_ACTIVATION_DEPTH.store(1, Ordering::Relaxed);
    if !producer.is_null() {
        unsafe {
            let producer = &*producer;
            (*producer.cold_ptr()).current_engine = engine_id;
            let _ = open_page(producer);
        }
    }
}

/// Build the session a `fork` child owes itself, from the parent's published
/// recipe (L2, design §6.3). Runs only on an ordinary boundary: it opens files
/// and spawns a thread, which the kernel-side fork hook may never do.
///
/// The child's first claim advances the generation it inherited, and that same
/// value ends up in both the file name and the header.
fn rebuild_session_from_recipe() -> bool {
    if !CHILD_NEEDS_REBUILD.swap(false, Ordering::AcqRel) {
        // Not a fork child, or the rebuild was already handled.
        return false;
    }
    let Some(recipe) = pending_rebuild_recipe() else {
        return false;
    };
    let _start = START_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if !ACTIVE.load(Ordering::Acquire).is_null() {
        // Another thread in this process already rebuilt, or a session was
        // started explicitly; either way this process is covered.
        return true;
    }
    let process_generation = claim_process_generation();
    let output = recipe
        .directory
        .join(format!("events-{}-{process_generation}.mlog", unsafe {
            libc::getpid()
        }));
    let attempt = || -> io::Result<()> {
        if std::fs::symlink_metadata(&output).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "the child capture file already exists",
            ));
        }
        let partial = partial_path(&output);
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&partial)?;
        let (core, writer) = build_session_core(
            file,
            &partial,
            output,
            recipe.page_budget_bytes,
            process_generation,
        )?;
        // The child's session has no owner that will call `finish`, so hand its
        // writer to the process exit hook.
        Box::leak(Box::new(CaptureSession {
            core,
            writer: Some(writer),
            owner_pid: unsafe { libc::getpid() },
            lingering: true,
        }));
        arm_lingering_writer(core);
        Ok(())
    };
    match attempt() {
        Ok(()) => true,
        Err(_) => {
            // Fails closed: recording stays off for this child instead of
            // retrying on every activation.
            clear_rebuild_recipe();
            false
        }
    }
}

type CaptureWriterHandle = std::thread::JoinHandle<io::Result<CaptureSummary>>;

/// Build one session's runtime state and start its writer. Shared by the normal
/// start path and by a `fork` child rebuilding its own session, so both get the
/// same page pool, writer protocol and publication order.
fn build_session_core(
    mut file: File,
    partial: &Path,
    final_path: PathBuf,
    page_budget_bytes: usize,
    process_generation: u64,
) -> io::Result<(&'static SessionCore, CaptureWriterHandle)> {
    let owner_pid = unsafe { libc::getpid() };
    let owner_tid = unsafe { libc::gettid() as u32 };
    let offset = write_file_header(&mut file, owner_pid, process_generation)?;
    let core = Box::leak(Box::new(SessionCore {
        phase: AtomicU8::new(PHASE_ARMED),
        active_roots: AtomicUsize::new(0),
        writer_state: AtomicU32::new(WRITER_AWAKE),
        writer_done: AtomicBool::new(false),
        producers: Mutex::new(Vec::new()),
        active_producers: Mutex::new(Vec::new()),
        next_producer: AtomicU64::new(1),
        next_thread_generation: AtomicU32::new(1),
        page_pool: PagePool::new(page_budget_bytes),
        sink_loss: AtomicU64::new(0),
        owner_tid,
    }));
    let core_addr = core as *const SessionCore as usize;
    // A fork child must be able to rebuild this session on its own, so the
    // recipe is published before the session becomes visible (L2).
    publish_rebuild_recipe(&final_path, page_budget_bytes);
    let partial = partial.to_path_buf();
    let writer = match std::thread::Builder::new()
        .name("mirvm-capture".into())
        .spawn(move || {
            // SAFETY: SessionCore is intentionally process-lifetime stable.
            let core = unsafe { &*(core_addr as *const SessionCore) };
            writer_main(core, file, offset, &partial, &final_path)
        }) {
        Ok(writer) => writer,
        Err(error) => {
            core.phase.store(PHASE_FINISHED, Ordering::Release);
            return Err(error);
        }
    };
    ACTIVE.store(core, Ordering::Release);
    Ok((core, writer))
}

/// Wait for a writer to finish and publish, bounded by `timeout` so an
/// unrecoverable I/O path cannot hang process exit. Returns whether the file
/// reached its final name (a timed-out file stays as `.partial` and is
/// recoverable).
fn wait_for_writer_publish(core: &SessionCore, timeout: Duration) -> bool {
    let started = Instant::now();
    while !core.writer_done.load(Ordering::Acquire) {
        if started.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    true
}

#[derive(Clone, Copy)]
pub(crate) struct ActivationToken {
    producer: *mut Producer,
    pub(super) session: *const SessionCore,
    outermost: bool,
    previous_engine: u64,
}

pub(crate) fn is_armed() -> bool {
    let core = ACTIVE.load(Ordering::Acquire);
    !core.is_null() && unsafe { (*core).phase.load(Ordering::Acquire) == PHASE_ARMED }
}

pub(crate) fn activation_enter(engine_id: u64) -> ActivationToken {
    if CHILD_NEEDS_REBUILD.load(Ordering::Acquire) {
        // First ordinary boundary after a fork: build this process's own
        // session before any producer tries to attach to a parent page.
        rebuild_session_from_recipe();
    }
    let active = TLS_ACTIVE_PRODUCER.load(Ordering::Relaxed);
    if !active.is_null() {
        TLS_ACTIVATION_DEPTH.fetch_add(1, Ordering::Relaxed);
        let previous_engine = unsafe { (*(*active).cold_ptr()).current_engine };
        if previous_engine != engine_id {
            unsafe { set_engine(&*active, engine_id) };
        }
        return ActivationToken {
            producer: active,
            session: unsafe { (*active).session },
            outermost: false,
            previous_engine,
        };
    }

    let core = ACTIVE.load(Ordering::Acquire);
    if core.is_null() || !unsafe { (*core).try_enter_root() } {
        return ActivationToken {
            producer: ptr::null_mut(),
            session: ptr::null(),
            outermost: false,
            previous_engine: 0,
        };
    }

    let producer = producer_for_session(core, engine_id);
    TLS_ACTIVE_PRODUCER.store(producer, Ordering::Relaxed);
    TLS_ACTIVATION_DEPTH.store(1, Ordering::Relaxed);
    if !producer.is_null() {
        unsafe {
            let producer = &*producer;
            (*producer.cold_ptr()).current_engine = engine_id;
            let _ = open_page(producer);
        }
    }
    ActivationToken {
        producer,
        session: core,
        outermost: true,
        previous_engine: 0,
    }
}

pub(crate) fn activation_exit(token: ActivationToken, restored_engine_id: u64) {
    if token.session.is_null() {
        return;
    }
    // A fork child cleared ACTIVE before returning to trace code. Stack guards
    // copied from the parent must never seal or account into that generation.
    if ACTIVE.load(Ordering::Acquire) != token.session.cast_mut() {
        return;
    }
    // SYS_exit/SYS_exit_group transfer the root ledger before entering the
    // kernel because no Rust guard will run on their successful path. A
    // seccomp policy may unexpectedly make such a syscall return; the copied
    // guard must then remain inert instead of decrementing the root twice.
    if TLS_ACTIVATION_DEPTH.load(Ordering::Relaxed) == 0 {
        return;
    }
    if token.outermost {
        if !token.producer.is_null() {
            unsafe { seal_page(&*token.producer) };
        }
        TLS_ACTIVE_PRODUCER.store(ptr::null_mut(), Ordering::Relaxed);
        TLS_ACTIVATION_DEPTH.store(0, Ordering::Relaxed);
        let core = unsafe { &*token.session };
        core.active_roots.fetch_sub(1, Ordering::Release);
        core.wake_writer();
        return;
    }

    TLS_ACTIVATION_DEPTH.fetch_sub(1, Ordering::Relaxed);
    debug_assert_eq!(token.previous_engine, restored_engine_id);
    if !token.producer.is_null() {
        unsafe { set_engine(&*token.producer, restored_engine_id) };
    }
}

/// Record the libc-semantics HostSyscall pair. With no active producer this is
/// exactly the original syscall path; stopped trace-capable Engines therefore
/// remain transparent.
pub(crate) fn host_syscall(nr: i64, args: &[u64]) -> i64 {
    let producer = TLS_ACTIVE_PRODUCER.load(Ordering::Relaxed);
    unsafe { run_libc_syscall(producer, nr, args, SyscallEnterPath::Cold).0 }
}

/// The trace code domain's syscall entry (design §5.2.3). The producer is the
/// one the activation boundary pinned, read from the register rather than from
/// thread-local storage, so recording performs no TLS lookup and no global
/// session check. Everything else -- the syscall itself, the fork guard, the
/// exit/exit_group hand-over and the errno that accompanies a `-1` result -- is
/// the same body the interpreter uses.
///
/// Returns the recorder the call actually used alongside the syscall result. A
/// `fork` child's first recording syscall replaces the recorder it inherited,
/// and only the trace domain needs that replacement told back to it, because
/// only the trace domain keeps a copy of the recorder in a register.
///
/// # Safety
///
/// `producer` must be the calling thread's live recorder, or null.
pub(crate) unsafe fn host_syscall_pinned(
    producer: *mut Producer,
    nr: i64,
    args: &[u64],
) -> (i64, *mut Producer) {
    unsafe { run_libc_syscall(producer, nr, args, SyscallEnterPath::Inline) }
}

/// Where an entry record comes from. The cold entry owns page rotation, drop
/// accounting and the sequence gap; the inline entry is the trace domain's hot
/// path and hands back to the cold entry whenever it cannot write in place.
#[derive(Clone, Copy)]
enum SyscallEnterPath {
    Cold,
    Inline,
}

/// The syscall result together with the recorder that recorded it.
unsafe fn run_libc_syscall(
    producer: *mut Producer,
    nr: i64,
    args: &[u64],
    path: SyscallEnterPath,
) -> (i64, *mut Producer) {
    let mut producer = producer;
    if CHILD_NEEDS_REBUILD.load(Ordering::Acquire) {
        // First ordinary boundary after a fork. `activation_enter` only runs
        // when an Engine is entered, and a fork child keeps running inside the
        // Engine it forked in, so the rebuild belongs here instead. The child
        // also took a copy of the parent's recorder -- in TLS and, for the trace
        // domain, in the pinned register -- so the caller has to be told which
        // recorder this call really used.
        rebuild_on_boundary();
        let attached = TLS_ACTIVE_PRODUCER.load(Ordering::Relaxed);
        if !attached.is_null() {
            producer = attached;
        }
    }
    // A real rt_sigreturn site belongs to the kernel signal frame and may not
    // touch the ordinary per-pthread page. The raw-site implementation in 1B
    // enforces the same bypass before it reaches this libc-oriented helper.
    if nr == libc::SYS_rt_sigreturn {
        return (crate::os::process::syscall(nr, args), producer);
    }
    if producer.is_null() {
        let result = crate::os::process::syscall(nr, args);
        if nr == libc::SYS_fork && result == 0 {
            after_fork_child();
        }
        return (result, producer);
    }

    let disposition = unsafe {
        match path {
            SyscallEnterPath::Cold => record_syscall_enter(&*producer, nr, args),
            SyscallEnterPath::Inline => match record_syscall_enter_inline(producer, nr, args) {
                HotEnter::Recorded => EnterDisposition::Recorded,
                HotEnter::NeedsColdPath => record_syscall_enter(&*producer, nr, args),
            },
        }
    };
    if nr == libc::SYS_exit || nr == libc::SYS_exit_group {
        unsafe { prepare_nonreturning_syscall(&*producer) };
        return (crate::os::process::syscall(nr, args), producer);
    }
    let result = crate::os::process::syscall(nr, args);
    if nr == libc::SYS_fork && result == 0 {
        after_fork_child();
        return (result, producer);
    }
    let errno = if result == -1 {
        unsafe { *((*(*producer).fast_ptr()).errno_ptr as *const i32) }
    } else {
        0
    };
    unsafe { record_syscall_exit(&*producer, disposition, result, errno) };
    (result, producer)
}

/// The calling thread's recorder, or null when this thread is not recording.
/// The activation boundary reads it once per entry to pin the trace domain's
/// register; no per-event path may call it (design §5.2.3).
pub(crate) fn current_producer() -> *mut Producer {
    TLS_ACTIVE_PRODUCER.load(Ordering::Relaxed)
}

unsafe fn prepare_nonreturning_syscall(producer: &Producer) {
    unsafe { seal_page(producer) };
    TLS_ACTIVE_PRODUCER.store(ptr::null_mut(), Ordering::Relaxed);
    TLS_CACHED_PRODUCER.store(ptr::null_mut(), Ordering::Relaxed);
    TLS_CACHED_SESSION.store(ptr::null_mut(), Ordering::Relaxed);
    TLS_ACTIVATION_DEPTH.store(0, Ordering::Relaxed);
    producer.retired.retired.store(true, Ordering::Release);

    let core = unsafe { &*producer.session };
    let previous = core.active_roots.fetch_sub(1, Ordering::Release);
    if previous == 0 {
        std::process::abort();
    }
    // If the session owner itself leaves through SYS_exit, no handle remains
    // to request a stop. Disarm new roots so the writer can finish once any
    // other already-active roots return.
    if producer.tid == core.owner_tid {
        let _ = core.phase.compare_exchange(
            PHASE_ARMED,
            PHASE_STOPPING,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
    core.wake_writer();
}

pub(crate) fn retire_current_thread() {
    let active = TLS_ACTIVE_PRODUCER.swap(ptr::null_mut(), Ordering::Relaxed);
    let cached = TLS_CACHED_PRODUCER.swap(ptr::null_mut(), Ordering::Relaxed);
    TLS_CACHED_SESSION.store(ptr::null_mut(), Ordering::Relaxed);
    TLS_ACTIVATION_DEPTH.store(0, Ordering::Relaxed);
    let producer = if !active.is_null() { active } else { cached };
    if producer.is_null() {
        return;
    }
    unsafe {
        // A cached producer is already quiescent: outer activation exit sealed
        // it before clearing TLS_ACTIVE_PRODUCER. Do not touch its cold ledger
        // here, because a stopping writer may already be summarizing it.
        if !active.is_null() {
            seal_page(&*producer);
        }
        (*producer).retired.retired.store(true, Ordering::Release);
        (&*(*producer).session).wake_writer();
    }
}

/// The child has no copy of the writer thread. Only stable TLS/global atomic
/// stores are permitted here; inherited locks, pages and file handles remain
/// unreachable until an ordinary boundary creates a new process generation.
/// Post-syscall hook for every path that can return in a `fork` child. The
/// generic `SYS_fork` (raw inline-asm syscall through `mirvm_syscall_dispatch`)
/// and the interpreter's HostFork builtin both end up here, which is what design
/// §6.3 requires: coverage may not depend on which spelling the guest used.
pub(crate) fn fork_child_guard(nr: i64, result: i64) {
    if nr == libc::SYS_fork && result == 0 {
        after_fork_child();
    }
}

pub(crate) fn after_fork_child() {
    ACTIVE.store(ptr::null_mut(), Ordering::Release);
    TLS_ACTIVE_PRODUCER.store(ptr::null_mut(), Ordering::Relaxed);
    TLS_CACHED_PRODUCER.store(ptr::null_mut(), Ordering::Relaxed);
    TLS_CACHED_SESSION.store(ptr::null_mut(), Ordering::Relaxed);
    TLS_ACTIVATION_DEPTH.store(0, Ordering::Relaxed);
    // The child kept the service-thread counter but none of the threads it
    // counted; fork duplicates only the calling thread. Re-pinning each
    // Engine's fork baseline happens lazily in the fork guard, which detects
    // the new pid (`guest_thread_count_for`).
    crate::os::thread::reset_service_threads_after_fork();
    // The inherited generation belongs to the parent; the child's first session
    // must take the next number so the two files cannot be read as one stream.
    reset_generation_after_fork();
    // A rebuild needs an ordinary boundary: it opens files and spawns a thread,
    // neither of which the kernel-side hook may do. Mark it and let the first
    // trace activation carry it out.
    CHILD_NEEDS_REBUILD.store(true, Ordering::Release);
}

fn producer_for_session(core: *mut SessionCore, engine_id: u64) -> *mut Producer {
    let cached_session = TLS_CACHED_SESSION.load(Ordering::Relaxed);
    let cached = TLS_CACHED_PRODUCER.load(Ordering::Relaxed);
    if cached_session == core && !cached.is_null() {
        return cached;
    }

    let errno_ptr = unsafe { libc::__errno_location() };
    let saved_errno = unsafe { *errno_ptr };
    let core_ref = unsafe { &*core };
    let pages = core_ref.page_pool.take_starter();
    let producer_id = core_ref.next_producer.fetch_add(1, Ordering::Relaxed);
    let thread_generation = core_ref
        .next_thread_generation
        .fetch_add(1, Ordering::Relaxed);
    let tid = unsafe { libc::gettid() as u32 };
    let producer = Box::into_raw(Box::new(Producer::new(
        core,
        pages,
        producer_id,
        thread_generation,
        tid,
        engine_id,
        errno_ptr,
    )));
    core_ref
        .producers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(producer as usize);
    core_ref
        .active_producers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(producer as usize);
    TLS_CACHED_SESSION.store(core, Ordering::Relaxed);
    TLS_CACHED_PRODUCER.store(producer, Ordering::Relaxed);
    // Allocation, registry locking and gettid are cold attach work, but none
    // may perturb the guest-visible libc errno value.
    unsafe { *errno_ptr = saved_errno };
    producer
}

pub(super) static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

/// Fork generation of this process. `fork` duplicates it, so the child sees the
/// parent's value and must not reuse it: the first session started after the
/// fork takes the next number (`reset_generation_after_fork` marks that). The
/// header value identifies which process's records a file holds, so a parent and
/// child writing concurrently cannot be mistaken for one stream
/// (design §5.5/§6.3, L2).
static PROCESS_GENERATION: AtomicU64 = AtomicU64::new(0);
static GENERATION_PENDING: AtomicBool = AtomicBool::new(false);

/// Generation this process's next capture session belongs to.
pub(crate) fn claim_process_generation() -> u64 {
    if GENERATION_PENDING.swap(false, Ordering::SeqCst) {
        PROCESS_GENERATION.fetch_add(1, Ordering::SeqCst) + 1
    } else {
        PROCESS_GENERATION.load(Ordering::SeqCst)
    }
}

/// Mark that this process is a `fork` child: its inherited generation belongs to
/// the parent, so the first session it starts must take the next one.
fn reset_generation_after_fork() {
    GENERATION_PENDING.store(true, Ordering::SeqCst);
}

/// Everything a `fork` child needs to build **its own** capture session, in a
/// form that survives `fork` unchanged (L2, design §5.5/§6.3).
///
/// The child cannot use the parent's session, page pool, writer or file
/// descriptors: none of them may be touched after a fork. It can only read
/// stable, immutable memory. So the parent publishes one leaked recipe for the
/// lifetime of the process and stores its address in an atomic; the pointer is
/// already there when the child's address space is duplicated, and reading it
/// needs neither the allocator nor a lock.
// The consumer (the fork child's rebuild) lands in the next L2 slice; until
// then only the publication and the tests read these.
#[allow(dead_code)]
pub(super) struct RebuildRecipe {
    pub(super) directory: PathBuf,
    pub(super) page_budget_bytes: usize,
}

/// Address of the immutable recipe, or 0 when no automatic rebuild is pending.
/// Written only on control paths (session start, session stop); read on the
/// child's first ordinary boundary.
pub(super) static REBUILD_RECIPE: AtomicUsize = AtomicUsize::new(0);

/// Publish the description a `fork` child needs to rebuild this session. The
/// output file is always `events-<pid>-<generation>.mlog` in the directory of
/// the parent's own output, so the child derives its own name from its own pid
/// and the generation it claims.
pub(super) fn publish_rebuild_recipe(output: &Path, page_budget_bytes: usize) {
    let Some(directory) = output.parent() else {
        REBUILD_RECIPE.store(0, Ordering::Release);
        return;
    };
    let recipe = Box::leak(Box::new(RebuildRecipe {
        directory: directory.to_path_buf(),
        page_budget_bytes,
    }));
    REBUILD_RECIPE.store(recipe as *const RebuildRecipe as usize, Ordering::Release);
}

/// Stop asking for an automatic rebuild. The leaked allocation stays valid, so a
/// child that already read the address is unaffected.
pub(super) fn clear_rebuild_recipe() {
    REBUILD_RECIPE.store(0, Ordering::Release);
}

/// The recipe a `fork` child must rebuild from, if one is published.
#[allow(dead_code)] // consumed by the fork child's rebuild (next L2 slice)
pub(super) fn pending_rebuild_recipe() -> Option<&'static RebuildRecipe> {
    let address = REBUILD_RECIPE.load(Ordering::Acquire);
    if address == 0 {
        return None;
    }
    // SAFETY: the recipe is leaked for the lifetime of the process, so the
    // pointer stays valid for as long as any child may read it.
    Some(unsafe { &*(address as *const RebuildRecipe) })
}
