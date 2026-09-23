//! Per-thread activation: the token a guest thread holds while its syscalls are recorded, the two
//! host-syscall entries it reaches the recorder through, and the producer it keeps for the thread's
//! lifetime.

use super::*;

use super::rebuild::{CHILD_NEEDS_REBUILD, rebuild_on_boundary, rebuild_session_from_recipe};

#[derive(Clone, Copy)]
pub(crate) struct ActivationToken {
    producer: *mut Producer,
    pub(crate) session: *const SessionCore,
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

/// The trace code domain's syscall entry. The producer is the
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
    // touch the ordinary per-pthread page. The raw-site implementation enforces
    // the same bypass before it reaches this libc-oriented helper.
    if nr == crate::os::process::SYS_RT_SIGRETURN {
        return (crate::os::process::syscall(nr, args), producer);
    }
    if producer.is_null() {
        let result = crate::os::process::syscall(nr, args);
        if nr == crate::os::process::SYS_FORK && result == 0 {
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
    if nr == crate::os::process::SYS_EXIT || nr == crate::os::process::SYS_EXIT_GROUP {
        unsafe { prepare_nonreturning_syscall(&*producer) };
        return (crate::os::process::syscall(nr, args), producer);
    }
    let result = crate::os::process::syscall(nr, args);
    if nr == crate::os::process::SYS_FORK && result == 0 {
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
/// register; no per-event path may call it.
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
pub(super) fn producer_for_session(core: *mut SessionCore, engine_id: u64) -> *mut Producer {
    let cached_session = TLS_CACHED_SESSION.load(Ordering::Relaxed);
    let cached = TLS_CACHED_PRODUCER.load(Ordering::Relaxed);
    if cached_session == core && !cached.is_null() {
        return cached;
    }

    let errno_ptr = crate::os::process::errno_location();
    let saved_errno = unsafe { *errno_ptr };
    let core_ref = unsafe { &*core };
    let pages = core_ref.page_pool.take_starter();
    let producer_id = core_ref.next_producer.fetch_add(1, Ordering::Relaxed);
    let thread_generation = core_ref
        .next_thread_generation
        .fetch_add(1, Ordering::Relaxed);
    let tid = crate::os::process::gettid() as u32;
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
