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
    AbiParam, InstBuilder, MemFlagsData, Signature, StackSlot, StackSlotData, StackSlotKind,
    TrapCode, Value, types,
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

// ===== 准入（M5.3 v1 标量子集 + M5.4a 内存操作数；拒绝 = 永久维持解释）=====

fn scalar_slot(p: &ScalarPlace) -> Option<Slot> {
    match p {
        ScalarPlace::Slot(s) => Some(*s),
        ScalarPlace::Mem { .. } => None,
    }
}

fn operand_ok(op: &Operand) -> bool {
    match op {
        Operand::Slot(_) | Operand::Imm { .. } => true,
        // M5.4a：内存/地址操作数（place 求值通道全部内联）
        Operand::Mem { expr, .. } | Operand::AddrOf(expr) => place_ok(expr),
        Operand::SubImm { base, .. } => operand_ok(base),
    }
}

/// PlaceExpr 准入：base 全可（Local=帧槽/Static=绝对地址立即数）；
/// VTableAlignOffset 的 meta 需 operand_ok（interp 恒等式内联，2 幂/溢出 → trap）。
fn place_ok(pe: &ir::PlaceExpr) -> bool {
    pe.steps.iter().all(|s| match s {
        ir::PlaceStep::Deref | ir::PlaceStep::Offset(_) | ir::PlaceStep::IndexScaled { .. } => true,
        ir::PlaceStep::VTableAlignOffset { meta, .. } => operand_ok(meta),
    })
}

fn mem_place_ok(p: &ScalarPlace) -> bool {
    match p {
        ScalarPlace::Slot(_) => true,
        ScalarPlace::Mem { expr, .. } => place_ok(expr),
    }
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
        // M5.4a 内存/地址族
        R::Ref(pe) => place_ok(pe),
        R::PtrOffset { ptr, count, .. } => operand_ok(ptr) && operand_ok(count),
        R::PtrDiff { a, b, stride } => *stride != 0 && operand_ok(a) && operand_ok(b),
        R::UMax { a, b } => operand_ok(a) && operand_ok(b),
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
                Stmt::Assign { dst, rv } => mem_place_ok(dst) && rvalue_ok(rv),
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
                // M5.4a：memmove/Repeat 两族（逐元素 CLIF 循环）
                Stmt::Copy { dst, src, .. } => place_ok(dst) && place_ok(src),
                Stmt::RepeatScalar { dst, val, .. } => place_ok(dst) && operand_ok(val),
                Stmt::RepeatBytes { first, .. } => place_ok(first),
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
    /// M5.4a：Copy/帧清零的宿主 memmove/memset 通道
    memmove: ClifFuncId,
    memset: ClifFuncId,
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
        jb.symbol("memmove", libc::memmove as *const u8);
        jb.symbol("memset", libc::memset as *const u8);
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
        // memmove(d, s, n) -> d；memset(d, c, n) -> d（M5.4a Copy/帧清零通道）
        let mut sig_mm = module.make_signature();
        for _ in 0..3 {
            sig_mm.params.push(AbiParam::new(types::I64));
        }
        sig_mm.returns.push(AbiParam::new(types::I64));
        let memmove = module
            .declare_function("memmove", Linkage::Import, &sig_mm)
            .unwrap();
        let memset = module
            .declare_function("memset", Linkage::Import, &sig_mm)
            .unwrap();

        Compiler {
            shared,
            module,
            fbc: FunctionBuilderContext::new(),
            c2i,
            unreachable,
            memmove,
            memset,
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
            let frame_offs = analyze_frame(body);
            let frame_ss = if frame_offs.is_empty() {
                None
            } else {
                Some(b.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    body.frame_size,
                    body.frame_align.trailing_zeros() as u8,
                )))
            };
            let mut tr = Translator {
                shared: self.shared,
                module: &mut self.module,
                b: &mut b,
                vars: std::collections::HashMap::new(),
                frame_offs,
                frame_ss,
                unreachable: self.unreachable,
                c2i: self.c2i,
                memmove: self.memmove,
                memset: self.memset,
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

/// 帧模型 v2 的槽存储分派（M5.4a，m5.4-design §3.1）：取址分析保守全集——
/// 任何被 `PlaceExpr::Local`/Mem/AddrOf/Ref/Copy/Repeat 等通道触及的 frame offset
/// 一律落栈帧内存；其余槽维持 SSA 提升（v1 的 I64 零扩到宽不变量原样）。
struct Translator<'a, 'b> {
    shared: &'static Shared,
    module: &'a mut JITModule,
    b: &'a mut FunctionBuilder<'b>,
    vars: std::collections::HashMap<u32, Variable>,
    /// 落帧 offset 集（analyze_frame 产出）
    frame_offs: std::collections::HashSet<u32>,
    /// guest 帧栈槽（frame_offs 非空时创建；frame_size 字节、frame_align 对齐）
    frame_ss: Option<StackSlot>,
    unreachable: ClifFuncId,
    c2i: ClifFuncId,
    memmove: ClifFuncId,
    memset: ClifFuncId,
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

    fn narrow_ty(w: Width) -> cranelift_codegen::ir::Type {
        match w {
            Width::W8 => types::I8,
            Width::W16 => types::I16,
            Width::W32 => types::I32,
            Width::W64 => types::I64,
        }
    }

    /// 读槽（分派：落帧 → 栈槽 load + 零扩；SSA → use_var）
    fn read_slot(&mut self, s: Slot) -> Value {
        if self.frame_offs.contains(&s.off) {
            let ss = self.frame_ss.expect("落帧 offset 必有帧槽");
            let v = self
                .b
                .ins()
                .stack_load(Self::narrow_ty(s.width), ss, s.off as i32);
            if s.width == Width::W64 {
                v
            } else {
                self.b.ins().uextend(types::I64, v)
            }
        } else {
            let v = self.var(s.off);
            self.b.use_var(v)
        }
    }

    /// 写槽（分派：落帧 → 掩宽 + 窄化 + 栈槽 store；SSA → 掩宽 def_var）
    fn write_slot(&mut self, s: Slot, v: Value) {
        let masked = self.mask_val(v, s.width);
        if self.frame_offs.contains(&s.off) {
            let ss = self.frame_ss.expect("落帧 offset 必有帧槽");
            let n = if s.width == Width::W64 {
                masked
            } else {
                self.b.ins().ireduce(Self::narrow_ty(s.width), masked)
            };
            self.b.ins().stack_store(n, ss, s.off as i32);
        } else {
            let var = self.var(s.off);
            self.b.def_var(var, masked);
        }
    }

    /// 取帧内 offset 的真地址（取址分析已保证其落帧）
    fn addr_of_local(&mut self, off: u32) -> Value {
        let ss = self
            .frame_ss
            .expect("取址 offset 必落帧（analyze_frame 全集）");
        self.b.ins().stack_addr(types::I64, ss, off as i32)
    }

    /// PlaceExpr 求值（interp::eval_place_addr 逐位镜像；Deref/Offset 为 wrapping 语义）
    fn place_addr(&mut self, pe: &ir::PlaceExpr) -> Value {
        let mut addr = match pe.base {
            ir::PlaceBase::Local(off) => self.addr_of_local(off),
            ir::PlaceBase::Static(a) => self.b.ins().iconst(types::I64, a as i64),
        };
        for step in pe.steps.iter() {
            match step {
                ir::PlaceStep::Deref => {
                    addr = self
                        .b
                        .ins()
                        .load(types::I64, MemFlagsData::trusted(), addr, 0)
                }
                ir::PlaceStep::Offset(d) => addr = self.b.ins().iadd_imm(addr, i64::from(*d)),
                ir::PlaceStep::IndexScaled { idx, stride } => {
                    let i = self.read_slot(*idx);
                    let scaled = self.b.ins().imul_imm(i, *stride as i64);
                    addr = self.b.ins().iadd(addr, scaled);
                }
                ir::PlaceStep::VTableAlignOffset {
                    meta,
                    unaligned,
                    packed,
                } => {
                    // interp 恒等式：align = *(vtable+16)；packed 取 min；非 2 幂/溢出即
                    // abort（JIT 侧 = mirvm_jit_trap 诊断退出，与 interp engine_abort 同口径）
                    let (vtable, _) = self.operand(meta);
                    let mut align =
                        self.b
                            .ins()
                            .load(types::I64, MemFlagsData::trusted(), vtable, 16);
                    if let Some(p) = packed {
                        let p = self.b.ins().iconst(types::I64, *p as i64);
                        align = self.b.ins().umin(align, p);
                    }
                    // 2 幂检查：align != 0 && (align & (align-1)) == 0，否则 trap
                    let is_zero = self.b.ins().icmp_imm(IntCC::Equal, align, 0);
                    let am1 = self.b.ins().iadd_imm(align, -1);
                    let pow2 = self.b.ins().band(align, am1);
                    let not_pow2 = self.b.ins().icmp_imm(IntCC::NotEqual, pow2, 0);
                    let bad = self.b.ins().bor(is_zero, not_pow2);
                    self.trap_if(bad, "dyn vtable alignment 非 2 的幂");
                    // (unaligned + align-1) & !(align-1)；checked_add 溢出 → trap
                    let uv = self.b.ins().iconst(types::I64, *unaligned as i64);
                    let sum = self
                        .b
                        .ins()
                        .uadd_overflow_trap(uv, am1, TrapCode::user(2).unwrap());
                    let off = self.b.ins().band_not(sum, am1);
                    addr = self.b.ins().iadd(addr, off);
                }
            }
        }
        addr
    }

    /// 条件即诊断退出（与 interp engine_abort 同口径的 JIT 形态）。
    fn trap_if(&mut self, cond: Value, _msg: &'static str) {
        let t_blk = self.b.create_block();
        let f_blk = self.b.create_block();
        self.b.ins().brif(cond, t_blk, &[], f_blk, &[]);
        self.b.switch_to_block(t_blk);
        let fref = self
            .module
            .declare_func_in_func(self.unreachable, self.b.func);
        let fv = self.b.ins().iconst(types::I64, 0);
        self.b.ins().call(fref, &[fv]);
        self.b.ins().trap(TrapCode::user(1).unwrap());
        self.b.switch_to_block(f_blk);
    }

    fn operand(&mut self, op: &Operand) -> (Value, Width) {
        match op {
            Operand::Slot(s) => (self.read_slot(*s), s.width),
            Operand::Imm { bits, width } => {
                let v = self
                    .b
                    .ins()
                    .iconst(types::I64, (*bits & width.mask()) as i64);
                (v, *width)
            }
            Operand::Mem { expr, width } => {
                let a = self.place_addr(expr);
                let v = self
                    .b
                    .ins()
                    .load(Self::narrow_ty(*width), MemFlagsData::trusted(), a, 0);
                let v = if *width == Width::W64 {
                    v
                } else {
                    self.b.ins().uextend(types::I64, v)
                };
                (v, *width)
            }
            Operand::AddrOf(expr) => {
                let a = self.place_addr(expr);
                (a, Width::W64)
            }
            Operand::SubImm { base, sub } => {
                let (v, w) = self.operand(base);
                (self.b.ins().iadd_imm(v, (*sub as i64).wrapping_neg()), w)
            }
        }
    }

    fn def_slot(&mut self, s: Slot, v: Value) {
        self.write_slot(s, v);
    }

    fn build(&mut self, func: u32, body: &ir::FuncBody, has_ret: bool) {
        let entry = self.b.create_block();
        self.b.append_block_params_for_function_params(entry);
        let blocks: Vec<_> = (0..body.blocks.len())
            .map(|_| self.b.create_block())
            .collect();

        self.b.switch_to_block(entry);
        // 槽变量 def 0（确定化；interp 帧不清零，但有效 MIR 无读前未写路径）。
        // 帧内存同步清零（v1 确定化纪律的延伸：JIT-on/off 差分对任何 MIR 形状确定）。
        let mut offs: Vec<u32> = Vec::new();
        collect_ssa_offs(body, &self.frame_offs, &mut offs);
        let zero = self.b.ins().iconst(types::I64, 0);
        for off in offs {
            let var = self.var(off);
            self.b.def_var(var, zero);
        }
        if let Some(ss) = self.frame_ss {
            let fref = self.module.declare_func_in_func(self.memset, self.b.func);
            let dst = self.b.ins().stack_addr(types::I64, ss, 0);
            let c0 = self.b.ins().iconst(types::I64, 0);
            let n = self.b.ins().iconst(types::I64, i64::from(body.frame_size));
            self.b.ins().call(fref, &[dst, c0, n]);
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
                match dst {
                    ScalarPlace::Slot(s) => {
                        let s = *s;
                        self.def_slot(s, v);
                    }
                    ScalarPlace::Mem { expr, width } => {
                        // 内存落点：掩宽 + 窄化 + store（与 interp mem_write 同口径）
                        let a = self.place_addr(expr);
                        let masked = self.mask_val(v, *width);
                        let n = if *width == Width::W64 {
                            masked
                        } else {
                            self.b.ins().ireduce(Self::narrow_ty(*width), masked)
                        };
                        self.b.ins().store(MemFlagsData::trusted(), n, a, 0);
                    }
                }
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
                    _ => unreachable!("admit 已排除"),
                };
                self.def_slot(sv, val);
                self.def_slot(sf, flag);
            }
            Stmt::Copy { dst, src, size } => {
                // memmove 语义（interp std::ptr::copy 同源：guest 侧重叠是 UB，
                // 引擎不因此崩——防御性一致）
                let d = self.place_addr(dst);
                let s = self.place_addr(src);
                let n = self.b.ins().iconst(types::I64, i64::from(*size));
                let fref = self.module.declare_func_in_func(self.memmove, self.b.func);
                self.b.ins().call(fref, &[d, s, n]);
            }
            Stmt::RepeatScalar {
                dst,
                val,
                count,
                elem_size,
            } => {
                // interp 循环镜像：for i in 0..count { mem_write(d + i*elem, w, v) }
                let d = self.place_addr(dst);
                let (v, w) = self.operand(val);
                debug_assert_eq!(w.bytes(), *elem_size);
                let masked = self.mask_val(v, w);
                let n = if w == Width::W64 {
                    masked
                } else {
                    self.b.ins().ireduce(Self::narrow_ty(w), masked)
                };
                self.repeat_loop(d, n, *count, u64::from(*elem_size), false);
            }
            Stmt::RepeatBytes {
                first,
                count,
                elem_size,
            } => {
                // interp 镜像：for i in 1..count { 逐元素 memmove（元素 0 不变 ⇒
                // 与 interp 的 copy_nonoverlapping 逐元素结果一致） }
                let src = self.place_addr(first);
                self.repeat_loop(src, src, *count, *elem_size, true);
            }
            _ => unreachable!("admit 已排除"),
        }
    }

    /// Repeat 两族的共用循环骨架：memmove_elem=true 时逐元素 memmove（RepeatBytes，
    /// 起始 i=1）；否则按标量存（RepeatScalar，起始 i=0）。
    fn repeat_loop(
        &mut self,
        base: Value,
        val_or_src: Value,
        count: u64,
        elem_size: u64,
        memmove_elem: bool,
    ) {
        let count_v = self.b.ins().iconst(types::I64, count as i64);
        let elem_v = self.b.ins().iconst(types::I64, elem_size as i64);
        let head = self.b.create_block();
        let body_blk = self.b.create_block();
        let tail = self.b.create_block();
        let ivar = self.b.declare_var(types::I64);
        let start = self
            .b
            .ins()
            .iconst(types::I64, if memmove_elem { 1 } else { 0 });
        self.b.def_var(ivar, start);
        self.b.ins().jump(head, &[]);
        self.b.switch_to_block(head);
        let iv = self.b.use_var(ivar);
        let done = self
            .b
            .ins()
            .icmp(IntCC::UnsignedGreaterThanOrEqual, iv, count_v);
        self.b.ins().brif(done, tail, &[], body_blk, &[]);
        self.b.switch_to_block(body_blk);
        let off = self.b.ins().imul(iv, elem_v);
        let p = self.b.ins().iadd(base, off);
        if memmove_elem {
            let fref = self.module.declare_func_in_func(self.memmove, self.b.func);
            self.b.ins().call(fref, &[p, val_or_src, elem_v]);
        } else {
            self.b
                .ins()
                .store(MemFlagsData::trusted(), val_or_src, p, 0);
        }
        let iv2 = self.b.use_var(ivar);
        let inc = self.b.ins().iadd_imm(iv2, 1);
        self.b.def_var(ivar, inc);
        self.b.ins().jump(head, &[]);
        self.b.switch_to_block(tail);
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
            // ===== M5.4a 内存/地址族 =====
            R::Ref(expr) => self.place_addr(expr),
            R::PtrOffset { ptr, count, stride } => {
                // 真实地址模型位透传（wrapping；与 interp 同）
                let (p, _) = self.operand(ptr);
                let (c, _) = self.operand(count);
                let scaled = self.b.ins().imul_imm(c, *stride as i64);
                self.b.ins().iadd(p, scaled)
            }
            R::PtrDiff { a, b, stride } => {
                // (a - b) / stride（i64 除法；stride 为冻结常量，admit 已拒 0）
                let (av, _) = self.operand(a);
                let (bv, _) = self.operand(b);
                let d = self.b.ins().isub(av, bv);
                let sv = self.b.ins().iconst(types::I64, *stride as i64);
                self.b.ins().sdiv(d, sv)
            }
            R::UMax { a, b } => {
                let (av, _) = self.operand(a);
                let (bv, _) = self.operand(b);
                self.b.ins().umax(av, bv)
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

/// M5.4a 取址分析（保守全集，m5.4-design §3.1/Q1）：收集必须落栈帧内存的 frame
/// offset——任何被 PlaceExpr::Local/Mem/AddrOf/Ref/Copy/Repeat/Indirect-ABI 触及者。
/// 判据 = 宁多勿漏：误提升（地址被取的槽错放 SSA）是错值级，多落帧只是慢一点。
/// or-pattern 全枚举 Stmt/Terminator——新增 place 通道变体 = 非穷尽编译错误。
fn analyze_frame(body: &ir::FuncBody) -> std::collections::HashSet<u32> {
    use std::collections::HashSet;
    fn scan_place(out: &mut HashSet<u32>, pe: &ir::PlaceExpr) {
        if let ir::PlaceBase::Local(off) = pe.base {
            out.insert(off);
        }
    }
    fn scan_op(out: &mut HashSet<u32>, op: &Operand) {
        match op {
            Operand::Mem { expr, .. } | Operand::AddrOf(expr) => scan_place(out, expr),
            Operand::SubImm { base, .. } => scan_op(out, base),
            Operand::Slot(_) | Operand::Imm { .. } => {}
        }
    }
    fn scan_sp(out: &mut HashSet<u32>, sp: &ScalarPlace) {
        if let ScalarPlace::Mem { expr, .. } = sp {
            scan_place(out, expr);
        }
    }
    fn scan_ret(out: &mut HashSet<u32>, r: &RetDest) {
        match r {
            RetDest::Ignore => {}
            RetDest::Scalar(sp) => scan_sp(out, sp),
            RetDest::Pair(a, b) => {
                scan_sp(out, a);
                scan_sp(out, b);
            }
            RetDest::Indirect(pe) => scan_place(out, pe),
        }
    }
    fn scan_rv(out: &mut HashSet<u32>, rv: &ir::Rvalue) {
        use ir::Rvalue as R;
        match rv {
            R::Ref(pe) => scan_place(out, pe),
            R::Use(o)
            | R::NotBits(o)
            | R::NotBool(o)
            | R::Neg(o)
            | R::Cast { a: o, .. }
            | R::BitUn { a: o, .. } => scan_op(out, o),
            R::IntBin { a, b, .. }
            | R::IntCmp { a, b, .. }
            | R::PtrDiff { a, b, .. }
            | R::UMax { a, b }
            | R::IntSat { a, b, .. }
            | R::MemCmp { a, b, .. }
            | R::IntCmp3 { a, b, .. }
            | R::FloatBin { a, b, .. }
            | R::FloatCmp { a, b, .. }
            | R::MathBin { a, b, .. } => {
                scan_op(out, a);
                scan_op(out, b);
            }
            R::PtrOffset { ptr, count, .. } => {
                scan_op(out, ptr);
                scan_op(out, count);
            }
            R::MathFma { a, b, c, .. } => {
                scan_op(out, a);
                scan_op(out, b);
                scan_op(out, c);
            }
            R::NicheDiscr { tag, .. }
            | R::MathUn { a: tag, .. }
            | R::FloatNeg { a: tag, .. }
            | R::FloatCast { a: tag, .. }
            | R::FloatToInt { a: tag, .. }
            | R::IntToFloat { a: tag, .. }
            | R::AtomicLoad { addr: tag, .. } => scan_op(out, tag),
            R::F128Cmp { a, b, .. } | R::Cmp128 { a, b, .. } => {
                scan_place(out, a);
                scan_place(out, b);
            }
            R::SimdBitmask { a, .. } | R::SimdReduce { a, .. } | R::SimdReduceArith { a, .. } => {
                scan_place(out, a);
            }
            R::TlsRef(_) => {}
        }
    }
    let mut out = HashSet::new();
    for blk in &body.blocks {
        for st in &blk.stmts {
            match st {
                Stmt::Assign { dst, rv } => {
                    scan_sp(&mut out, dst);
                    scan_rv(&mut out, rv);
                }
                Stmt::AssignOverflow {
                    a,
                    b,
                    dst_val,
                    dst_flag,
                    ..
                } => {
                    scan_op(&mut out, a);
                    scan_op(&mut out, b);
                    scan_sp(&mut out, dst_val);
                    scan_sp(&mut out, dst_flag);
                }
                Stmt::Copy { dst, src, .. } => {
                    scan_place(&mut out, dst);
                    scan_place(&mut out, src);
                }
                Stmt::RepeatScalar { dst, val, .. } => {
                    scan_place(&mut out, dst);
                    scan_op(&mut out, val);
                }
                Stmt::RepeatBytes { first, .. } => scan_place(&mut out, first),
                Stmt::VolatileLoad { addr, dst, .. } => {
                    scan_op(&mut out, addr);
                    scan_place(&mut out, dst);
                }
                Stmt::VolatileStore { addr, src, .. } => {
                    scan_op(&mut out, addr);
                    scan_place(&mut out, src);
                }
                Stmt::AtomicStore { addr, val, .. } => {
                    scan_op(&mut out, addr);
                    scan_op(&mut out, val);
                }
                Stmt::AtomicCxchg {
                    addr,
                    expected,
                    new,
                    dst_val,
                    dst_ok,
                    ..
                } => {
                    scan_op(&mut out, addr);
                    scan_op(&mut out, expected);
                    scan_op(&mut out, new);
                    scan_sp(&mut out, dst_val);
                    scan_sp(&mut out, dst_ok);
                }
                Stmt::AtomicRmw { addr, val, dst, .. } => {
                    scan_op(&mut out, addr);
                    scan_op(&mut out, val);
                    scan_sp(&mut out, dst);
                }
                Stmt::MemCopy {
                    dst, src, count, ..
                } => {
                    scan_op(&mut out, dst);
                    scan_op(&mut out, src);
                    scan_op(&mut out, count);
                }
                Stmt::MemSet {
                    dst, val, count, ..
                } => {
                    scan_op(&mut out, dst);
                    scan_op(&mut out, val);
                    scan_op(&mut out, count);
                }
                Stmt::SimdBin { dst, a, b, .. } | Stmt::SimdSelectBitmask { dst, a, b, .. } => {
                    scan_place(&mut out, dst);
                    scan_place(&mut out, a);
                    scan_place(&mut out, b);
                }
                Stmt::SimdFma { dst, a, b, c, .. } => {
                    scan_place(&mut out, dst);
                    scan_place(&mut out, a);
                    scan_place(&mut out, b);
                    scan_place(&mut out, c);
                }
                Stmt::SimdUn { dst, a, .. } | Stmt::SimdCast { dst, src: a, .. } => {
                    scan_place(&mut out, dst);
                    scan_place(&mut out, a);
                }
                Stmt::SimdExtractDyn { src, idx, dst, .. } => {
                    scan_place(&mut out, src);
                    scan_op(&mut out, idx);
                    scan_sp(&mut out, dst);
                }
                Stmt::SimdArithOffset {
                    ptrs, offsets, dst, ..
                } => {
                    scan_place(&mut out, ptrs);
                    scan_place(&mut out, offsets);
                    scan_place(&mut out, dst);
                }
                Stmt::SimdFunnel {
                    dst, a, b, shift, ..
                } => {
                    scan_place(&mut out, dst);
                    scan_place(&mut out, a);
                    scan_place(&mut out, b);
                    scan_place(&mut out, shift);
                }
                Stmt::SimdSelect {
                    mask, a, b, dst, ..
                } => {
                    scan_place(&mut out, mask);
                    scan_place(&mut out, a);
                    scan_place(&mut out, b);
                    scan_place(&mut out, dst);
                }
                Stmt::SimdGather {
                    passthru,
                    ptrs,
                    mask,
                    dst,
                    ..
                } => {
                    scan_place(&mut out, passthru);
                    scan_place(&mut out, ptrs);
                    scan_place(&mut out, mask);
                    scan_place(&mut out, dst);
                }
                Stmt::SimdScatter {
                    values, ptrs, mask, ..
                } => {
                    scan_place(&mut out, values);
                    scan_place(&mut out, ptrs);
                    scan_place(&mut out, mask);
                }
                Stmt::SimdMaskedLoad {
                    mask,
                    base,
                    passthru,
                    dst,
                    ..
                } => {
                    scan_place(&mut out, mask);
                    scan_op(&mut out, base);
                    scan_place(&mut out, passthru);
                    scan_place(&mut out, dst);
                }
                Stmt::SimdMaskedStore {
                    mask, base, values, ..
                } => {
                    scan_place(&mut out, mask);
                    scan_op(&mut out, base);
                    scan_place(&mut out, values);
                }
                Stmt::SimdInsertDyn {
                    src, idx, val, dst, ..
                } => {
                    scan_place(&mut out, src);
                    scan_op(&mut out, idx);
                    scan_op(&mut out, val);
                    scan_place(&mut out, dst);
                }
                Stmt::SimdSplat { dst, val, .. } => {
                    scan_place(&mut out, dst);
                    scan_op(&mut out, val);
                }
                Stmt::Bin128 { a, b, dst, .. } => {
                    scan_place(&mut out, a);
                    if let ir::Bin128Rhs::Wide(w) = b {
                        scan_place(&mut out, w);
                    }
                    scan_place(&mut out, dst);
                }
                Stmt::Wide128ToFloat { src, dst, .. } => {
                    scan_place(&mut out, src);
                    scan_sp(&mut out, dst);
                }
                Stmt::FloatToWide128 { src, dst, .. } => {
                    scan_op(&mut out, src);
                    scan_place(&mut out, dst);
                }
                Stmt::Bit128 { src, dst, .. } => {
                    scan_place(&mut out, src);
                    scan_place(&mut out, dst);
                }
                Stmt::Bit128Count { src, dst, .. } => {
                    scan_place(&mut out, src);
                    scan_sp(&mut out, dst);
                }
                Stmt::F128Bin { a, b, dst, .. } | Stmt::F128Fma { a, b, dst, .. } => {
                    scan_place(&mut out, a);
                    scan_place(&mut out, b);
                    scan_place(&mut out, dst);
                }
                Stmt::F128MathBin { a, b, dst, .. } => {
                    scan_place(&mut out, a);
                    if let ir::F128Rhs::Wide(w) = b {
                        scan_place(&mut out, w);
                    }
                    if let ir::F128Rhs::Scalar(o) = b {
                        scan_op(&mut out, o);
                    }
                    scan_place(&mut out, dst);
                }
                Stmt::F128Un { a, dst, .. } => {
                    scan_place(&mut out, a);
                    scan_place(&mut out, dst);
                }
                Stmt::F128FromScalar { src, dst, .. } => {
                    scan_op(&mut out, src);
                    scan_place(&mut out, dst);
                }
                Stmt::F128ToScalar { src, dst, .. } => {
                    scan_place(&mut out, src);
                    scan_sp(&mut out, dst);
                }
                Stmt::F128FromWideInt { src, dst, .. } | Stmt::F128ToWideInt { src, dst, .. } => {
                    scan_place(&mut out, src);
                    scan_place(&mut out, dst);
                }
                Stmt::NicheDiscr128 { tag, dst, .. } => {
                    scan_place(&mut out, tag);
                    scan_sp(&mut out, dst);
                }
                Stmt::Trap(_) | Stmt::Nop | Stmt::Fence { .. } => {}
            }
        }
        match &blk.term {
            Terminator::Goto(_) | Terminator::Return | Terminator::Unreachable => {}
            Terminator::SwitchInt { discr, .. } => match discr {
                SwitchDiscr::Scalar(o) => scan_op(&mut out, o),
                SwitchDiscr::Wide(pe) => scan_place(&mut out, pe),
            },
            Terminator::Call { args, ret, .. }
            | Terminator::CallBuiltin { args, ret, .. }
            | Terminator::CallForeign { args, ret, .. }
            | Terminator::CallIndirect { args, ret, .. } => {
                for a in args {
                    scan_op(&mut out, a);
                }
                scan_ret(&mut out, ret);
            }
            Terminator::InlineAsm { ins, outs, .. } => {
                for (_, o) in ins {
                    scan_op(&mut out, o);
                }
                for (_, sp) in outs {
                    scan_sp(&mut out, sp);
                }
            }
            Terminator::Resume | Terminator::TerminateAbort | Terminator::Trap(_) => {}
        }
    }
    // Indirect ABI（M5.4c 准入；保守纳入——取址性最强）
    if let RetAbi::Indirect {
        ret_off, sret_off, ..
    } = &body.ret
    {
        out.insert(*ret_off);
        out.insert(*sret_off);
    }
    for p in &body.params {
        if let ParamAbi::Indirect { off, .. } = p {
            out.insert(*off);
        }
    }
    out
}

/// 收集 SSA 候选槽偏移（def 0 初始化用）= 全部 Slot 引用减去落帧集。
fn collect_ssa_offs(
    body: &ir::FuncBody,
    frame_offs: &std::collections::HashSet<u32>,
    out: &mut Vec<u32>,
) {
    let mut push = |s: &Slot| {
        if !frame_offs.contains(&s.off) && !out.contains(&s.off) {
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
                        R::Use(a)
                        | R::NotBits(a)
                        | R::NotBool(a)
                        | R::Neg(a)
                        | R::Cast { a, .. }
                        | R::BitUn { a, .. } => op(a, &mut push),
                        R::IntBin { a, b, .. }
                        | R::IntCmp { a, b, .. }
                        | R::PtrDiff { a, b, .. }
                        | R::UMax { a, b } => {
                            op(a, &mut push);
                            op(b, &mut push);
                        }
                        R::PtrOffset { ptr, count, .. } => {
                            op(ptr, &mut push);
                            op(count, &mut push);
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

// ===== M5.4 前置 probe：LSDA 管线最小验证（cg_clif GccExceptTable 同构）=====
//
// 验证链（任一环失败即 M5.4 LSDA 方案需要重评）：
// try_call（tag0=cleanup，`BlockArg::TryCallExn(0)` 传异常指针）→ 从
// `buffer.call_sites()` 手工构建 GccExceptTable（ret_addr-1 单字节 call-site 项）
// → CIE(rust_eh_personality, absptr) + FDE.lsda → `__register_frame` →
// 宿主 panic 载荷（resume_unwind，与 spike3/M4.2 同形态）→ cleanup pad 执行 →
// `_Unwind_Resume(exn)` 续传至宿主 catch_unwind。
#[cfg(all(test, target_arch = "x86_64", target_os = "linux"))]
mod lsda_probe {
    use cranelift_codegen::ir::{
        AbiParam, BlockArg, BlockCall, ExceptionTableData, ExceptionTableItem, ExceptionTag,
        InstBuilder, Signature, types,
    };
    use cranelift_codegen::isa::unwind::UnwindInfo;
    use cranelift_codegen::isa::{CallConv, TargetIsa};
    use cranelift_codegen::settings::{self, Configurable};
    use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
    use cranelift_jit::{JITBuilder, JITModule};
    use cranelift_module::{Linkage, Module as ClifModule};
    use gimli::RunTimeEndian;
    use gimli::write::{Address, EhFrame, EndianVec, FrameTable};
    use std::sync::atomic::{AtomicU64, Ordering};

    /// pad 执行标记（0=未走, 1=pad 已走, 2=正常返回）；宿主侧断言用
    static PAD_MARK: AtomicU64 = AtomicU64::new(0);

    /// 宿主 panic 载荷源（spike3/M4.2 同形态：resume_unwind 携带 Rust payload）
    extern "C-unwind" fn probe_raise() {
        std::panic::resume_unwind(Box::new(0x2a_i32));
    }
    extern "C-unwind" fn probe_mark(x: u64) {
        PAD_MARK.store(x, Ordering::SeqCst);
    }
    extern "C-unwind" fn probe_unwind_resume(ex: *mut u8) -> ! {
        unsafe { _Unwind_Resume(ex) }
    }
    unsafe extern "C" {
        fn _Unwind_Resume(ex: *mut u8) -> !;
        fn rust_eh_personality();
    }

    /// 手工 GccExceptTable（cleanup-only，无 type_info；cg_clif 版式 + **全覆盖**）：
    /// - 无 handler 的调用点：(ret_addr-1, len=1, lpad=0, action=0) —— 命中即
    ///   EHAction::None（rust find_eh_action 的 cs_lpad==0 分支）
    /// - cleanup handler 调用点：(ret_addr-1, len=1, pad, action=0)
    /// **rust 版 find_eh_action 对"ip 不在表中"返回 EHAction::Terminate（= _URC_FATAL），
    /// 与 libgcc 的 __gcc_personality_v0（no-entry = None）不同——call-site 表必须覆盖
    /// 函数内全部调用点**（cg_clif 对无 handler 站点同样发 lpad=0 项的原因）。
    /// 项按 buffer.call_sites() 序（= 指令序，满足 rust 解析器的有序表假设）。
    fn build_lsda(call_sites: &[(u64, Option<u64>)]) -> Vec<u8> {
        fn uleb(out: &mut Vec<u8>, mut v: u64) {
            loop {
                let mut b = (v & 0x7f) as u8;
                v >>= 7;
                if v != 0 {
                    b |= 0x80;
                }
                out.push(b);
                if v == 0 {
                    break;
                }
            }
        }
        let mut out = vec![0xff, 0xff, 0x01]; // lpStart=omit, ttype=omit, csEncoding=uleb128
        let mut body = Vec::new();
        for &(ret_addr, pad) in call_sites {
            uleb(&mut body, ret_addr - 1);
            uleb(&mut body, 1);
            uleb(&mut body, pad.unwrap_or(0));
            uleb(&mut body, 0); // action=0
        }
        uleb(&mut out, body.len() as u64);
        out.extend_from_slice(&body);
        while !out.len().is_multiple_of(4) {
            out.push(0);
        }
        out
    }

    /// 定义后取 (UnwindInfo, [(ret_addr, Option<landing_pad>)])——全调用点
    /// （cg_clif add_function 同数据源同口径：无 handler → None（lpad=0 项））
    fn unwind_and_sites(
        isa: &dyn TargetIsa,
        cctx: &cranelift_codegen::Context,
    ) -> (UnwindInfo, Vec<(u64, Option<u64>)>) {
        let cc = cctx.compiled_code().unwrap();
        let ui = cc.create_unwind_info(isa).unwrap().expect("unwind_info");
        let mut cs = Vec::new();
        for site in cc.buffer.call_sites() {
            if site.exception_handlers.is_empty() {
                cs.push((u64::from(site.ret_addr), None));
            }
            for h in site.exception_handlers {
                if let cranelift_codegen::FinalizedMachExceptionHandler::Tag(tag, lp) = h {
                    assert_eq!(tag.as_u32(), 0, "probe 只发 cleanup tag");
                    cs.push((u64::from(site.ret_addr), Some(u64::from(*lp))));
                }
            }
        }
        (ui, cs)
    }

    /// 二分定位（分支 -1）：纯宿主基线——catch_unwind(probe_raise) 无 JIT 参与。
    /// 此分支若挂 = 测试二进制的 unwind 基线本身坏了，与 JIT 无关。
    #[test]
    fn host_baseline_catch() {
        let r = std::panic::catch_unwind(|| probe_raise());
        let p = r.expect_err("宿主基线应收到 payload");
        assert_eq!(*p.downcast::<i32>().unwrap(), 0x2a);
    }

    /// 二分定位（probe 分支 0）：导入符号直调——probe_mark 可见即 import 调用链好。
    #[test]
    fn import_call_works() {
        PAD_MARK.store(0, Ordering::SeqCst);
        let mut fb = settings::builder();
        fb.set("opt_level", "speed").unwrap();
        let isa = cranelift_native::builder()
            .unwrap()
            .finish(settings::Flags::new(fb))
            .unwrap();
        let mut jb = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
        jb.symbol("probe_mark", probe_mark as *const u8);
        let mut module = JITModule::new(jb);
        let mut fbc = FunctionBuilderContext::new();
        let mark_sig = {
            let mut s = module.make_signature();
            s.params.push(AbiParam::new(types::I64));
            s
        };
        let mark = module
            .declare_function("probe_mark", Linkage::Import, &mark_sig)
            .unwrap();
        let caller_id = module
            .declare_function("caller", Linkage::Local, &mark_sig)
            .unwrap();
        {
            let mut cctx = module.make_context();
            cctx.func.signature = mark_sig.clone();
            let mut b = FunctionBuilder::new(&mut cctx.func, &mut fbc);
            let entry = b.create_block();
            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            let mref = module.declare_func_in_func(mark, b.func);
            let x = b.ins().iconst(types::I64, 7);
            b.ins().call(mref, &[x]);
            b.ins().return_(&[]);
            b.seal_all_blocks();
            b.finalize();
            module.define_function(caller_id, &mut cctx).unwrap();
            module.clear_context(&mut cctx);
        }
        module.finalize_definitions().unwrap();
        let addr = module.get_finalized_function(caller_id) as u64;
        let f: unsafe extern "C-unwind" fn(u64) = unsafe { std::mem::transmute(addr) };
        unsafe { f(0) };
        assert_eq!(PAD_MARK.load(Ordering::SeqCst), 7, "import 直调未生效");
    }

    /// 二分定位（分支 A0）：单 JIT 帧穿越（caller 直调 probe_raise，无中间帧）
    #[test]
    fn cfi_single_frame() {
        let mut fb = settings::builder();
        fb.set("opt_level", "speed").unwrap();
        fb.set("unwind_info", "true").unwrap();
        fb.set("preserve_frame_pointers", "true").unwrap();
        let isa = cranelift_native::builder()
            .unwrap()
            .finish(settings::Flags::new(fb))
            .unwrap();
        let mut jb = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
        jb.symbol("probe_raise", probe_raise as *const u8);
        let mut module = JITModule::new(jb);
        let mut fbc = FunctionBuilderContext::new();
        let empty_sig = module.make_signature();
        let raise = module
            .declare_function("probe_raise", Linkage::Import, &empty_sig)
            .unwrap();
        let caller_id = module
            .declare_function("caller", Linkage::Local, &empty_sig)
            .unwrap();
        let ui_caller;
        {
            let mut cctx = module.make_context();
            cctx.func.signature = empty_sig.clone();
            let mut b = FunctionBuilder::new(&mut cctx.func, &mut fbc);
            let entry = b.create_block();
            b.switch_to_block(entry);
            let rref = module.declare_func_in_func(raise, b.func);
            b.ins().call(rref, &[]);
            b.ins().return_(&[]);
            b.seal_all_blocks();
            b.finalize();
            module.define_function(caller_id, &mut cctx).unwrap();
            let (ui, _) = unwind_and_sites(module.isa(), &cctx);
            ui_caller = ui;
            module.clear_context(&mut cctx);
        }
        module.finalize_definitions().unwrap();
        let mut table = FrameTable::default();
        let cie = table.add_cie(module.isa().create_systemv_cie().expect("cie"));
        let caller_addr = module.get_finalized_function(caller_id) as u64;
        if let UnwindInfo::SystemV(info) = ui_caller {
            table.add_fde(cie, info.to_fde(Address::Constant(caller_addr)));
        } else {
            panic!("无 SystemV UnwindInfo");
        }
        let mut eh = EhFrame(EndianVec::new(RunTimeEndian::Little));
        table.write_eh_frame(&mut eh).unwrap();
        let mut bytes = eh.0.into_vec();
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        let buf: &'static [u8] = Box::leak(bytes.into_boxed_slice());
        unsafe extern "C" {
            fn __register_frame(fde: *const u8);
        }
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
        let caller_fn: unsafe extern "C-unwind" fn() = unsafe { std::mem::transmute(caller_addr) };
        let result = std::panic::catch_unwind(|| unsafe { caller_fn() });
        let payload = result
            .expect_err("单帧分支应收到 payload")
            .downcast::<i32>()
            .expect("载荷类型错");
        assert_eq!(*payload, 0x2a);
    }

    /// 二分定位（probe 分支 A）：纯 CFI 穿越——无 personality/LSDA，宿主 panic 经
    /// 两个 JIT 帧（普通 call）传回宿主 catch_unwind。此分支不过 = 基础注册坏；
    /// 过 = 问题在 LSDA/personality/pad 半区。
    #[test]
    fn cfi_only_passthrough() {
        let mut fb = settings::builder();
        fb.set("opt_level", "speed").unwrap();
        fb.set("unwind_info", "true").unwrap();
        fb.set("preserve_frame_pointers", "true").unwrap();
        let isa = cranelift_native::builder()
            .unwrap()
            .finish(settings::Flags::new(fb))
            .unwrap();
        let mut jb = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
        jb.symbol("probe_raise", probe_raise as *const u8);
        let mut module = JITModule::new(jb);
        let mut fbc = FunctionBuilderContext::new();
        let empty_sig = module.make_signature();
        let raise = module
            .declare_function("probe_raise", Linkage::Import, &empty_sig)
            .unwrap();

        let raiser_id = module
            .declare_function("raiser", Linkage::Local, &empty_sig)
            .unwrap();
        let ui_raiser;
        {
            let mut cctx = module.make_context();
            cctx.func.signature = empty_sig.clone();
            let mut b = FunctionBuilder::new(&mut cctx.func, &mut fbc);
            let entry = b.create_block();
            b.switch_to_block(entry);
            let rref = module.declare_func_in_func(raise, b.func);
            b.ins().call(rref, &[]);
            b.ins().return_(&[]);
            b.seal_all_blocks();
            b.finalize();
            module.define_function(raiser_id, &mut cctx).unwrap();
            let (ui, _) = unwind_and_sites(module.isa(), &cctx);
            ui_raiser = ui;
            module.clear_context(&mut cctx);
        }
        let caller_id = module
            .declare_function("caller", Linkage::Local, &empty_sig)
            .unwrap();
        let ui_caller;
        {
            let mut cctx = module.make_context();
            cctx.func.signature = empty_sig.clone();
            let mut b = FunctionBuilder::new(&mut cctx.func, &mut fbc);
            let entry = b.create_block();
            b.switch_to_block(entry);
            let rref = module.declare_func_in_func(raiser_id, b.func);
            b.ins().call(rref, &[]);
            b.ins().return_(&[]);
            b.seal_all_blocks();
            b.finalize();
            module.define_function(caller_id, &mut cctx).unwrap();
            let (ui, _) = unwind_and_sites(module.isa(), &cctx);
            ui_caller = ui;
            module.clear_context(&mut cctx);
        }
        module.finalize_definitions().unwrap();

        let mut table = FrameTable::default();
        let cie = table.add_cie(module.isa().create_systemv_cie().expect("cie"));
        let raiser_addr = module.get_finalized_function(raiser_id) as u64;
        let caller_addr = module.get_finalized_function(caller_id) as u64;
        for (ui, addr) in [(ui_raiser, raiser_addr), (ui_caller, caller_addr)] {
            if let UnwindInfo::SystemV(info) = ui {
                table.add_fde(cie, info.to_fde(Address::Constant(addr)));
            } else {
                panic!("无 SystemV UnwindInfo");
            }
        }
        let mut eh = EhFrame(EndianVec::new(RunTimeEndian::Little));
        table.write_eh_frame(&mut eh).unwrap();
        let mut bytes = eh.0.into_vec();
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        let buf: &'static [u8] = Box::leak(bytes.into_boxed_slice());
        unsafe extern "C" {
            fn __register_frame(fde: *const u8);
        }
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

        let caller_fn: unsafe extern "C-unwind" fn() = unsafe { std::mem::transmute(caller_addr) };
        let result = std::panic::catch_unwind(|| unsafe { caller_fn() });
        let payload = result
            .expect_err("CFI-only 分支应收到宿主 payload（基础注册疑似坏）")
            .downcast::<i32>()
            .expect("载荷类型错");
        assert_eq!(*payload, 0x2a);
    }

    #[test]
    fn lsda_cleanup_pad_executes_and_resume_continues() {
        PAD_MARK.store(0, Ordering::SeqCst);

        let mut fb = settings::builder();
        fb.set("opt_level", "speed").unwrap();
        fb.set("unwind_info", "true").unwrap();
        fb.set("preserve_frame_pointers", "true").unwrap();
        let isa = cranelift_native::builder()
            .unwrap()
            .finish(settings::Flags::new(fb))
            .unwrap();
        let mut jb = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
        jb.symbol("probe_raise", probe_raise as *const u8);
        jb.symbol("probe_mark", probe_mark as *const u8);
        jb.symbol("_Unwind_Resume", {
            unsafe extern "C" {
                fn _Unwind_Resume(ex: *mut u8) -> !;
            }
            _Unwind_Resume as *const u8
        });
        let mut module = JITModule::new(jb);
        let mut fbc = FunctionBuilderContext::new();

        let empty_sig = module.make_signature(); // () -> ()
        let mark_sig = {
            let mut s = module.make_signature();
            s.params.push(AbiParam::new(types::I64));
            s
        };
        let resume_sig = {
            let mut s = module.make_signature();
            s.params.push(AbiParam::new(types::I64));
            s
        };
        let raise = module
            .declare_function("probe_raise", Linkage::Import, &empty_sig)
            .unwrap();
        let mark = module
            .declare_function("probe_mark", Linkage::Import, &mark_sig)
            .unwrap();
        let resume = module
            .declare_function("_Unwind_Resume", Linkage::Import, &resume_sig)
            .unwrap();

        // raiser：调 probe_raise（宿主 resume_unwind 载荷经其帧穿过）
        let raiser_id = module
            .declare_function("raiser", Linkage::Local, &empty_sig)
            .unwrap();
        let ui_raiser;
        {
            let mut cctx = module.make_context();
            cctx.func.signature = empty_sig.clone();
            let mut b = FunctionBuilder::new(&mut cctx.func, &mut fbc);
            let entry = b.create_block();
            b.switch_to_block(entry);
            let rref = module.declare_func_in_func(raise, b.func);
            b.ins().call(rref, &[]);
            b.ins().return_(&[]);
            b.seal_all_blocks();
            b.finalize();
            module.define_function(raiser_id, &mut cctx).unwrap();
            let (ui, _) = unwind_and_sites(module.isa(), &cctx);
            ui_raiser = ui;
            module.clear_context(&mut cctx);
        }

        // caller：try_call(raiser)；normal → ok(mark 2)；tag0 pad(mark 1 → _Unwind_Resume(exn))
        let caller_id = module
            .declare_function("caller", Linkage::Local, &empty_sig)
            .unwrap();
        let (ui_caller, call_sites);
        {
            let mut cctx = module.make_context();
            cctx.func.signature = empty_sig.clone();
            let mut b = FunctionBuilder::new(&mut cctx.func, &mut fbc);
            let entry = b.create_block();
            let ok = b.create_block();
            let pad = b.create_block();
            b.append_block_param(pad, types::I64); // TryCallExn(0) 的落点块参
            b.switch_to_block(entry);

            let rref = module.declare_func_in_func(raiser_id, b.func);
            let sig0 = b.func.import_signature(Signature::new(CallConv::SystemV));
            let normal = BlockCall::new(ok, [], &mut b.func.dfg.value_lists);
            let pad_call = b.func.dfg.block_call(pad, &[BlockArg::TryCallExn(0)]);
            let et = b.func.dfg.exception_tables.push(ExceptionTableData::new(
                sig0,
                normal,
                [ExceptionTableItem::Tag(
                    ExceptionTag::with_number(0).unwrap(),
                    pad_call,
                )],
            ));
            b.ins().try_call(rref, &[], et);

            b.switch_to_block(ok);
            let mref = module.declare_func_in_func(mark, b.func);
            let two = b.ins().iconst(types::I64, 2);
            b.ins().call(mref, &[two]);
            b.ins().return_(&[]);

            b.switch_to_block(pad);
            let exn = b.block_params(pad)[0];
            let mref2 = module.declare_func_in_func(mark, b.func);
            let one = b.ins().iconst(types::I64, 1);
            b.ins().call(mref2, &[one]);
            let resref = module.declare_func_in_func(resume, b.func);
            b.ins().call(resref, &[exn]);
            b.ins()
                .trap(cranelift_codegen::ir::TrapCode::user(1).unwrap());
            b.seal_all_blocks();
            b.finalize();

            module.define_function(caller_id, &mut cctx).unwrap();
            let (ui, cs) = unwind_and_sites(module.isa(), &cctx);
            ui_caller = ui;
            call_sites = cs;
            module.clear_context(&mut cctx);
        }
        assert!(
            call_sites.len() >= 2,
            "caller 应有多个 call-site（try_call + 其余调用点全覆盖）"
        );
        module.finalize_definitions().unwrap();

        // eh_frame：CIE0 无 personality（raiser）；CIE1 = rust_eh_personality + lsda（caller）。
        // personality 走 DW.ref 间接（cg_clif 形态）：CIE 的 personality 指针指向一个
        // 持有真 personality 地址的静态 u64——absptr 直嵌在本环境被证伪（空 LSDA 也 abort）。
        static PERS_REF: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        PERS_REF.store(rust_eh_personality as *const u8 as u64, Ordering::SeqCst);
        let mut table = FrameTable::default();
        let cie_plain = table.add_cie(module.isa().create_systemv_cie().expect("cie"));
        let mut cie_pers = module.isa().create_systemv_cie().expect("cie");
        cie_pers.lsda_encoding = Some(gimli::DW_EH_PE_absptr);
        cie_pers.personality = Some((
            gimli::DwEhPe(gimli::DW_EH_PE_indirect.0 | gimli::DW_EH_PE_absptr.0),
            Address::Constant(&PERS_REF as *const std::sync::atomic::AtomicU64 as u64),
        ));
        let cie_pers_id = table.add_cie(cie_pers);

        let raiser_addr = module.get_finalized_function(raiser_id) as u64;
        let caller_addr = module.get_finalized_function(caller_id) as u64;
        if let UnwindInfo::SystemV(info) = ui_raiser {
            table.add_fde(cie_plain, info.to_fde(Address::Constant(raiser_addr)));
        } else {
            panic!("raiser 无 SystemV UnwindInfo");
        }
        let lsda_bytes = build_lsda(&call_sites);
        let lsda_addr = lsda_bytes.as_ptr() as u64;
        std::mem::forget(lsda_bytes); // FDE/LSDA 终身有效（probe 进程期）
        if let UnwindInfo::SystemV(info) = ui_caller {
            let mut fde = info.to_fde(Address::Constant(caller_addr));
            fde.lsda = Some(Address::Constant(lsda_addr));
            table.add_fde(cie_pers_id, fde);
        } else {
            panic!("caller 无 SystemV UnwindInfo");
        }

        // spike5 同款注册：FrameTable → eh_frame 字节 + 终止零长 + 逐 FDE __register_frame
        let mut eh = EhFrame(EndianVec::new(RunTimeEndian::Little));
        table.write_eh_frame(&mut eh).unwrap();
        let mut bytes = eh.0.into_vec();
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        let buf: &'static [u8] = Box::leak(bytes.into_boxed_slice());
        unsafe extern "C" {
            fn __register_frame(fde: *const u8);
        }
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

        // 全链点火：宿主 catch_unwind 应收到 42；pad 应已走（mark=1，而非 2）
        let caller_fn: unsafe extern "C-unwind" fn() = unsafe { std::mem::transmute(caller_addr) };
        let result = std::panic::catch_unwind(|| unsafe { caller_fn() });
        assert!(
            result.is_err(),
            "caller 未抛出（pad/unwind 链断裂；PAD_MARK={}）",
            PAD_MARK.load(Ordering::SeqCst)
        );
        let payload = result.unwrap_err();
        assert_eq!(payload.downcast_ref::<i32>(), Some(&0x2a), "载荷丢失/替换");
        assert_eq!(
            PAD_MARK.load(Ordering::SeqCst),
            1,
            "cleanup pad 未执行（LSDA/personality 未命中）"
        );
    }
}
