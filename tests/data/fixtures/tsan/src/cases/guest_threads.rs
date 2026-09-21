//! Many host threads attaching, running interpreted frames and retiring concurrently.
//!
//! Each round spawns `THREADS` real OS threads; every one does `ctx::attach` (its own per-thread
//! `Ctx`), runs a hand-built `interp::interp_frame`, and brackets a real `pthread_setspecific`
//! with `deferred::prepare_pthread_operation` before exiting. Several rounds exercise the
//! per-thread attach/retire churn, and the TSD operation brackets race the process-global
//! registration registry (`TSD_KEYS`, keyed per Engine and pthread) the way guest
//! `pthread_setspecific`/`pthread_key_delete` calls do.
//!
//! The engine's thread accounting must return to its baseline after the joins: the fork-guard
//! baseline pinned before the rounds (`guest_spawned_threads`) has to read back single-threaded.
//! A stale count, a wrong interpreted value or a TSan report is an engine bug.
//!
//! Under the TSan configuration the `Ctx` pthread-key destructor is deliberately not registered
//! (`cfg(sanitize = "thread")` in `ctx::attach`: TSan's own thread state dies before the TSD
//! phase), so the 3-round re-hang teardown in `ctx_key_dtor` cannot run here. This case pins the
//! attach/run/retire churn and the concurrent TSD registry instead; the destructor-round protocol
//! is covered by the product's `threads_panic` differential test in the non-sanitized config.

use std::sync::{Arc, mpsc};

use crate::vm::ctx::{
    Engine, EngineState, Shared, attach, guest_spawned_threads, set_fork_baseline,
};
use crate::vm::deferred::{TsdRegistration, prepare_pthread_operation};
use crate::vm::interp;
use crate::vm::ir::{
    Block, FuncBody, IntBinOp, Module, Operand, ParamAbi, RetAbi, Rvalue, ScalarPlace, Slot, Stmt,
    Terminator, Width,
};

const THREADS: usize = 8;
const ROUNDS: usize = 6;
const BASE: u64 = 0x1000;

/// Hand-built Module: fn0 `add3(x) -> x + 3`. Frame: `_0 ret @0`, `_1 arg @8`.
fn add3_module() -> Module {
    let ret = Slot {
        off: 0,
        width: Width::W64,
    };
    let arg = Slot {
        off: 8,
        width: Width::W64,
    };
    let body = FuncBody {
        frame_size: 16,
        frame_align: 8,
        ret: RetAbi::Scalar(ret),
        params: vec![ParamAbi::Scalar(arg)],
        caller_loc_off: None,
        blocks: vec![Block {
            stmts: vec![Stmt::Assign {
                dst: ScalarPlace::Slot(ret),
                rv: Rvalue::IntBin {
                    op: IntBinOp::Add,
                    signed: false,
                    a: Operand::Slot(arg),
                    b: Operand::Imm {
                        bits: 3,
                        width: Width::W64,
                    },
                },
            }],
            term: Terminator::Return,
        }],
        name: "tsan_guest_threads::add3".into(),
    };
    Module {
        funcs: vec![body].into(),
        ..Module::default()
    }
}

struct Outcome {
    wrong_value: u64,
    tsd_ops: u64,
}

/// One guest thread's whole life: attach, run one interpreted frame, then set and clear this
/// thread's managed TSD value. The tracking operation brackets the real libc call, matching the
/// product's `pthread_setspecific` wrapper.
fn guest_thread(shared: Arc<Shared>, key: libc::pthread_key_t, x: u64) -> Outcome {
    let ctx = attach(&shared);
    let got = interp::interp_frame(ctx, 0, &[x]).0;
    let mut outcome = Outcome {
        wrong_value: (got != x.wrapping_add(3)) as u64,
        tsd_ops: 0,
    };

    let value = x | 1;
    if let Some(operation) =
        prepare_pthread_operation(&shared, "pthread_setspecific", &[key as u64, value])
    {
        let result = unsafe { libc::pthread_setspecific(key, value as *const libc::c_void) };
        operation.complete(result as u64);
        outcome.tsd_ops += 1;
    }
    if let Some(operation) =
        prepare_pthread_operation(&shared, "pthread_setspecific", &[key as u64, 0])
    {
        let result = unsafe { libc::pthread_setspecific(key, std::ptr::null()) };
        operation.complete(result as u64);
        outcome.tsd_ops += 1;
    }
    outcome
}

pub(crate) fn run_guest_threads() -> bool {
    let engine = Engine::new(Shared::new(add3_module()));
    let shared = Arc::clone(engine.shared());

    // A committed registration gives the concurrent operations a key to track. The registration
    // itself owns a deferred hold, so the Engine cannot finalize until the key is deleted.
    let registration = TsdRegistration::pending(engine.control()).expect("TSD registration");
    let mut key: libc::pthread_key_t = 0;
    let created = unsafe { libc::pthread_key_create(&mut key, None) } == 0;
    if created {
        registration.commit(crate::os::thread::TlsKey::from_raw(key));
    }

    set_fork_baseline(&shared);
    let main_ctx = attach(&shared);

    let mut wrong_value = 0u64;
    let mut tsd_ops = 0u64;
    for round in 0..ROUNDS {
        let (tx, rx) = mpsc::channel();
        let mut handles = Vec::with_capacity(THREADS);
        for t in 0..THREADS {
            let shared = Arc::clone(&shared);
            let tx = tx.clone();
            let x = BASE + (round as u64) * 256 + t as u64;
            handles.push(std::thread::spawn(move || {
                tx.send(guest_thread(shared, key, x)).unwrap();
            }));
        }
        drop(tx);
        for _ in 0..THREADS {
            let outcome = rx.recv().unwrap();
            wrong_value += outcome.wrong_value;
            tsd_ops += outcome.tsd_ops;
        }
        for handle in handles {
            handle.join().expect("guest thread panicked");
        }
    }

    // Every spawned thread has been joined, so guest-attributable thread count is back to the
    // baseline pinned before the rounds.
    let spawned_after_joins = unsafe { guest_spawned_threads(main_ctx) };

    // Delete the tracked key last: it revokes the registration, releases its deferred hold, and
    // clears any per-thread value that a retiring thread could otherwise strand.
    let mut delete_ok = false;
    if created {
        let operation = prepare_pthread_operation(&shared, "pthread_key_delete", &[key as u64]);
        if let Some(operation) = operation {
            let result = unsafe { libc::pthread_key_delete(key) };
            operation.complete(result as u64);
            delete_ok = result == 0;
        }
    }

    let waited = engine.wait_closed().is_ok();
    let closed = engine.state() == EngineState::Closed;
    let expected_ops = 2 * THREADS as u64 * ROUNDS as u64;
    let ok = created
        && delete_ok
        && wrong_value == 0
        && tsd_ops == expected_ops
        && !spawned_after_joins
        && waited
        && closed;
    let verdict = if ok { "PASS" } else { "FAIL" };
    println!(
        "{verdict} guest-threads threads={THREADS} rounds={ROUNDS} wrong_value={wrong_value} \
         tsd_ops={tsd_ops}/{expected_ops} spawned_after_joins={spawned_after_joins} \
         created={created} delete_ok={delete_ok} closed={closed}"
    );
    ok
}
