//! M5.3b：字节码 → Cranelift 翻译器（标量子集）+ 编译服务线程（m5.3-design §4，D3/D4/D5）。
//!
//! 输入 = 冻结的 `ir::FuncBody`（D3：JIT 吃字节码不吃 MIR；tcx 不出执行相）。
//! 语义契约 = **与解释器逐位一致**（JIT-on/off 差分是第一 oracle）：所有值保持
//! "I64 零扩到宽"的槽不变量，运算按 interp 的 int_bin/int_cmp/int_ovf 恒等式镜像，
//! 结果按宽 band 掩回。帧局部全部提升 Cranelift SSA 变量（v1 准入排除取址/内存
//! 操作数 ⇒ 无栈帧内存）；入口统一 def 0（有效 MIR 无读前未写路径，此为确定化）。
//!
//! 调用（D5 两入口 + PLT）：
//! - **fast**：纯 guest 签名（n×I64 → 0/1×I64）。编译码间经 `slots_fast[callee]`
//!   内存间接（load + call_indirect，调用点恒定形状）；未编译 callee 的槽先发
//!   **c2i 蹦床**（fast 形状，内部打包实参调 `mirvm_c2i` 回解释器）。
//! - **packed**：`extern "C-unwind" fn(*const u64, *mut u64)`——interp 的 i2c 一跳
//!   （call_guest 读 `slots[f]`）。
//!
//! 发布序 = 先 fast 后 packed（Release）；call_guest Acquire 读 ⇒ 进入编译码的
//! 线程必见其 callee 蹦床/入口（happens-before 链）。
//!
//! unwind（D6 v1 = CFI-only）：spike5 管线——create_unwind_info → gimli FrameTable
//! → 逐 FDE `__register_frame`（libgcc 语义 + CIE 判别字段）。准入已排除 cleanup 边
//! （unwind-transparent：panic 只穿透，不着陆）。
//!
//! 单 worker 线程持 JITModule（代码内存进程生命周期，cranelift-jit 无逐函数释放）。

use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, Sender};

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{
    AbiParam, InstBuilder, MemFlagsData, Signature, StackSlotData, StackSlotKind, TrapCode, Value,
    types,
};
use cranelift_codegen::isa::unwind::UnwindInfo;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{FuncId as ClifFuncId, Linkage, Module as ClifModule};

use super::ctx::Shared;
use super::ir::{
    self, IntBinOp, IntCc, Operand, OvfOp, ParamAbi, RetAbi, RetDest, ScalarPlace, Slot, Stmt,
    SwitchDiscr, Terminator, UnwindAction, Width,
};

/// c2i 壳的引擎定位（单引擎进程模型，与 TRACK_DIAGNOSTIC 全局钩同一假设面）。
static SHARED: std::sync::atomic::AtomicPtr<Shared> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

/// 启动编译服务（run_vm_engine 在 Shared 定型后调用；--jit off 时不启动）。
pub fn start(shared: &'static Shared) {
    if !shared.jit.enabled {
        return;
    }
    SHARED.store(shared as *const Shared as *mut Shared, Ordering::Release);
    let (tx, rx): (Sender<u32>, Receiver<u32>) = std::sync::mpsc::channel();
    *shared.jit.queue.lock().unwrap() = Some(tx);
    // 编译失败/线程死亡 = 静默维持解释（语义面零依赖 JIT）
    let _ = std::thread::Builder::new()
        .name("mirvm-jit".into())
        .spawn(move || worker(shared, rx));
}

fn worker(shared: &'static Shared, rx: Receiver<u32>) {
    let dbg = std::env::var_os("MIRVM_JIT_DEBUG").is_some();
    let mut c = Compiler::new(shared);
    while let Ok(func) = rx.recv() {
        if dbg {
            eprintln!(
                "mirvm-jit-debug: 收到 f{func}（{}）",
                shared.module.funcs[func as usize].name
            );
        }
        c.compile(func);
        if dbg {
            let ok = shared.jit.slots[func as usize].load(Ordering::Acquire) != 0;
            eprintln!("mirvm-jit-debug: f{func} 发布={ok}");
        }
    }
}

// ===== 运行期助手（JIT 码经 import symbol 调回引擎）=====

/// c2i 万能壳：编译码调未编译 guest 函数（经蹦床打包）→ 回解释器。
/// ctx 恢复 = 边界 TLS attach（thunk 工厂同款，幂等）。
extern "C-unwind" fn mirvm_c2i(func: u64, args: *const u64, n: u64, ret: *mut u64) {
    let shared = unsafe { &*SHARED.load(Ordering::Acquire) };
    let ctx = super::ctx::attach(shared);
    let a = unsafe { std::slice::from_raw_parts(args, n as usize) };
    let (lo, hi) = super::interp::call_guest(ctx, func as u32, a);
    unsafe {
        *ret = lo;
        *ret.add(1) = hi;
    }
}

/// Unreachable 终止子的诊断口径与解释器一致（不用裸 trap 的 SIGILL）。
extern "C-unwind" fn mirvm_jit_unreachable(func: u64) -> ! {
    let shared = unsafe { &*SHARED.load(Ordering::Acquire) };
    let name = shared
        .module
        .funcs
        .get(func as usize)
        .map(|f| &*f.name)
        .unwrap_or("?");
    eprintln!("mirvm[jit]: 到达 Unreachable（fn {name}）");
    std::process::abort();
}

// ===== 准入（v1 标量子集；拒绝 = 永久维持解释）=====

fn scalar_slot(p: &ScalarPlace) -> Option<Slot> {
    match p {
        ScalarPlace::Slot(s) => Some(*s),
        ScalarPlace::Mem { .. } => None,
    }
}

fn operand_ok(op: &Operand) -> bool {
    matches!(op, Operand::Slot(_) | Operand::Imm { .. })
}

fn rvalue_ok(rv: &ir::Rvalue) -> bool {
    use ir::Rvalue as R;
    match rv {
        R::Use(a) | R::NotBits(a) | R::NotBool(a) | R::Neg(a) => operand_ok(a),
        R::Cast { a, .. } => operand_ok(a),
        // Div/Rem 有除零 abort 路径（v2 与助手口径一并接），先拒
        R::IntBin { op, a, b, .. } => {
            !matches!(op, IntBinOp::Div | IntBinOp::Rem) && operand_ok(a) && operand_ok(b)
        }
        R::IntCmp { a, b, .. } => operand_ok(a) && operand_ok(b),
        _ => false,
    }
}

/// callee 的 ABI 必须全标量（蹦床/fast 签名的成立前提）。返回 (实参槽数, 有无返回值)。
fn callee_abi(body: &ir::FuncBody) -> Option<(usize, bool)> {
    if body.caller_loc_off.is_some() {
        return None; // track_caller 幻影尾参 v2
    }
    let mut n = 0usize;
    for p in &body.params {
        match p {
            ParamAbi::Scalar(_) => n += 1,
            ParamAbi::Zst => {}
            _ => return None,
        }
    }
    match body.ret {
        RetAbi::Scalar(_) => Some((n, true)),
        RetAbi::Zst => Some((n, false)),
        _ => None,
    }
}

fn admit(shared: &Shared, body: &ir::FuncBody) -> bool {
    if callee_abi(body).is_none() {
        return false;
    }
    for blk in &body.blocks {
        for st in &blk.stmts {
            let ok = match st {
                Stmt::Assign { dst, rv } => scalar_slot(dst).is_some() && rvalue_ok(rv),
                Stmt::AssignOverflow {
                    a,
                    b,
                    dst_val,
                    dst_flag,
                    ..
                } => {
                    operand_ok(a)
                        && operand_ok(b)
                        && scalar_slot(dst_val).is_some()
                        && scalar_slot(dst_flag).is_some()
                }
                _ => false,
            };
            if !ok {
                return false;
            }
        }
        let ok = match &blk.term {
            Terminator::Goto(_) | Terminator::Return | Terminator::Unreachable => true,
            Terminator::SwitchInt { discr, targets, .. } => match discr {
                SwitchDiscr::Scalar(op) => {
                    // 判别值必须落在 u64（宽度 ≤64 时天然成立；防御断言）
                    operand_ok(op) && targets.iter().all(|(v, _)| *v <= u64::MAX as u128)
                }
                SwitchDiscr::Wide(_) => false,
            },
            Terminator::Call {
                callee,
                args,
                ret,
                unwind,
                ..
            } => {
                // unwind-transparent：只收 Continue（穿透）；cleanup/terminate 边 v2（LSDA 期）。
                // callee ABI 不设限：全标量 ABI 走 PLT 快路，否则调用点直接 c2i 回解释
                // （interp 本就吃展平 av，任意 ABI 语义一致——panic 类冷路径的归宿）。
                matches!(unwind, UnwindAction::Continue)
                    && args.iter().all(operand_ok)
                    && matches!(ret, RetDest::Ignore | RetDest::Scalar(ScalarPlace::Slot(_)))
                    && shared.module.funcs.get(*callee as usize).is_some()
            }
            _ => false,
        };
        if !ok {
            return false;
        }
    }
    true
}

// ===== 编译器 =====

struct Compiler {
    shared: &'static Shared,
    module: JITModule,
    fbc: FunctionBuilderContext,
    c2i: ClifFuncId,
    unreachable: ClifFuncId,
    /// 本批 (clif id, unwind info)——finalize 后统一注册 eh_frame
    pending_unwind: Vec<(ClifFuncId, UnwindInfo)>,
}

impl Compiler {
    fn new(shared: &'static Shared) -> Self {
        let mut fb = settings::builder();
        fb.set("opt_level", "speed").unwrap();
        fb.set("unwind_info", "true").unwrap();
        fb.set("preserve_frame_pointers", "true").unwrap();
        let isa = cranelift_native::builder()
            .unwrap()
            .finish(settings::Flags::new(fb))
            .unwrap();
        let mut jb = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
        jb.symbol("mirvm_c2i", mirvm_c2i as *const u8);
        jb.symbol("mirvm_jit_unreachable", mirvm_jit_unreachable as *const u8);
        let mut module = JITModule::new(jb);

        let mut sig_c2i = module.make_signature();
        for _ in 0..4 {
            sig_c2i.params.push(AbiParam::new(types::I64));
        }
        let c2i = module
            .declare_function("mirvm_c2i", Linkage::Import, &sig_c2i)
            .unwrap();
        let mut sig_unr = module.make_signature();
        sig_unr.params.push(AbiParam::new(types::I64));
        let unreachable = module
            .declare_function("mirvm_jit_unreachable", Linkage::Import, &sig_unr)
            .unwrap();

        Compiler {
            shared,
            module,
            fbc: FunctionBuilderContext::new(),
            c2i,
            unreachable,
            pending_unwind: Vec::new(),
        }
    }

    fn fast_sig(&mut self, nparams: usize, has_ret: bool) -> Signature {
        let mut sig = self.module.make_signature();
        for _ in 0..nparams {
            sig.params.push(AbiParam::new(types::I64));
        }
        if has_ret {
            sig.returns.push(AbiParam::new(types::I64));
        }
        sig
    }

    /// 编译一个函数（过阈值请求）。拒绝/失败 = 静默维持解释。
    fn compile(&mut self, func: u32) {
        let jit = &self.shared.jit;
        if jit.slots[func as usize].load(Ordering::Acquire) != 0 {
            return; // 已编译
        }
        let Some(body) = self.shared.module.funcs.get(func as usize) else {
            return;
        };
        if !admit(self.shared, body) {
            return;
        }
        let (nparams, has_ret) = callee_abi(body).expect("admit 已验");

        // PLT 快路 callee 的槽预热：未编译者发 c2i 蹦床（fast 形状，调用点形状恒定）。
        // 非全标量 ABI / 实参数不合的 callee 不在此列——其调用点直接 c2i（cold path）。
        let mut callees: Vec<(u32, usize, bool)> = Vec::new();
        for blk in &body.blocks {
            if let Terminator::Call { callee, args, .. } = &blk.term
                && *callee != func
                && !callees.iter().any(|(c, _, _)| c == callee)
                && let Some((cn, cret)) = callee_abi(&self.shared.module.funcs[*callee as usize])
                && cn == args.len()
            {
                callees.push((*callee, cn, cret));
            }
        }
        for (c, cn, cret) in callees {
            if jit.slots_fast[c as usize].load(Ordering::Acquire) == 0 {
                let tramp = self.define_c2i_trampoline(c, cn, cret);
                jit.slots_fast[c as usize].store(tramp as u64, Ordering::Release);
            }
        }

        let fast_id = self.define_fast(func, body, nparams, has_ret);
        let packed_id = self.define_packed(func, body, nparams, has_ret, fast_id);

        self.module.finalize_definitions().unwrap();
        self.register_pending_eh_frames();

        let fast = self.module.get_finalized_function(fast_id) as u64;
        let packed = self.module.get_finalized_function(packed_id) as u64;
        // 发布序：先 fast（自递归/他人调我）后 packed（interp 才可能进入编译码）
        jit.slots_fast[func as usize].store(fast, Ordering::Release);
        jit.slots[func as usize].store(packed, Ordering::Release);
    }

    /// c2i 蹦床：fast 签名，打包实参进栈上数组，调 mirvm_c2i 回解释器。
    fn define_c2i_trampoline(&mut self, target: u32, nparams: usize, has_ret: bool) -> *const u8 {
        let sig = self.fast_sig(nparams, has_ret);
        let id = self
            .module
            .declare_function(&format!("t{target}"), Linkage::Local, &sig)
            .unwrap();
        let mut cctx = self.module.make_context();
        cctx.func.signature = sig;
        {
            let mut b = FunctionBuilder::new(&mut cctx.func, &mut self.fbc);
            let entry = b.create_block();
            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            let params = b.block_params(entry).to_vec();
            let args_ss = b.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                (nparams.max(1) * 8) as u32,
                3,
            ));
            for (i, p) in params.iter().enumerate() {
                b.ins().stack_store(*p, args_ss, (i * 8) as i32);
            }
            let ret_ss =
                b.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 16, 3));
            let fref = self.module.declare_func_in_func(self.c2i, b.func);
            let fv = b.ins().iconst(types::I64, target as i64);
            let ap = b.ins().stack_addr(types::I64, args_ss, 0);
            let nv = b.ins().iconst(types::I64, nparams as i64);
            let rp = b.ins().stack_addr(types::I64, ret_ss, 0);
            b.ins().call(fref, &[fv, ap, nv, rp]);
            if has_ret {
                let lo = b.ins().stack_load(types::I64, ret_ss, 0);
                b.ins().return_(&[lo]);
            } else {
                b.ins().return_(&[]);
            }
            b.seal_all_blocks();
            b.finalize();
        }
        self.module.define_function(id, &mut cctx).unwrap();
        if let Some(ui) = cctx
            .compiled_code()
            .unwrap()
            .create_unwind_info(self.module.isa())
            .unwrap()
        {
            self.pending_unwind.push((id, ui));
        }
        self.module.clear_context(&mut cctx);
        self.module.finalize_definitions().unwrap();
        self.register_pending_eh_frames();
        self.module.get_finalized_function(id)
    }

    /// fast 本体：字节码块 → CLIF；槽 → SSA 变量（I64 零扩到宽不变量）。
    fn define_fast(
        &mut self,
        func: u32,
        body: &ir::FuncBody,
        nparams: usize,
        has_ret: bool,
    ) -> ClifFuncId {
        let sig = self.fast_sig(nparams, has_ret);
        let id = self
            .module
            .declare_function(&format!("f{func}"), Linkage::Local, &sig)
            .unwrap();
        let mut cctx = self.module.make_context();
        cctx.func.signature = sig;
        {
            let mut b = FunctionBuilder::new(&mut cctx.func, &mut self.fbc);
            let mut tr = Translator {
                shared: self.shared,
                module: &mut self.module,
                b: &mut b,
                vars: std::collections::HashMap::new(),
                unreachable: self.unreachable,
                c2i: self.c2i,
            };
            tr.build(func, body, has_ret);
            b.seal_all_blocks();
            b.finalize();
        }
        self.module.define_function(id, &mut cctx).unwrap();
        if let Some(ui) = cctx
            .compiled_code()
            .unwrap()
            .create_unwind_info(self.module.isa())
            .unwrap()
        {
            self.pending_unwind.push((id, ui));
        }
        self.module.clear_context(&mut cctx);
        id
    }

    /// packed 入口：`(args: *const u64, ret: *mut u64)`——interp i2c 一跳。
    fn define_packed(
        &mut self,
        func: u32,
        body: &ir::FuncBody,
        nparams: usize,
        has_ret: bool,
        fast: ClifFuncId,
    ) -> ClifFuncId {
        let _ = body;
        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(types::I64));
        sig.params.push(AbiParam::new(types::I64));
        let id = self
            .module
            .declare_function(&format!("p{func}"), Linkage::Local, &sig)
            .unwrap();
        let mut cctx = self.module.make_context();
        cctx.func.signature = sig;
        {
            let mut b = FunctionBuilder::new(&mut cctx.func, &mut self.fbc);
            let entry = b.create_block();
            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            let ps = b.block_params(entry).to_vec();
            let (argp, retp) = (ps[0], ps[1]);
            let mut args: Vec<Value> = Vec::with_capacity(nparams);
            for i in 0..nparams {
                args.push(
                    b.ins()
                        .load(types::I64, MemFlagsData::trusted(), argp, (i * 8) as i32),
                );
            }
            let fref = self.module.declare_func_in_func(fast, b.func);
            let call = b.ins().call(fref, &args);
            let lo = if has_ret {
                b.inst_results(call)[0]
            } else {
                b.ins().iconst(types::I64, 0)
            };
            let zero = b.ins().iconst(types::I64, 0);
            b.ins().store(MemFlagsData::trusted(), lo, retp, 0);
            b.ins().store(MemFlagsData::trusted(), zero, retp, 8);
            b.ins().return_(&[]);
            b.seal_all_blocks();
            b.finalize();
        }
        self.module.define_function(id, &mut cctx).unwrap();
        if let Some(ui) = cctx
            .compiled_code()
            .unwrap()
            .create_unwind_info(self.module.isa())
            .unwrap()
        {
            self.pending_unwind.push((id, ui));
        }
        self.module.clear_context(&mut cctx);
        id
    }

    /// spike5 管线：FrameTable → eh_frame 字节 → 逐 FDE __register_frame（libgcc
    /// 语义；CIE 判别 = 长度域后 4 字节为 0）。字节 leak（FDE 注册要求终身有效）。
    fn register_pending_eh_frames(&mut self) {
        if self.pending_unwind.is_empty() {
            return;
        }
        use gimli::RunTimeEndian;
        use gimli::write::{Address, EhFrame, EndianVec, FrameTable};
        unsafe extern "C" {
            fn __register_frame(fde: *const u8);
        }
        let isa = self.module.isa();
        let mut table = FrameTable::default();
        let cie = isa.create_systemv_cie().expect("systemv cie");
        let cie_id = table.add_cie(cie);
        for (id, ui) in self.pending_unwind.drain(..) {
            if let UnwindInfo::SystemV(info) = ui {
                let addr = self.module.get_finalized_function(id) as u64;
                table.add_fde(cie_id, info.to_fde(Address::Constant(addr)));
            }
        }
        let mut eh = EhFrame(EndianVec::new(RunTimeEndian::Little));
        table.write_eh_frame(&mut eh).unwrap();
        let mut bytes = eh.0.into_vec();
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        let buf: &'static [u8] = Box::leak(bytes.into_boxed_slice());
        unsafe {
            let start = buf.as_ptr();
            let end = start.add(buf.len());
            let mut cur = start;
            while cur < end {
                let len = u32::from_le_bytes(std::ptr::read(cur as *const [u8; 4])) as usize;
                if len == 0 {
                    break;
                }
                let cie_ptr = u32::from_le_bytes(std::ptr::read(cur.add(4) as *const [u8; 4]));
                if cie_ptr != 0 {
                    __register_frame(cur);
                }
                cur = cur.add(len + 4);
            }
        }
    }
}

// ===== 函数体翻译 =====

struct Translator<'a, 'b> {
    shared: &'static Shared,
    module: &'a mut JITModule,
    b: &'a mut FunctionBuilder<'b>,
    vars: std::collections::HashMap<u32, Variable>,
    unreachable: ClifFuncId,
    c2i: ClifFuncId,
}

impl Translator<'_, '_> {
    fn var(&mut self, off: u32) -> Variable {
        if let Some(&v) = self.vars.get(&off) {
            return v;
        }
        let v = self.b.declare_var(types::I64);
        self.vars.insert(off, v);
        v
    }

    fn mask_val(&mut self, v: Value, w: Width) -> Value {
        if w == Width::W64 {
            return v;
        }
        self.b.ins().band_imm(v, w.mask() as i64)
    }

    /// 槽不变量下的符号扩展视图（i64）：w=64 原样；否则 ireduce→sextend。
    fn sext_val(&mut self, v: Value, w: Width) -> Value {
        let t = match w {
            Width::W8 => types::I8,
            Width::W16 => types::I16,
            Width::W32 => types::I32,
            Width::W64 => return v,
        };
        let narrow = self.b.ins().ireduce(t, v);
        self.b.ins().sextend(types::I64, narrow)
    }

    fn operand(&mut self, op: &Operand) -> (Value, Width) {
        match op {
            Operand::Slot(s) => {
                let v = self.var(s.off);
                (self.b.use_var(v), s.width)
            }
            Operand::Imm { bits, width } => {
                let v = self
                    .b
                    .ins()
                    .iconst(types::I64, (*bits & width.mask()) as i64);
                (v, *width)
            }
            _ => unreachable!("admit 已排除"),
        }
    }

    fn def_slot(&mut self, s: Slot, v: Value) {
        let masked = self.mask_val(v, s.width);
        let var = self.var(s.off);
        self.b.def_var(var, masked);
    }

    fn build(&mut self, func: u32, body: &ir::FuncBody, has_ret: bool) {
        let entry = self.b.create_block();
        self.b.append_block_params_for_function_params(entry);
        let blocks: Vec<_> = (0..body.blocks.len())
            .map(|_| self.b.create_block())
            .collect();

        self.b.switch_to_block(entry);
        // 所有槽变量 def 0（确定化；interp 帧不清零，但有效 MIR 无读前未写路径）
        let mut offs: Vec<u32> = Vec::new();
        collect_slot_offs(body, &mut offs);
        let zero = self.b.ins().iconst(types::I64, 0);
        for off in offs {
            let var = self.var(off);
            self.b.def_var(var, zero);
        }
        // 参数落槽（packed/interp 侧按同一展平序）
        let params = self.b.block_params(entry).to_vec();
        let mut pi = 0usize;
        for p in &body.params {
            if let ParamAbi::Scalar(s) = p {
                self.def_slot(*s, params[pi]);
                pi += 1;
            }
        }
        self.b.ins().jump(blocks[0], &[]);

        for (bi, blk) in body.blocks.iter().enumerate() {
            self.b.switch_to_block(blocks[bi]);
            for st in &blk.stmts {
                self.stmt(st);
            }
            self.term(func, body, &blk.term, &blocks, has_ret);
        }
    }

    fn stmt(&mut self, st: &Stmt) {
        match st {
            Stmt::Assign { dst, rv } => {
                let v = self.rvalue(rv);
                let s = match dst {
                    ScalarPlace::Slot(s) => *s,
                    _ => unreachable!(),
                };
                self.def_slot(s, v);
            }
            Stmt::AssignOverflow {
                op,
                signed,
                a,
                b,
                dst_val,
                dst_flag,
            } => {
                let (av, w) = self.operand(a);
                let (bv, _) = self.operand(b);
                let (val, flag) = self.int_ovf(*op, *signed, av, bv, w);
                let (sv, sf) = match (dst_val, dst_flag) {
                    (ScalarPlace::Slot(v), ScalarPlace::Slot(f)) => (*v, *f),
                    _ => unreachable!(),
                };
                self.def_slot(sv, val);
                self.def_slot(sf, flag);
            }
            _ => unreachable!("admit 已排除"),
        }
    }

    fn rvalue(&mut self, rv: &ir::Rvalue) -> Value {
        use ir::Rvalue as R;
        match rv {
            R::Use(a) => self.operand(a).0,
            R::IntBin { op, signed, a, b } => {
                let (av, w) = self.operand(a);
                let (bv, _) = self.operand(b);
                self.int_bin(*op, *signed, av, bv, w)
            }
            R::IntCmp { cc, signed, a, b } => {
                let (av, w) = self.operand(a);
                let (bv, _) = self.operand(b);
                let (x, y) = if *signed {
                    (self.sext_val(av, w), self.sext_val(bv, w))
                } else {
                    (av, bv) // 槽不变量已 zext
                };
                let c = match (cc, signed) {
                    (IntCc::Eq, _) => IntCC::Equal,
                    (IntCc::Ne, _) => IntCC::NotEqual,
                    (IntCc::Lt, true) => IntCC::SignedLessThan,
                    (IntCc::Le, true) => IntCC::SignedLessThanOrEqual,
                    (IntCc::Gt, true) => IntCC::SignedGreaterThan,
                    (IntCc::Ge, true) => IntCC::SignedGreaterThanOrEqual,
                    (IntCc::Lt, false) => IntCC::UnsignedLessThan,
                    (IntCc::Le, false) => IntCC::UnsignedLessThanOrEqual,
                    (IntCc::Gt, false) => IntCC::UnsignedGreaterThan,
                    (IntCc::Ge, false) => IntCC::UnsignedGreaterThanOrEqual,
                };
                let b1 = self.b.ins().icmp(c, x, y);
                self.b.ins().uextend(types::I64, b1)
            }
            R::NotBits(a) => {
                let (v, w) = self.operand(a);
                let n = self.b.ins().bnot(v);
                self.mask_val(n, w)
            }
            R::NotBool(a) => {
                let (v, _) = self.operand(a);
                self.b.ins().bxor_imm(v, 1)
            }
            R::Neg(a) => {
                let (v, w) = self.operand(a);
                let n = self.b.ins().ineg(v);
                self.mask_val(n, w)
            }
            R::Cast { from, to, a } => {
                let (v, _) = self.operand(a);
                let x = if from.1 {
                    let s = self.sext_val(v, from.0);
                    // sext 后按 64 位视图，再掩到目标宽
                    s
                } else {
                    self.mask_val(v, from.0)
                };
                self.mask_val(x, *to)
            }
            _ => unreachable!("admit 已排除"),
        }
    }

    /// interp::int_bin 的逐位镜像（Add/Sub/Mul 掩后签名无关；移位 mod-64 同
    /// wrapping_shl/shr；符号 Shr 用 sext 视图算术右移，b&63 与 CLIF mod-64 一致）。
    fn int_bin(&mut self, op: IntBinOp, signed: bool, a: Value, b: Value, w: Width) -> Value {
        let r = match op {
            IntBinOp::Add => self.b.ins().iadd(a, b),
            IntBinOp::Sub => self.b.ins().isub(a, b),
            IntBinOp::Mul => self.b.ins().imul(a, b),
            IntBinOp::BitAnd => return self.b.ins().band(a, b),
            IntBinOp::BitOr => return self.b.ins().bor(a, b),
            IntBinOp::BitXor => return self.b.ins().bxor(a, b),
            IntBinOp::Shl => {
                // signed 分支的 sext 高位左移后必然溢出掩区（见 interp 注释），同型
                let s = self.b.ins().ishl(a, b);
                return self.mask_val(s, w);
            }
            IntBinOp::Shr => {
                let s = if signed {
                    let x = self.sext_val(a, w);
                    self.b.ins().sshr(x, b)
                } else {
                    self.b.ins().ushr(a, b)
                };
                return self.mask_val(s, w);
            }
            IntBinOp::Div | IntBinOp::Rem => unreachable!("admit 已排除"),
        };
        self.mask_val(r, w)
    }

    /// interp::int_ovf 的逐位镜像（128 位提升的 64 位恒等式）：
    /// - unsigned w<64：和/积在 64 位内精确 ⇒ ovf = 精确值 > mask；Sub ovf = a<b。
    /// - unsigned W64：Add ovf = 回绕（r<a）；Mul ovf = umulhi≠0；Sub 同上。
    /// - signed w<64：sext 后 64 位精确 ⇒ 与 [lo,hi] 比界。
    /// - signed W64：Add/Sub 标准符号恒等式；Mul ovf = smulhi ≠ (r>>63)。
    fn int_ovf(&mut self, op: OvfOp, signed: bool, a: Value, b: Value, w: Width) -> (Value, Value) {
        let bb = &mut *self.b;
        if !signed {
            match (op, w) {
                (OvfOp::Sub, _) => {
                    let r = bb.ins().isub(a, b);
                    let f8 = bb.ins().icmp(IntCC::UnsignedLessThan, a, b);
                    let f = bb.ins().uextend(types::I64, f8);
                    (self.mask_val(r, w), f)
                }
                (_, Width::W64) => match op {
                    OvfOp::Add => {
                        let r = bb.ins().iadd(a, b);
                        let f8 = bb.ins().icmp(IntCC::UnsignedLessThan, r, a);
                        let f = bb.ins().uextend(types::I64, f8);
                        (r, f)
                    }
                    OvfOp::Mul => {
                        let r = bb.ins().imul(a, b);
                        let hi = bb.ins().umulhi(a, b);
                        let f8 = bb.ins().icmp_imm(IntCC::NotEqual, hi, 0);
                        let f = bb.ins().uextend(types::I64, f8);
                        (r, f)
                    }
                    OvfOp::Sub => unreachable!(),
                },
                (_, _) => {
                    // w<64：64 位内精确
                    let exact = match op {
                        OvfOp::Add => bb.ins().iadd(a, b),
                        OvfOp::Mul => bb.ins().imul(a, b),
                        OvfOp::Sub => unreachable!(),
                    };
                    let f8 = bb
                        .ins()
                        .icmp_imm(IntCC::UnsignedGreaterThan, exact, w.mask() as i64);
                    let f = bb.ins().uextend(types::I64, f8);
                    (self.mask_val(exact, w), f)
                }
            }
        } else {
            let x = self.sext_val(a, w);
            let y = self.sext_val(b, w);
            let bb = &mut *self.b;
            if w == Width::W64 {
                let r = match op {
                    OvfOp::Add => bb.ins().iadd(x, y),
                    OvfOp::Sub => bb.ins().isub(x, y),
                    OvfOp::Mul => bb.ins().imul(x, y),
                };
                let f8 = match op {
                    // add：符号同入异出；sub：入异且出与被减数异
                    OvfOp::Add => {
                        let t1 = bb.ins().bxor(r, x);
                        let t2 = bb.ins().bxor(r, y);
                        let t = bb.ins().band(t1, t2);
                        bb.ins().icmp_imm(IntCC::SignedLessThan, t, 0)
                    }
                    OvfOp::Sub => {
                        let t1 = bb.ins().bxor(x, y);
                        let t2 = bb.ins().bxor(r, x);
                        let t = bb.ins().band(t1, t2);
                        bb.ins().icmp_imm(IntCC::SignedLessThan, t, 0)
                    }
                    OvfOp::Mul => {
                        let hi = bb.ins().smulhi(x, y);
                        let sgn = bb.ins().sshr_imm(r, 63);
                        bb.ins().icmp(IntCC::NotEqual, hi, sgn)
                    }
                };
                let f = bb.ins().uextend(types::I64, f8);
                (self.mask_val(r, w), f)
            } else {
                let r = match op {
                    OvfOp::Add => bb.ins().iadd(x, y),
                    OvfOp::Sub => bb.ins().isub(x, y),
                    OvfOp::Mul => bb.ins().imul(x, y),
                };
                let (lo, hi) = match w {
                    Width::W8 => (i8::MIN as i64, i8::MAX as i64),
                    Width::W16 => (i16::MIN as i64, i16::MAX as i64),
                    Width::W32 => (i32::MIN as i64, i32::MAX as i64),
                    Width::W64 => unreachable!(),
                };
                let under = bb.ins().icmp_imm(IntCC::SignedLessThan, r, lo);
                let over = bb.ins().icmp_imm(IntCC::SignedGreaterThan, r, hi);
                let f8 = bb.ins().bor(under, over);
                let f = bb.ins().uextend(types::I64, f8);
                (self.mask_val(r, w), f)
            }
        }
    }

    fn term(
        &mut self,
        func: u32,
        body: &ir::FuncBody,
        t: &Terminator,
        blocks: &[cranelift_codegen::ir::Block],
        has_ret: bool,
    ) {
        match t {
            Terminator::Goto(bb) => {
                self.b.ins().jump(blocks[*bb as usize], &[]);
            }
            Terminator::SwitchInt {
                discr,
                targets,
                otherwise,
            } => {
                let SwitchDiscr::Scalar(op) = discr else {
                    unreachable!()
                };
                let (v, _) = self.operand(op);
                // icmp+brif 链（v1；值稀疏，br_table 留优化项）
                for (val, bb) in targets {
                    let hit = self.b.ins().icmp_imm(IntCC::Equal, v, *val as u64 as i64);
                    let next = self.b.create_block();
                    self.b.ins().brif(hit, blocks[*bb as usize], &[], next, &[]);
                    self.b.switch_to_block(next);
                }
                self.b.ins().jump(blocks[*otherwise as usize], &[]);
            }
            Terminator::Call {
                callee,
                args,
                ret,
                target,
                ..
            } => {
                let mut av: Vec<Value> = Vec::with_capacity(args.len());
                for a in args {
                    av.push(self.operand(a).0);
                }
                let cb = &self.shared.module.funcs[*callee as usize];
                let plt = callee_abi(cb).filter(|(cn, _)| *cn == av.len());
                if let Some((_, cret)) = plt {
                    // 热路：PLT 内存间接——load slots_fast[callee] + call_indirect
                    //（恒定形状；蹦床→fast 的升级对调用点透明）
                    let slot_addr = &self.shared.jit.slots_fast[*callee as usize]
                        as *const std::sync::atomic::AtomicU64
                        as i64;
                    let ap = self.b.ins().iconst(types::I64, slot_addr);
                    let fp = self
                        .b
                        .ins()
                        .load(types::I64, MemFlagsData::trusted(), ap, 0);
                    let sig = {
                        let mut s = self.module.make_signature();
                        for _ in 0..av.len() {
                            s.params.push(AbiParam::new(types::I64));
                        }
                        if cret {
                            s.returns.push(AbiParam::new(types::I64));
                        }
                        s
                    };
                    let sigref = self.b.import_signature(sig);
                    let call = self.b.ins().call_indirect(sigref, fp, &av);
                    if let RetDest::Scalar(ScalarPlace::Slot(s)) = ret {
                        let lo = if cret {
                            self.b.inst_results(call)[0]
                        } else {
                            self.b.ins().iconst(types::I64, 0)
                        };
                        self.def_slot(*s, lo);
                    }
                } else {
                    // 冷路：调用点直接 c2i（打包展平实参回解释器——interp 本就吃
                    // 展平 av，callee 任意 ABI 语义一致；panic 类分支的归宿）
                    let args_ss = self.b.create_sized_stack_slot(StackSlotData::new(
                        StackSlotKind::ExplicitSlot,
                        (av.len().max(1) * 8) as u32,
                        3,
                    ));
                    for (i, v) in av.iter().enumerate() {
                        self.b.ins().stack_store(*v, args_ss, (i * 8) as i32);
                    }
                    let ret_ss = self.b.create_sized_stack_slot(StackSlotData::new(
                        StackSlotKind::ExplicitSlot,
                        16,
                        3,
                    ));
                    let fref = self.module.declare_func_in_func(self.c2i, self.b.func);
                    let fv = self.b.ins().iconst(types::I64, *callee as i64);
                    let ap = self.b.ins().stack_addr(types::I64, args_ss, 0);
                    let nv = self.b.ins().iconst(types::I64, av.len() as i64);
                    let rp = self.b.ins().stack_addr(types::I64, ret_ss, 0);
                    self.b.ins().call(fref, &[fv, ap, nv, rp]);
                    if let RetDest::Scalar(ScalarPlace::Slot(s)) = ret {
                        let lo = self.b.ins().stack_load(types::I64, ret_ss, 0);
                        self.def_slot(*s, lo);
                    }
                }
                self.b.ins().jump(blocks[*target as usize], &[]);
            }
            Terminator::Return => {
                if has_ret {
                    let RetAbi::Scalar(s) = body.ret else {
                        unreachable!()
                    };
                    let var = self.var(s.off);
                    let v = self.b.use_var(var);
                    self.b.ins().return_(&[v]);
                } else {
                    self.b.ins().return_(&[]);
                }
            }
            Terminator::Unreachable => {
                let fref = self
                    .module
                    .declare_func_in_func(self.unreachable, self.b.func);
                let fv = self.b.ins().iconst(types::I64, func as i64);
                self.b.ins().call(fref, &[fv]);
                self.b.ins().trap(TrapCode::user(1).unwrap());
            }
            _ => unreachable!("admit 已排除"),
        }
    }
}

/// 收集函数体触及的全部槽偏移（入口 def 0 用）。
fn collect_slot_offs(body: &ir::FuncBody, out: &mut Vec<u32>) {
    let mut push = |s: &Slot| {
        if !out.contains(&s.off) {
            out.push(s.off);
        }
    };
    let op = |o: &Operand, push: &mut dyn FnMut(&Slot)| {
        if let Operand::Slot(s) = o {
            push(s);
        }
    };
    if let RetAbi::Scalar(s) = &body.ret {
        push(s);
    }
    for p in &body.params {
        if let ParamAbi::Scalar(s) = p {
            push(s);
        }
    }
    for blk in &body.blocks {
        for st in &blk.stmts {
            match st {
                Stmt::Assign { dst, rv } => {
                    if let ScalarPlace::Slot(s) = dst {
                        push(s);
                    }
                    use ir::Rvalue as R;
                    match rv {
                        R::Use(a) | R::NotBits(a) | R::NotBool(a) | R::Neg(a) => op(a, &mut push),
                        R::Cast { a, .. } => op(a, &mut push),
                        R::IntBin { a, b, .. } | R::IntCmp { a, b, .. } => {
                            op(a, &mut push);
                            op(b, &mut push);
                        }
                        _ => {}
                    }
                }
                Stmt::AssignOverflow {
                    a,
                    b,
                    dst_val,
                    dst_flag,
                    ..
                } => {
                    op(a, &mut push);
                    op(b, &mut push);
                    if let ScalarPlace::Slot(s) = dst_val {
                        push(s);
                    }
                    if let ScalarPlace::Slot(s) = dst_flag {
                        push(s);
                    }
                }
                _ => {}
            }
        }
        match &blk.term {
            Terminator::SwitchInt {
                discr: SwitchDiscr::Scalar(o),
                ..
            } => op(o, &mut push),
            Terminator::Call { args, ret, .. } => {
                for a in args {
                    op(a, &mut push);
                }
                if let RetDest::Scalar(ScalarPlace::Slot(s)) = ret {
                    push(s);
                }
            }
            _ => {}
        }
    }
}
