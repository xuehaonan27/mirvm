//! Cross-thread signal delivery into per-thread inboxes.
//!
//! One guest handler for one signal is installed through the engine's real sigaction path
//! (`signal::install_signal_resolved` with a hand-built handler function), and a second host
//! thread raises it at the owning thread with `pthread_kill`. Each owner drains at a VM safe
//! point (`ctx::drain_pending_signals`, the public spelling of `signal::take_current_thread_delivery`
//! plus dispatch), so the case pins:
//!
//! * the handler runs exactly once per raise, on the owning pthread;
//! * a delivery aimed at thread A never appears in B's target-pthread inbox;
//! * a kernel-blocked signal stays pending (no delivery, no inbox event) and is delivered at
//!   the next safe point once unblocked.
//!
//! The installed disposition is restored before the Engine closes, so the case cannot leak a
//! handler into the rest of the harness.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;

use crate::os::signal::Sigaction;
use crate::vm::ctx::{Engine, Shared, attach, drain_pending_signals};
use crate::vm::ir::{
    Block, FfiKind, ForeignSig, FuncBody, Module, Operand, ParamAbi, RetAbi, RetDest, Slot,
    Terminator, UnwindAction, Width,
};
use crate::vm::{signal, thunks};

/// Guest-visible handler address; the engine resolves it through `Module.fn_addrs`.
const HANDLER_ADDR: u64 = 0xe2b1;
const TEST_SIGNAL: i32 = libc::SIGUSR1;

static HANDLER_RUNS: AtomicU64 = AtomicU64::new(0);
static HANDLER_THREAD: AtomicU64 = AtomicU64::new(0);

/// The native body the guest handler thunks to: it records the running pthread and count.
unsafe extern "C-unwind" fn note_handler_run() {
    HANDLER_THREAD.store(unsafe { libc::pthread_self() } as u64, Ordering::SeqCst);
    HANDLER_RUNS.fetch_add(1, Ordering::SeqCst);
}

fn handler_sig() -> ForeignSig {
    ForeignSig {
        args: Vec::new(),
        ret: FfiKind::Void,
        fixed: None,
        thunk_args: Vec::new(),
        unwind: true,
    }
}

/// `fn handler(signum) -> ()` whose body calls the recording callback. The shape (Zst return,
/// one scalar param, no caller location) is what `resolve_signal_handler` accepts.
fn handler_module() -> Module {
    let handler = FuncBody {
        frame_size: 16,
        frame_align: 8,
        ret: RetAbi::Zst,
        params: vec![ParamAbi::Scalar(Slot {
            off: 8,
            width: Width::W64,
        })],
        caller_loc_off: None,
        blocks: vec![
            Block {
                stmts: Vec::new(),
                term: Terminator::CallIndirect {
                    callee: Operand::Imm {
                        bits: note_handler_run as *const () as usize as u64,
                        width: Width::W64,
                    },
                    args: Vec::new(),
                    ret: RetDest::Ignore,
                    target: 1,
                    unwind: UnwindAction::Continue,
                    null_ok: false,
                    native_sig: Some(handler_sig()),
                },
            },
            Block {
                stmts: Vec::new(),
                term: Terminator::Return,
            },
        ],
        name: "tsan_signal::handler".into(),
    };
    let mut module = Module {
        funcs: vec![handler].into(),
        ..Default::default()
    };
    module.fn_addrs.insert(HANDLER_ADDR, 0);
    module
}

/// Restore the host disposition even if the case returns early.
struct RestoreSignal {
    signum: i32,
    action: Sigaction,
}

impl Drop for RestoreSignal {
    fn drop(&mut self) {
        let _ = self.action.replace_exact(self.signum);
    }
}

/// Main -> worker command words, one per phase.
#[derive(Clone, Copy)]
enum Cmd {
    Probe,
    Drain,
    Block,
    DrainBlocked,
    Unblock,
    Exit,
}

/// Worker -> main acknowledgements.
#[derive(Clone, Copy)]
enum Ack {
    Pending,
    Drained { runs: u64, pending: bool },
    Isolated { pending: bool, taken_none: bool },
    Done,
}

fn recv<T>(rx: &mpsc::Receiver<T>) -> T {
    rx.recv().expect("tsan_signal worker disappeared")
}

/// Spin until this pthread's fixed adapter has published its delivery. The adapter runs on
/// this same pthread, so the signal frame interrupts this loop and the flag becomes visible
/// without any sleep.
fn wait_for_inbox_delivery() {
    while !signal::current_thread_has_pending() {
        thread::yield_now();
    }
}

fn run_count() -> u64 {
    HANDLER_RUNS.load(Ordering::SeqCst)
}

pub(crate) fn run_signal_delivery() -> bool {
    HANDLER_RUNS.store(0, Ordering::SeqCst);
    HANDLER_THREAD.store(0, Ordering::SeqCst);

    let engine = Engine::new(Shared::new(handler_module()));
    let shared = Arc::clone(engine.shared());
    let control = Arc::clone(engine.control());

    let saved = match Sigaction::query(TEST_SIGNAL) {
        Ok(saved) => saved,
        Err(errno) => {
            println!("FAIL signal-delivery: cannot query signal {TEST_SIGNAL}: errno {errno}");
            return false;
        }
    };
    let resolution = thunks::resolve_signal_handler(&shared, HANDLER_ADDR);
    if let Err(error) =
        signal::install_signal_resolved(&control, TEST_SIGNAL, HANDLER_ADDR as usize, resolution)
    {
        println!("FAIL signal-delivery: install failed: {error}");
        return false;
    }
    let guard = RestoreSignal {
        signum: TEST_SIGNAL,
        action: saved,
    };

    // Owner A runs the handler, masks the signal, and reports run counts.
    let (a_cmd_tx, a_cmd_rx) = mpsc::channel::<Cmd>();
    let (a_ack_tx, a_ack_rx) = mpsc::channel::<Ack>();
    let (a_ready_tx, a_ready_rx) = mpsc::channel::<u64>();
    let a_shared = Arc::clone(&shared);
    let owner = thread::spawn(move || {
        let ctx = attach(&a_shared);
        a_ready_tx
            .send(unsafe { libc::pthread_self() } as u64)
            .unwrap();

        match recv(&a_cmd_rx) {
            Cmd::Probe => {}
            _ => return,
        }
        wait_for_inbox_delivery();
        a_ack_tx.send(Ack::Pending).unwrap();

        match recv(&a_cmd_rx) {
            Cmd::Drain => {}
            _ => return,
        }
        drain_pending_signals(ctx);
        a_ack_tx
            .send(Ack::Drained {
                runs: run_count(),
                pending: signal::current_thread_has_pending(),
            })
            .unwrap();

        match recv(&a_cmd_rx) {
            Cmd::Block => {}
            _ => return,
        }
        let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
        unsafe {
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, TEST_SIGNAL);
        }
        assert_eq!(
            unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) },
            0
        );
        a_ack_tx.send(Ack::Done).unwrap();

        match recv(&a_cmd_rx) {
            Cmd::DrainBlocked => {}
            _ => return,
        }
        drain_pending_signals(ctx);
        a_ack_tx
            .send(Ack::Drained {
                runs: run_count(),
                pending: signal::current_thread_has_pending(),
            })
            .unwrap();

        match recv(&a_cmd_rx) {
            Cmd::Unblock => {}
            _ => return,
        }
        assert_eq!(
            unsafe { libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut()) },
            0
        );
        wait_for_inbox_delivery();
        drain_pending_signals(ctx);
        a_ack_tx
            .send(Ack::Drained {
                runs: run_count(),
                pending: signal::current_thread_has_pending(),
            })
            .unwrap();

        match recv(&a_cmd_rx) {
            Cmd::Exit => {}
            _ => return,
        }
        drain_pending_signals(ctx);
        signal::deactivate_current_thread_inbox();
        a_ack_tx.send(Ack::Done).unwrap();
    });

    // Owner B is never targeted; its inbox must stay empty while A has a pending delivery.
    let (b_cmd_tx, b_cmd_rx) = mpsc::channel::<Cmd>();
    let (b_ack_tx, b_ack_rx) = mpsc::channel::<Ack>();
    let (b_ready_tx, b_ready_rx) = mpsc::channel::<()>();
    let b_shared = Arc::clone(&shared);
    let neighbour = thread::spawn(move || {
        let ctx = attach(&b_shared);
        b_ready_tx.send(()).unwrap();

        match recv(&b_cmd_rx) {
            Cmd::Probe => {}
            _ => return,
        }
        let pending = signal::current_thread_has_pending();
        let taken_none = signal::take_current_thread_delivery(0).is_none();
        b_ack_tx
            .send(Ack::Isolated {
                pending,
                taken_none,
            })
            .unwrap();

        match recv(&b_cmd_rx) {
            Cmd::Exit => {}
            _ => return,
        }
        drain_pending_signals(ctx);
        signal::deactivate_current_thread_inbox();
        b_ack_tx.send(Ack::Done).unwrap();
    });

    let owner_pthread = a_ready_rx.recv().expect("owner did not attach") as libc::pthread_t;
    b_ready_rx.recv().expect("neighbour did not attach");

    let mut failures: Vec<String> = Vec::new();

    // Round 1: one raise aimed at the owner.
    if unsafe { libc::pthread_kill(owner_pthread, TEST_SIGNAL) } != 0 {
        failures.push("pthread_kill failed".into());
    }
    a_cmd_tx.send(Cmd::Probe).unwrap();
    if !matches!(recv(&a_ack_rx), Ack::Pending) {
        failures.push("owner never saw its inbox delivery".into());
    }

    // Isolation: the very same delivery window is empty on the neighbour.
    b_cmd_tx.send(Cmd::Probe).unwrap();
    match recv(&b_ack_rx) {
        Ack::Isolated {
            pending: false,
            taken_none: true,
        } => {}
        Ack::Isolated {
            pending,
            taken_none,
        } => failures.push(format!(
            "neighbour inbox saw a thread-directed raise (pending={pending} empty_take={taken_none})"
        )),
        _ => failures.push("neighbour did not report its inbox".into()),
    }

    a_cmd_tx.send(Cmd::Drain).unwrap();
    let runs_after_first = match recv(&a_ack_rx) {
        Ack::Drained { runs, pending } => {
            if pending {
                failures.push("delivery survived the safe-point drain".into());
            }
            runs
        }
        _ => {
            failures.push("owner miscounted the first drain".into());
            0
        }
    };
    if runs_after_first != 1 {
        failures.push(format!(
            "first raise ran the handler {runs_after_first} times"
        ));
    }
    if HANDLER_THREAD.load(Ordering::SeqCst) != owner_pthread as u64 {
        failures.push("handler ran off the owning pthread".into());
    }

    // Round 2: a kernel-blocked raise must stay pending, then be delivered once unblocked.
    a_cmd_tx.send(Cmd::Block).unwrap();
    if !matches!(recv(&a_ack_rx), Ack::Done) {
        failures.push("owner failed to block the signal".into());
    }
    if unsafe { libc::pthread_kill(owner_pthread, TEST_SIGNAL) } != 0 {
        failures.push("pthread_kill (blocked) failed".into());
    }
    a_cmd_tx.send(Cmd::DrainBlocked).unwrap();
    match recv(&a_ack_rx) {
        Ack::Drained { runs, pending } => {
            if runs != runs_after_first || pending {
                failures.push(format!(
                    "blocked raise was not left pending (runs={runs} pending={pending})"
                ));
            }
        }
        _ => failures.push("owner miscounted the blocked drain".into()),
    }

    a_cmd_tx.send(Cmd::Unblock).unwrap();
    match recv(&a_ack_rx) {
        Ack::Drained { runs, pending } => {
            if runs != runs_after_first + 1 || pending {
                failures.push(format!(
                    "unblocked raise did not deliver exactly once (runs={runs} pending={pending})"
                ));
            }
        }
        _ => failures.push("owner miscounted the unblocked drain".into()),
    }

    // No further raise can route to the stub past this point; put the host back first.
    drop(guard);

    a_cmd_tx.send(Cmd::Exit).unwrap();
    b_cmd_tx.send(Cmd::Exit).unwrap();
    if !matches!(recv(&a_ack_rx), Ack::Done) || !matches!(recv(&b_ack_rx), Ack::Done) {
        failures.push("owners did not retire cleanly".into());
    }
    owner.join().expect("owner pthread panicked");
    neighbour.join().expect("neighbour pthread panicked");

    if run_count() != 2 {
        failures.push(format!("handler ran {} times for two raises", run_count()));
    }
    engine.wait_closed().expect("engine did not close");

    if failures.is_empty() {
        println!(
            "PASS signal-delivery two raises, one handler run each, on the owning pthread; \
             blocked raise stayed pending until unblocked; neighbour inbox stayed empty"
        );
        true
    } else {
        println!("FAIL signal-delivery: {}", failures.join("; "));
        false
    }
}
