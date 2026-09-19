//! Spike 2: interp<->compiled adapters (i2c/c2i) on a mixed stack.
//!
//! Validates model A's **core bet**: interpreted and compiled frames share one native stack
//! and interoperate cheaply through a thin adapter.
//!
//! - A compiled frame is a **hand-written `extern "C" fn`** (not Cranelift). For validating
//!   "adapter shape + mixed stack", a rustc-compiled extern C function is equivalent at the
//!   adapter layer to Cranelift-JIT code: both follow the convention, run on the native
//!   stack, and call back through a ctx pointer. Whether Cranelift can emit this convention
//!   is a separate question, deferred.
//! - **ctx is passed as an explicit first `*mut Ctx` argument** (Cranelift/Wasmtime's vmctx).
//! - **Re-entry forces ctx to be a raw pointer rather than a Rust `&mut`**: c2i re-enters
//!   interp_frame (needing `&mut` on the operand region again), and `&mut` cannot express
//!   re-entrant shared mutability. So ctx is `*mut` throughout and fields are borrowed
//!   transiently and field-by-field, never held across call_guest. Safety rests on the
//!   operand region being a disciplined stack (caller slots below, callee slots above,
//!   disjoint slices) plus single-threaded sequential execution. The borrow must be
//!   field-level (`&mut (*ctx).region`, not `&mut *ctx`), otherwise it conflicts with the
//!   long-lived `&(*ctx).prog` (Stacked/Tree Borrows). The explicit borrows on raw-pointer
//!   method calls also sidestep edition 2024's `dangerous_implicit_autorefs`; they are
//!   wrapped in the helpers below.

use std::process::ExitCode;

use super::bytecode::{
    BinOp, Block, Body, Operand, Program, Rvalue, Stmt, Terminator, UnwindAction,
};
use super::frame::{OperandRegion, Word};
use super::memory::GuestMemory;

const FIB_A: u32 = 0;
const FIB_B: u32 = 1;

/// Compiled-frame calling convention: `(vmctx, single u64 arg) -> u64` (skeleton; the real
/// Rust ABI differs).
type CompiledFn = extern "C" fn(*mut Ctx, u64) -> u64;

/// Implementation form of each guest function. The fn pointer is Copy, so FuncKind is Copy.
#[derive(Clone, Copy)]
enum FuncKind {
    Interp,
    Compiled(CompiledFn),
}

/// Execution context (= vmctx). Owns the Program, with no lifetime parameter, so it does not
/// couple to the fn pointer's lifetime.
struct Ctx {
    prog: Program,
    kinds: Vec<FuncKind>,
    region: OperandRegion,
    mem: GuestMemory,
}

impl Ctx {
    fn new(prog: Program, kinds: Vec<FuncKind>) -> Self {
        Ctx {
            prog,
            kinds,
            region: OperandRegion::new(),
            mem: GuestMemory::new(1 << 20),
        }
    }
}

// ---- field-level transient access helpers for the raw-pointer ctx ----
// Each helper borrows explicitly and field-level (avoiding dangerous_implicit_autorefs and
// any conflict with `&prog`); the borrow never escapes the helper, so c2i re-entry is fine.

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
fn mem_alloc(ctx: *mut Ctx, size: u64) -> u64 {
    let m: &mut GuestMemory = unsafe { &mut (*ctx).mem };
    m.alloc(size)
}
#[inline]
fn mem_load(ctx: *mut Ctx, addr: u64) -> u64 {
    let m: &GuestMemory = unsafe { &(*ctx).mem };
    unsafe { m.load(addr) }
}
#[inline]
fn mem_store(ctx: *mut Ctx, addr: u64, v: u64) {
    let m: &GuestMemory = unsafe { &(*ctx).mem };
    unsafe { m.store(addr, v) };
}

/// Unified dispatch: both **i2c** (an interpreted frame calls a compiled one, routed to the
/// Compiled arm) and **c2i** (a compiled frame calls it, routed back to the Interp arm). The
/// adapter is this thin: a single u64 arg moves straight in/out of registers, with no VM
/// frame marshalling.
fn call_guest(ctx: *mut Ctx, func: u32, args: &[Word]) -> Word {
    let kinds: &Vec<FuncKind> = unsafe { &(*ctx).kinds };
    match kinds[func as usize] {
        FuncKind::Interp => interp_frame(ctx, func, args),
        FuncKind::Compiled(f) => f(ctx, args[0]), // i2c: single u64 straight into a register
    }
}

/// Tree-walking interpreter (spike 1's logic routed through dispatch). ctx is a raw pointer;
/// all region/memory access goes through helpers that borrow transiently and field-level and
/// never hold a borrow across `call_guest`, so on c2i re-entry the operand region's `&mut`
/// does not overlap this frame's (the stack slices are disjoint and the times do not overlap
/// either).
fn interp_frame(ctx: *mut Ctx, func: u32, args: &[Word]) -> Word {
    // prog is immutable; rebuild a `&Program` covering the whole loop for body/block/stmt to
    // borrow. Only its `.prog` field is read, disjoint from the `&mut` on `.region`/`.mem` in
    // the helpers.
    let prog: &Program = unsafe { &(*ctx).prog };
    let body: &Body = &prog.funcs[func as usize];

    let base = reg_reserve(ctx, body.num_slots);
    for (i, a) in args.iter().enumerate() {
        reg_write(ctx, base, (i + 1) as u32, *a); // slot0=ret, 1..=args
    }

    let mut blk = 0usize;
    loop {
        let block: &Block = &body.blocks[blk];
        for stmt in &block.stmts {
            match stmt {
                Stmt::Assign(dst, rv) => {
                    let v = eval_rvalue(ctx, base, rv);
                    reg_write(ctx, base, *dst, v);
                }
                Stmt::Store(ptr, val) => {
                    let addr = eval_operand(ctx, base, *ptr);
                    let v = eval_operand(ctx, base, *val);
                    mem_store(ctx, addr, v);
                }
            }
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
                ..
            } => {
                let av: Vec<Word> = aops.iter().map(|o| eval_operand(ctx, base, *o)).collect();
                let r = call_guest(ctx, *callee, &av); // re-entry point: no borrow held
                reg_write(ctx, base, *dst, r);
                blk = *target as usize;
            }
            Terminator::Return => {
                let r = reg_read(ctx, base, 0);
                reg_restore(ctx, base);
                return r;
            }
            t => unreachable!("spike2 bytecode subset has no unwind constructors: {t:?}"),
        }
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
        Rvalue::Binary(op, l, r) => apply_binop(
            *op,
            eval_operand(ctx, base, *l),
            eval_operand(ctx, base, *r),
        ),
        Rvalue::Alloc(size) => {
            let sz = eval_operand(ctx, base, *size);
            mem_alloc(ctx, sz)
        }
        Rvalue::Load(ptr) => {
            let addr = eval_operand(ctx, base, *ptr);
            mem_load(ctx, addr)
        }
        rv => unreachable!("spike2 bytecode subset has no concurrency constructors: {rv:?}"),
    }
}

fn apply_binop(op: BinOp, a: Word, b: Word) -> Word {
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

// ---- hand-written compiled frames: follow the vmctx convention and call the other side
// through call_guest (dynamically routed to interp or compiled) ----

extern "C" fn compiled_fib_a(ctx: *mut Ctx, n: u64) -> u64 {
    if n < 2 {
        return n;
    }
    call_guest(ctx, FIB_B, &[n - 1]).wrapping_add(call_guest(ctx, FIB_B, &[n - 2]))
}

extern "C" fn compiled_fib_b(ctx: *mut Ctx, n: u64) -> u64 {
    if n < 2 {
        return n;
    }
    call_guest(ctx, FIB_A, &[n - 1]).wrapping_add(call_guest(ctx, FIB_A, &[n - 2]))
}

// ---- bytecode version: two mutually recursive fibs (fib_a calls FIB_B, fib_b calls FIB_A) ----

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

/// fib(n)=n<2?n:callee(n-1)+callee(n-2). Slots: 0=ret 1=n 2=cond 3=n-1 4=r1 5=n-2 6=r2
fn fib_body(callee: u32) -> Body {
    use BinOp::*;
    use Rvalue::Use;
    use Terminator::*;
    let blocks = vec![
        Block {
            stmts: vec![asgn(2, bin(Lt, s(1), k(2)))],
            term: SwitchInt {
                discr: s(2),
                targets: vec![(0, 2)],
                otherwise: 1,
            },
        },
        Block {
            stmts: vec![asgn(0, Use(s(1)))],
            term: Return,
        },
        Block {
            stmts: vec![asgn(3, bin(Sub, s(1), k(1)))],
            term: Call {
                func: callee,
                args: vec![s(3)],
                dst: 4,
                target: 3,
                unwind: UnwindAction::Continue,
            },
        },
        Block {
            stmts: vec![asgn(5, bin(Sub, s(1), k(2)))],
            term: Call {
                func: callee,
                args: vec![s(5)],
                dst: 6,
                target: 4,
                unwind: UnwindAction::Continue,
            },
        },
        Block {
            stmts: vec![asgn(0, bin(Add, s(4), s(6)))],
            term: Return,
        },
    ];
    Body {
        num_slots: 7,
        num_args: 1,
        blocks,
    }
}

fn build_two_fib() -> Program {
    Program {
        funcs: vec![fib_body(FIB_B), fib_body(FIB_A)],
    }
}

fn fib_ref(n: u64) -> u64 {
    if n < 2 {
        n
    } else {
        fib_ref(n - 1) + fib_ref(n - 2)
    }
}

pub fn run() -> ExitCode {
    use FuncKind::{Compiled, Interp};
    // (name, fib_a form, fib_b form) -- four configs covering all four transitions
    let configs: [(&str, FuncKind, FuncKind); 4] = [
        ("interp  / interp  ", Interp, Interp), // interp->interp
        (
            "compiled/ compiled",
            Compiled(compiled_fib_a),
            Compiled(compiled_fib_b),
        ), // cc->cc
        ("interp  / compiled", Interp, Compiled(compiled_fib_b)), // i2c + c2i alternating
        ("compiled/ interp  ", Compiled(compiled_fib_a), Interp), // mirror
    ];

    let mut ok = true;
    for (name, ka, kb) in configs {
        let mut ctx = Ctx::new(build_two_fib(), vec![ka, kb]);
        let mut bad = false;
        for n in 0..=30u64 {
            let got = call_guest(&mut ctx as *mut Ctx, FIB_A, &[n]);
            let want = fib_ref(n);
            if got != want {
                println!("FAIL [{name}] fib({n}): {got} != {want}");
                bad = true;
                break;
            }
        }
        if bad {
            ok = false;
        } else {
            println!("PASS [{name}] fib n=0..=30");
        }
    }

    if ok {
        println!(
            "--- spike2: all PASS (i2c/c2i + mixed stack verified; model A core bet holds) ---"
        );
        ExitCode::SUCCESS
    } else {
        println!("--- spike2: FAIL ---");
        ExitCode::from(1)
    }
}
