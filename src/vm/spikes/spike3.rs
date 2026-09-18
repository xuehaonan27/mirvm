//! Spike 3: mixed-stack unwind (**the hardest bone**, frame-abi-bytecode.md §7 candidate A
//! validation).
//!
//! Validation: on a native stack mixing interp frames + compiled frames, guest panic unwinds
//! frames along the stack, runs Drop in guest order, is caught by catch_unwind (or crossing a
//! plain-C frame = abort).
//!
//! Mechanism selection (see docs/spike3-mixed-stack-unwind.md):
//! - **guest exception = host Rust panic carrying `GuestPanic`** — this is the concrete form of
//!   candidate A, not a replacement: Rust panic itself is "platform unwinder (_Unwind_RaiseException)
//!   + Rust personality + landing pad". Raise uses `resume_unwind` (does not trigger panic hook,
//!   no noise). The catch point downcasts to distinguish: GuestPanic is handled per guest semantics;
//!   host panic (VM bug) is re-raised as-is, never swallowed.
//! - **interp frame's unwind participation = `CleanupGuard`** (host Rust spelling of landing pad):
//!   when unwind crosses the frame, the guard's Drop runs → runs this frame's cleanup chain along
//!   the current unwind edge → restores the operand region → returns (unwind continues
//!   automatically). The dynamic `unwind_edge` inside the guard is the interp frame's **dynamic
//!   LSDA** (in compiled frames this is the static call-site → landing pad table).
//! - **compiled-frame stand-in = `extern "C-unwind" fn` + drop guard**: rustc compiles the guard's
//!   Drop into that frame's landing pad, literally the same mechanism as Cranelift emitting a
//!   landing pad for compiled frames to run drop glue. Must use `"C-unwind"` ABI — plain
//!   `extern "C"` aborts when unwind crosses (Rust 1.81+), which is the first deliverable of
//!   "JIT calling convention must be unwind-capable", and also gives a free test mechanism for
//!   cross-FFI abort.

use std::cell::Cell;
use std::panic::{self, AssertUnwindSafe};
use std::process::ExitCode;

use super::bytecode::{
    BinOp, Block, Body, Operand, Program, Rvalue, Stmt, Terminator, UnwindAction,
};
use super::frame::{OperandRegion, Word};

// ===== guest exception object =====

/// guest panic payload. Carried by host Rust panic mechanism (candidate A: same unwinder + personality).
struct GuestPanic {
    payload: Word,
}

/// Initiate guest panic. `resume_unwind` does not trigger panic hook → no noise output.
fn raise_guest(payload: Word) -> ! {
    panic::resume_unwind(Box::new(GuestPanic { payload }))
}

// ===== execution context (vmctx, same discipline as spike2 / docs/designs/vmctx-passing.md) =====

type CompiledFn = extern "C-unwind" fn(*mut Ctx, u64) -> u64;

#[derive(Clone, Copy)]
enum FuncKind {
    Interp,
    Compiled(CompiledFn),
}

struct Ctx {
    prog: Program,
    kinds: Vec<FuncKind>,
    region: OperandRegion,
    /// Drop-order validation log: each Drop (interp-frame terminator / compiled-frame guard)
    /// records a value
    drop_log: Vec<Word>,
}

impl Ctx {
    fn new(prog: Program, kinds: Vec<FuncKind>) -> Self {
        Ctx {
            prog,
            kinds,
            region: OperandRegion::new(),
            drop_log: Vec::new(),
        }
    }
}

// Field-level transient borrow helpers (do not take a whole &mut *ctx — conflicts with long-lived
// &prog; see spike2 lessons)
#[inline]
fn reg_reserve(ctx: *mut Ctx, n: u32) -> usize {
    let r: &mut OperandRegion = unsafe { &mut (*ctx).region };
    r.reserve(n)
}
#[inline]
fn reg_restore(ctx: *mut Ctx, base: usize) {
    let r: &mut OperandRegion = unsafe { &mut (*ctx).region };
    r.restore(base);
}
#[inline]
fn reg_read(ctx: *mut Ctx, base: usize, slot: u32) -> Word {
    let r: &OperandRegion = unsafe { &(*ctx).region };
    r.read(base, slot)
}
#[inline]
fn reg_write(ctx: *mut Ctx, base: usize, slot: u32, v: Word) {
    let r: &mut OperandRegion = unsafe { &mut (*ctx).region };
    r.write(base, slot, v);
}
#[inline]
fn log_drop(ctx: *mut Ctx, v: Word) {
    let l: &mut Vec<Word> = unsafe { &mut (*ctx).drop_log };
    l.push(v);
}

/// Unified dispatch (same as spike2: both i2c and c2i).
fn call_guest(ctx: *mut Ctx, func: u32, args: &[Word]) -> Word {
    let kinds: &Vec<FuncKind> = unsafe { &(*ctx).kinds };
    match kinds[func as usize] {
        FuncKind::Interp => interp_frame(ctx, func, args),
        FuncKind::Compiled(f) => f(ctx, args[0]),
    }
}

// ===== interp frame's unwind participation =====

/// Frame guard = interp frame's landing pad. Forgotten on normal return; when guest panic crosses
/// the frame, its Drop runs during unwind: runs cleanup chain (if there is an edge) → restores the
/// operand region (§2.2 "unwind: restore region SP").
///
/// `unwind_edge` is updated before each unwindable terminator (Call/Panic) executes — it is the
/// interp frame's **dynamic LSDA** (the static equivalent in compiled frames: call-site → landing
/// pad table).
struct CleanupGuard {
    ctx: *mut Ctx,
    func: u32,
    base: usize,
    unwind_edge: Cell<Option<u32>>,
}

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        if let Some(blk) = self.unwind_edge.get() {
            run_cleanup_chain(self.ctx, self.func, self.base, blk);
        }
        reg_restore(self.ctx, self.base);
    }
}

/// Interpret cleanup chain (subset of Drop/Goto/Call, `Resume` ends). Runs inside guard::drop
/// (i.e. landing pad); Call in the chain can re-enter mixed execution (e.g. calling a compiled
/// helper — routine in C++ destructors, we must support it too). Panic inside chain = double panic
/// → abort (same as native).
fn run_cleanup_chain(ctx: *mut Ctx, func: u32, base: usize, entry: u32) {
    let prog: &Program = unsafe { &(*ctx).prog };
    let body: &Body = &prog.funcs[func as usize];
    let mut blk = entry as usize;
    loop {
        let block: &Block = &body.blocks[blk];
        for stmt in &block.stmts {
            exec_stmt(ctx, base, stmt);
        }
        match &block.term {
            Terminator::Goto(t) => blk = *t as usize,
            Terminator::Drop { slot, target, .. } => {
                log_drop(ctx, reg_read(ctx, base, *slot));
                blk = *target as usize;
            }
            Terminator::Call {
                func: callee,
                args: aops,
                dst,
                target,
                ..
            } => {
                let av: Vec<Word> = aops.iter().map(|o| eval_operand(ctx, base, *o)).collect();
                let r = call_guest(ctx, *callee, &av);
                reg_write(ctx, base, *dst, r);
                blk = *target as usize;
            }
            Terminator::Resume => return, // end of chain: return to guard, unwind continues automatically
            t => unreachable!("illegal cleanup terminator: {t:?}"),
        }
    }
}

#[inline]
fn edge(u: &UnwindAction) -> Option<u32> {
    match u {
        UnwindAction::Continue => None,
        UnwindAction::Cleanup(b) => Some(*b),
    }
}

// ===== interpreter main loop (unwind evolution of spike2) =====

fn interp_frame(ctx: *mut Ctx, func: u32, args: &[Word]) -> Word {
    let prog: &Program = unsafe { &(*ctx).prog };
    let body: &Body = &prog.funcs[func as usize];

    let base = reg_reserve(ctx, body.num_slots);
    for (i, a) in args.iter().enumerate() {
        reg_write(ctx, base, (i + 1) as u32, *a);
    }
    let guard = CleanupGuard {
        ctx,
        func,
        base,
        unwind_edge: Cell::new(None),
    };

    let mut blk = 0usize;
    loop {
        let block: &Block = &body.blocks[blk];
        for stmt in &block.stmts {
            exec_stmt(ctx, base, stmt);
        }
        match &block.term {
            Terminator::Goto(t) => blk = *t as usize,
            Terminator::SwitchInt {
                discr,
                targets,
                otherwise,
            } => {
                let d = eval_operand(ctx, base, *discr);
                blk = targets
                    .iter()
                    .find(|(v, _)| *v == d)
                    .map(|(_, b)| *b)
                    .unwrap_or(*otherwise) as usize;
            }
            Terminator::Call {
                func: callee,
                args: aops,
                dst,
                target,
                unwind,
            } => {
                let av: Vec<Word> = aops.iter().map(|o| eval_operand(ctx, base, *o)).collect();
                guard.unwind_edge.set(edge(unwind)); // if callee panics, this frame cleans up from this edge
                let r = call_guest(ctx, *callee, &av);
                guard.unwind_edge.set(None);
                reg_write(ctx, base, *dst, r);
                blk = *target as usize;
            }
            Terminator::Drop { slot, target, .. } => {
                log_drop(ctx, reg_read(ctx, base, *slot));
                blk = *target as usize;
            }
            Terminator::Panic { payload, unwind } => {
                let p = eval_operand(ctx, base, *payload);
                guard.unwind_edge.set(edge(unwind)); // this frame's live Drops are run by its own guard
                raise_guest(p);
            }
            Terminator::CatchCall {
                func: callee,
                args: aops,
                dst,
                catch_dst,
                target,
                catch_target,
            } => {
                let callee = *callee;
                let av: Vec<Word> = aops.iter().map(|o| eval_operand(ctx, base, *o)).collect();
                match panic::catch_unwind(AssertUnwindSafe(|| call_guest(ctx, callee, &av))) {
                    Ok(r) => {
                        reg_write(ctx, base, *dst, r);
                        blk = *target as usize;
                    }
                    Err(e) => match e.downcast::<GuestPanic>() {
                        Ok(gp) => {
                            reg_write(ctx, base, *catch_dst, gp.payload);
                            blk = *catch_target as usize;
                        }
                        // host panic (VM bug) is not a guest exception: re-raise as-is, never swallow
                        Err(host) => panic::resume_unwind(host),
                    },
                }
            }
            Terminator::Return => {
                let r = reg_read(ctx, base, 0);
                reg_restore(ctx, base);
                std::mem::forget(guard); // normal path disarms guard (unwind semantics only apply to unwind path)
                return r;
            }
            Terminator::Resume => {
                unreachable!("Resume only appears in cleanup chains (executed by CleanupGuard)")
            }
        }
    }
}

fn exec_stmt(ctx: *mut Ctx, base: usize, stmt: &Stmt) {
    match stmt {
        Stmt::Assign(dst, rv) => {
            let v = eval_rvalue(ctx, base, rv);
            reg_write(ctx, base, *dst, v);
        }
        Stmt::Store(..) => unreachable!("spike3 does not use memory constructs"),
    }
}

fn eval_operand(ctx: *mut Ctx, base: usize, op: Operand) -> Word {
    match op {
        Operand::Slot(s) => reg_read(ctx, base, s),
        Operand::Const(c) => c,
    }
}

fn eval_rvalue(ctx: *mut Ctx, base: usize, rv: &Rvalue) -> Word {
    match rv {
        Rvalue::Use(op) => eval_operand(ctx, base, *op),
        Rvalue::Binary(op, l, r) => {
            let a = eval_operand(ctx, base, *l);
            let b = eval_operand(ctx, base, *r);
            match op {
                BinOp::Add => a.wrapping_add(b),
                BinOp::Sub => a.wrapping_sub(b),
                BinOp::Mul => a.wrapping_mul(b),
                BinOp::Lt => (a < b) as u64,
                BinOp::Le => (a <= b) as u64,
                BinOp::Eq => (a == b) as u64,
                BinOp::Gt => (a > b) as u64,
                BinOp::Ge => (a >= b) as u64,
            }
        }
        rv => unreachable!("spike3 does not use memory constructs: {rv:?}"),
    }
}

// ===== compiled-frame stand-in =====

/// Compiled-frame landing-pad stand-in: rustc compiles this guard's Drop into the frame's landing
/// pad — literally the same mechanism as Cranelift emitting a landing pad for compiled frames to
/// run drop glue.
/// (normal path also drops — aligned with bytecode frame "Drop on both paths".)
struct CGuard {
    ctx: *mut Ctx,
    v: Word,
}
impl Drop for CGuard {
    fn drop(&mut self) {
        log_drop(self.ctx, self.v);
    }
}

// --- case 2: compiled frames in the mixed chain ---
extern "C-unwind" fn cc2_f1(ctx: *mut Ctx, x: u64) -> u64 {
    let _g = CGuard { ctx, v: 101 };
    call_guest(ctx, 2, &[x]).wrapping_add(1)
}
extern "C-unwind" fn cc2_f3(ctx: *mut Ctx, x: u64) -> u64 {
    let _g = CGuard { ctx, v: 103 };
    call_guest(ctx, 4, &[x]).wrapping_add(1)
}
extern "C-unwind" fn cc2_f5(ctx: *mut Ctx, _x: u64) -> u64 {
    let _g = CGuard { ctx, v: 105 };
    raise_guest(777)
}
/// Compiled helper called from cleanup chain (re-enter mixed execution inside landing pad)
extern "C-unwind" fn cc_logger(ctx: *mut Ctx, v: u64) -> u64 {
    log_drop(ctx, v);
    v
}

// --- case 3: catch in compiled frame (simulates JIT catch landing pad) ---
extern "C-unwind" fn cc3_f1_catch(ctx: *mut Ctx, x: u64) -> u64 {
    let _g = CGuard { ctx, v: 101 };
    match panic::catch_unwind(AssertUnwindSafe(|| call_guest(ctx, 2, &[x]))) {
        Ok(v) => v.wrapping_add(1),
        Err(e) => match e.downcast::<GuestPanic>() {
            Ok(gp) => gp.payload.wrapping_add(500),
            Err(host) => panic::resume_unwind(host),
        },
    }
}
extern "C-unwind" fn cc3_f3_raise(ctx: *mut Ctx, _x: u64) -> u64 {
    let _g = CGuard { ctx, v: 103 };
    raise_guest(777)
}

// --- case 4: cross-FFI abort ---
/// Simulates a real C frame: plain `extern "C"`, unwind crossing = abort (Rust 1.81+ semantics,
/// = real C handling)
extern "C" fn cc4_plain_c(ctx: *mut Ctx, x: u64) -> u64 {
    call_guest(ctx, 2, &[x])
}
extern "C-unwind" fn cc4_ffi_entry(ctx: *mut Ctx, x: u64) -> u64 {
    cc4_plain_c(ctx, x)
}
extern "C-unwind" fn cc4_raise(ctx: *mut Ctx, _x: u64) -> u64 {
    let _g = CGuard { ctx, v: 103 };
    raise_guest(777)
}

// ===== bytecode builder =====

fn s(n: u32) -> Operand {
    Operand::Slot(n)
}
fn k(n: u64) -> Operand {
    Operand::Const(n)
}
fn asgn(dst: u32, rv: Rvalue) -> Stmt {
    Stmt::Assign(dst, rv)
}
fn bin(op: BinOp, a: Operand, b: Operand) -> Rvalue {
    Rvalue::Binary(op, a, b)
}

/// Middle frame: own=100+d; calls callee (unwind edge points to cleanup);
/// normal: Drop(own), ret=callee_ret+1; cleanup: Drop(own) → Resume.
/// slots: 0=ret 1=x 2=own 3=callret 4=scratch
fn mid_body(d: u64, callee: u32) -> Body {
    use Terminator::*;
    Body {
        num_slots: 6,
        num_args: 1,
        blocks: vec![
            // bb0
            Block {
                stmts: vec![asgn(2, Rvalue::Use(k(100 + d)))],
                term: Call {
                    func: callee,
                    args: vec![s(1)],
                    dst: 3,
                    target: 1,
                    unwind: UnwindAction::Cleanup(3),
                },
            },
            // bb1 (normal): Drop(own) -> bb2
            Block {
                stmts: vec![],
                term: Drop {
                    slot: 2,
                    target: 2,
                    unwind: UnwindAction::Continue,
                },
            },
            // bb2: ret = callret + 1
            Block {
                stmts: vec![asgn(0, bin(BinOp::Add, s(3), k(1)))],
                term: Return,
            },
            // bb3 (cleanup): Drop(own) -> bb4
            Block {
                stmts: vec![],
                term: Drop {
                    slot: 2,
                    target: 4,
                    unwind: UnwindAction::Continue,
                },
            },
            // bb4: Resume
            Block {
                stmts: vec![],
                term: Resume,
            },
        ],
    }
}

/// Middle-frame variant: extra Call to compiled helper in cleanup (re-enter mixed execution inside
/// landing pad)
fn mid_body_cleanup_call(d: u64, callee: u32, logger: u32) -> Body {
    use Terminator::*;
    Body {
        num_slots: 6,
        num_args: 1,
        blocks: vec![
            Block {
                stmts: vec![asgn(2, Rvalue::Use(k(100 + d)))],
                term: Call {
                    func: callee,
                    args: vec![s(1)],
                    dst: 3,
                    target: 1,
                    unwind: UnwindAction::Cleanup(3),
                },
            },
            Block {
                stmts: vec![],
                term: Drop {
                    slot: 2,
                    target: 2,
                    unwind: UnwindAction::Continue,
                },
            },
            Block {
                stmts: vec![asgn(0, bin(BinOp::Add, s(3), k(1)))],
                term: Return,
            },
            // bb3 (cleanup): Drop(own) -> bb4
            Block {
                stmts: vec![],
                term: Drop {
                    slot: 2,
                    target: 4,
                    unwind: UnwindAction::Continue,
                },
            },
            // bb4: cleanup Calls compiled logger(9002) -> bb5
            Block {
                stmts: vec![],
                term: Call {
                    func: logger,
                    args: vec![k(9002)],
                    dst: 4,
                    target: 5,
                    unwind: UnwindAction::Continue,
                },
            },
            // bb5: Resume
            Block {
                stmts: vec![],
                term: Resume,
            },
        ],
    }
}

/// Bottom frame: own=100+d; Panic(777) (unwind edge covers its own live Drop).
fn bottom_body(d: u64) -> Body {
    use Terminator::*;
    Body {
        num_slots: 6,
        num_args: 1,
        blocks: vec![
            Block {
                stmts: vec![asgn(2, Rvalue::Use(k(100 + d)))],
                term: Panic {
                    payload: k(777),
                    unwind: UnwindAction::Cleanup(1),
                },
            },
            Block {
                stmts: vec![],
                term: Drop {
                    slot: 2,
                    target: 2,
                    unwind: UnwindAction::Continue,
                },
            },
            Block {
                stmts: vec![],
                term: Resume,
            },
        ],
    }
}

/// Top frame (catch): own=100; CatchCall(callee); normal: ret=dst, Drop(own);
/// caught: ret=payload, Drop(own). slots: 2=own 3=dst 4=catch_dst
fn top_catch_body(callee: u32) -> Body {
    use Terminator::*;
    Body {
        num_slots: 6,
        num_args: 1,
        blocks: vec![
            Block {
                stmts: vec![asgn(2, Rvalue::Use(k(100)))],
                term: CatchCall {
                    func: callee,
                    args: vec![s(1)],
                    dst: 3,
                    catch_dst: 4,
                    target: 1,
                    catch_target: 3,
                },
            },
            // bb1 (normal): ret=dst; Drop(own) -> bb2
            Block {
                stmts: vec![asgn(0, Rvalue::Use(s(3)))],
                term: Drop {
                    slot: 2,
                    target: 2,
                    unwind: UnwindAction::Continue,
                },
            },
            Block {
                stmts: vec![],
                term: Return,
            },
            // bb3 (caught): ret=payload; Drop(own) -> bb4
            Block {
                stmts: vec![asgn(0, Rvalue::Use(s(4)))],
                term: Drop {
                    slot: 2,
                    target: 4,
                    unwind: UnwindAction::Continue,
                },
            },
            Block {
                stmts: vec![],
                term: Return,
            },
        ],
    }
}

/// Top frame (no catch, for case3/4): own=100; Call(callee); normal Drop(own), ret=callret+1.
fn top_plain_body(callee: u32) -> Body {
    use Terminator::*;
    Body {
        num_slots: 6,
        num_args: 1,
        blocks: vec![
            Block {
                stmts: vec![asgn(2, Rvalue::Use(k(100)))],
                term: Call {
                    func: callee,
                    args: vec![s(1)],
                    dst: 3,
                    target: 1,
                    unwind: UnwindAction::Cleanup(3),
                },
            },
            Block {
                stmts: vec![],
                term: Drop {
                    slot: 2,
                    target: 2,
                    unwind: UnwindAction::Continue,
                },
            },
            Block {
                stmts: vec![asgn(0, bin(BinOp::Add, s(3), k(1)))],
                term: Return,
            },
            Block {
                stmts: vec![],
                term: Drop {
                    slot: 2,
                    target: 4,
                    unwind: UnwindAction::Continue,
                },
            },
            Block {
                stmts: vec![],
                term: Resume,
            },
        ],
    }
}

fn dummy_body() -> Body {
    Body {
        num_slots: 1,
        num_args: 1,
        blocks: vec![Block {
            stmts: vec![],
            term: Terminator::Return,
        }],
    }
}

// ===== programs + kinds for each case =====

/// case 1: 6-frame pure interp chain; bottom Panic(777); top frame catches.
fn case1_prog() -> (Program, Vec<FuncKind>) {
    let funcs = vec![
        top_catch_body(1),
        mid_body(1, 2),
        mid_body(2, 3),
        mid_body(3, 4),
        mid_body(4, 5),
        bottom_body(5),
    ];
    (Program { funcs }, vec![FuncKind::Interp; 6])
}

/// case 2 (headline): interp/compiled alternating; f2's cleanup Calls compiled logger.
fn case2_prog() -> (Program, Vec<FuncKind>) {
    use FuncKind::{Compiled, Interp};
    let funcs = vec![
        top_catch_body(1),              // 0 interp (catch)
        dummy_body(),                   // 1 compiled cc2_f1
        mid_body_cleanup_call(2, 3, 6), // 2 interp (cleanup Calls logger)
        dummy_body(),                   // 3 compiled cc2_f3
        mid_body(4, 5),                 // 4 interp
        dummy_body(),                   // 5 compiled cc2_f5 (raise)
        dummy_body(),                   // 6 compiled cc_logger
    ];
    let kinds = vec![
        Interp,
        Compiled(cc2_f1),
        Interp,
        Compiled(cc2_f3),
        Interp,
        Compiled(cc2_f5),
        Compiled(cc_logger),
    ];
    (Program { funcs }, kinds)
}

/// case 3: catch in compiled frame; top frame interp (no catch).
fn case3_prog() -> (Program, Vec<FuncKind>) {
    use FuncKind::{Compiled, Interp};
    let funcs = vec![
        top_plain_body(1),
        dummy_body(),
        mid_body(2, 3),
        dummy_body(),
    ];
    let kinds = vec![
        Interp,
        Compiled(cc3_f1_catch),
        Interp,
        Compiled(cc3_f3_raise),
    ];
    (Program { funcs }, kinds)
}

/// case 4: plain extern "C" frame inserted in chain; deep panic → expected abort.
fn case4_prog() -> (Program, Vec<FuncKind>) {
    use FuncKind::{Compiled, Interp};
    let funcs = vec![
        top_plain_body(1),
        dummy_body(),
        mid_body(2, 3),
        dummy_body(),
    ];
    let kinds = vec![Interp, Compiled(cc4_ffi_entry), Interp, Compiled(cc4_raise)];
    (Program { funcs }, kinds)
}

// ===== native reference implementation (structural mirror; drop order verified by comparison) =====

mod nref {
    use std::cell::RefCell;
    use std::panic::{self, AssertUnwindSafe};

    use super::GuestPanic;

    thread_local! {
        static LOG: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
    }
    fn log(v: u64) {
        LOG.with(|l| l.borrow_mut().push(v));
    }
    fn take_log() -> Vec<u64> {
        LOG.with(|l| std::mem::take(&mut *l.borrow_mut()))
    }

    struct D(u64);
    impl Drop for D {
        fn drop(&mut self) {
            log(self.0);
        }
    }

    fn raise(payload: u64) -> ! {
        panic::resume_unwind(Box::new(GuestPanic { payload }))
    }
    fn catch<F: FnOnce() -> u64>(f: F) -> Result<u64, u64> {
        match panic::catch_unwind(AssertUnwindSafe(f)) {
            Ok(v) => Ok(v),
            Err(e) => match e.downcast::<GuestPanic>() {
                Ok(gp) => Err(gp.payload),
                Err(host) => panic::resume_unwind(host),
            },
        }
    }

    // case 1: 6-frame chain, bottom raise
    fn c1_chain(d: u64) -> u64 {
        let _v = D(100 + d);
        if d == 5 {
            raise(777);
        }
        c1_chain(d + 1) + 1
    }
    pub fn case1() -> (u64, Vec<u64>) {
        let top = D(100);
        let r = match catch(|| c1_chain(1)) {
            Ok(v) => v,
            Err(p) => p,
        };
        drop(top);
        (r, take_log())
    }

    // case 2: same structure as case1, f2's Drop additionally logs 9002 via helper
    // (mirrors Call inside cleanup)
    struct D2(u64);
    impl Drop for D2 {
        fn drop(&mut self) {
            log(self.0);
            helper_9002();
        }
    }
    fn helper_9002() {
        log(9002);
    }
    fn c2_f1(x: u64) -> u64 {
        let _v = D(101);
        c2_f2(x) + 1
    }
    fn c2_f2(x: u64) -> u64 {
        let _v = D2(102);
        c2_f3(x) + 1
    }
    fn c2_f3(x: u64) -> u64 {
        let _v = D(103);
        c2_f4(x) + 1
    }
    fn c2_f4(x: u64) -> u64 {
        let _v = D(104);
        c2_f5(x) + 1
    }
    fn c2_f5(_x: u64) -> u64 {
        let _v = D(105);
        raise(777)
    }
    pub fn case2() -> (u64, Vec<u64>) {
        let top = D(100);
        let r = match catch(|| c2_f1(0)) {
            Ok(v) => v,
            Err(p) => p,
        };
        drop(top);
        (r, take_log())
    }

    // case 3: catch in middle frame
    fn c3_f1(x: u64) -> u64 {
        let _v = D(101);
        match catch(|| c3_f2(x)) {
            Ok(v) => v + 1,
            Err(p) => p + 500,
        }
    }
    fn c3_f2(x: u64) -> u64 {
        let _v = D(102);
        c3_f3(x) + 1
    }
    fn c3_f3(_x: u64) -> u64 {
        let _v = D(103);
        raise(777)
    }
    pub fn case3() -> (u64, Vec<u64>) {
        let top = D(100);
        let r = c3_f1(0) + 1;
        drop(top);
        (r, take_log())
    }
}

// ===== harness =====

fn vm_case(prog: Program, kinds: Vec<FuncKind>) -> (Word, Vec<Word>) {
    let mut ctx = Ctx::new(prog, kinds);
    let r = call_guest(&mut ctx as *mut Ctx, 0, &[0]);
    let log = std::mem::take(&mut ctx.drop_log);
    (r, log)
}

fn check(name: &str, vm: (Word, Vec<Word>), native: (Word, Vec<Word>)) -> bool {
    if vm == native {
        println!("PASS {name}  result={} drops={:?}", vm.0, vm.1);
        true
    } else {
        println!("FAIL {name}");
        println!("  vm     = ({}, {:?})", vm.0, vm.1);
        println!("  native = ({}, {:?})", native.0, native.1);
        false
    }
}

pub fn run(mut argv: impl Iterator<Item = String>) -> ExitCode {
    // child-process mode: expected to abort when mixed chain crosses a plain extern "C" frame
    if argv.next().as_deref() == Some("--case=ffi-abort") {
        let (prog, kinds) = case4_prog();
        let (r, _) = vm_case(prog, kinds);
        println!("unexpected: ffi-abort case returned {r}");
        return ExitCode::from(3);
    }

    let mut ok = true;
    {
        let (prog, kinds) = case1_prog();
        ok &= check(
            "case1 pure-interp-chain panic+Drop+catch",
            vm_case(prog, kinds),
            nref::case1(),
        );
    }
    {
        let (prog, kinds) = case2_prog();
        ok &= check(
            "case2 mixed-stack alternating (headline) + Call inside cleanup",
            vm_case(prog, kinds),
            nref::case2(),
        );
    }
    {
        let (prog, kinds) = case3_prog();
        ok &= check("case3 catch in compiled frame", vm_case(prog, kinds), nref::case3());
    }

    // case 4: cross-FFI abort (child process, assert SIGABRT)
    let exe = std::env::current_exe().expect("current_exe");
    let out = std::process::Command::new(exe)
        .args(["spike3", "--case=ffi-abort"])
        .output()
        .expect("spawn spike3 child");
    use std::os::unix::process::ExitStatusExt as _;
    let sig = out.status.signal();
    if sig == Some(libc::SIGABRT) || sig == Some(libc::SIGILL) {
        println!("PASS case4 cross-FFI abort (child signal {})", sig.unwrap());
    } else {
        println!(
            "FAIL case4 cross-FFI abort: child status {:?} (expected SIGABRT)",
            out.status
        );
        ok = false;
    }

    if ok {
        println!("--- spike3: all PASS (mixed-stack unwind validation passed, candidate A confirmed) ---");
        ExitCode::SUCCESS
    } else {
        println!("--- spike3: FAIL present ---");
        ExitCode::from(1)
    }
}
