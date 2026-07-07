//! Spike 2：interp↔compiled 适配（i2c/c2i）+ 混合栈。
//!
//! 验证模型 A 的**核心赌注**：解释帧与编译帧同在一条 native 栈上、靠薄适配器廉价互操作。
//!
//! - 编译帧 = **手写 `extern "C" fn`**（非 Cranelift）。对"适配器形状 + 混合栈"的验证，
//!   rustc 编译的 extern C 函数与 Cranelift-JIT 的码在适配器层等价（都是遵循约定、在
//!   native 栈上、经 ctx 指针回调的真 native 码）。"Cranelift 能否发此约定"是另一问题（后置）。
//! - **ctx 传递 = 显式 `*mut Ctx` 首参**（= Cranelift/Wasmtime 的 vmctx 参数）。
//! - **再入迫使 ctx 为裸指针而非 Rust `&mut`**：c2i 会再入 interp_frame（又要 &mut 操作数区），
//!   `&mut` 无法表达再入式共享可变。故 ctx 全程 `*mut`，对字段做**瞬态、字段级**的显式借用、
//!   绝不跨 call_guest 持有。安全性来自：操作数区是纪律化的栈（caller 槽在下、callee 槽在上，
//!   切片不相交）+ 单线程顺序执行。注意必须**字段级** `&mut (*ctx).region`（非整体 `&mut *ctx`），
//!   否则与长活的 `&(*ctx).prog` 冲突（Stacked/Tree Borrows）。裸指针方法调用的显式借用还规避了
//!   edition 2024 的 `dangerous_implicit_autorefs`——封进下面几个 helper。
//!
//! 详见 docs/spike2-interp-compiled-adapters.md。

use std::process::ExitCode;

use super::bytecode::{BinOp, Block, Body, Operand, Program, Rvalue, Stmt, Terminator};
use super::frame::{OperandRegion, Word};
use super::memory::GuestMemory;

const FIB_A: u32 = 0;
const FIB_B: u32 = 1;

/// 编译帧的调用约定：`(vmctx, 单 u64 参) -> u64`（skeleton；真 Rust ABI 见文档教训）。
type CompiledFn = extern "C" fn(*mut Ctx, u64) -> u64;

/// 每个 guest 函数的实现形态。fn 指针 Copy → FuncKind Copy。
#[derive(Clone, Copy)]
enum FuncKind {
    Interp,
    Compiled(CompiledFn),
}

/// 执行上下文（= vmctx）。拥有 Program（无生命周期参，避开 fn 指针的生命周期耦合）。
struct Ctx {
    prog: Program,
    kinds: Vec<FuncKind>,
    region: OperandRegion,
    mem: GuestMemory,
}

impl Ctx {
    fn new(prog: Program, kinds: Vec<FuncKind>) -> Self {
        Ctx { prog, kinds, region: OperandRegion::new(), mem: GuestMemory::new(1 << 20) }
    }
}

// ---- 裸指针 ctx 的字段级瞬态访问 helper ----
// 每个 helper 内做**显式、字段级**借用（规避 dangerous_implicit_autorefs；不与 &prog 整体冲突），
// 借用不跨调用外泄，容许 c2i 再入。

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

/// 统一 dispatch：既是 **i2c**（解释帧调编译帧：路由到 Compiled 分支）也是 **c2i**（编译帧
/// 调它、路由回 Interp 分支）。适配器就这么薄——单 u64 参直接进/出寄存器，无 VM 帧编组。
fn call_guest(ctx: *mut Ctx, func: u32, args: &[Word]) -> Word {
    let kinds: &Vec<FuncKind> = unsafe { &(*ctx).kinds };
    match kinds[func as usize] {
        FuncKind::Interp => interp_frame(ctx, func, args),
        FuncKind::Compiled(f) => f(ctx, args[0]), // i2c：单 u64 直进寄存器
    }
}

/// tree-walking 解释器（Spike 1 逻辑的 dispatch 化演进）。ctx 为裸指针；一切区/内存访问经
/// helper 做瞬态、字段级借用，绝不把借用跨 `call_guest` 持有——这样 c2i 再入时对操作数区的
/// `&mut` 与本帧不重叠（栈切片不相交，时序也不重叠）。
fn interp_frame(ctx: *mut Ctx, func: u32, args: &[Word]) -> Word {
    // prog 不可变，重建一个覆盖整循环的 &Program 供 body/block/stmt 借用；只读它的 .prog 字段，
    // 与 helper 里对 .region/.mem 字段的 &mut 互不重叠。
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
            Terminator::SwitchInt { discr, targets, otherwise } => {
                let d = eval_operand(ctx, base, *discr);
                blk = targets
                    .iter()
                    .find(|(v, _)| *v == d)
                    .map(|(_, b)| *b)
                    .unwrap_or(*otherwise) as usize;
            }
            Terminator::Call { func: callee, args: aops, dst, target } => {
                let av: Vec<Word> = aops.iter().map(|o| eval_operand(ctx, base, *o)).collect();
                let r = call_guest(ctx, *callee, &av); // 再入点：不持任何借用
                reg_write(ctx, base, *dst, r);
                blk = *target as usize;
            }
            Terminator::Return => {
                let r = reg_read(ctx, base, 0);
                reg_restore(ctx, base);
                return r;
            }
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
        Rvalue::Binary(op, l, r) => {
            apply_binop(*op, eval_operand(ctx, base, *l), eval_operand(ctx, base, *r))
        }
        Rvalue::Alloc(size) => {
            let sz = eval_operand(ctx, base, *size);
            mem_alloc(ctx, sz)
        }
        Rvalue::Load(ptr) => {
            let addr = eval_operand(ctx, base, *ptr);
            mem_load(ctx, addr)
        }
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

// ---- 手写编译帧：遵循 vmctx 约定，经 call_guest 调对方（动态路由到 interp 或 compiled）----

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

// ---- 字节码版：两个互递归 fib（fib_a 调 FIB_B、fib_b 调 FIB_A）----

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

/// fib(n)=n<2?n:callee(n-1)+callee(n-2)。槽：0=ret 1=n 2=cond 3=n-1 4=r1 5=n-2 6=r2
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
            term: Call { func: callee, args: vec![s(3)], dst: 4, target: 3 },
        },
        Block {
            stmts: vec![asgn(5, bin(Sub, s(1), k(2)))],
            term: Call { func: callee, args: vec![s(5)], dst: 6, target: 4 },
        },
        Block { stmts: vec![asgn(0, bin(Add, s(4), s(6)))], term: Return },
    ];
    Body { num_slots: 7, num_args: 1, blocks }
}

fn build_two_fib() -> Program {
    Program { funcs: vec![fib_body(FIB_B), fib_body(FIB_A)] }
}

fn fib_ref(n: u64) -> u64 {
    if n < 2 { n } else { fib_ref(n - 1) + fib_ref(n - 2) }
}

pub fn run() -> ExitCode {
    use FuncKind::{Compiled, Interp};
    // (名称, fib_a 形态, fib_b 形态) —— 4 配置覆盖四种转移
    let configs: [(&str, FuncKind, FuncKind); 4] = [
        ("interp  / interp  ", Interp, Interp), // interp→interp
        ("compiled/ compiled", Compiled(compiled_fib_a), Compiled(compiled_fib_b)), // cc→cc
        ("interp  / compiled", Interp, Compiled(compiled_fib_b)), // i2c + c2i 交替（混合栈）
        ("compiled/ interp  ", Compiled(compiled_fib_a), Interp), // 镜像
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
        println!("--- spike2: 全 PASS（i2c/c2i + 混合栈验证通过，模型 A 核心赌注成立）---");
        ExitCode::SUCCESS
    } else {
        println!("--- spike2: 有 FAIL ---");
        ExitCode::from(1)
    }
}
