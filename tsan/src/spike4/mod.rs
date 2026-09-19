//! Concurrency workload for the TSan harness: N **real host threads** each run `interp_frame`,
//! and the verdict is TSan-clean (gate: `tests/suites/runtime/tsan.sh`).
//!
//! Three claims the cases pin:
//! 1. **Engine is Sync, no GIL**: shared read-only program + per-thread execution state, zero
//!    data races on VM-owned state. The cases keep the guest race-free (guest races are out
//!    of contract), so any TSan report is an engine bug.
//! 2. **Blocking syscall liveness**: the socketpair scenario that deadlocks under tier-0
//!    cooperative scheduling must pass on real threads -- blocking stalls only its own OS
//!    thread.
//! 3. **Atomics = host atomics, cross-tier interoperable**: the interpreter executing a guest
//!    atomic must issue a real host atomic (tier-0 may simulate with plain reads/writes, which
//!    is legal single-threaded; on real threads that becomes an engine data race that TSan
//!    catches). Interpreter and compiler threads interoperate on the same real address through
//!    atomic RMW (real address model).
//!
//! The three-part state is realized in the skeleton: `Shared` (read-only after publication) +
//! `Ctx` (per-thread private cell). `Shared` is pure immutable data -> Rust Sync -> `&Shared`
//! works across scoped threads, the type-level expression of "execution phase is tcx-free =>
//! engine Sync" (the skeleton has no tcx, matching the mode B runtime shape). `Ctx.shared` is
//! a raw pointer (not `&`): it avoids burdening CompiledFn with HRTB lifetimes and matches the
//! vmctx discipline; `thread::scope` guarantees the lifetime.
//!
//! These cases were written as spike 4 and moved here when the spike tree was archived; the
//! harness owns them now, so `bytecode`/`frame`/`memory` below are private to this module
//! rather than shared product sources.

mod bytecode;
mod frame;
mod memory;

use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering};

use self::bytecode::{
    BinOp, Block, Body, Operand, Program, Rvalue, Stmt, Terminator, UnwindAction,
};
use self::frame::{OperandRegion, Word};
use self::memory::GuestMemory;

// ===== guest exceptions (same mechanism as spike3, self-contained) =====

struct GuestPanic {
    payload: Word,
}

fn raise_guest(payload: Word) -> ! {
    panic::resume_unwind(Box::new(GuestPanic { payload }))
}

// ===== Three-part state: Shared (read-only after publication) + Ctx (per-thread private) =====

type CompiledFn = extern "C-unwind" fn(*mut Ctx, u64) -> u64;

#[derive(Clone, Copy)]
enum FuncKind {
    Interp,
    Compiled(CompiledFn),
}

/// Read-only after publication: built before spawn, threads only hold `&` references,
/// lock-free reads. Pure immutable data -> automatically Sync.
struct Shared {
    prog: Program,
    kinds: Vec<FuncKind>,
}

/// Per-thread execution state (one vmctx per thread).
struct Ctx {
    shared: *const Shared,
    region: OperandRegion,
    drop_log: Vec<Word>,
}

impl Ctx {
    fn new(shared: &Shared) -> Self {
        Ctx {
            shared,
            region: OperandRegion::new(),
            drop_log: Vec::new(),
        }
    }
}

// Field-level transient borrows (same discipline as spike2/3)
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

/// Unified dispatch (i2c/c2i, same as spike2/3).
fn call_guest(ctx: *mut Ctx, func: u32, args: &[Word]) -> Word {
    let sh: &Shared = unsafe { &*(*ctx).shared };
    match sh.kinds[func as usize] {
        FuncKind::Interp => interp_frame(ctx, func, args),
        FuncKind::Compiled(f) => f(ctx, args[0]),
    }
}

// ===== unwind participation (same protocol as spike3, self-contained) =====

struct CleanupGuard {
    ctx: *mut Ctx,
    func: u32,
    base: usize,
    unwind_edge: std::cell::Cell<Option<u32>>,
}

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        if let Some(blk) = self.unwind_edge.get() {
            run_cleanup_chain(self.ctx, self.func, self.base, blk);
        }
        reg_restore(self.ctx, self.base);
    }
}

fn run_cleanup_chain(ctx: *mut Ctx, func: u32, base: usize, entry: u32) {
    let sh: &Shared = unsafe { &*(*ctx).shared };
    let body: &Body = &sh.prog.funcs[func as usize];
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
            Terminator::Resume => return,
            t => unreachable!("cleanup chain has illegal terminator: {t:?}"),
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

// ===== Interpreter (spike3 protocol + AtomicAdd) =====

fn interp_frame(ctx: *mut Ctx, func: u32, args: &[Word]) -> Word {
    let sh: &Shared = unsafe { &*(*ctx).shared };
    let body: &Body = &sh.prog.funcs[func as usize];

    let base = reg_reserve(ctx, body.num_slots);
    for (i, a) in args.iter().enumerate() {
        reg_write(ctx, base, (i + 1) as u32, *a);
    }
    let guard = CleanupGuard {
        ctx,
        func,
        base,
        unwind_edge: std::cell::Cell::new(None),
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
                guard.unwind_edge.set(edge(unwind));
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
                guard.unwind_edge.set(edge(unwind));
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
                        Err(host) => panic::resume_unwind(host),
                    },
                }
            }
            Terminator::Return => {
                let r = reg_read(ctx, base, 0);
                reg_restore(ctx, base);
                std::mem::forget(guard);
                return r;
            }
            Terminator::Resume => unreachable!("Resume only appears in cleanup chains"),
        }
    }
}

fn exec_stmt(ctx: *mut Ctx, base: usize, stmt: &Stmt) {
    match stmt {
        Stmt::Assign(dst, rv) => {
            let v = eval_rvalue(ctx, base, rv);
            reg_write(ctx, base, *dst, v);
        }
        Stmt::Store(..) => unreachable!("spike4 does not use Store"),
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
        Rvalue::AtomicAdd(p, v) => {
            let addr = eval_operand(ctx, base, *p);
            let val = eval_operand(ctx, base, *v);
            // Engine obligation: the interpreter executing a guest atomic must issue a real host
            // atomic instruction.
            // (Implementing this with plain reads/writes would make TSan report an engine data race
            // here under real threads -- the deciding point of this spike.)
            let a = unsafe { AtomicU64::from_ptr(addr as *mut u64) };
            a.fetch_add(val, Ordering::SeqCst)
        }
        rv => unreachable!("spike4 does not use memory constructors: {rv:?}"),
    }
}

// ===== Compiled frame stand-ins =====

/// Landing-pad stand-in (same as spike3): logs on drop (normal and unwind paths alike).
struct CGuard {
    ctx: *mut Ctx,
    v: Word,
}
impl Drop for CGuard {
    fn drop(&mut self) {
        log_drop(self.ctx, self.v);
    }
}

// --- case A: compiled half of mixed fib ---
extern "C-unwind" fn cc_fib_b(ctx: *mut Ctx, n: u64) -> u64 {
    if n < 2 {
        return n;
    }
    call_guest(ctx, 0, &[n - 1]).wrapping_add(call_guest(ctx, 0, &[n - 2]))
}

// --- case B: compiled-side atomic loop (cross-tier interop on the same address) ---
const ATOMIC_ITERS: u64 = 50_000;

extern "C-unwind" fn cc_atomic_loop(_ctx: *mut Ctx, addr: u64) -> u64 {
    let a = unsafe { AtomicU64::from_ptr(addr as *mut u64) };
    for _ in 0..ATOMIC_ITERS {
        a.fetch_add(1, Ordering::SeqCst);
    }
    0
}

// --- case C: blocking IO ---
extern "C-unwind" fn cc_blocking_read(_ctx: *mut Ctx, fd: u64) -> u64 {
    let mut buf = [0u8; 1];
    // Real blocking read(2): stalls only this OS thread (unlike tier-0 cooperative
    // scheduling, where this step froze every guest thread).
    let n = unsafe { libc::read(fd as i32, buf.as_mut_ptr() as *mut libc::c_void, 1) };
    assert!(n == 1, "read failed: {n}");
    buf[0] as u64
}
extern "C-unwind" fn cc_write_after_delay(_ctx: *mut Ctx, fd: u64) -> u64 {
    std::thread::sleep(std::time::Duration::from_millis(50));
    let b = [42u8];
    let n = unsafe { libc::write(fd as i32, b.as_ptr() as *const libc::c_void, 1) };
    assert!(n == 1, "write failed: {n}");
    0
}

// --- case D: compiled bottom frame for concurrent mixed-stack unwind ---
extern "C-unwind" fn cc_d_raise(ctx: *mut Ctx, _x: u64) -> u64 {
    let _g = CGuard { ctx, v: 102 };
    raise_guest(777)
}

// ===== bytecode builders =====

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

/// Mutually recursive fib (same shape as spike2): fib(n)=n<2?n:callee(n-1)+callee(n-2)
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

/// Atomic counting loop: args = (cell_addr, iters). Slots: 3=i 4=cond 5=discarded previous
/// value
fn atomic_loop_body() -> Body {
    use BinOp::*;
    use Terminator::*;
    let blocks = vec![
        // bb0: i=0
        Block {
            stmts: vec![asgn(3, Rvalue::Use(k(0)))],
            term: Goto(1),
        },
        // bb1: cond = i<iters; switch{0=>exit}
        Block {
            stmts: vec![asgn(4, bin(Lt, s(3), s(2)))],
            term: SwitchInt {
                discr: s(4),
                targets: vec![(0, 3)],
                otherwise: 2,
            },
        },
        // bb2: _5 = atomic_add(cell, 1); i+=1
        Block {
            stmts: vec![
                asgn(5, Rvalue::AtomicAdd(s(1), k(1))),
                asgn(3, bin(Add, s(3), k(1))),
            ],
            term: Goto(1),
        },
        // bb3: ret=0
        Block {
            stmts: vec![asgn(0, Rvalue::Use(k(0)))],
            term: Return,
        },
    ];
    Body {
        num_slots: 6,
        num_args: 2,
        blocks,
    }
}

/// Forwarding frame: ret = callee(x); gives the blocking-IO case one interpreted frame.
fn wrap_body(callee: u32) -> Body {
    use Terminator::*;
    let blocks = vec![
        Block {
            stmts: vec![],
            term: Call {
                func: callee,
                args: vec![s(1)],
                dst: 3,
                target: 1,
                unwind: UnwindAction::Continue,
            },
        },
        Block {
            stmts: vec![asgn(0, Rvalue::Use(s(3)))],
            term: Return,
        },
    ];
    Body {
        num_slots: 4,
        num_args: 1,
        blocks,
    }
}

/// Middle frame (for unwind, same shape as spike3): own=100+d; cleanup Drop -> Resume
fn mid_body(d: u64, callee: u32) -> Body {
    use Terminator::*;
    let blocks = vec![
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
    ];
    Body {
        num_slots: 6,
        num_args: 1,
        blocks,
    }
}

/// Top frame (catch, same shape as spike3)
fn top_catch_body(callee: u32) -> Body {
    use Terminator::*;
    let blocks = vec![
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
    ];
    Body {
        num_slots: 6,
        num_args: 1,
        blocks,
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

fn fib_ref(n: u64) -> u64 {
    if n < 2 {
        n
    } else {
        fib_ref(n - 1) + fib_ref(n - 2)
    }
}

// ===== cases =====

const N_THREADS: usize = 8;

/// A: 8 threads run mixed fib in parallel (i2c/c2i happen concurrently; the shared read-only
/// program is read lock-free).
fn case_a() -> bool {
    let shared = Shared {
        prog: Program {
            funcs: vec![fib_body(1), dummy_body()],
        },
        kinds: vec![FuncKind::Interp, FuncKind::Compiled(cc_fib_b)],
    };
    let want = fib_ref(22);
    let results: Vec<Word> = std::thread::scope(|sc| {
        let handles: Vec<_> = (0..N_THREADS)
            .map(|_| {
                let sh = &shared;
                sc.spawn(move || {
                    let mut ctx = Ctx::new(sh);
                    call_guest(&mut ctx as *mut Ctx, 0, &[22])
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let pass = results.iter().all(|&r| r == want);
    if pass {
        println!(
            "PASS caseA parallel mixed fib ({N_THREADS} threads x fib(22)={want}, i2c/c2i concurrent)"
        );
    } else {
        println!("FAIL caseA: {results:?} != {want}");
    }
    pass
}

/// B: cross-tier atomic counting -- 4 interpreted threads (AtomicAdd bytecode) + 4 compiled
/// threads (fetch_add) on the same address.
fn case_b() -> bool {
    let mut mem = GuestMemory::new(4096);
    let cell = mem.alloc(8);
    unsafe { mem.store(cell, 0) };

    let shared = Shared {
        prog: Program {
            funcs: vec![atomic_loop_body(), dummy_body()],
        },
        kinds: vec![FuncKind::Interp, FuncKind::Compiled(cc_atomic_loop)],
    };
    std::thread::scope(|sc| {
        for t in 0..N_THREADS {
            let sh = &shared;
            sc.spawn(move || {
                let mut ctx = Ctx::new(sh);
                let func = (t % 2) as u32; // even = interpreted AtomicAdd, odd = compiled fetch_add
                call_guest(&mut ctx as *mut Ctx, func, &[cell, ATOMIC_ITERS]);
            });
        }
    });
    let total = unsafe { mem.load(cell) }; // scope join provides happens-before
    let want = N_THREADS as u64 * ATOMIC_ITERS;
    let pass = total == want;
    if pass {
        println!(
            "PASS caseB cross-tier atomic counting (4 interpreted + 4 compiled threads, same address, total={total})"
        );
    } else {
        println!("FAIL caseB: total={total} != {want}");
    }
    pass
}

/// C: blocking syscall liveness -- the isomorphic program deadlocks under tier-0 cooperative
/// scheduling.
fn case_c() -> bool {
    let mut fds = [0i32; 2];
    let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
    assert!(rc == 0, "socketpair failed");
    let (rfd, wfd) = (fds[0] as u64, fds[1] as u64);

    let shared = Shared {
        prog: Program {
            funcs: vec![wrap_body(2), wrap_body(3), dummy_body(), dummy_body()],
        },
        kinds: vec![
            FuncKind::Interp,
            FuncKind::Interp,
            FuncKind::Compiled(cc_blocking_read),
            FuncKind::Compiled(cc_write_after_delay),
        ],
    };
    let got = std::thread::scope(|sc| {
        let sh = &shared;
        // guest thread A: interpreted frame -> compiled frame -> real blocking read(2)
        // (waits for B's write)
        let a = sc.spawn(move || {
            let mut ctx = Ctx::new(sh);
            call_guest(&mut ctx as *mut Ctx, 0, &[rfd])
        });
        // guest thread B: write after a delay -- on real threads A's block does not stall B
        let sh = &shared;
        let b = sc.spawn(move || {
            let mut ctx = Ctx::new(sh);
            call_guest(&mut ctx as *mut Ctx, 1, &[wfd])
        });
        b.join().unwrap();
        a.join().unwrap()
    });
    unsafe {
        libc::close(fds[0]);
        libc::close(fds[1]);
    }
    let pass = got == 42;
    if pass {
        println!("PASS caseC blocking IO liveness (a real read(2) stalls only its own thread)");
    } else {
        println!("FAIL caseC: got {got} != 42");
    }
    pass
}

/// D: 8 threads unwind concurrently on mixed stacks (per-thread panic -> Drop -> catch; the
/// unwind machinery is per-thread).
fn case_d() -> bool {
    let shared = Shared {
        prog: Program {
            funcs: vec![top_catch_body(1), mid_body(1, 2), dummy_body()],
        },
        kinds: vec![
            FuncKind::Interp,
            FuncKind::Interp,
            FuncKind::Compiled(cc_d_raise),
        ],
    };
    let expect: (Word, Vec<Word>) = (777, vec![102, 101, 100]);
    let results: Vec<(Word, Vec<Word>)> = std::thread::scope(|sc| {
        let handles: Vec<_> = (0..N_THREADS)
            .map(|_| {
                let sh = &shared;
                sc.spawn(move || {
                    let mut ctx = Ctx::new(sh);
                    let r = call_guest(&mut ctx as *mut Ctx, 0, &[0]);
                    (r, std::mem::take(&mut ctx.drop_log))
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let pass = results.iter().all(|r| *r == expect);
    if pass {
        println!(
            "PASS caseD concurrent mixed-stack unwind ({N_THREADS} threads x panic+Drop+catch, per-thread logs correct)"
        );
    } else {
        println!("FAIL caseD: {results:?} != {expect:?}");
    }
    pass
}

// ===== entry point =====

/// Run all four cases; `false` means at least one failed (the harness then exits non-zero).
pub fn run_cases() -> bool {
    let mut ok = true;
    ok &= case_a();
    ok &= case_b();
    ok &= case_c();
    ok &= case_d();
    ok
}
