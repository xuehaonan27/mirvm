//! Spike 5: real Cranelift integration -- practical test of compiled code ↔ VM execution
//! state transitions.
//!
//! Five unknowns this spike settled:
//! 1. **i2c to real JIT code**: interp takes the finalized code ptr and directly calls Cranelift
//!    machine code;
//! 2. **c2i from real JIT code**: JIT code calls back into the interpreter via the imported
//!    `mirvm_call_guest` shim;
//! 3. **cc→cc direct call**: direct call inside two JIT fib modules (FuncRef), not via dispatch;
//! 4. **vmctx internal convention P vs R**: P = explicit ctx first arg (Wasmtime style), R =
//!    pinned r15 (HotSpot style: boundary entry set_pinned_reg, internal get_pinned_reg,
//!    multi-entry structure) -- both variants are feasible; vcode and micro-benchmark are
//!    produced below;
//! 5. **unwind through real JIT frames (probe)**: a bare run aborts, because cranelift-jit does
//!    not register system eh_frame and its own exception path (the wasmtime two-phase unwinder)
//!    is not interoperable with the host Rust unwinder -- so we do not adopt it. Self-emitting
//!    SystemV unwind info, assembling it into .eh_frame and registering it via
//!    `__register_frame`, makes propagation succeed.
//!
//! The interpreter core is self-contained and uses `CleanupGuard` with field-level transient
//! borrowing.

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

// ===== interpreter core (self-contained) =====

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

/// c2i shim: JIT code calls back to unified dispatch via imported symbol (the c2i half of i2c/c2i).
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
            t => unreachable!("illegal cleanup-chain terminator: {t:?}"),
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
            Terminator::Resume => unreachable!("Resume only appears in cleanup chain"),
        }
    }
}

fn exec_stmt(ctx: *mut Ctx, base: usize, stmt: &Stmt) {
    match stmt {
        Stmt::Assign(dst, rv) => {
            let v = eval_rvalue(ctx, base, rv);
            reg_write(ctx, base, *dst, v);
        }
        Stmt::Store(..) => unreachable!("spike5 does not use Store"),
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
        rv => unreachable!("spike5 does not use memory constructor: {rv:?}"),
    }
}

// ===== interpreter-side bytecode =====

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

/// Mutually recursive fib over the two functions 0 and 1.
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

/// top frame (catch, used by probe; own=100)
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

/// probe bottom frame: own=102; Panic(777)
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

// ===== Cranelift assembly =====

/// The two variants of the vmctx internal convention.
#[derive(Clone, Copy, PartialEq)]
enum Conv {
    /// P: explicit ctx first arg, passed through every internal call (Wasmtime style)
    ExplicitCtx,
    /// R: pinned r15, boundary entry set_pinned_reg, internal get_pinned_reg (HotSpot-style multi-entry)
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

/// JIT call target: direct inside module (cc→cc) or via c2i shim.
enum Callee {
    Direct(ClifFuncId),
    Shim { shim: ClifFuncId, partner: i64 },
}

/// JIT artifact for one variant. module kept alive (code memory owned by it).
struct Jitted {
    module: JITModule,
    /// fib boundary entry called via shim to the other side (used by mixed matrix)
    shim_a: *const u8,
    shim_b: *const u8,
    /// fib boundary entry for direct mutual recursion inside module (cc→cc + benchmark)
    direct_a: *const u8,
    /// probe intermediate frame (only built in P module)
    probe_mid: *const u8,
    /// (function, unwind info) — used for eh_frame registration
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
                Some(c) => vec![c, karg], // P: ctx passed level by level
                None => vec![karg],       // R fast: internal calls do not carry ctx
            };
            let call = b.ins().call(fref, &args);
            b.inst_results(call)[0]
        }
        Callee::Shim { shim, partner } => {
            let fref = module.declare_func_in_func(*shim, b.func);
            // the moment ctx is needed: P uses the parameter, R uses the pinned register
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

/// Build a fib function body: `has_ctx_param`=P (first arg ctx) / R-fast (no ctx arg).
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

/// R variant boundary entry:
/// save into r15 → set_pinned_reg(ctx) → call fast(n) → restore r15 → return.
/// save/restore keeps r15 callee-saved semantics for the host caller (host rustc code may freely use r15).
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

/// probe intermediate frame (P convention): mid(ctx, x) = shim(ctx, BOTTOM, x) + 1
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

/// Assemble a variant's JIT module: shim mutual-recursion pair + direct mutual-recursion pair (+ P's probe_mid).
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
    let mut sig_fast = module.make_signature(); // (n) -> r (R internal)
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

    // guest FuncId convention (matrix): 0=fib_a, 1=fib_b
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
            // fast internal functions (no ctx arg) + boundary entry (f_boundary: save/set/restore pinned)
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

// ===== eh_frame self-registration =====

/// Assemble each JIT function's SystemV unwind info into .eh_frame and register it with the system unwinder,
/// so host Rust panic propagation can walk through JIT frames (CFI-only, no personality/landing pad).
/// This is the same approach cg_clif's JIT mode takes.
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
        assert_eq!(r, want, "bench {name} result incorrect");
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

    // -- two variants x 4 config matrix --
    for (vname, conv) in [
        ("P explicit-ctx arg", Conv::ExplicitCtx),
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
            "jit    / jit (direct call)",
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

        // -- micro-benchmark (fib(30), direct-call config) --
        if ok {
            let want = fib_ref(30);
            println!("bench [{vname}] fib(30):");
            let prog = Program {
                funcs: vec![fib_body(1), fib_body(0)],
            };
            let mut ictx = Ctx::new(prog, vec![FuncKind::Interp, FuncKind::Interp]);
            bench_one(
                "interp (skeleton tree-walk)",
                || call_guest(&mut ictx as *mut Ctx, 0, &[30]),
                want,
            );
            let prog2 = Program {
                funcs: vec![dummy_body(), dummy_body()],
            };
            let mut jctx = Ctx::new(prog2, vec![FuncKind::Interp, FuncKind::Interp]);
            bench_one(
                "jit (cc→cc direct call)",
                || fd(&mut jctx as *mut Ctx, 30),
                want,
            );
            bench_one(
                "native rustc -O",
                || fib_native(std::hint::black_box(30)),
                want,
            );
        }
        // jit.module lives until after this point (pointers no longer used)
        drop(jit);
    }

    // -- unwind through real JIT frames (subprocess probe) --
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
            format!(
                "signal {sig} (abort, as expected: JIT frames have no CFI, system unwinder cannot walk them)"
            )
        }
        (Some(c), _) => format!("exit code {c}"),
        _ => "unknown".into(),
    };
    println!("probe bare run (no eh_frame registration): {bare_desc}");

    let reg_out = String::from_utf8_lossy(&reg.stdout);
    if reg.status.success() && reg_out.contains("probe result=777 drops=[102, 100]") {
        println!(
            "PASS probe after eh_frame registration: guest panic propagates through real JIT frames + catch correct ({})",
            reg_out.trim()
        );
    } else {
        println!(
            "FAIL probe after eh_frame registration: status={:?} signal={:?} out={}",
            reg.status.code(),
            reg.status.signal(),
            reg_out.trim()
        );
        ok = false;
    }

    if ok {
        println!(
            "--- spike5: all PASS (real Cranelift: i2c/c2i/cc→cc direct call/P&R conventions/unwind through JIT frames) ---"
        );
        ExitCode::SUCCESS
    } else {
        println!("--- spike5: has FAIL ---");
        ExitCode::from(1)
    }
}
