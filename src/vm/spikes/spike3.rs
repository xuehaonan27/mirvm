//! Spike 3：混合栈 unwind（**头号硬骨头**，frame-abi-bytecode.md §7 候选 A 的验证）。
//!
//! 验证：一条混着解释帧 + 编译帧的 native 栈上，guest panic 沿栈退帧、按 guest 顺序跑
//! Drop、被 catch_unwind 接住（或穿 plain-C 帧 = abort）。
//!
//! 机制选型（详见 docs/spike3-mixed-stack-unwind.md）：
//! - **guest 异常 = 宿主 Rust panic 载 `GuestPanic`** —— 这是候选 A 的具象而非替代品：
//!   Rust panic 本身就是"平台 unwinder（_Unwind_RaiseException）+ Rust personality +
//!   landing pad"。raise 用 `resume_unwind`（不触发 panic hook，无噪声）。catch 点
//!   downcast 区分：GuestPanic 按 guest 语义处理；宿主 panic（VM bug）原样续传，绝不吞。
//! - **解释帧的 unwind 参与 = `CleanupGuard`**（landing pad 的宿主 Rust 写法）：unwind
//!   穿帧时 guard 的 Drop 执行 → 按"当前 unwind 边"解释跑本帧 cleanup 链 → 恢复操作数区
//!   → 返回（unwind 自动继续）。guard 里的动态 `unwind_edge` = 解释帧的"动态 LSDA"
//!   （编译帧里这是静态 call-site → landing pad 表）。
//! - **编译帧替身 = `extern "C-unwind" fn` + drop guard**：rustc 把 guard 的 Drop 编进
//!   该帧的 landing pad，与 Cranelift 给编译帧发 landing pad 跑 drop glue 机制字面相同。
//!   注意必须 `"C-unwind"` ABI —— plain `extern "C"` 在 unwind 穿过时 abort（Rust 1.81+），
//!   这正是"JIT 调用约定必须 unwind-capable"的第一条产出，也免费给了跨 FFI abort 的测试机制。

use std::cell::Cell;
use std::panic::{self, AssertUnwindSafe};
use std::process::ExitCode;

use super::bytecode::{
    BinOp, Block, Body, Operand, Program, Rvalue, Stmt, Terminator, UnwindAction,
};
use super::frame::{OperandRegion, Word};

// ===== guest 异常对象 =====

/// guest panic 载荷。宿主 Rust panic 机制承载（候选 A：同一 unwinder + personality）。
struct GuestPanic {
    payload: Word,
}

/// 发起 guest panic。`resume_unwind` 不触发 panic hook → 无噪声输出。
fn raise_guest(payload: Word) -> ! {
    panic::resume_unwind(Box::new(GuestPanic { payload }))
}

// ===== 执行上下文（vmctx，纪律同 spike2 / docs/vmctx-passing.md）=====

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
    /// Drop 顺序验证日志：每个 Drop（解释帧终止子 / 编译帧 guard）记一个值
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

// 字段级瞬态借用 helper（勿整体 &mut *ctx——与长活 &prog 冲突；见 spike2 教训）
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

/// 统一 dispatch（同 spike2：既是 i2c 也是 c2i）。
fn call_guest(ctx: *mut Ctx, func: u32, args: &[Word]) -> Word {
    let kinds: &Vec<FuncKind> = unsafe { &(*ctx).kinds };
    match kinds[func as usize] {
        FuncKind::Interp => interp_frame(ctx, func, args),
        FuncKind::Compiled(f) => f(ctx, args[0]),
    }
}

// ===== 解释帧的 unwind 参与 =====

/// 帧守卫 = 解释帧的 landing pad。正常返回被 forget；guest panic 穿帧时其 Drop 在
/// unwind 中执行：跑 cleanup 链（若有边）→ 恢复操作数区（§2.2 "unwind: 恢复区 SP"）。
///
/// `unwind_edge` 在每个可 unwind 终止子（Call/Panic）执行前更新——它就是解释帧的
/// **动态 LSDA**（编译帧的静态等价物：call-site → landing pad 表）。
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

/// 解释执行 cleanup 链（Drop/Goto/Call 子集，`Resume` 结束）。在 guard::drop（即
/// landing pad）里跑；链中的 Call 可再入混合执行（如调编译 helper——C++ 析构调函数的
/// 日常，我们必须也行）。链中再 panic = 双 panic → abort（与 native 一致）。
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
            Terminator::Resume => return, // 链尾：返回 guard，unwind 自动继续
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

// ===== 解释器主循环（spike2 的 unwind 化演进）=====

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
                guard.unwind_edge.set(edge(unwind)); // callee 若 panic，本帧从这条边清理
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
                guard.unwind_edge.set(edge(unwind)); // 本帧 live Drop 由自己的 guard 跑
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
                        // 宿主 panic（VM bug）不是 guest 异常：原样续传，绝不吞
                        Err(host) => panic::resume_unwind(host),
                    },
                }
            }
            Terminator::Return => {
                let r = reg_read(ctx, base, 0);
                reg_restore(ctx, base);
                std::mem::forget(guard); // 正常路径解除守卫（unwind 语义只属 unwind 路径）
                return r;
            }
            Terminator::Resume => {
                unreachable!("Resume 只出现在 cleanup 链（由 CleanupGuard 执行）")
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
        Stmt::Store(..) => unreachable!("spike3 不用内存构造"),
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
        rv => unreachable!("spike3 不用内存构造: {rv:?}"),
    }
}

// ===== 编译帧替身 =====

/// 编译帧的 landing pad 替身：rustc 把本 guard 的 Drop 编进帧的 landing pad——
/// 与 Cranelift 给编译帧发 landing pad 跑 drop glue 机制字面相同。
/// （normal 路径同样 drop——与字节码帧"两条路径都 Drop"对齐。）
struct CGuard {
    ctx: *mut Ctx,
    v: Word,
}
impl Drop for CGuard {
    fn drop(&mut self) {
        log_drop(self.ctx, self.v);
    }
}

// --- case 2：混合链的编译帧 ---
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
/// cleanup 链里被 Call 的编译 helper（landing pad 内再入混合执行）
extern "C-unwind" fn cc_logger(ctx: *mut Ctx, v: u64) -> u64 {
    log_drop(ctx, v);
    v
}

// --- case 3：catch 在编译帧（模拟 JIT 的 catch landing pad）---
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

// --- case 4：跨 FFI abort ---
/// 模拟真 C 帧：plain `extern "C"`，unwind 穿过 = abort（Rust 1.81+ 语义，= 真 C 的处置）
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

/// 中间帧：own=100+d；调 callee（unwind 边指向 cleanup）；
/// 正常：Drop(own)、ret=callee_ret+1；cleanup：Drop(own) → Resume。
/// 槽：0=ret 1=x 2=own 3=callret 4=scratch
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

/// 中间帧变体：cleanup 里多一个对编译 helper 的 Call（landing pad 内再入混合执行）
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
            // bb4: cleanup 内 Call 编译 logger(9002) -> bb5
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

/// 底帧：own=100+d；Panic(777)（unwind 边覆盖自己的 live Drop）。
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

/// 顶帧（catch）：own=100；CatchCall(callee)；正常：ret=dst、Drop(own)；
/// 接住：ret=payload、Drop(own)。槽：2=own 3=dst 4=catch_dst
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

/// 顶帧（不 catch，case3/4 用）：own=100；Call(callee)；正常 Drop(own)、ret=callret+1。
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

// ===== 各用例的程序 + kinds =====

/// case 1：6 帧纯解释链；底部 Panic(777)；顶帧 catch。
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

/// case 2（headline）：interp/compiled 交替；f2 的 cleanup 里 Call 编译 logger。
fn case2_prog() -> (Program, Vec<FuncKind>) {
    use FuncKind::{Compiled, Interp};
    let funcs = vec![
        top_catch_body(1),              // 0 interp（catch）
        dummy_body(),                   // 1 compiled cc2_f1
        mid_body_cleanup_call(2, 3, 6), // 2 interp（cleanup 内 Call logger）
        dummy_body(),                   // 3 compiled cc2_f3
        mid_body(4, 5),                 // 4 interp
        dummy_body(),                   // 5 compiled cc2_f5（raise）
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

/// case 3：catch 在编译帧；顶帧解释（不 catch）。
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

/// case 4：链中插 plain extern "C" 帧；深处 panic → 预期 abort。
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

// ===== native 参考实现（结构镜像；drop 顺序不靠手推，靠对拍）=====

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

    // case 1：6 帧链，底部 raise
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

    // case 2：结构同 case1，f2 的 Drop 额外经 helper 记 9002（镜像 cleanup 内 Call）
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

    // case 3：catch 在中间帧
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
    // 子进程模式：预期在混合链穿 plain extern "C" 帧时 abort
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
            "case1 纯解释链 panic+Drop+catch",
            vm_case(prog, kinds),
            nref::case1(),
        );
    }
    {
        let (prog, kinds) = case2_prog();
        ok &= check(
            "case2 混合栈交替（headline）+ cleanup 内 Call",
            vm_case(prog, kinds),
            nref::case2(),
        );
    }
    {
        let (prog, kinds) = case3_prog();
        ok &= check("case3 catch 在编译帧", vm_case(prog, kinds), nref::case3());
    }

    // case 4：跨 FFI abort（子进程，断言 SIGABRT）
    let exe = std::env::current_exe().expect("current_exe");
    let out = std::process::Command::new(exe)
        .args(["spike3", "--case=ffi-abort"])
        .output()
        .expect("spawn spike3 child");
    use std::os::unix::process::ExitStatusExt as _;
    let sig = out.status.signal();
    if sig == Some(libc::SIGABRT) || sig == Some(libc::SIGILL) {
        println!("PASS case4 跨 FFI abort（子进程信号 {}）", sig.unwrap());
    } else {
        println!(
            "FAIL case4 跨 FFI abort: 子进程状态 {:?}（期望 SIGABRT）",
            out.status
        );
        ok = false;
    }

    if ok {
        println!("--- spike3: 全 PASS（混合栈 unwind 验证通过，候选 A 坐实）---");
        ExitCode::SUCCESS
    } else {
        println!("--- spike3: 有 FAIL ---");
        ExitCode::from(1)
    }
}
