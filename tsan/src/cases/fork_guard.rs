//! Fork while the engine is live, on a threaded process.
//!
//! The case pins the engine's fork-child handling, not the fork itself: with a guest-attached
//! thread actively running interpreted code, the forking thread calls `libc::fork()`, the child
//! runs `capture::fork_child_guard` (which is what both the generic `SYS_fork` dispatch and the
//! `HostFork` builtin funnel into), and then asserts that
//!
//! * the child's process generation advances exactly once and is then stable;
//! * the inherited fork baseline (parent pid) is recomputed to the child and the child is
//!   single-threaded as far as `ctx::guest_spawned_threads` is concerned;
//! * a recording syscall in the child returns the child's own result;
//!
//! before `_exit(0)`. The parent waits, checks the child's exit status, and re-runs the engine.
//!
//! ## TSan and `fork`
//!
//! TSan's runtime survives a multi-threaded `fork` in the child as long as the child does not
//! create new threads. Measured in this harness (temporarily arming a capture session and
//! letting the child's post-hook recording syscall drive the rebuild):
//!
//! ```text
//! ==697525==ThreadSanitizer: starting new threads after multi-threaded fork is not supported.
//! Dying (set die_after_fork=0 to override)
//! FAIL fork-guard child_ok=false (waited=697525 exited=true code=66)
//! ```
//!
//! The engine's only post-fork thread creation is the capture-session rebuild
//! (`rebuild_session_from_recipe` opens the child's own `.mlog` and spawns a `mirvm-capture`
//! writer), so that rebuild path cannot be exercised under TSan. This case therefore does not
//! start a capture session, and the child runs its recording entry (`capture::host_syscall`)
//! *before* the hook sets `CHILD_NEEDS_REBUILD` so no rebuild can be triggered even if a
//! session's recipe is inherited; the assertion is that the passthrough still returns the
//! child's real result. Everything else the engine does in a fork child -- the generation
//! advance, the service-thread reset and the baseline repair -- is covered.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;

use crate::telemetry::capture;
use crate::vm::engine::ctx::{self, Engine, Shared, attach};
use crate::vm::engine::interp;
use crate::vm::engine::ir::{
    Block, FuncBody, MemOrd, Module, Operand, ParamAbi, RetAbi, RmwOp, ScalarPlace, Slot, Stmt,
    Terminator, Width,
};

/// `fn bump(addr) -> previous`, an interpreted guest body the parent keeps running after fork.
fn build_module() -> Module {
    let ret_slot = Slot {
        off: 0,
        width: Width::W64,
    };
    let arg_slot = Slot {
        off: 8,
        width: Width::W64,
    };
    let body = FuncBody {
        frame_size: 16,
        frame_align: 8,
        ret: RetAbi::Scalar(ret_slot),
        params: vec![ParamAbi::Scalar(arg_slot)],
        caller_loc_off: None,
        blocks: vec![Block {
            stmts: vec![Stmt::AtomicRmw {
                order: MemOrd::SeqCst,
                op: RmwOp::Add,
                addr: Operand::Slot(arg_slot),
                val: Operand::Imm {
                    bits: 1,
                    width: Width::W64,
                },
                dst: ScalarPlace::Slot(ret_slot),
            }],
            term: Terminator::Return,
        }],
        name: "tsan_fork::bump".into(),
    };
    Module {
        funcs: vec![body].into(),
        ..Default::default()
    }
}

pub(crate) fn run_fork_guard() -> bool {
    static CELL: AtomicU64 = AtomicU64::new(0);
    static STOP: AtomicBool = AtomicBool::new(false);

    let engine = Engine::new(Shared::new(build_module()));
    let shared = Arc::clone(engine.shared());
    let addr = CELL.as_ptr() as u64;

    // Guest main start pins the baseline; do the same here so the child has an inherited
    // (parent-owned) value to detect and repair.
    ctx::set_fork_baseline(&shared);
    let parent_generation = capture::claim_process_generation();
    let parent_pid = unsafe { libc::getpid() };

    // A guest-attached thread actively interpreting, so the fork happens on a genuinely
    // multi-threaded process. It only touches an atomic cell, so the process stays race-free.
    let (ready_tx, ready_rx) = mpsc::channel::<()>();
    let guest = {
        let shared = Arc::clone(&shared);
        thread::spawn(move || {
            let ctx = attach(&shared);
            ready_tx.send(()).unwrap();
            while !STOP.load(Ordering::Relaxed) {
                interp::interp_frame(ctx, 0, &[addr]);
            }
        })
    };
    ready_rx.recv().expect("guest thread did not attach");

    // The forking pthread is attached, so the child inherits a live Ctx for its one thread.
    let ctx_main = attach(&shared);

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        STOP.store(true, Ordering::Relaxed);
        guest.join().unwrap();
        println!("FAIL fork-guard: fork() failed");
        return false;
    }

    if pid == 0 {
        // Child: `libc::fork` (not a raw SYS_fork) so TSan's own fork interceptor resets its
        // runtime before any instrumented code runs. Recording entry first, before the engine
        // hook can arm the capture rebuild (see the module comment).
        let recorded = capture::host_syscall(libc::SYS_getpid, &[]);
        let child_pid = unsafe { libc::getpid() };
        let syscall_ok = recorded == child_pid as i64;

        capture::fork_child_guard(libc::SYS_fork, 0);
        let first_generation = capture::claim_process_generation();
        let second_generation = capture::claim_process_generation();
        let generation_ok =
            first_generation == parent_generation + 1 && second_generation == first_generation;

        let inherited_pid = shared.fork_baseline_pid.load(Ordering::SeqCst);
        ctx::set_fork_baseline(&shared);
        let baseline_ok = inherited_pid == parent_pid
            && shared.fork_baseline_pid.load(Ordering::SeqCst) == child_pid
            && !unsafe { ctx::guest_spawned_threads(ctx_main) };

        let code = if syscall_ok && generation_ok && baseline_ok {
            0
        } else {
            3
        };
        unsafe { libc::_exit(code) };
    }

    let mut status = 0;
    let waited = unsafe { libc::waitpid(pid, &mut status, 0) };
    let child_exited = libc::WIFEXITED(status);
    let child_code = if child_exited {
        libc::WEXITSTATUS(status)
    } else {
        -1
    };
    let child_ok = waited == pid && child_exited && child_code == 0;

    // The child's generation advance must not leak back into the parent.
    let parent_generation_stable = capture::claim_process_generation() == parent_generation;

    // Stop the guest thread so the engine check below is deterministic, then prove the
    // parent's engine still runs interpreted guest code.
    STOP.store(true, Ordering::Relaxed);
    guest.join().expect("guest thread panicked");
    let before = CELL.load(Ordering::SeqCst);
    for _ in 0..1_000 {
        interp::interp_frame(ctx_main, 0, &[addr]);
    }
    let parent_ok = CELL.load(Ordering::SeqCst) == before + 1_000;

    engine.wait_closed().expect("engine did not close");

    let ok = child_ok && parent_generation_stable && parent_ok;
    if ok {
        println!(
            "PASS fork-guard child pid {pid} exit 0; generation advanced once, baseline \
             recomputed to the child, recording syscall returned {pid}; parent engine \
             still interpreted 1000 guest calls"
        );
    } else {
        println!(
            "FAIL fork-guard child_ok={child_ok} (waited={waited} exited={child_exited} \
             code={child_code}) generation_stable={parent_generation_stable} parent_ok={parent_ok}"
        );
    }
    ok
}
