//! What a `fork` child does to the capture: the lingering writer it inherits and must not share,
//! the immutable recipe the parent publishes for it to reopen the session from, and the rebuild
//! itself.

use super::*;

use super::activation::producer_for_session;
use super::generation::{claim_process_generation, reset_generation_after_fork};

/// A `fork` child may be inside a trace activation when it forks, so the rebuild
/// is deferred to the activation boundary rather than run from the kernel-side
/// hook, which may not open files or spawn threads.
pub(super) static CHILD_NEEDS_REBUILD: AtomicBool = AtomicBool::new(false);

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
    // chunks. `seal_page` publishes and wakes the writer itself.
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
pub(crate) fn arm_lingering_writer(core: &'static SessionCore) {
    static HOOK: std::sync::Once = std::sync::Once::new();
    HOOK.call_once(|| {
        crate::os::process::atexit_native(drain_lingering_writer);
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
/// recipe. Runs only on an ordinary boundary: it opens files
/// and spawns a thread, which the kernel-side fork hook may never do.
///
/// The child's first claim advances the generation it inherited, and that same
/// value ends up in both the file name and the header.
pub(super) fn rebuild_session_from_recipe() -> bool {
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
    let output = recipe.directory.join(format!(
        "events-{}-{process_generation}.mlog",
        crate::os::process::getpid()
    ));
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
            owner_pid: crate::os::process::getpid(),
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
pub(crate) fn build_session_core(
    mut file: File,
    partial: &Path,
    final_path: PathBuf,
    page_budget_bytes: usize,
    process_generation: u64,
) -> io::Result<(&'static SessionCore, CaptureWriterHandle)> {
    let owner_pid = crate::os::process::getpid();
    let owner_tid = crate::os::process::gettid() as u32;
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
    // recipe is published before the session becomes visible.
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
pub(crate) fn wait_for_writer_publish(core: &SessionCore, timeout: Duration) -> bool {
    let started = Instant::now();
    while !core.writer_done.load(Ordering::Acquire) {
        if started.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    true
}

/// Everything a `fork` child needs to build **its own** capture session, in a
/// form that survives `fork` unchanged.
///
/// The child cannot use the parent's session, page pool, writer or file
/// descriptors: none of them may be touched after a fork. It can only read
/// stable, immutable memory. So the parent publishes one leaked recipe for the
/// lifetime of the process and stores its address in an atomic; the pointer is
/// already there when the child's address space is duplicated, and reading it
/// needs neither the allocator nor a lock.
// NOTE: the parent publishes one of these and only the fork child's rebuild reads it back.
#[allow(dead_code)]
pub(crate) struct RebuildRecipe {
    pub(crate) directory: PathBuf,
    pub(crate) page_budget_bytes: usize,
}

/// Address of the immutable recipe, or 0 when no automatic rebuild is pending.
/// Written only on control paths (session start, session stop); read on the
/// child's first ordinary boundary.
pub(crate) static REBUILD_RECIPE: AtomicUsize = AtomicUsize::new(0);

/// Publish the description a `fork` child needs to rebuild this session. The
/// output file is always `events-<pid>-<generation>.mlog` in the directory of
/// the parent's own output, so the child derives its own name from its own pid
/// and the generation it claims.
pub(crate) fn publish_rebuild_recipe(output: &Path, page_budget_bytes: usize) {
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
pub(crate) fn clear_rebuild_recipe() {
    REBUILD_RECIPE.store(0, Ordering::Release);
}

/// The recipe a `fork` child must rebuild from, if one is published.
#[allow(dead_code)] // consumed by the fork child's rebuild
pub(crate) fn pending_rebuild_recipe() -> Option<&'static RebuildRecipe> {
    let address = REBUILD_RECIPE.load(Ordering::Acquire);
    if address == 0 {
        return None;
    }
    // SAFETY: the recipe is leaked for the lifetime of the process, so the
    // pointer stays valid for as long as any child may read it.
    Some(unsafe { &*(address as *const RebuildRecipe) })
}

/// stores are permitted here; inherited locks, pages and file handles remain
/// unreachable until an ordinary boundary creates a new process generation.
/// Post-syscall hook for every path that can return in a `fork` child. The
/// generic `SYS_fork` (raw inline-asm syscall through `mirvm_syscall_dispatch`)
/// and the interpreter's HostFork builtin both end up here, so capture coverage
/// cannot depend on which spelling the guest used.
pub(crate) fn fork_child_guard(nr: i64, result: i64) {
    if nr == crate::os::process::SYS_FORK && result == 0 {
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
