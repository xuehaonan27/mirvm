//! M4 引擎多线程 TSan 用例（spike4 义务的 M4 真身；tests/spike4_tsan.sh 载体）。
//!
//! 8 宿主线程共享一个 `Shared`，各自边界 attach 拿每线程 Ctx：
//! ① 解释执行 guest 原子自增（AtomicRmw——引擎必须发真宿主原子指令，spike4 义务）；
//! ② thunk 工厂并发 get_or_create（Mutex 缓存，同键必得同一真码）+ 跨线程调 thunk
//!   （trampoline → attach → interp_frame 再入）。
//! TSan 零警告 = 执行相状态三分（每线程私有 / 发布后只读 / 显式同步）成立。

use std::sync::atomic::{AtomicU64, Ordering};

use super::ctx::{Shared, attach};
use super::ir::{
    Block, FfiKind, ForeignSig, FuncBody, IntBinOp, MemOrd, Module, Operand, ParamAbi, RetAbi,
    RmwOp, Rvalue, ScalarPlace, Slot, Stmt, Terminator, Width,
};
use super::{interp, thunks};

/// 手构 Module：fn0 `bump(addr)->旧值`（原子 +1）；fn1 `add3(x)->x+3`（thunk 目标）。
/// 帧布局（两函数同形）：_0 ret @0、_1 参 @8。
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
        funcs: vec![bump, add3],
        ..Default::default()
    }
}

pub fn run() -> bool {
    let shared: &'static Shared = Box::leak(Box::new(Shared::new(build_module())));
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
            std::thread::spawn(move || {
                let ctx = attach(shared);
                let addr = CELL.as_ptr() as u64;
                for _ in 0..N {
                    // M5.3a Q4 豁免：TSan harness 自用入口不经 call_guest 收拢
                    //（TSan 通道不编 cranelift，分层派发在此无意义）
                    interp::interp_frame(ctx, 0, &[addr]);
                }
                // thunk 工厂并发（同键）+ 跨线程真码调用（再入 attach）
                let code = thunks::get_or_create(shared, 0x1000, 1, &sig);
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
        let (c, a) = h.join().expect("tsan_mt 线程 panic");
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
    println!(
        "tsan_mt: total={total}（期望 {}）same_thunk={same_thunk} accs_ok={accs_ok}",
        THREADS * N
    );
    ok
}
