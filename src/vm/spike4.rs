//! Spike 4：并发——N 条**真宿主线程**各跑 interp_frame，引擎过 TSan（4-spike 收官）。
//!
//! 验证 concurrency-arch.md 的三个核心主张（RFC 验收 = 过 TSan）：
//! 1. **引擎 Sync、无 GIL**：共享只读程序 + per-thread 执行态，VM 自有状态零数据竞争。
//!    用例全部设计成 guest 无竞争（C4 排除 guest 竞争），故任何 TSan 报告 = 引擎 bug。
//! 2. **阻塞 syscall 活性**（corpus §2.1 收束）：tier-0 协作调度上挂死的 socketpair
//!    场景（c_blocking_io），真线程引擎上必须跑通——阻塞只挡自己那条线程。
//! 3. **原子 = 宿主原子指令、跨 tier 互操作**：解释器执行 guest 原子必须发真宿主原子
//!    （tier-0 用普通读写模拟，单线程合法；真线程下那是引擎自身的数据竞争，TSan 会抓）；
//!    解释线程与编译线程对同一真地址原子 RMW 天然互操作（真实地址模型）。
//!
//! 状态三分（RFC §2）在骨架上落地：`Shared`（发布后只读格）+ `Ctx`（每线程私有格）。
//! `Shared` 是纯不可变数据 → Rust 自动 Sync → `&Shared` 跨 scoped 线程**编译通过**，
//! 即"执行相 tcx-free ⇒ 引擎 Sync"（C8）的类型层体现（骨架无 tcx = 模式 B 运行形态）。
//! `Ctx.shared` 用裸指针（非 &'s）：避免 CompiledFn 背 HRTB 生命周期，且与 vmctx 纪律一致；
//! 生存期由 thread::scope 保证。
//!
//! TSan 入口：`run_cases()`（tsan/ harness 复用同一份源码，见 tests/spike4_tsan.sh）。

use std::panic::{self, AssertUnwindSafe};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};

use super::bytecode::{
    BinOp, Block, Body, Operand, Program, Rvalue, Stmt, Terminator, UnwindAction,
};
use super::frame::{OperandRegion, Word};
use super::memory::GuestMemory;

// ===== guest 异常（同 spike3 机制，自含）=====

struct GuestPanic {
    payload: Word,
}

fn raise_guest(payload: Word) -> ! {
    panic::resume_unwind(Box::new(GuestPanic { payload }))
}

// ===== 状态三分：Shared（发布后只读）+ Ctx（每线程私有）=====

type CompiledFn = extern "C-unwind" fn(*mut Ctx, u64) -> u64;

#[derive(Clone, Copy)]
enum FuncKind {
    Interp,
    Compiled(CompiledFn),
}

/// 发布后只读：spawn 前建好，线程只 `&` 共享，lock-free 读。纯不可变数据 → 自动 Sync。
struct Shared {
    prog: Program,
    kinds: Vec<FuncKind>,
}

/// 每线程执行态（vmctx，每线程一份；vmctx-passing.md §1.2）。
struct Ctx {
    shared: *const Shared,
    region: OperandRegion,
    drop_log: Vec<Word>,
}

impl Ctx {
    fn new(shared: &Shared) -> Self {
        Ctx { shared, region: OperandRegion::new(), drop_log: Vec::new() }
    }
}

// 字段级瞬态借用（纪律同 spike2/3）
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

/// 统一 dispatch（i2c/c2i，同 spike2/3）。
fn call_guest(ctx: *mut Ctx, func: u32, args: &[Word]) -> Word {
    let sh: &Shared = unsafe { &*(*ctx).shared };
    match sh.kinds[func as usize] {
        FuncKind::Interp => interp_frame(ctx, func, args),
        FuncKind::Compiled(f) => f(ctx, args[0]),
    }
}

// ===== unwind 参与（同 spike3 协议，自含）=====

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
            Terminator::Call { func: callee, args: aops, dst, target, .. } => {
                let av: Vec<Word> = aops.iter().map(|o| eval_operand(ctx, base, *o)).collect();
                let r = call_guest(ctx, *callee, &av);
                reg_write(ctx, base, *dst, r);
                blk = *target as usize;
            }
            Terminator::Resume => return,
            t => unreachable!("cleanup 链非法终止子: {t:?}"),
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

// ===== 解释器（spike3 协议 + AtomicAdd）=====

fn interp_frame(ctx: *mut Ctx, func: u32, args: &[Word]) -> Word {
    let sh: &Shared = unsafe { &*(*ctx).shared };
    let body: &Body = &sh.prog.funcs[func as usize];

    let base = reg_reserve(ctx, body.num_slots);
    for (i, a) in args.iter().enumerate() {
        reg_write(ctx, base, (i + 1) as u32, *a);
    }
    let guard = CleanupGuard { ctx, func, base, unwind_edge: std::cell::Cell::new(None) };

    let mut blk = 0usize;
    loop {
        let block: &Block = &body.blocks[blk];
        for stmt in &block.stmts {
            exec_stmt(ctx, base, stmt);
        }
        match &block.term {
            Terminator::Goto(t) => blk = *t as usize,
            Terminator::SwitchInt { discr, targets, otherwise } => {
                let d = eval_operand(ctx, base, *discr);
                blk = targets
                    .iter()
                    .find(|(v, _)| *v == d)
                    .map(|(_, b)| *b)
                    .unwrap_or(*otherwise) as usize;
            }
            Terminator::Call { func: callee, args: aops, dst, target, unwind } => {
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
            Terminator::CatchCall { func: callee, args: aops, dst, catch_dst, target, catch_target } => {
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
            Terminator::Resume => unreachable!("Resume 只出现在 cleanup 链"),
        }
    }
}

fn exec_stmt(ctx: *mut Ctx, base: usize, stmt: &Stmt) {
    match stmt {
        Stmt::Assign(dst, rv) => {
            let v = eval_rvalue(ctx, base, rv);
            reg_write(ctx, base, *dst, v);
        }
        Stmt::Store(..) => unreachable!("spike4 不用 Store"),
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
            // 引擎义务：解释器执行 guest 原子必须发真宿主原子指令。
            // （用普通读写实现的话，真线程下 TSan 在此报引擎数据竞争——本 spike 的判定点。）
            let a = unsafe { AtomicU64::from_ptr(addr as *mut u64) };
            a.fetch_add(val, Ordering::SeqCst)
        }
        rv => unreachable!("spike4 不用内存构造: {rv:?}"),
    }
}

// ===== 编译帧替身 =====

/// landing pad 替身（同 spike3）：drop 时记日志（正常/unwind 两路径一致）。
struct CGuard {
    ctx: *mut Ctx,
    v: Word,
}
impl Drop for CGuard {
    fn drop(&mut self) {
        log_drop(self.ctx, self.v);
    }
}

// --- case A：混合 fib 的编译半边 ---
extern "C-unwind" fn cc_fib_b(ctx: *mut Ctx, n: u64) -> u64 {
    if n < 2 {
        return n;
    }
    call_guest(ctx, 0, &[n - 1]).wrapping_add(call_guest(ctx, 0, &[n - 2]))
}

// --- case B：编译侧原子循环（跨 tier 同址互操作）---
const ATOMIC_ITERS: u64 = 50_000;

extern "C-unwind" fn cc_atomic_loop(_ctx: *mut Ctx, addr: u64) -> u64 {
    let a = unsafe { AtomicU64::from_ptr(addr as *mut u64) };
    for _ in 0..ATOMIC_ITERS {
        a.fetch_add(1, Ordering::SeqCst);
    }
    0
}

// --- case C：阻塞 IO（corpus §2.1 收束）---
extern "C-unwind" fn cc_blocking_read(_ctx: *mut Ctx, fd: u64) -> u64 {
    let mut buf = [0u8; 1];
    // 真阻塞 read(2)：只挡本条 OS 线程（协作 tier-0 上这一步冻结全部 guest 线程）
    let n = unsafe { libc::read(fd as i32, buf.as_mut_ptr() as *mut libc::c_void, 1) };
    assert!(n == 1, "read 失败: {n}");
    buf[0] as u64
}
extern "C-unwind" fn cc_write_after_delay(_ctx: *mut Ctx, fd: u64) -> u64 {
    std::thread::sleep(std::time::Duration::from_millis(50));
    let b = [42u8];
    let n = unsafe { libc::write(fd as i32, b.as_ptr() as *const libc::c_void, 1) };
    assert!(n == 1, "write 失败: {n}");
    0
}

// --- case D：并发混合栈 unwind 的编译底帧 ---
extern "C-unwind" fn cc_d_raise(ctx: *mut Ctx, _x: u64) -> u64 {
    let _g = CGuard { ctx, v: 102 };
    raise_guest(777)
}

// ===== 字节码 builder =====

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

/// 互递归 fib（同 spike2 形状）：fib(n)=n<2?n:callee(n-1)+callee(n-2)
fn fib_body(callee: u32) -> Body {
    use BinOp::*;
    use Rvalue::Use;
    use Terminator::*;
    let blocks = vec![
        Block {
            stmts: vec![asgn(2, bin(Lt, s(1), k(2)))],
            term: SwitchInt { discr: s(2), targets: vec![(0, 2)], otherwise: 1 },
        },
        Block { stmts: vec![asgn(0, Use(s(1)))], term: Return },
        Block {
            stmts: vec![asgn(3, bin(Sub, s(1), k(1)))],
            term: Call { func: callee, args: vec![s(3)], dst: 4, target: 3, unwind: UnwindAction::Continue },
        },
        Block {
            stmts: vec![asgn(5, bin(Sub, s(1), k(2)))],
            term: Call { func: callee, args: vec![s(5)], dst: 6, target: 4, unwind: UnwindAction::Continue },
        },
        Block { stmts: vec![asgn(0, bin(Add, s(4), s(6)))], term: Return },
    ];
    Body { num_slots: 7, num_args: 1, blocks }
}

/// 原子计数循环：args = (cell_addr, iters)。槽：3=i 4=cond 5=旧值弃置
fn atomic_loop_body() -> Body {
    use BinOp::*;
    use Terminator::*;
    let blocks = vec![
        // bb0: i=0
        Block { stmts: vec![asgn(3, Rvalue::Use(k(0)))], term: Goto(1) },
        // bb1: cond = i<iters; switch{0=>exit}
        Block {
            stmts: vec![asgn(4, bin(Lt, s(3), s(2)))],
            term: SwitchInt { discr: s(4), targets: vec![(0, 3)], otherwise: 2 },
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
        Block { stmts: vec![asgn(0, Rvalue::Use(k(0)))], term: Return },
    ];
    Body { num_slots: 6, num_args: 2, blocks }
}

/// 转发帧：ret = callee(x)（给阻塞 IO 用例造一层解释帧）
fn wrap_body(callee: u32) -> Body {
    use Terminator::*;
    let blocks = vec![
        Block {
            stmts: vec![],
            term: Call { func: callee, args: vec![s(1)], dst: 3, target: 1, unwind: UnwindAction::Continue },
        },
        Block { stmts: vec![asgn(0, Rvalue::Use(s(3)))], term: Return },
    ];
    Body { num_slots: 4, num_args: 1, blocks }
}

/// 中间帧（unwind 用，同 spike3 形状）：own=100+d；cleanup Drop → Resume
fn mid_body(d: u64, callee: u32) -> Body {
    use Terminator::*;
    let blocks = vec![
        Block {
            stmts: vec![asgn(2, Rvalue::Use(k(100 + d)))],
            term: Call { func: callee, args: vec![s(1)], dst: 3, target: 1, unwind: UnwindAction::Cleanup(3) },
        },
        Block { stmts: vec![], term: Drop { slot: 2, target: 2, unwind: UnwindAction::Continue } },
        Block { stmts: vec![asgn(0, bin(BinOp::Add, s(3), k(1)))], term: Return },
        Block { stmts: vec![], term: Drop { slot: 2, target: 4, unwind: UnwindAction::Continue } },
        Block { stmts: vec![], term: Resume },
    ];
    Body { num_slots: 6, num_args: 1, blocks }
}

/// 顶帧（catch，同 spike3 形状）
fn top_catch_body(callee: u32) -> Body {
    use Terminator::*;
    let blocks = vec![
        Block {
            stmts: vec![asgn(2, Rvalue::Use(k(100)))],
            term: CatchCall { func: callee, args: vec![s(1)], dst: 3, catch_dst: 4, target: 1, catch_target: 3 },
        },
        Block {
            stmts: vec![asgn(0, Rvalue::Use(s(3)))],
            term: Drop { slot: 2, target: 2, unwind: UnwindAction::Continue },
        },
        Block { stmts: vec![], term: Return },
        Block {
            stmts: vec![asgn(0, Rvalue::Use(s(4)))],
            term: Drop { slot: 2, target: 4, unwind: UnwindAction::Continue },
        },
        Block { stmts: vec![], term: Return },
    ];
    Body { num_slots: 6, num_args: 1, blocks }
}

fn dummy_body() -> Body {
    Body { num_slots: 1, num_args: 1, blocks: vec![Block { stmts: vec![], term: Terminator::Return }] }
}

fn fib_ref(n: u64) -> u64 {
    if n < 2 { n } else { fib_ref(n - 1) + fib_ref(n - 2) }
}

// ===== 用例 =====

const N_THREADS: usize = 8;

/// A：8 线程并行跑混合 fib（i2c/c2i 并发发生；共享只读程序 lock-free 读）
fn case_a() -> bool {
    let shared = Shared {
        prog: Program { funcs: vec![fib_body(1), dummy_body()] },
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
        println!("PASS caseA 并行混合 fib（{N_THREADS} 线程 × fib(22)={want}，i2c/c2i 并发）");
    } else {
        println!("FAIL caseA: {results:?} != {want}");
    }
    pass
}

/// B：跨 tier 原子计数——4 解释线程（AtomicAdd 字节码）+ 4 编译线程（fetch_add）同址
fn case_b() -> bool {
    let mut mem = GuestMemory::new(4096);
    let cell = mem.alloc(8);
    unsafe { mem.store(cell, 0) };

    let shared = Shared {
        prog: Program { funcs: vec![atomic_loop_body(), dummy_body()] },
        kinds: vec![FuncKind::Interp, FuncKind::Compiled(cc_atomic_loop)],
    };
    std::thread::scope(|sc| {
        for t in 0..N_THREADS {
            let sh = &shared;
            sc.spawn(move || {
                let mut ctx = Ctx::new(sh);
                let func = (t % 2) as u32; // 偶=解释 AtomicAdd，奇=编译 fetch_add
                call_guest(&mut ctx as *mut Ctx, func, &[cell, ATOMIC_ITERS]);
            });
        }
    });
    let total = unsafe { mem.load(cell) }; // scope join 提供 happens-before
    let want = N_THREADS as u64 * ATOMIC_ITERS;
    let pass = total == want;
    if pass {
        println!("PASS caseB 跨 tier 原子计数（4 解释 + 4 编译线程同址，total={total}）");
    } else {
        println!("FAIL caseB: total={total} != {want}");
    }
    pass
}

/// C：阻塞 syscall 活性（corpus §2.1 收束）——tier-0 协作调度上同构程序挂死（c_blocking_io）
fn case_c() -> bool {
    let mut fds = [0i32; 2];
    let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
    assert!(rc == 0, "socketpair 失败");
    let (rfd, wfd) = (fds[0] as u64, fds[1] as u64);

    let shared = Shared {
        prog: Program { funcs: vec![wrap_body(2), wrap_body(3), dummy_body(), dummy_body()] },
        kinds: vec![
            FuncKind::Interp,
            FuncKind::Interp,
            FuncKind::Compiled(cc_blocking_read),
            FuncKind::Compiled(cc_write_after_delay),
        ],
    };
    let got = std::thread::scope(|sc| {
        let sh = &shared;
        // guest 线程 A：解释帧 → 编译帧 → 真阻塞 read(2)（等 B 写）
        let a = sc.spawn(move || {
            let mut ctx = Ctx::new(sh);
            call_guest(&mut ctx as *mut Ctx, 0, &[rfd])
        });
        // guest 线程 B：延时后写——真线程下 A 的阻塞不挡 B
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
        println!("PASS caseC 阻塞 IO 活性（真 read(2) 只挡自己；tier-0 同构程序挂死 → 已收束）");
    } else {
        println!("FAIL caseC: got {got} != 42");
    }
    pass
}

/// D：8 线程并发混合栈 unwind（per-thread panic→Drop→catch，unwind 机器每线程独立）
fn case_d() -> bool {
    let shared = Shared {
        prog: Program { funcs: vec![top_catch_body(1), mid_body(1, 2), dummy_body()] },
        kinds: vec![FuncKind::Interp, FuncKind::Interp, FuncKind::Compiled(cc_d_raise)],
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
        println!("PASS caseD 并发混合栈 unwind（{N_THREADS} 线程 × panic+Drop+catch，逐线程日志正确）");
    } else {
        println!("FAIL caseD: {results:?} != {expect:?}");
    }
    pass
}

// ===== 双入口 =====

/// TSan harness（tsan/）与 CLI 共用的用例入口。
pub fn run_cases() -> bool {
    let mut ok = true;
    ok &= case_a();
    ok &= case_b();
    ok &= case_c();
    ok &= case_d();
    ok
}

pub fn run() -> ExitCode {
    if run_cases() {
        println!("--- spike4: 全 PASS（真线程引擎并发验证通过，4-spike 收官）---");
        ExitCode::SUCCESS
    } else {
        println!("--- spike4: 有 FAIL ---");
        ExitCode::from(1)
    }
}
