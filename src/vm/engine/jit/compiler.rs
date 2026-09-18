//! M5.3b: bytecode → Cranelift translator (scalar subset) + compiler service thread
//! (m5.3-design §4, D3/D4/D5).
//!
//! Input = frozen `ir::FuncBody` (D3: JIT consumes bytecode, not MIR; tcx does not enter the
//! execution phase).
//! Semantic contract = **bit-identical to the interpreter** (JIT-on/off differential is the
//! first oracle): all values preserve the "I64 zero-extended to width" slot invariant; operations
//! mirror interp's int_bin/int_cmp/int_ovf identities; results are masked back by width band.
//! Frame locals are all promoted to Cranelift SSA variables (v1 admission excludes address-of /
//! memory operands ⇒ no stack frame memory); entry uniformly defs 0 (valid MIR has no
//! read-before-write paths, this makes it deterministic).
//!
//! Calls (D5 two entries + PLT):
//! - **fast**: pure guest signature (n×I64 → 0/1×I64). Compiled code calls via `slots_fast[callee]`
//!   memory indirection (load + call_indirect, call site has constant shape); slots of not-yet-
//!   compiled callees first receive a **c2i trampoline** (fast shape, internally packs arguments
//!   and calls `mirvm_c2i` back to the interpreter).
//! - **packed**: `extern "C-unwind" fn(*const u64, *mut u64)` — one interp i2c hop
//!   (call_guest reads `slots[f]`).
//!
//! Publish order = fast first, then packed (Release); call_guest Acquire read ⇒ any thread
//! entering compiled code must see its callee trampoline/entry (happens-before chain).
//!
//! unwind (D6 v1 = CFI-only): spike5 pipeline — create_unwind_info → gimli FrameTable
//! → whole `.eh_frame` section registered at once. Admission already excludes cleanup edges
//! (unwind-transparent: panic only passes through, does not land).
//!
//! Single worker thread holds the JITModule (code memory lives for the process lifetime;
//! cranelift-jit has no per-function release).

use super::admit::{CalleeAbi, admit, callee_abi};
use super::frame::analyze_frame;
use super::helpers::_Unwind_Resume;
use super::helpers::*;
use super::translate::Translator;
use super::*;

/// Start the compiler service (called by run_vm_engine after Shared is finalized; not started when --jit off).
pub fn start(shared: &std::sync::Arc<Shared>) {
    if !shared.jit.enabled {
        return;
    }
    shared.jit.stopping.store(false, Ordering::Release);
    let (tx, rx): (Sender<u32>, Receiver<u32>) = std::sync::mpsc::channel();
    *shared.jit.queue.lock().unwrap() = Some(tx);
    // 编译失败/线程死亡 = 静默维持解释（语义面零依赖 JIT）
    let worker_shared = std::sync::Arc::clone(shared);
    let worker = std::thread::Builder::new()
        .name("mirvm-jit".into())
        .spawn(move || worker(worker_shared, rx));
    *shared.jit.worker.lock().unwrap() = worker.ok();
}

/// 结束本 Engine 的编译服务。已发布机器码继续有效；尚未发布的请求回到解释器
/// 兜底。每个 Engine 自己 join，不能让一个进程全局指针替最后启动者收尾。
pub fn stop(shared: &Shared) {
    shared.jit.stopping.store(true, Ordering::Release);
    shared.jit.queue.lock().unwrap().take();
    if let Some(worker) = shared.jit.worker.lock().unwrap().take() {
        let _ = worker.join();
    }
}

fn worker(shared: std::sync::Arc<Shared>, rx: Receiver<u32>) {
    let dbg = std::env::var_os("MIRVM_JIT_DEBUG").is_some();
    let mut c = Compiler::new(&shared);
    while let Ok(func) = rx.recv() {
        if shared.jit.stopping.load(Ordering::Acquire) {
            break;
        }
        if dbg {
            eprintln!(
                "mirvm-jit-debug: received compilation request for f{func} ({})",
                shared.module.funcs[func as usize].name
            );
        }
        c.compile(func);
        if dbg {
            let s = shared.jit.slots[func as usize].load(Ordering::Acquire);
            let ok = s != 0 && s != FAIL_SENTINEL;
            if ok {
                let addr = shared.jit.slots[func as usize].load(Ordering::Acquire);
                let fast = shared.jit.slots_fast[func as usize].load(Ordering::Acquire);
                eprintln!(
                    "mirvm-jit-debug: f{func} release={ok} @{addr:#x} fast@{fast:#x}({})",
                    shared.module.funcs[func as usize].name
                );
            } else {
                eprintln!("mirvm-jit-debug: f{func} release={ok}");
            }
        }
    }
    // JIT 代码一经发布就可能仍在进程级线程池的休眠栈上。退出收尾必须
    // join 编译线程，不能让它与 libc 清理并发；但也不能析构 JITModule
    // 并解除已发布代码映射。地址空间由随后的进程退出一次性回收。
    std::mem::forget(c);
}

// ===== 运行期助手（JIT 码经 import symbol 调回引擎）=====

/// c2i 万能壳：编译码调未编译 guest 函数（经蹦床打包）→ 回解释器。
/// ctx 恢复 = 边界 TLS attach（thunk 工厂同款，幂等）。
struct Compiler<'a> {
    shared: &'a Shared,
    module: JITModule,
    fbc: FunctionBuilderContext,
    c2i: ClifFuncId,
    call_main_catch: ClifFuncId,
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
    /// JIT cleanup pad 按实际接住的异常指针识别 EngineFault。
    exception_is_engine_fault: ClifFuncId,
    /// T1-d：Trap 占位助手（interp engine_abort 同文案同退出码）
    trap: ClifFuncId,
    /// T1-d：SIMD/宽 stmt 与 SIMD rvalue 三件的统一助手（interp simd_exec 共享本体）
    simd_stmt: ClifFuncId,
    simd_rv: ClifFuncId,
    /// Checks stack headroom before a compiled body allocates its frame.
    stack_guard: ClifFuncId,
    /// Deferred async-signal delivery at compiled block boundaries.
    poll_signals: ClifFuncId,
    /// 本批 (clif id, unwind info, try_call 函数的 LSDA 字节)——finalize 后统一注册
    pending_unwind: Vec<(ClifFuncId, UnwindInfo, Option<Vec<u8>>)>,
    #[cfg(test)]
    fail_after_symbol: Option<JitSymbolRole>,
}

struct PendingJitSymbol {
    id: ClifFuncId,
    func: u32,
    role: JitSymbolRole,
    size: u64,
}

impl<'a> Compiler<'a> {
    fn new(shared: &'a Shared) -> Self {
        // T3（M5.5）：MIRVM_JIT_STATS=1 时开启助手频度统计（进程级一次）
        stat_init();
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
        jb.symbol("mirvm_call_main_catch", mirvm_call_main_catch as *const u8);
        jb.symbol("mirvm_jit_unreachable", mirvm_jit_unreachable as *const u8);
        jb.symbol("mirvm_jit_div_zero", mirvm_jit_div_zero as *const u8);
        jb.symbol("mirvm_volatile_load", mirvm_volatile_load as *const u8);
        jb.symbol("mirvm_volatile_store", mirvm_volatile_store as *const u8);
        jb.symbol("mirvm_call_indirect", mirvm_call_indirect as *const u8);
        jb.symbol(
            "mirvm_jit_terminate_abort",
            mirvm_jit_terminate_abort as *const u8,
        );
        jb.symbol("mirvm_call_terminate", mirvm_call_terminate as *const u8);
        jb.symbol(
            "mirvm_exception_is_engine_fault",
            mirvm_exception_is_engine_fault as *const u8,
        );
        jb.symbol("_Unwind_Resume", _Unwind_Resume as *const u8);
        jb.symbol("mirvm_jit_trap", mirvm_jit_trap as *const u8);
        jb.symbol("mirvm_simd_stmt", mirvm_simd_stmt as *const u8);
        jb.symbol("mirvm_simd_rv", mirvm_simd_rv as *const u8);
        jb.symbol("mirvm_tls_ref", mirvm_tls_ref as *const u8);
        jb.symbol("mirvm_call_foreign", mirvm_call_foreign as *const u8);
        jb.symbol("mirvm_call_builtin", mirvm_call_builtin as *const u8);
        jb.symbol("mirvm_alloc", mirvm_alloc as *const u8);
        jb.symbol("mirvm_jit_stack_guard", mirvm_jit_stack_guard as *const u8);
        jb.symbol("mirvm_poll_signals", mirvm_poll_signals as *const u8);
        // M5.4b-3 助手注册表
        jb.symbol("mirvm_bin128_ovf", mirvm_bin128_ovf as *const u8);
        jb.symbol("mirvm_bin128_divrem", mirvm_bin128_divrem as *const u8);
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
        jb.symbol("mirvm_wide_to_f32", mirvm_wide_to_f32 as *const u8);
        jb.symbol("mirvm_wide_to_f64", mirvm_wide_to_f64 as *const u8);
        jb.symbol("mirvm_f16_bin", mirvm_f16_bin as *const u8);
        jb.symbol("mirvm_f16_cmp", mirvm_f16_cmp as *const u8);
        jb.symbol("mirvm_f16_neg", mirvm_f16_neg as *const u8);
        jb.symbol("mirvm_f16_cast", mirvm_f16_cast as *const u8);
        jb.symbol("mirvm_f16_to_int", mirvm_f16_to_int as *const u8);
        jb.symbol("mirvm_f16_from_int", mirvm_f16_from_int as *const u8);
        jb.symbol("mirvm_f16_math_un", mirvm_f16_math_un as *const u8);
        jb.symbol("mirvm_f16_math_bin", mirvm_f16_math_bin as *const u8);
        jb.symbol("mirvm_f16_fma", mirvm_f16_fma as *const u8);
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
        let call_main_catch = module
            .declare_function("mirvm_call_main_catch", Linkage::Import, &sig_c2i)
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
        // mirvm_call_foreign 八参：sp/sl/sg/ap/nv/ret_dst/fv + terminate 旗
        //（T1-c 加旗时漏改本签名，verifier 拒收致含 CallForeign 函数静默留解释）
        let mut sig_cf = module.make_signature();
        for _ in 0..8 {
            sig_cf.params.push(AbiParam::new(types::I64));
        }
        sig_cf.returns.push(AbiParam::new(types::I64));
        let call_foreign = module
            .declare_function("mirvm_call_foreign", Linkage::Import, &sig_cf)
            .unwrap();
        // T1-b CallBuiltin 助手（builtin 指针 + av 数组 + n + ret_dst + caller +
        // (lo,hi) 写出指针 + terminate 旗（T1-c）+ 调用职责）；分配系快路六参直返 u64
        let mut sig_cb = module.make_signature();
        for _ in 0..8 {
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
        let mut sig_efi = module.make_signature();
        sig_efi.params.push(AbiParam::new(types::I64));
        sig_efi.returns.push(AbiParam::new(types::I64));
        let exception_is_engine_fault = module
            .declare_function("mirvm_exception_is_engine_fault", Linkage::Import, &sig_efi)
            .unwrap();
        // T1-d：Trap 助手（reason 指针/长度 + func（u64::MAX = stmt 形））
        let mut sig_tr = module.make_signature();
        for _ in 0..3 {
            sig_tr.params.push(AbiParam::new(types::I64));
        }
        let trap = module
            .declare_function("mirvm_jit_trap", Linkage::Import, &sig_tr)
            .unwrap();
        // T1-d：SIMD/宽 stmt 统一助手（7 参 1 返）与 SIMD rvalue 三件助手
        // （2 参 1 返）——薄壳重匹配后调 interp simd_exec 共享本体
        let mut sig_ss = module.make_signature();
        for _ in 0..7 {
            sig_ss.params.push(AbiParam::new(types::I64));
        }
        sig_ss.returns.push(AbiParam::new(types::I64));
        let simd_stmt = module
            .declare_function("mirvm_simd_stmt", Linkage::Import, &sig_ss)
            .unwrap();
        let mut sig_sr = module.make_signature();
        for _ in 0..2 {
            sig_sr.params.push(AbiParam::new(types::I64));
        }
        sig_sr.returns.push(AbiParam::new(types::I64));
        let simd_rv = module
            .declare_function("mirvm_simd_rv", Linkage::Import, &sig_sr)
            .unwrap();
        let mut sig_sg = module.make_signature();
        sig_sg.params.push(AbiParam::new(types::I64));
        sig_sg.params.push(AbiParam::new(types::I64));
        let stack_guard = module
            .declare_function("mirvm_jit_stack_guard", Linkage::Import, &sig_sg)
            .unwrap();
        let poll_signals = module
            .declare_function(
                "mirvm_poll_signals",
                Linkage::Import,
                &module.make_signature(),
            )
            .unwrap();

        Compiler {
            shared,
            module,
            fbc: FunctionBuilderContext::new(),
            c2i,
            call_main_catch,
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
            exception_is_engine_fault,
            trap,
            simd_stmt,
            simd_rv,
            stack_guard,
            poll_signals,
            pending_unwind: Vec::new(),
            #[cfg(test)]
            fail_after_symbol: None,
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
            // strict 记录：不准入集合（设计上的留解释，非失败；MIRVM_JIT_DEBUG 门）
            if jit.sync && std::env::var_os("MIRVM_JIT_DEBUG").is_some() {
                eprintln!(
                    "mirvm-jit-strict: f{func} 不准入（{}）",
                    self.shared.module.funcs[func as usize].name
                );
            }
            return;
        }
        let abi = callee_abi(body).expect("admit 已验");

        // PLT 快路 callee 的槽预热：未编译者发 c2i 蹦床（fast 形状，调用点形状恒定）。
        // 形态不合的 callee 不在此列——其调用点直接 c2i（cold path）。
        let mut callees: Vec<(u32, CalleeAbi)> = Vec::new();
        for blk in &body.blocks {
            if let Terminator::Call {
                callee, args, ret, ..
            } = &blk.term
                && *callee != func
                && !callees.iter().any(|(c, _)| c == callee)
                && let Some(cabi) = callee_abi(&self.shared.module.funcs[*callee as usize])
                && cabi.nparams == args.len() + usize::from(matches!(ret, RetDest::Indirect(_)))
            {
                callees.push((*callee, cabi));
            }
        }
        for (c, cabi) in callees {
            if jit.slots_fast[c as usize].load(Ordering::Acquire) == 0
                && let Some((tramp, ranges)) = self.define_c2i_trampoline(c, cabi)
            {
                jit.publish_c2i_entry(c, tramp as u64, ranges);
            }
        }

        // 静默失败纪律（m5.3-design D4 / 防静默错值：编译失败 = 维持解释，绝不向
        // stderr 吐 panic——差分 oracle 的 stderr 逐字节比对会被线程 id 污染，实测抓获）；
        // MIRVM_JIT_SYNC 验证模式例外：可准入失败 = FAIL 哨兵响亮记（audit F-05）
        // TODO: 加入 log 系统之后应该向 log 系统输出错误
        let Some((fast_id, fast_symbol)) = self.define_fast(func, body, abi) else {
            self.strict_fail(func);
            return;
        };
        let mut symbols = vec![fast_symbol];
        #[cfg(test)]
        if self.fail_after_symbol == Some(JitSymbolRole::FastBody) {
            return;
        }
        let Some((guarded_id, guarded_symbol)) = self.define_guarded_fast(func, body, abi, fast_id)
        else {
            self.strict_fail(func);
            return;
        };
        symbols.push(guarded_symbol);
        #[cfg(test)]
        if self.fail_after_symbol == Some(JitSymbolRole::Guarded) {
            return;
        }
        let Some((packed_id, packed_symbol)) = self.define_packed(func, body, abi, guarded_id)
        else {
            self.strict_fail(func);
            return;
        };
        symbols.push(packed_symbol);
        #[cfg(test)]
        if self.fail_after_symbol == Some(JitSymbolRole::Packed) {
            return;
        }
        if self.module.finalize_definitions().is_err() {
            self.strict_fail(func);
            return;
        }
        self.register_pending_eh_frames();
        let ranges = self.finalized_symbol_ranges(symbols);

        let fast = self.module.get_finalized_function(guarded_id) as u64;
        let packed = self.module.get_finalized_function(packed_id) as u64;
        // 发布序：先 fast（自递归/他人调我）后 packed（interp 才可能进入编译码）
        // 所有内存范围在这两个 Release store 前完成；perf-map 只由显式 stop 写出。
        jit.publish_compiled_entries(func, fast, packed, ranges);
    }

    /// Published fast entry. Keeping the check in a separate slot-free
    /// function is important: putting it in `define_fast` would run only after
    /// Cranelift's prologue had already moved the native stack pointer.
    fn define_guarded_fast(
        &mut self,
        func: u32,
        body: &ir::FuncBody,
        abi: CalleeAbi,
        fast: ClifFuncId,
    ) -> Option<(ClifFuncId, PendingJitSymbol)> {
        let sig = self.fast_sig(abi);
        let id = self
            .module
            .declare_function(&format!("g{func}"), Linkage::Local, &sig)
            .ok()?;
        let mut cctx = self.module.make_context();
        cctx.func.signature = sig;
        {
            let mut b = FunctionBuilder::new(&mut cctx.func, &mut self.fbc);
            let entry = b.create_block();
            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            let params = b.block_params(entry).to_vec();
            let guard = self.module.declare_func_in_func(self.stack_guard, b.func);
            let fv = b.ins().iconst(types::I64, func as i64);
            let explicit = u64::from(body.frame_size)
                .saturating_add(u64::from(body.frame_align.saturating_sub(16)));
            let frame = b.ins().iconst(types::I64, explicit as i64);
            b.ins().call(guard, &[fv, frame]);
            let target = self.module.declare_func_in_func(fast, b.func);
            let call = b.ins().call(target, &params);
            let results = b.inst_results(call).to_vec();
            b.ins().return_(&results);
            b.seal_all_blocks();
            b.finalize();
        }
        if let Err(e) = self.module.define_function(id, &mut cctx) {
            if std::env::var_os("MIRVM_JIT_DEBUG").is_some() {
                eprintln!("mirvm-jit-debug: guarded entry define failed: {e:#?}");
            }
            return None;
        }
        if let Some(ui) = cctx
            .compiled_code()
            .and_then(|cc| cc.create_unwind_info(self.module.isa()).ok().flatten())
        {
            self.pending_unwind.push((id, ui, None));
        }
        let size = cctx.compiled_code()?.code_buffer().len() as u64;
        self.module.clear_context(&mut cctx);
        Some((
            id,
            PendingJitSymbol {
                id,
                func,
                role: JitSymbolRole::Guarded,
                size,
            },
        ))
    }

    /// strict 验证模式（MIRVM_JIT_SYNC，audit F-05）：可准入函数编译失败 =
    /// 响亮记 FAIL 哨兵（SYNC 等待方据此 abort）。非 strict 模式绝不调用
    /// 本路径——静默维持解释纪律不变。
    fn strict_fail(&self, func: u32) {
        let jit = &self.shared.jit;
        if jit.sync {
            eprintln!(
                "mirvm-jit-strict: f{func} ({}) meets compilation threshold but failed to be compiled",
                self.shared.module.funcs[func as usize].name
            );
            jit.slots[func as usize].store(FAIL_SENTINEL, Ordering::Release);
        }
    }

    /// c2i 蹦床：fast 签名，打包实参进栈上数组，调 mirvm_c2i 回解释器。
    /// 任何编译失败 = None（调用方跳过本槽预热，静默维持解释）。
    fn define_c2i_trampoline(
        &mut self,
        target: u32,
        abi: CalleeAbi,
    ) -> Option<(*const u8, Vec<JitSymbolRange>)> {
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
                eprintln!("mirvm-jit-debug: define_function failed: {e:#?}");
            }
            return None;
        }
        if let Some(ui) = cctx
            .compiled_code()
            .and_then(|cc| cc.create_unwind_info(self.module.isa()).ok().flatten())
        {
            self.pending_unwind.push((id, ui, None));
        }
        let size = cctx.compiled_code()?.code_buffer().len() as u64;
        let symbol = PendingJitSymbol {
            id,
            func: target,
            role: JitSymbolRole::C2i,
            size,
        };
        self.module.clear_context(&mut cctx);
        if self.module.finalize_definitions().is_err() {
            return None;
        }
        self.register_pending_eh_frames();
        let entry = self.module.get_finalized_function(id);
        let ranges = self.finalized_symbol_ranges(vec![symbol]);
        Some((entry, ranges))
    }

    /// fast 本体：字节码块 → CLIF；槽 → SSA 变量（I64 零扩到宽不变量）。
    /// 任何编译失败 = None（静默维持解释——绝不 panic 污染 stderr 差分）。
    fn define_fast(
        &mut self,
        func: u32,
        body: &ir::FuncBody,
        abi: CalleeAbi,
    ) -> Option<(ClifFuncId, PendingJitSymbol)> {
        let sig = self.fast_sig(abi);
        let id = self
            .module
            .declare_function(&format!("f{func}"), Linkage::Local, &sig)
            .unwrap();
        let mut cctx = self.module.make_context();
        cctx.func.signature = sig;
        // T1-c：has_try_call 由 Translator 在 build 期间置位（块外读以生成 LSDA）
        let has_try_call;
        {
            let mut b = FunctionBuilder::new(&mut cctx.func, &mut self.fbc);
            let frame_offs = analyze_frame(body);
            let frame_ss = if !frame_offs.needs_frame() {
                None
            } else {
                Some(b.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    // 0 字节强征档（force）以 1 字节物化；off 仍以 0 计，语义不变。
                    // frame_align > 16：cranelift x86_64 栈基只保证 16 对齐（无
                    // 动态重排机制），槽内补 (align-16) 字节余量，入口由翻译器
                    // 用 (addr+align-1)&-align 代码级对齐兜底（nano-gemm 的
                    // __m256d 局部经 mem::zeroed 的 32 字节 precondition 实证）
                    if body.frame_align > 16 {
                        body.frame_size + (body.frame_align - 16)
                    } else {
                        body.frame_size.max(1)
                    },
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
                call_main_catch: self.call_main_catch,
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
                exception_is_engine_fault: self.exception_is_engine_fault,
                trap: self.trap,
                simd_stmt: self.simd_stmt,
                simd_rv: self.simd_rv,
                poll_signals: self.poll_signals,
                exception_var: None,
                has_try_call: false,
                frame_base_var: None,
            };
            tr.build(func, body);
            has_try_call = tr.has_try_call;
            b.seal_all_blocks();
            b.finalize();
        }
        if let Err(e) = self.module.define_function(id, &mut cctx) {
            if std::env::var_os("MIRVM_JIT_DEBUG").is_some() {
                eprintln!("mirvm-jit-debug: define_function 失败: {e:#?}");
            }
            if std::env::var_os("MIRVM_JIT_DEBUG_DUMP").is_some() {
                eprintln!(
                    "mirvm-jit-debug: 失败函数 CLIF 转储 f{func}:\n{}",
                    cctx.func.display()
                );
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
        let size = cctx.compiled_code()?.code_buffer().len() as u64;
        self.module.clear_context(&mut cctx);
        Some((
            id,
            PendingJitSymbol {
                id,
                func,
                role: JitSymbolRole::FastBody,
                size,
            },
        ))
    }

    /// packed 入口：`(args: *const u64, ret: *mut u64)`——interp i2c 一跳。
    fn define_packed(
        &mut self,
        func: u32,
        body: &ir::FuncBody,
        abi: CalleeAbi,
        fast: ClifFuncId,
    ) -> Option<(ClifFuncId, PendingJitSymbol)> {
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
                eprintln!("mirvm-jit-debug: define_function failed: {e:#?}");
            }
            return None;
        }
        if let Some(ui) = cctx
            .compiled_code()
            .and_then(|cc| cc.create_unwind_info(self.module.isa()).ok().flatten())
        {
            self.pending_unwind.push((id, ui, None));
        }
        let size = cctx.compiled_code()?.code_buffer().len() as u64;
        self.module.clear_context(&mut cctx);
        Some((
            id,
            PendingJitSymbol {
                id,
                func,
                role: JitSymbolRole::Packed,
                size,
            },
        ))
    }

    /// spike5 管线：FrameTable → eh_frame 字节 → 整段注册。字节由注册入口保留到
    /// 进程结束，因为 unwinder 后续仍会读取其中共享的 CIE 和各函数的 FDE。
    fn register_pending_eh_frames(&mut self) {
        if self.pending_unwind.is_empty() {
            return;
        }
        use gimli::RunTimeEndian;
        use gimli::write::{Address, EhFrame, EndianVec, FrameTable};
        unsafe extern "C" {
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
        super::register_eh_frame_section(eh.0.into_vec());
    }

    fn finalized_symbol_ranges(&self, symbols: Vec<PendingJitSymbol>) -> Vec<JitSymbolRange> {
        symbols
            .into_iter()
            .map(|pending| {
                let guest_name = &self.shared.module.funcs[pending.func as usize].name;
                let start = self.module.get_finalized_function(pending.id) as u64;
                JitSymbolRange::new(
                    self.shared.id,
                    pending.func,
                    pending.role,
                    start,
                    pending.size,
                    guest_name,
                )
            })
            .collect()
    }
}

/// personality CIE 的 DW.ref 间接单元（单格全表共享；T1-c，lsda_probe 同款形态）。
static PERS_REF: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

// ===== LSDA 生成（lsda_probe 配方的产品化，版式逐行照抄勿创新）=====

/// 手工 GccExceptTable（cleanup-only，无 type_info；cg_clif 版式 + **全覆盖**）：
/// - 无 handler 的调用点：(ret_addr-1, len=1, lpad=0, action=0) —— 命中即
///   EHAction::None（rust find_eh_action 的 cs_lpad==0 分支）
/// - cleanup handler 调用点：(ret_addr-1, len=1, pad, action=0)
///   **rust 版 find_eh_action 对"ip 不在表中"返回 EHAction::Terminate（= _URC_FATAL），
///   与 libgcc 的 __gcc_personality_v0（no-entry = None）不同——call-site 表必须覆盖
///   函数内全部调用点**（cg_clif 对无 handler 站点同样发 lpad=0 项的原因）。
///   项按 buffer.call_sites() 序（= 指令序，满足 rust 解析器的有序表假设）。
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

#[cfg(test)]
mod tests {
    use super::*;

    fn body(name: &str, first: Terminator) -> ir::FuncBody {
        ir::FuncBody {
            frame_size: 8,
            frame_align: 8,
            ret: RetAbi::Zst,
            params: Vec::new(),
            caller_loc_off: None,
            blocks: vec![
                ir::Block {
                    stmts: Vec::new(),
                    term: first,
                },
                ir::Block {
                    stmts: Vec::new(),
                    term: Terminator::Return,
                },
            ],
            name: name.into(),
        }
    }

    #[test]
    fn real_compilation_registers_every_executable_role() {
        let caller = body(
            "profile_caller",
            Terminator::Call {
                callee: 1,
                args: Vec::new(),
                ret: RetDest::Ignore,
                target: 1,
                unwind: UnwindAction::Continue,
                role: ir::CallRole::Normal,
            },
        );
        let callee = body("profile_callee", Terminator::Return);
        let shared = Shared::new(ir::Module {
            funcs: vec![caller, callee].into(),
            ..ir::Module::default()
        });
        let mut compiler = Compiler::new(&shared);

        compiler.compile(0);

        assert_ne!(shared.jit.slots[0].load(Ordering::Acquire), 0);
        let ranges = shared.jit.symbol_ranges();
        assert_eq!(ranges.len(), 4);
        assert!(ranges.iter().any(|range| {
            range.func_id == 0 && range.role == JitSymbolRole::FastBody && range.size != 0
        }));
        assert!(ranges.iter().any(|range| {
            range.func_id == 0 && range.role == JitSymbolRole::Guarded && range.size != 0
        }));
        assert!(ranges.iter().any(|range| {
            range.func_id == 0 && range.role == JitSymbolRole::Packed && range.size != 0
        }));
        assert!(ranges.iter().any(|range| {
            range.func_id == 1 && range.role == JitSymbolRole::C2i && range.size != 0
        }));
        assert_eq!(
            ranges
                .iter()
                .filter(|range| shared.jit.guest_func_at(range.start) == Some(range.func_id))
                .count(),
            1,
            "only the fast body may become a MIRVM guest backtrace frame"
        );

        // Published code and registered unwind metadata have process lifetime
        // in production; keep that same lifetime in this direct compiler test.
        std::mem::forget(compiler);
    }

    #[test]
    fn failed_request_cannot_leak_symbols_into_the_next_compile_batch() {
        let shared = Shared::new(ir::Module {
            funcs: vec![
                body("failed_profile_target", Terminator::Return),
                body("successful_profile_target", Terminator::Return),
            ]
            .into(),
            ..ir::Module::default()
        });
        let mut compiler = Compiler::new(&shared);
        compiler.fail_after_symbol = Some(JitSymbolRole::FastBody);

        compiler.compile(0);

        assert_eq!(shared.jit.slots[0].load(Ordering::Acquire), 0);
        assert!(shared.jit.symbol_ranges().is_empty());

        compiler.fail_after_symbol = None;
        compiler.compile(1);

        assert_ne!(shared.jit.slots[1].load(Ordering::Acquire), 0);
        let ranges = shared.jit.symbol_ranges();
        assert_eq!(ranges.len(), 3);
        assert!(ranges.iter().all(|range| range.func_id == 1));
        assert!(
            ranges
                .iter()
                .any(|range| range.role == JitSymbolRole::FastBody)
        );
        assert!(
            ranges
                .iter()
                .any(|range| range.role == JitSymbolRole::Guarded)
        );
        assert!(
            ranges
                .iter()
                .any(|range| range.role == JitSymbolRole::Packed)
        );
        std::mem::forget(compiler);
    }
}

// ===== 函数体翻译 =====
