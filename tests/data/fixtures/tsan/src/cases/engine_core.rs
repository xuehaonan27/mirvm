//! The product engine on N real threads: per-thread attach + guest atomics + the thunk cache.
//!
//! 8 host threads share one `Shared`; each attaches at the boundary to get its per-thread Ctx:
//! ① interpret a guest atomic increment (AtomicRmw -- the engine must issue a real host
//!   atomic instruction);
//! ② hit the thunk factory concurrently via get_or_create (Mutex cache, same key always
//!   yields the same real code) and call the thunk across threads
//!   (trampoline -> attach -> re-enter interp_frame).
//! Zero TSan warnings = the execution-phase three-way state split holds (per-thread private /
//! read-only after publication / explicitly synchronized).

use std::sync::atomic::{AtomicU64, Ordering};

use crate::vm::ctx::{Engine, Shared, attach};
use crate::vm::ir::{
    Block, FfiKind, ForeignSig, FuncBody, IntBinOp, MemOrd, Module, Operand, ParamAbi, RetAbi,
    RmwOp, Rvalue, ScalarPlace, Slot, Stmt, Terminator, Width,
};
use crate::vm::{interp, thunks};

/// Hand-built Module: fn0 `bump(addr) -> previous value` (atomic +1); fn1
/// `add3(x) -> x+3` (the thunk target).
/// Frame layout (identical for both): _0 ret @0, _1 arg @8.
fn build_module() -> Module {
    let ret_slot = Slot {
        off: 0,
        width: Width::W64,
    };
    let arg_slot = Slot {
        off: 8,
        width: Width::W64,
    };
    let mk = |stmts: Vec<Stmt>, name: &str| FuncBody {
        frame_size: 16,
        frame_align: 8,
        ret: RetAbi::Scalar(ret_slot),
        params: vec![ParamAbi::Scalar(arg_slot)],
        caller_loc_off: None,
        blocks: vec![Block {
            stmts,
            term: Terminator::Return,
        }],
        name: name.into(),
    };
    let bump = mk(
        vec![Stmt::AtomicRmw {
            order: MemOrd::SeqCst,
            op: RmwOp::Add,
            addr: Operand::Slot(arg_slot),
            val: Operand::Imm {
                bits: 1,
                width: Width::W64,
            },
            dst: ScalarPlace::Slot(ret_slot),
        }],
        "tsan_mt::bump",
    );
    let add3 = mk(
        vec![Stmt::Assign {
            dst: ScalarPlace::Slot(ret_slot),
            rv: Rvalue::IntBin {
                op: IntBinOp::Add,
                signed: false,
                a: Operand::Slot(arg_slot),
                b: Operand::Imm {
                    bits: 3,
                    width: Width::W64,
                },
            },
        }],
        "tsan_mt::add3",
    );
    Module {
        funcs: vec![bump, add3].into(),
        ..Default::default()
    }
}

pub(crate) fn run_engine_atomics_thunk_cache() -> bool {
    let engine = Engine::new(Shared::new(build_module()));
    let shared = std::sync::Arc::clone(engine.shared());
    static CELL: AtomicU64 = AtomicU64::new(0);
    const THREADS: u64 = 8;
    const N: u64 = 2000;
    let sig = ForeignSig {
        args: vec![FfiKind::U64],
        ret: FfiKind::U64,
        fixed: None,
        thunk_args: vec![],
        unwind: false,
    };

    let handles: Vec<_> = (0..THREADS)
        .map(|t| {
            let sig = sig.clone();
            let shared = std::sync::Arc::clone(&shared);
            std::thread::spawn(move || {
                let ctx = attach(&shared);
                let addr = CELL.as_ptr() as u64;
                for _ in 0..N {
                    // TSan-harness-only entry that bypasses call_guest: this channel never
                    // compiles cranelift, so layered dispatch is meaningless here.
                    interp::interp_frame(ctx, 0, &[addr]);
                }
                // Concurrent thunk factory (same key) + cross-thread call into real code
                // (re-enters attach).
                let code = thunks::get_or_create(&shared, 0x1000, 1, &sig);
                let f: unsafe extern "C" fn(u64) -> u64 =
                    unsafe { std::mem::transmute(code as usize) };
                let mut acc = 0u64;
                for i in 0..64u64 {
                    acc = acc.wrapping_add(unsafe { f(i + t) });
                }
                (code, acc)
            })
        })
        .collect();

    let mut codes = Vec::new();
    let mut accs = Vec::new();
    for h in handles {
        let (c, a) = h.join().expect("tsan_mt thread panicked");
        codes.push(c);
        accs.push(a);
    }
    let total = CELL.load(Ordering::SeqCst);
    let same_thunk = codes.windows(2).all(|w| w[0] == w[1]);
    // acc(t) = Σ_{i<64}(i+t+3) = 64t + 2208
    let accs_ok = accs
        .iter()
        .enumerate()
        .all(|(t, &a)| a == 64 * t as u64 + 2208);
    let ok = total == THREADS * N && same_thunk && accs_ok;
    let verdict = if ok { "PASS" } else { "FAIL" };
    println!(
        "{verdict} engine-atomics-thunk-cache total={total} (want {}) same_thunk={same_thunk} accs_ok={accs_ok}",
        THREADS * N
    );
    ok
}
