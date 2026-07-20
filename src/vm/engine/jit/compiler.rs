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

use super::*;
use super::helpers::*;
use super::helpers::_Unwind_Resume;
use super::admit::{CalleeAbi, admit, callee_abi};
use super::frame::analyze_frame;
use super::translate::Translator;

/// c2i 壳的引擎定位（单引擎进程模型，与 TRACK_DIAGNOSTIC 全局钩同一假设面）。
pub(super) static SHARED: std::sync::atomic::AtomicPtr<Shared> =
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
            if ok {
                let addr = shared.jit.slots[func as usize].load(Ordering::Acquire);
                let fast = shared.jit.slots_fast[func as usize].load(Ordering::Acquire);
                eprintln!(
                    "mirvm-jit-debug: f{func} 发布={ok} @{addr:#x} fast@{fast:#x}（{}）",
                    shared.module.funcs[func as usize].name
                );
            } else {
                eprintln!("mirvm-jit-debug: f{func} 发布={ok}");
            }
        }
    }
}

// ===== 运行期助手（JIT 码经 import symbol 调回引擎）=====

/// c2i 万能壳：编译码调未编译 guest 函数（经蹦床打包）→ 回解释器。
/// ctx 恢复 = 边界 TLS attach（thunk 工厂同款，幂等）。

struct Compiler {
    shared: &'static Shared,
    module: JITModule,
    fbc: FunctionBuilderContext,
    c2i: ClifFuncId,
    unreachable: ClifFuncId,
    /// M5.4a：Copy/帧清零的宿主 memmove/memset 通道
    memmove: ClifFuncId,
    memset: ClifFuncId,
    /// M5.4b-1：MemCmp（compare_bytes intrinsic）
    memcmp: ClifFuncId,
    /// M5.4b-1：除零诊断退出（interp engine_abort 同文案同码）
    div_zero: ClifFuncId,
    /// M5.4b-1：volatile 读/写（interp opaque 字节载体同一实现）
    volatile_load: ClifFuncId,
    volatile_store: ClifFuncId,
    /// T1-b：CallIndirect/TlsRef 助手（helpers.rs 同本体）
    call_indirect: ClifFuncId,
    tls_ref: ClifFuncId,
    call_foreign: ClifFuncId,
    /// T1-b：CallBuiltin/分配系快路助手（helpers.rs 同 exec_builtin 本体）
    call_builtin: ClifFuncId,
    alloc: ClifFuncId,
    /// T1-c unwind 产品化：TerminateAbort 纯助手 / Terminate 边界直接调用 /
    /// _Unwind_Resume 导入
    terminate_abort: ClifFuncId,
    call_terminate: ClifFuncId,
    unwind_resume: ClifFuncId,
    /// 本批 (clif id, unwind info, try_call 函数的 LSDA 字节)——finalize 后统一注册
    pending_unwind: Vec<(ClifFuncId, UnwindInfo, Option<Vec<u8>>)>,
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
        jb.symbol("mirvm_jit_div_zero", mirvm_jit_div_zero as *const u8);
        jb.symbol("mirvm_volatile_load", mirvm_volatile_load as *const u8);
        jb.symbol("mirvm_volatile_store", mirvm_volatile_store as *const u8);
        jb.symbol("mirvm_call_indirect", mirvm_call_indirect as *const u8);
        jb.symbol("mirvm_jit_terminate_abort", mirvm_jit_terminate_abort as *const u8);
        jb.symbol("mirvm_call_terminate", mirvm_call_terminate as *const u8);
        jb.symbol("_Unwind_Resume", _Unwind_Resume as *const u8);
        jb.symbol("mirvm_tls_ref", mirvm_tls_ref as *const u8);
        jb.symbol("mirvm_call_foreign", mirvm_call_foreign as *const u8);
        jb.symbol("mirvm_call_builtin", mirvm_call_builtin as *const u8);
        jb.symbol("mirvm_alloc", mirvm_alloc as *const u8);
        // M5.4b-3 助手注册表
        jb.symbol("mirvm_bin128_ovf", mirvm_bin128_ovf as *const u8);
        jb.symbol("mirvm_f128_bin", mirvm_f128_bin as *const u8);
        jb.symbol("mirvm_f128_cmp", mirvm_f128_cmp as *const u8);
        jb.symbol("mirvm_f128_un", mirvm_f128_un as *const u8);
        jb.symbol("mirvm_f128_math", mirvm_f128_math as *const u8);
        jb.symbol(
            "mirvm_f128_from_scalar",
            mirvm_f128_from_scalar as *const u8,
        );
        jb.symbol("mirvm_f128_to_scalar", mirvm_f128_to_scalar as *const u8);
        jb.symbol("mirvm_f128_from_wide", mirvm_f128_from_wide as *const u8);
        jb.symbol("mirvm_f128_to_wide", mirvm_f128_to_wide as *const u8);
        jb.symbol("mirvm_float_to_wide", mirvm_float_to_wide as *const u8);
        jb.symbol("mirvm_wide_to_f16", mirvm_wide_to_f16 as *const u8);
        jb.symbol("mirvm_f16_bin", mirvm_f16_bin as *const u8);
        jb.symbol("mirvm_f16_cmp", mirvm_f16_cmp as *const u8);
        jb.symbol("mirvm_f16_neg", mirvm_f16_neg as *const u8);
        jb.symbol("mirvm_f16_cast", mirvm_f16_cast as *const u8);
        jb.symbol("mirvm_f16_to_int", mirvm_f16_to_int as *const u8);
        jb.symbol("mirvm_f16_from_int", mirvm_f16_from_int as *const u8);
        jb.symbol("memmove", crate::os::process::memmove_addr());
        jb.symbol("memset", crate::os::process::memset_addr());
        jb.symbol("memcmp", crate::os::process::memcmp_addr());
        for (n, p) in libm_syms() {
            jb.symbol(n, p as *const u8);
        }
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
        // memcmp(s1, s2, n) -> c_int（i32！I64 返回声明会把 sextend.i64 喂给
        // verifier——diff_cargo ecosystem 实测抓获）
        let mut sig_memcmp = module.make_signature();
        for _ in 0..3 {
            sig_memcmp.params.push(AbiParam::new(types::I64));
        }
        sig_memcmp.returns.push(AbiParam::new(types::I32));
        let memcmp = module
            .declare_function("memcmp", Linkage::Import, &sig_memcmp)
            .unwrap();
        let div_zero = module
            .declare_function("mirvm_jit_div_zero", Linkage::Import, &sig_unr)
            .unwrap();
        let volatile_load = module
            .declare_function("mirvm_volatile_load", Linkage::Import, &sig_mm)
            .unwrap();
        let volatile_store = module
            .declare_function("mirvm_volatile_store", Linkage::Import, &sig_mm)
            .unwrap();
        // T1-b 调用助手（helpers.rs 本体 = interp 派发/惰性物化同构）
        let mut sig_ci = module.make_signature();
        for _ in 0..8 {
            sig_ci.params.push(AbiParam::new(types::I64));
        }
        let call_indirect = module
            .declare_function("mirvm_call_indirect", Linkage::Import, &sig_ci)
            .unwrap();
        let mut sig_tls = module.make_signature();
        sig_tls.params.push(AbiParam::new(types::I64));
        sig_tls.returns.push(AbiParam::new(types::I64));
        let tls_ref = module
            .declare_function("mirvm_tls_ref", Linkage::Import, &sig_tls)
            .unwrap();
        let mut sig_cf = module.make_signature();
        for _ in 0..7 {
            sig_cf.params.push(AbiParam::new(types::I64));
        }
        sig_cf.returns.push(AbiParam::new(types::I64));
        let call_foreign = module
            .declare_function("mirvm_call_foreign", Linkage::Import, &sig_cf)
            .unwrap();
        // T1-b CallBuiltin 助手（builtin 指针 + av 数组 + n + ret_dst + caller +
        // (lo,hi) 写出指针 + terminate 旗（T1-c））；分配系快路六参直返 u64
        let mut sig_cb = module.make_signature();
        for _ in 0..7 {
            sig_cb.params.push(AbiParam::new(types::I64));
        }
        let call_builtin = module
            .declare_function("mirvm_call_builtin", Linkage::Import, &sig_cb)
            .unwrap();
        let mut sig_alloc = module.make_signature();
        for _ in 0..6 {
            sig_alloc.params.push(AbiParam::new(types::I64));
        }
        sig_alloc.returns.push(AbiParam::new(types::I64));
        let alloc = module
            .declare_function("mirvm_alloc", Linkage::Import, &sig_alloc)
            .unwrap();
        // T1-c unwind 产品化：TerminateAbort 纯助手 / Terminate 边界直接调用 /
        // _Unwind_Resume（Resume 终止子经 exception_slot 直调）
        let sig_ta = module.make_signature();
        let terminate_abort = module
            .declare_function("mirvm_jit_terminate_abort", Linkage::Import, &sig_ta)
            .unwrap();
        let mut sig_ct = module.make_signature();
        for _ in 0..4 {
            sig_ct.params.push(AbiParam::new(types::I64));
        }
        let call_terminate = module
            .declare_function("mirvm_call_terminate", Linkage::Import, &sig_ct)
            .unwrap();
        let mut sig_ur = module.make_signature();
        sig_ur.params.push(AbiParam::new(types::I64));
        let unwind_resume = module
            .declare_function("_Unwind_Resume", Linkage::Import, &sig_ur)
            .unwrap();

        Compiler {
            shared,
            module,
            fbc: FunctionBuilderContext::new(),
            c2i,
            unreachable,
            memmove,
            memset,
            memcmp,
            div_zero,
            volatile_load,
            volatile_store,
            call_indirect,
            tls_ref,
            call_foreign,
            call_builtin,
            alloc,
            terminate_abort,
            call_terminate,
            unwind_resume,
            pending_unwind: Vec::new(),
        }
    }

    fn fast_sig(&mut self, abi: CalleeAbi) -> Signature {
        let mut sig = self.module.make_signature();
        for _ in 0..abi.nparams {
            sig.params.push(AbiParam::new(types::I64));
        }
        for _ in 0..abi.nrets {
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
        let abi = callee_abi(body).expect("admit 已验");

        // PLT 快路 callee 的槽预热：未编译者发 c2i 蹦床（fast 形状，调用点形状恒定）。
        // 形态不合的 callee 不在此列——其调用点直接 c2i（cold path）。
        let mut callees: Vec<(u32, CalleeAbi)> = Vec::new();
        for blk in &body.blocks {
            if let Terminator::Call {
                callee,
                args,
                ret,
                ..
            } = &blk.term
                && *callee != func
                && !callees.iter().any(|(c, _)| c == callee)
                && let Some(cabi) = callee_abi(&self.shared.module.funcs[*callee as usize])
                && cabi.nparams
                    == args.len() + usize::from(matches!(ret, RetDest::Indirect(_)))
            {
                callees.push((*callee, cabi));
            }
        }
        for (c, cabi) in callees {
            if jit.slots_fast[c as usize].load(Ordering::Acquire) == 0 {
                if let Some(tramp) = self.define_c2i_trampoline(c, cabi) {
                    jit.slots_fast[c as usize].store(tramp as u64, Ordering::Release);
                }
            }
        }

        // 静默失败纪律（m5.3-design D4 / 防静默错值：编译失败 = 维持解释，绝不向
        // stderr 吐 panic——差分 oracle 的 stderr 逐字节比对会被线程 id 污染，实测抓获）
        let Some(fast_id) = self.define_fast(func, body, abi) else {
            return;
        };
        let Some(packed_id) = self.define_packed(func, body, abi, fast_id) else {
            return;
        };
        if self.module.finalize_definitions().is_err() {
            return;
        }
        self.register_pending_eh_frames();

        let fast = self.module.get_finalized_function(fast_id) as u64;
        let packed = self.module.get_finalized_function(packed_id) as u64;
        // 发布序：先 fast（自递归/他人调我）后 packed（interp 才可能进入编译码）
        jit.slots_fast[func as usize].store(fast, Ordering::Release);
        jit.slots[func as usize].store(packed, Ordering::Release);
    }

    /// c2i 蹦床：fast 签名，打包实参进栈上数组，调 mirvm_c2i 回解释器。
    /// 任何编译失败 = None（调用方跳过本槽预热，静默维持解释）。
    fn define_c2i_trampoline(&mut self, target: u32, abi: CalleeAbi) -> Option<*const u8> {
        let sig = self.fast_sig(abi);
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
                (abi.nparams.max(1) * 8) as u32,
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
            let nv = b.ins().iconst(types::I64, abi.nparams as i64);
            let rp = b.ins().stack_addr(types::I64, ret_ss, 0);
            b.ins().call(fref, &[fv, ap, nv, rp]);
            // mirvm_c2i 恒写 (lo,hi) 两槽（helpers.rs:8-17），按形态取回
            match abi.nrets {
                0 => {
                    b.ins().return_(&[]);
                }
                1 => {
                    let lo = b.ins().stack_load(types::I64, ret_ss, 0);
                    b.ins().return_(&[lo]);
                }
                _ => {
                    let lo = b.ins().stack_load(types::I64, ret_ss, 0);
                    let hi = b.ins().stack_load(types::I64, ret_ss, 8);
                    b.ins().return_(&[lo, hi]);
                }
            }
            b.seal_all_blocks();
            b.finalize();
        }
        if let Err(e) = self.module.define_function(id, &mut cctx) {
            if std::env::var_os("MIRVM_JIT_DEBUG").is_some() {
                eprintln!("mirvm-jit-debug: define_function 失败: {e}");
            }
            return None;
        }
        if let Some(ui) = cctx
            .compiled_code()
            .and_then(|cc| cc.create_unwind_info(self.module.isa()).ok().flatten())
        {
            self.pending_unwind.push((id, ui, None));
        }
        self.module.clear_context(&mut cctx);
        if self.module.finalize_definitions().is_err() {
            return None;
        }
        self.register_pending_eh_frames();
        Some(self.module.get_finalized_function(id))
    }

    /// fast 本体：字节码块 → CLIF；槽 → SSA 变量（I64 零扩到宽不变量）。
    /// 任何编译失败 = None（静默维持解释——绝不 panic 污染 stderr 差分）。
    fn define_fast(
        &mut self,
        func: u32,
        body: &ir::FuncBody,
        abi: CalleeAbi,
    ) -> Option<ClifFuncId> {
        let sig = self.fast_sig(abi);
        let id = self
            .module
            .declare_function(&format!("f{func}"), Linkage::Local, &sig)
            .unwrap();
        let mut cctx = self.module.make_context();
        cctx.func.signature = sig;
        // T1-c：has_try_call 由 Translator 在 build 期间置位（块外读以生成 LSDA）
        #[allow(unused_assignments)]
        let mut has_try_call = false;
        {
            let mut b = FunctionBuilder::new(&mut cctx.func, &mut self.fbc);
            let frame_offs = analyze_frame(body);
            let frame_ss = if !frame_offs.needs_frame() {
                None
            } else {
                Some(b.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    // 0 字节强征档（force）以 1 字节物化；off 仍以 0 计，语义不变
                    body.frame_size.max(1),
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
                memcmp: self.memcmp,
                div_zero: self.div_zero,
                volatile_load: self.volatile_load,
                volatile_store: self.volatile_store,
                call_indirect: self.call_indirect,
                tls_ref: self.tls_ref,
                call_foreign: self.call_foreign,
                call_builtin: self.call_builtin,
                alloc: self.alloc,
                terminate_abort: self.terminate_abort,
                call_terminate: self.call_terminate,
                unwind_resume: self.unwind_resume,
                exception_var: None,
                has_try_call: false,
            };
            tr.build(func, body);
            has_try_call = tr.has_try_call;
            b.seal_all_blocks();
            b.finalize();
        }
        if let Err(e) = self.module.define_function(id, &mut cctx) {
            if std::env::var_os("MIRVM_JIT_DEBUG").is_some() {
                eprintln!("mirvm-jit-debug: define_function 失败: {e}");
            }
            return None;
        }
        if let Some(ui) = cctx
            .compiled_code()
            .and_then(|cc| cc.create_unwind_info(self.module.isa()).ok().flatten())
        {
            // T1-c：有 try_call 的函数收集全调用点并生成 LSDA（全覆盖准则：
            // 无 handler 站点同样发 lpad=0 项，rust personality 无项 = Terminate）
            let lsda = if has_try_call {
                Some(build_lsda(&collect_call_sites(&cctx)))
            } else {
                None
            };
            self.pending_unwind.push((id, ui, lsda));
        }
        self.module.clear_context(&mut cctx);
        Some(id)
    }

    /// packed 入口：`(args: *const u64, ret: *mut u64)`——interp i2c 一跳。
    fn define_packed(
        &mut self,
        func: u32,
        body: &ir::FuncBody,
        abi: CalleeAbi,
        fast: ClifFuncId,
    ) -> Option<ClifFuncId> {
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
            let mut args: Vec<Value> = Vec::with_capacity(abi.nparams);
            for i in 0..abi.nparams {
                args.push(
                    b.ins()
                        .load(types::I64, MemFlagsData::trusted(), argp, (i * 8) as i32),
                );
            }
            let fref = self.module.declare_func_in_func(fast, b.func);
            let call = b.ins().call(fref, &args);
            // (lo,hi) 两槽恒写（T1-a 补 hi 现役语义洞）：sret/nrets=0 形态写零
            let r0 = if abi.nrets >= 1 {
                b.inst_results(call)[0]
            } else {
                b.ins().iconst(types::I64, 0)
            };
            let r1 = if abi.nrets >= 2 {
                b.inst_results(call)[1]
            } else {
                b.ins().iconst(types::I64, 0)
            };
            b.ins().store(MemFlagsData::trusted(), r0, retp, 0);
            b.ins().store(MemFlagsData::trusted(), r1, retp, 8);
            b.ins().return_(&[]);
            b.seal_all_blocks();
            b.finalize();
        }
        if let Err(e) = self.module.define_function(id, &mut cctx) {
            if std::env::var_os("MIRVM_JIT_DEBUG").is_some() {
                eprintln!("mirvm-jit-debug: define_function 失败: {e}");
            }
            return None;
        }
        if let Some(ui) = cctx
            .compiled_code()
            .and_then(|cc| cc.create_unwind_info(self.module.isa()).ok().flatten())
        {
            self.pending_unwind.push((id, ui, None));
        }
        self.module.clear_context(&mut cctx);
        Some(id)
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
            fn rust_eh_personality();
        }
        // T1-c 双 CIE：无 try_call 的函数走 plain CIE（今日管线不变）；有者走
        // personality CIE = DW.ref 间接 rust_eh_personality（lsda_encoding=absptr；
        // absptr 直嵌已被 lsda_probe 证伪）+ fde.lsda 挂接。
        PERS_REF.store(rust_eh_personality as *const u8 as u64, Ordering::SeqCst);
        let isa = self.module.isa();
        let mut table = FrameTable::default();
        let cie_plain = table.add_cie(isa.create_systemv_cie().expect("systemv cie"));
        let mut cie_pers = isa.create_systemv_cie().expect("systemv cie");
        cie_pers.lsda_encoding = Some(gimli::DW_EH_PE_absptr);
        cie_pers.personality = Some((
            gimli::DwEhPe(gimli::DW_EH_PE_indirect.0 | gimli::DW_EH_PE_absptr.0),
            Address::Constant(&PERS_REF as *const std::sync::atomic::AtomicU64 as u64),
        ));
        let cie_pers_id = table.add_cie(cie_pers);
        for (id, ui, lsda) in self.pending_unwind.drain(..) {
            if let UnwindInfo::SystemV(info) = ui {
                let addr = self.module.get_finalized_function(id) as u64;
                match lsda {
                    Some(bytes) => {
                        let lsda_addr = bytes.as_ptr() as u64;
                        std::mem::forget(bytes); // FDE.lsda 终身有效（leaked Vec v2）
                        let mut fde = info.to_fde(Address::Constant(addr));
                        fde.lsda = Some(Address::Constant(lsda_addr));
                        table.add_fde(cie_pers_id, fde);
                    }
                    None => {
                        table.add_fde(cie_plain, info.to_fde(Address::Constant(addr)));
                    }
                }
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

/// personality CIE 的 DW.ref 间接单元（单格全表共享；T1-c，lsda_probe 同款形态）。
static PERS_REF: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

// ===== LSDA 生成（lsda_probe 配方的产品化，版式逐行照抄勿创新）=====

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

/// 定义后取全调用点（cg_clif add_function 同数据源同口径：无 handler → None
/// （lpad=0 项）；cleanup tag → Some(landing pad 地址)）。
fn collect_call_sites(cctx: &cranelift_codegen::Context) -> Vec<(u64, Option<u64>)> {
    let cc = cctx.compiled_code().expect("call_sites 须在 define 后收集");
    let mut cs = Vec::new();
    for site in cc.buffer.call_sites() {
        if site.exception_handlers.is_empty() {
            cs.push((u64::from(site.ret_addr), None));
        }
        for h in site.exception_handlers {
            if let cranelift_codegen::FinalizedMachExceptionHandler::Tag(tag, lp) = h {
                assert_eq!(tag.as_u32(), 0, "本管线只发 cleanup tag");
                cs.push((u64::from(site.ret_addr), Some(u64::from(*lp))));
            }
        }
    }
    cs
}

// ===== 函数体翻译 =====
