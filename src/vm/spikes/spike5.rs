//! Spike 5：真 Cranelift 接入——编译码 ↔ VM 执行态转换的实测。
//!
//! 兑现挂起的 M4 检查点（spike2"Cranelift 能否发此约定"、vmctx-passing"内部约定等真数据"、
//! spike3"真 Cranelift 的 CFI/LSDA 留复核"）。杀五个未知：
//! 1. **i2c 对真 JIT 码**：interp 拿 finalize 出的 code ptr 直接调 Cranelift 机器码；
//! 2. **c2i 从真 JIT 码**：JIT 码经 imported `mirvm_call_guest` shim 调回解释器；
//! 3. **cc→cc 直接调用**：两个 JIT fib 模块内直接 call（FuncRef），非经 dispatch；
//! 4. **vmctx 内部约定 P vs R**：P=显式 ctx 首参（Wasmtime 式）、R=pinned r15
//!    （HotSpot 式：边界入口 set_pinned_reg，内部 get_pinned_reg，多入口结构）——
//!    两变体可行性 + vcode + 微基准；
//! 5. **unwind 穿真 JIT 帧（probe）**：裸跑预期 abort（cranelift-jit 不注册系统 eh_frame，
//!    其异常路线是 wasmtime-unwinder=自有两阶段 unwinder，与宿主 Rust unwinder 不互操作
//!    ——我们不采）；stretch = 自发射 SystemV unwind info → gimli 拼 .eh_frame →
//!    `__register_frame` 注册（cg_clif JIT 模式同款）→ 传播应成功。
//!
//! 解释器核心沿用 spike3 协议（CleanupGuard/字段级瞬态借用），自含。

use std::mem;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use cranelift_codegen::Context as ClifContext;
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{AbiParam, InstBuilder, Signature, Value, types};
use cranelift_codegen::isa::TargetIsa;
use cranelift_codegen::isa::unwind::UnwindInfo;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{FuncId as ClifFuncId, Linkage, Module};

use super::bytecode::{
    BinOp, Block, Body, Operand, Program, Rvalue, Stmt, Terminator, UnwindAction,
};
use super::frame::{OperandRegion, Word};

// ===== 解释器核心（spike3 形状，自含）=====

struct GuestPanic {
    payload: Word,
}

fn raise_guest(payload: Word) -> ! {
    std::panic::resume_unwind(Box::new(GuestPanic { payload }))
}

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

fn call_guest(ctx: *mut Ctx, func: u32, args: &[Word]) -> Word {
    let kinds: &Vec<FuncKind> = unsafe { &(*ctx).kinds };
    match kinds[func as usize] {
        FuncKind::Interp => interp_frame(ctx, func, args),
        FuncKind::Compiled(f) => f(ctx, args[0]),
    }
}

/// c2i shim：JIT 码经 imported symbol 调回统一 dispatch（i2c/c2i 的 c2i 半边）。
extern "C-unwind" fn mirvm_call_guest(ctx: *mut Ctx, func: u64, arg: u64) -> u64 {
    call_guest(ctx, func as u32, &[arg])
}

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
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    call_guest(ctx, callee, &av)
                })) {
                    Ok(r) => {
                        reg_write(ctx, base, *dst, r);
                        blk = *target as usize;
                    }
                    Err(e) => match e.downcast::<GuestPanic>() {
                        Ok(gp) => {
                            reg_write(ctx, base, *catch_dst, gp.payload);
                            blk = *catch_target as usize;
                        }
                        Err(host) => std::panic::resume_unwind(host),
                    },
                }
            }
            Terminator::Return => {
                let r = reg_read(ctx, base, 0);
                reg_restore(ctx, base);
                mem::forget(guard);
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
        Stmt::Store(..) => unreachable!("spike5 不用 Store"),
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
        rv => unreachable!("spike5 不用内存构造: {rv:?}"),
    }
}

// ===== 解释侧字节码 =====

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

/// 互递归 fib（同 spike2 形状）
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

/// 顶帧（catch，probe 用；own=100）
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

/// probe 底帧：own=102；Panic(777)
fn probe_bottom_body() -> Body {
    use Terminator::*;
    let blocks = vec![
        Block {
            stmts: vec![asgn(2, Rvalue::Use(k(102)))],
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

// ===== Cranelift 装配 =====

/// vmctx 内部约定两变体（docs/designs/vmctx-passing.md §5.2 的实测对象）
#[derive(Clone, Copy, PartialEq)]
enum Conv {
    /// P：显式 ctx 首参，内部调用层层传（Wasmtime 式）
    ExplicitCtx,
    /// R：pinned r15，边界入口 set_pinned_reg、内部 get_pinned_reg（HotSpot 式多入口）
    PinnedReg,
}

fn make_isa(pinned: bool) -> Arc<dyn TargetIsa> {
    let mut fb = settings::builder();
    fb.set("opt_level", "speed").unwrap();
    fb.set("unwind_info", "true").unwrap();
    fb.set("preserve_frame_pointers", "true").unwrap();
    if pinned {
        fb.set("enable_pinned_reg", "true").unwrap();
    }
    cranelift_native::builder()
        .unwrap()
        .finish(settings::Flags::new(fb))
        .unwrap()
}

/// JIT 调用目标：模块内直接（cc→cc）或经 c2i shim。
enum Callee {
    Direct(ClifFuncId),
    Shim { shim: ClifFuncId, partner: i64 },
}

/// 一个变体的 JIT 产物。module 保活（代码内存归它管）。
struct Jitted {
    module: JITModule,
    /// 经 shim 调对方的 fib 边界入口（混合矩阵用）
    shim_a: *const u8,
    shim_b: *const u8,
    /// 模块内直接互递归的 fib 边界入口（cc→cc + 基准用）
    direct_a: *const u8,
    /// probe 中间帧（仅 P 模块构建）
    probe_mid: *const u8,
    /// (函数, unwind info)——eh_frame 注册用
    unwind: Vec<(ClifFuncId, UnwindInfo)>,
    vcode: String,
}

fn emit_call(
    b: &mut FunctionBuilder,
    module: &mut JITModule,
    callee: &Callee,
    ctxv: Option<Value>,
    karg: Value,
) -> Value {
    match callee {
        Callee::Direct(fid) => {
            let fref = module.declare_func_in_func(*fid, b.func);
            let args: Vec<Value> = match ctxv {
                Some(c) => vec![c, karg], // P：ctx 层层传
                None => vec![karg],       // R fast：内部调用不带 ctx
            };
            let call = b.ins().call(fref, &args);
            b.inst_results(call)[0]
        }
        Callee::Shim { shim, partner } => {
            let fref = module.declare_func_in_func(*shim, b.func);
            // 需要 ctx 的时刻：P 用参数，R 用 pinned reg（vmctx doc §5 的 get_pinned_reg 时刻）
            let c = match ctxv {
                Some(c) => c,
                None => b.ins().get_pinned_reg(types::I64),
            };
            let p = b.ins().iconst(types::I64, *partner);
            let call = b.ins().call(fref, &[c, p, karg]);
            b.inst_results(call)[0]
        }
    }
}

/// 造一个 fib 函数体：`has_ctx_param`=P（首参 ctx）/ R-fast（无 ctx 参）。
fn define_fib(
    module: &mut JITModule,
    fbc: &mut FunctionBuilderContext,
    id: ClifFuncId,
    sig: &Signature,
    has_ctx_param: bool,
    callee: Callee,
    out: &mut (Vec<(ClifFuncId, UnwindInfo)>, String),
) {
    let mut cctx = module.make_context();
    cctx.func.signature = sig.clone();
    cctx.set_disasm(true);
    {
        let mut b = FunctionBuilder::new(&mut cctx.func, fbc);
        let entry = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let params = b.block_params(entry).to_vec();
        let (ctxv, n) = if has_ctx_param {
            (Some(params[0]), params[1])
        } else {
            (None, params[0])
        };

        let base_bb = b.create_block();
        let rec_bb = b.create_block();
        let cond = b.ins().icmp_imm(IntCC::UnsignedLessThan, n, 2);
        b.ins().brif(cond, base_bb, &[], rec_bb, &[]);

        b.switch_to_block(base_bb);
        b.ins().return_(&[n]);

        b.switch_to_block(rec_bb);
        let n1 = b.ins().iadd_imm(n, -1);
        let r1 = emit_call(&mut b, module, &callee, ctxv, n1);
        let n2 = b.ins().iadd_imm(n, -2);
        let r2 = emit_call(&mut b, module, &callee, ctxv, n2);
        let r = b.ins().iadd(r1, r2);
        b.ins().return_(&[r]);

        b.seal_all_blocks();
        b.finalize();
    }
    finish_define(module, id, &mut cctx, out);
}

/// R 变体的边界入口（= vmctx doc §5 的 f_boundary）：
/// 保存入 r15 → set_pinned_reg(ctx) → call fast(n) → 恢复 r15 → return。
/// 保存/恢复让 r15 对宿主调用方保持 callee-saved 语义（宿主 rustc 代码自由用 r15）。
fn define_entry_r(
    module: &mut JITModule,
    fbc: &mut FunctionBuilderContext,
    id: ClifFuncId,
    sig_entry: &Signature,
    fast: ClifFuncId,
    out: &mut (Vec<(ClifFuncId, UnwindInfo)>, String),
) {
    let mut cctx = module.make_context();
    cctx.func.signature = sig_entry.clone();
    cctx.set_disasm(true);
    {
        let mut b = FunctionBuilder::new(&mut cctx.func, fbc);
        let entry = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let params = b.block_params(entry).to_vec();
        let (ctxp, n) = (params[0], params[1]);

        let saved = b.ins().get_pinned_reg(types::I64);
        b.ins().set_pinned_reg(ctxp);
        let fref = module.declare_func_in_func(fast, b.func);
        let call = b.ins().call(fref, &[n]);
        let r = b.inst_results(call)[0];
        b.ins().set_pinned_reg(saved);
        b.ins().return_(&[r]);

        b.seal_all_blocks();
        b.finalize();
    }
    finish_define(module, id, &mut cctx, out);
}

/// probe 中间帧（P 约定）：mid(ctx, x) = shim(ctx, BOTTOM, x) + 1
fn define_probe_mid(
    module: &mut JITModule,
    fbc: &mut FunctionBuilderContext,
    id: ClifFuncId,
    sig_entry: &Signature,
    shim: ClifFuncId,
    bottom_guest_id: i64,
    out: &mut (Vec<(ClifFuncId, UnwindInfo)>, String),
) {
    let mut cctx = module.make_context();
    cctx.func.signature = sig_entry.clone();
    cctx.set_disasm(true);
    {
        let mut b = FunctionBuilder::new(&mut cctx.func, fbc);
        let entry = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let params = b.block_params(entry).to_vec();
        let (ctxp, x) = (params[0], params[1]);
        let fref = module.declare_func_in_func(shim, b.func);
        let bid = b.ins().iconst(types::I64, bottom_guest_id);
        let call = b.ins().call(fref, &[ctxp, bid, x]);
        let r0 = b.inst_results(call)[0];
        let r = b.ins().iadd_imm(r0, 1);
        b.ins().return_(&[r]);
        b.seal_all_blocks();
        b.finalize();
    }
    finish_define(module, id, &mut cctx, out);
}

fn finish_define(
    module: &mut JITModule,
    id: ClifFuncId,
    cctx: &mut ClifContext,
    out: &mut (Vec<(ClifFuncId, UnwindInfo)>, String),
) {
    module.define_function(id, cctx).unwrap();
    let code = cctx.compiled_code().unwrap();
    if let Some(ui) = code.create_unwind_info(module.isa()).unwrap() {
        out.0.push((id, ui));
    }
    if let Some(v) = &code.vcode {
        out.1.push_str(v);
        out.1.push('\n');
    }
    module.clear_context(cctx);
}

/// 装配一个变体的 JIT 模块：shim 互递归对 + 直接互递归对（+ P 的 probe_mid）。
fn build_jit(conv: Conv) -> Jitted {
    let isa = make_isa(conv == Conv::PinnedReg);
    let mut jb = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
    jb.symbol("mirvm_call_guest", mirvm_call_guest as *const u8);
    let mut module = JITModule::new(jb);
    let mut fbc = FunctionBuilderContext::new();

    let i64t = types::I64;
    let mut sig_entry = module.make_signature(); // (ctx, n) -> r
    sig_entry.params.push(AbiParam::new(i64t));
    sig_entry.params.push(AbiParam::new(i64t));
    sig_entry.returns.push(AbiParam::new(i64t));
    let mut sig_fast = module.make_signature(); // (n) -> r（R 内部）
    sig_fast.params.push(AbiParam::new(i64t));
    sig_fast.returns.push(AbiParam::new(i64t));
    let mut sig_shim = module.make_signature(); // (ctx, func, arg) -> r
    for _ in 0..3 {
        sig_shim.params.push(AbiParam::new(i64t));
    }
    sig_shim.returns.push(AbiParam::new(i64t));

    let shim = module
        .declare_function("mirvm_call_guest", Linkage::Import, &sig_shim)
        .unwrap();
    let mut out = (Vec::new(), String::new());

    // guest FuncId 约定（矩阵）：0=fib_a, 1=fib_b
    let (shim_a, shim_b, direct_a, probe_mid);
    match conv {
        Conv::ExplicitCtx => {
            let sa = module
                .declare_function("fib_shim_a_p", Linkage::Local, &sig_entry)
                .unwrap();
            let sb = module
                .declare_function("fib_shim_b_p", Linkage::Local, &sig_entry)
                .unwrap();
            let da = module
                .declare_function("fib_direct_a_p", Linkage::Local, &sig_entry)
                .unwrap();
            let db = module
                .declare_function("fib_direct_b_p", Linkage::Local, &sig_entry)
                .unwrap();
            let pm = module
                .declare_function("probe_mid_p", Linkage::Local, &sig_entry)
                .unwrap();
            define_fib(
                &mut module,
                &mut fbc,
                sa,
                &sig_entry,
                true,
                Callee::Shim { shim, partner: 1 },
                &mut out,
            );
            define_fib(
                &mut module,
                &mut fbc,
                sb,
                &sig_entry,
                true,
                Callee::Shim { shim, partner: 0 },
                &mut out,
            );
            define_fib(
                &mut module,
                &mut fbc,
                da,
                &sig_entry,
                true,
                Callee::Direct(db),
                &mut out,
            );
            define_fib(
                &mut module,
                &mut fbc,
                db,
                &sig_entry,
                true,
                Callee::Direct(da),
                &mut out,
            );
            define_probe_mid(&mut module, &mut fbc, pm, &sig_entry, shim, 2, &mut out);
            module.finalize_definitions().unwrap();
            shim_a = module.get_finalized_function(sa);
            shim_b = module.get_finalized_function(sb);
            direct_a = module.get_finalized_function(da);
            probe_mid = module.get_finalized_function(pm);
        }
        Conv::PinnedReg => {
            // fast 内部函数（无 ctx 参）+ 边界入口（f_boundary：save/set/restore pinned）
            let fsa = module
                .declare_function("fast_shim_a_r", Linkage::Local, &sig_fast)
                .unwrap();
            let fsb = module
                .declare_function("fast_shim_b_r", Linkage::Local, &sig_fast)
                .unwrap();
            let fda = module
                .declare_function("fast_direct_a_r", Linkage::Local, &sig_fast)
                .unwrap();
            let fdb = module
                .declare_function("fast_direct_b_r", Linkage::Local, &sig_fast)
                .unwrap();
            let esa = module
                .declare_function("entry_shim_a_r", Linkage::Local, &sig_entry)
                .unwrap();
            let esb = module
                .declare_function("entry_shim_b_r", Linkage::Local, &sig_entry)
                .unwrap();
            let eda = module
                .declare_function("entry_direct_a_r", Linkage::Local, &sig_entry)
                .unwrap();
            define_fib(
                &mut module,
                &mut fbc,
                fsa,
                &sig_fast,
                false,
                Callee::Shim { shim, partner: 1 },
                &mut out,
            );
            define_fib(
                &mut module,
                &mut fbc,
                fsb,
                &sig_fast,
                false,
                Callee::Shim { shim, partner: 0 },
                &mut out,
            );
            define_fib(
                &mut module,
                &mut fbc,
                fda,
                &sig_fast,
                false,
                Callee::Direct(fdb),
                &mut out,
            );
            define_fib(
                &mut module,
                &mut fbc,
                fdb,
                &sig_fast,
                false,
                Callee::Direct(fda),
                &mut out,
            );
            define_entry_r(&mut module, &mut fbc, esa, &sig_entry, fsa, &mut out);
            define_entry_r(&mut module, &mut fbc, esb, &sig_entry, fsb, &mut out);
            define_entry_r(&mut module, &mut fbc, eda, &sig_entry, fda, &mut out);
            module.finalize_definitions().unwrap();
            shim_a = module.get_finalized_function(esa);
            shim_b = module.get_finalized_function(esb);
            direct_a = module.get_finalized_function(eda);
            probe_mid = std::ptr::null();
        }
    }
    let (unwind, vcode) = out;
    Jitted {
        module,
        shim_a,
        shim_b,
        direct_a,
        probe_mid,
        unwind,
        vcode,
    }
}

// ===== eh_frame 自注册（stretch；cg_clif JIT 模式同款）=====

/// 把各 JIT 函数的 SystemV unwind info 拼成 .eh_frame 并注册给系统 unwinder，
/// 让宿主 Rust panic 的传播能走过 JIT 帧（CFI-only，无 personality/landing pad）。
fn register_eh_frames(jit: &Jitted) {
    use gimli::RunTimeEndian;
    use gimli::write::{Address, EhFrame, EndianVec, FrameTable};

    let isa = jit.module.isa();
    let mut table = FrameTable::default();
    let cie = isa.create_systemv_cie().expect("systemv cie");
    let cie_id = table.add_cie(cie);
    for (id, ui) in &jit.unwind {
        if let UnwindInfo::SystemV(info) = ui {
            let addr = jit.module.get_finalized_function(*id) as u64;
            table.add_fde(cie_id, info.to_fde(Address::Constant(addr)));
        }
    }
    let mut eh = EhFrame(EndianVec::new(RunTimeEndian::Little));
    table.write_eh_frame(&mut eh).unwrap();
    crate::vm::engine::jit::register_eh_frame_section(eh.0.into_vec());
}

// ===== harness =====

fn run_matrix(variant: &str, name: &str, ka: FuncKind, kb: FuncKind) -> bool {
    let prog = Program {
        funcs: vec![fib_body(1), fib_body(0)],
    };
    let mut ctx = Ctx::new(prog, vec![ka, kb]);
    for n in (0..=20u64).chain([24]) {
        let got = call_guest(&mut ctx as *mut Ctx, 0, &[n]);
        let want = fib_ref(n);
        if got != want {
            println!("FAIL [{variant}] {name} fib({n}): {got} != {want}");
            return false;
        }
    }
    println!("PASS [{variant}] {name}");
    true
}

fn bench_one(name: &str, mut f: impl FnMut() -> u64, want: u64) {
    let mut best = f64::INFINITY;
    for _ in 0..5 {
        let t = Instant::now();
        let r = std::hint::black_box(f());
        let dt = t.elapsed().as_secs_f64() * 1e3;
        assert_eq!(r, want, "bench {name} 结果错误");
        best = best.min(dt);
    }
    println!("  {name:26} {best:9.3} ms");
}

fn fib_native(n: u64) -> u64 {
    if n < 2 {
        n
    } else {
        fib_native(n - 1) + fib_native(n - 2)
    }
}

fn probe_child(register: bool) -> ExitCode {
    let jit = build_jit(Conv::ExplicitCtx);
    if register {
        register_eh_frames(&jit);
    }
    let prog = Program {
        funcs: vec![top_catch_body(1), dummy_body(), probe_bottom_body()],
    };
    let mid: CompiledFn = unsafe { mem::transmute::<*const u8, CompiledFn>(jit.probe_mid) };
    let mut ctx = Ctx::new(
        prog,
        vec![FuncKind::Interp, FuncKind::Compiled(mid), FuncKind::Interp],
    );
    let r = call_guest(&mut ctx as *mut Ctx, 0, &[0]);
    println!("probe result={} drops={:?}", r, ctx.drop_log);
    if r == 777 && ctx.drop_log == vec![102, 100] {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(3)
    }
}

pub fn run(mut argv: impl Iterator<Item = String>) -> ExitCode {
    match argv.next().as_deref() {
        Some("--case=probe-bare") => return probe_child(false),
        Some("--case=probe-reg") => return probe_child(true),
        _ => {}
    }

    let mut ok = true;

    // —— 两变体 × 4 配置矩阵 ——
    for (vname, conv) in [
        ("P 显式ctx参", Conv::ExplicitCtx),
        ("R pinned-r15", Conv::PinnedReg),
    ] {
        let jit = build_jit(conv);
        std::fs::write(
            format!(
                "/tmp/spike5-vcode-{}.txt",
                if conv == Conv::ExplicitCtx { "p" } else { "r" }
            ),
            &jit.vcode,
        )
        .ok();
        let fa: CompiledFn = unsafe { mem::transmute::<*const u8, CompiledFn>(jit.shim_a) };
        let fb: CompiledFn = unsafe { mem::transmute::<*const u8, CompiledFn>(jit.shim_b) };
        let fd: CompiledFn = unsafe { mem::transmute::<*const u8, CompiledFn>(jit.direct_a) };

        ok &= run_matrix(
            vname,
            "interp / interp        ",
            FuncKind::Interp,
            FuncKind::Interp,
        );
        ok &= run_matrix(
            vname,
            "jit    / jit（直接调用）",
            FuncKind::Compiled(fd),
            FuncKind::Interp,
        );
        ok &= run_matrix(
            vname,
            "interp / jit           ",
            FuncKind::Interp,
            FuncKind::Compiled(fb),
        );
        ok &= run_matrix(
            vname,
            "jit    / interp        ",
            FuncKind::Compiled(fa),
            FuncKind::Interp,
        );

        // —— 微基准（fib(30)，直接调用配置）——
        if ok {
            let want = fib_ref(30);
            println!("bench [{vname}] fib(30):");
            let prog = Program {
                funcs: vec![fib_body(1), fib_body(0)],
            };
            let mut ictx = Ctx::new(prog, vec![FuncKind::Interp, FuncKind::Interp]);
            bench_one(
                "interp（骨架 tree-walk）",
                || call_guest(&mut ictx as *mut Ctx, 0, &[30]),
                want,
            );
            let prog2 = Program {
                funcs: vec![dummy_body(), dummy_body()],
            };
            let mut jctx = Ctx::new(prog2, vec![FuncKind::Interp, FuncKind::Interp]);
            bench_one(
                "jit（cc→cc 直接调用）",
                || fd(&mut jctx as *mut Ctx, 30),
                want,
            );
            bench_one(
                "native rustc -O",
                || fib_native(std::hint::black_box(30)),
                want,
            );
        }
        // jit.module 活到此处之后（指针使用完毕）
        drop(jit);
    }

    // —— unwind 穿真 JIT 帧（子进程 probe）——
    let exe = std::env::current_exe().expect("current_exe");
    use std::os::unix::process::ExitStatusExt as _;
    let bare = std::process::Command::new(&exe)
        .args(["spike5", "--case=probe-bare"])
        .output()
        .unwrap();
    let reg = std::process::Command::new(&exe)
        .args(["spike5", "--case=probe-reg"])
        .output()
        .unwrap();

    let bare_desc = match (bare.status.code(), bare.status.signal()) {
        (_, Some(sig)) => {
            format!("信号 {sig}（abort，如预期：JIT 帧无 CFI，系统 unwinder 走不过）")
        }
        (Some(c), _) => format!("退出码 {c}"),
        _ => "未知".into(),
    };
    println!("probe 裸跑（无 eh_frame 注册）: {bare_desc}");

    let reg_out = String::from_utf8_lossy(&reg.stdout);
    if reg.status.success() && reg_out.contains("probe result=777 drops=[102, 100]") {
        println!(
            "PASS probe eh_frame 注册后: guest panic 穿真 JIT 帧传播 + catch 正确（{}）",
            reg_out.trim()
        );
    } else {
        println!(
            "FAIL probe eh_frame 注册后: status={:?} signal={:?} out={}",
            reg.status.code(),
            reg.status.signal(),
            reg_out.trim()
        );
        ok = false;
    }

    if ok {
        println!(
            "--- spike5: 全 PASS（真 Cranelift：i2c/c2i/cc→cc 直接调用/P&R 两约定/unwind 穿 JIT 帧）---"
        );
        ExitCode::SUCCESS
    } else {
        println!("--- spike5: 有 FAIL ---");
        ExitCode::from(1)
    }
}
