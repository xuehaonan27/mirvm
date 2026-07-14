//! 加载相：MIR → M4 引擎字节码的降低（rustc_private 域，tcx 关在这里）。
//!
//! 编排（M4.0 设计 §3 + D1 修正，debt-map §2-A）：mono 收集给**种子**（collector 是
//! codegen/链接视角，跨 crate 非泛型函数不收）→ **worklist 闭包扩集**（lower 遇到不在表
//! 里的 callee 就分配 FuncId 入队——解释视角没有"链接 libstd.so"可言，一切 MIR 自己降）
//! → 产出纯 Rust 的 `ir::Module`。
//!
//! **Trap-stub 全覆盖**：对全集 lowering 是全量的——不认识的构造绝不中止，
//! 就地降为 `Trap(诊断)`；只有被执行到的路径必须 trap-free（M4 增量协议）。

pub mod asm;
pub mod collect;
pub mod frame;
pub mod func;
pub mod global_asm;

use std::collections::VecDeque;

use rustc_data_structures::fx::FxHashMap;
use rustc_middle::mir::interpret::{AllocId, ConstAllocation, GlobalAlloc};
use rustc_middle::ty::{Instance, InstanceKind, TyCtxt, TypingEnv};
use rustc_span::Symbol;

use crate::vm::engine::frozen::FrozenArena;
use crate::vm::engine::ir;

/// 调用目标的解析结果（foreign 三路处置，debt-map §2-B）。
pub(crate) enum Callee {
    /// 普通 guest 函数（含链接仿真②解出的 std 实现、intrinsic fallback body 补收）
    Func(ir::FuncId),
    /// 引擎原语①（std runtime extern 边界：alloc/unwind 系 + stub）
    Builtin(ir::Builtin),
    /// os:: 直通③（dlsym+libffi）：固定参数 FfiKind 已冻结；变参尾由调用点实参补。
    /// thunk_args = fn-ptr 类型的参数位 + 其内层冻结签名（M4.4 D1 thunk 工厂）
    Foreign {
        sym: Box<str>,
        args: Vec<ir::FfiKind>,
        ret: ir::FfiKind,
        variadic: bool,
        thunk_args: Vec<(usize, ir::ForeignSig)>,
    },
}

/// 危险符号（P7 denylist）：绝不直通 native——会绕开进程/线程模型。
/// M4.4 D2：pthread_create/join/detach 已移出（真线程直通，fn-ptr 实参经 thunk 工厂）。
/// M4.5 D3：posix_spawn 系移出（子体立即 exec，VM 状态从不在子进程运行——与裸 fork
/// 带完整 VM 镜像着陆本质不同；file_actions/attr 是不透明指针，真实地址直传成立）。
/// 保留 pthread_exit（glibc 强制 unwind 绕过 FrameGuard）与裸 fork/exec/setjmp 系。
const DENY_EXACT: &[&str] = &[
    "fork",
    "vfork",
    "clone",
    "clone3",
    "setjmp",
    "longjmp",
    "sigsetjmp",
    "siglongjmp",
    "pthread_exit",
    "pthread_atfork",
];
const DENY_PREFIX: &[&str] = &["exec"];

/// 加载相"链接器"：FuncId 分配 + worklist 闭包扩集（D1 修正），外加 native 链接器
/// 职责的仿真——**特判的不是"panic 是什么"，是"链接器本来会做什么"**（debt-map §2-B）：
/// ① 引擎原语表（codegen allocator-shim 的同一符号清单）；
/// ② 导出符号解析（weak lang item：core 的 extern `panic_impl` → std 的 `rust_begin_unwind`）；
/// ③ 未知 foreign 暂 Trap（os:: 注册表 M4.3）。
pub(crate) struct Linker<'tcx> {
    tcx: TyCtxt<'tcx>,
    /// instance → FuncId（去重集；含已降与在队的）
    ids: FxHashMap<Instance<'tcx>, ir::FuncId>,
    /// 待降低队列（FuncId 已分配，体未产出）
    queue: VecDeque<(ir::FuncId, Instance<'tcx>)>,
    /// ①引擎原语表：mangled 符号 → Builtin
    builtins: FxHashMap<Symbol, ir::Builtin>,
    /// ②链接仿真：导出符号名 → (定义 instance, is_weak)（strong 覆盖 weak；惰性构建）
    exports: Option<FxHashMap<Symbol, (Instance<'tcx>, bool)>>,
    /// 冻结区（statics/常量池/fn 条目）——lower 期物化，结束移交 Module
    frozen: FrozenArena,
    /// 已物化的 alloc → 冻结区真地址（去重 + 先分后填破指针环）
    alloc_addrs: FxHashMap<AllocId, u64>,
    /// fn-ptr 条目：instance → 条目真地址（D4 每 instance 一个真地址身份）
    fn_entries: FxHashMap<Instance<'tcx>, u64>,
    /// 反查：条目真地址 → FuncId（间接调用派发用，移交 Module）
    fn_addrs: FxHashMap<u64, ir::FuncId>,
    /// guest TLS：`#[thread_local]` static → 稠密 TlsId + 槽表（M4.4 D3，移交 Module）
    tls_ids: FxHashMap<rustc_hir::def_id::DefId, ir::TlsId>,
    tls_slots: Vec<ir::TlsSlot>,
    /// asm-stub wrapper 文本（M5.0）：AsmStubId → GAS 源；lower 结束批量 cc+dlopen 物化
    asm_sites: Vec<String>,
}

impl<'tcx> Linker<'tcx> {
    fn new(tcx: TyCtxt<'tcx>) -> Self {
        Linker {
            tcx,
            ids: FxHashMap::default(),
            queue: VecDeque::new(),
            builtins: engine_builtins(tcx),
            exports: None,
            frozen: FrozenArena::new(),
            alloc_addrs: FxHashMap::default(),
            fn_entries: FxHashMap::default(),
            fn_addrs: FxHashMap::default(),
            tls_ids: FxHashMap::default(),
            tls_slots: Vec::new(),
            asm_sites: Vec::new(),
        }
    }

    /// 预留一个 asm-stub 槽（M5.0），返回其 AsmStubId；文本随后 set_asm_stub 回填。
    /// 分两步是因为 wrapper 名 `mirvm_asm_{id}` 要先于文本生成确定（自引用 .size 指令）。
    fn reserve_asm_stub(&mut self) -> ir::AsmStubId {
        let id = self.asm_sites.len() as ir::AsmStubId;
        self.asm_sites.push(String::new());
        id
    }
    fn set_asm_stub(&mut self, id: ir::AsmStubId, text: String) {
        self.asm_sites[id as usize] = text;
    }

    /// `#[thread_local]` static → 稠密 TlsId（M4.4 D3）。模板 = 初始化器求值产物
    /// 物化进冻结区（ensure_alloc 复用，重定位白拿——运行期只作字节源，无人写）。
    pub(crate) fn tls_id(&mut self, def_id: rustc_hir::def_id::DefId) -> Result<ir::TlsId, String> {
        if let Some(&id) = self.tls_ids.get(&def_id) {
            return Ok(id);
        }
        let alloc = self
            .tcx
            .eval_static_initializer(def_id)
            .map_err(|e| format!("TLS static 初始化器求值失败: {e:?}"))?;
        let (size, align) = (alloc.inner().size().bytes(), alloc.inner().align.bytes());
        let alloc_id = self.tcx.reserve_and_set_static_alloc(def_id);
        let template = self.ensure_alloc(alloc_id)?;
        let id = self.tls_slots.len() as ir::TlsId;
        self.tls_slots.push(ir::TlsSlot {
            template,
            size,
            align: align as u32,
        });
        self.tls_ids.insert(def_id, id);
        Ok(id)
    }

    /// fn-ptr 条目地址（D4）：每 instance 一个 16 对齐真地址；内容 = FuncId（调试用）。
    /// 比较/转型语义正确；间接调用经反查表派发（M4.1 第 5 步接 CallIndirect）。
    pub(crate) fn fn_entry_addr(&mut self, inst: Instance<'tcx>) -> u64 {
        if let Some(&a) = self.fn_entries.get(&inst) {
            return a;
        }
        let fid = self.func_id(inst);
        let addr = self.frozen.alloc(8, 16);
        unsafe { (addr as *mut u64).write(fid as u64) };
        self.fn_entries.insert(inst, addr);
        self.fn_addrs.insert(addr, fid);
        addr
    }

    /// 裸字节物化进冻结区（128 位常量等小常量的通用道）。
    pub(crate) fn frozen_alloc_bytes(&mut self, bytes: &[u8]) -> u64 {
        let p = self.frozen.alloc(bytes.len() as u64, 16);
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), p as *mut u8, bytes.len()) };
        p
    }

    /// alloc → 冻结区真地址（按需递归物化；先分后填 ⇒ 指针环安全）。
    pub(crate) fn ensure_alloc(&mut self, id: AllocId) -> Result<u64, String> {
        if let Some(&a) = self.alloc_addrs.get(&id) {
            return Ok(a);
        }
        match self.tcx.global_alloc(id) {
            GlobalAlloc::Memory(alloc) => self.materialize(id, alloc),
            GlobalAlloc::Static(def_id) => {
                // extern static = 真符号（os:: 直通）：
                // - weak（gettid 等 fn 符号判空模式）：判空 cell 写 0（缺席）——weak
                //   **fn** 符号即便存在也不能给真地址（guest 拿去调用 = 跳 native 代码，
                //   条目反查失败；M4.4 thunk 前统一走 fallback 路径）
                // - 非 weak（environ 等数据符号）：alloc 基址 = dlsym 真地址
                if self.tcx.is_foreign_item(def_id) {
                    let name = self.tcx.item_name(def_id);
                    // extern block 内 item 的 linkage 在 import_linkage 字段
                    let weak = self.tcx.codegen_fn_attrs(def_id).import_linkage
                        == Some(rustc_hir::attrs::Linkage::ExternalWeak);
                    if weak {
                        let cell = self.frozen.alloc(8, 8); // 清零 cell = 符号缺席
                        self.alloc_addrs.insert(id, cell);
                        return Ok(cell);
                    }
                    let cname = std::ffi::CString::new(name.as_str())
                        .map_err(|_| "符号名含 NUL".to_string())?;
                    let p = unsafe { libc::dlsym(std::ptr::null_mut(), cname.as_ptr()) } as u64;
                    if p == 0 {
                        return Err(format!("extern static `{name}` dlsym 未命中"));
                    }
                    self.alloc_addrs.insert(id, p);
                    return Ok(p);
                }
                // static 的字节 = 初始化器求值产物；可写（static mut/内部可变性）
                let alloc = self
                    .tcx
                    .eval_static_initializer(def_id)
                    .map_err(|e| format!("static 初始化器求值失败: {e:?}"))?;
                self.materialize(id, alloc)
            }
            GlobalAlloc::Function { instance } => {
                let addr = self.fn_entry_addr(instance);
                self.alloc_addrs.insert(id, addr);
                Ok(addr)
            }
            GlobalAlloc::VTable(ty, dyn_ty) => {
                // 现成的 vtable 分配（F5）——递归走 Memory 路径（含 fn 条目重定位）
                let principal = dyn_ty
                    .principal()
                    .map(|b| self.tcx.instantiate_bound_regions_with_erased(b));
                let vt_id = self.tcx.vtable_allocation((ty, principal));
                let addr = self.ensure_alloc(vt_id)?;
                self.alloc_addrs.insert(id, addr);
                Ok(addr)
            }
            GlobalAlloc::TypeId { .. } => {
                // TypeId"分配"：基址 0——重定位 base+addend 后值 = 128 位类型哈希的
                // 指针宽片段本身（tier-0 resolve_addr/Miri 同款）
                self.alloc_addrs.insert(id, 0);
                Ok(0)
            }
        }
    }

    /// 物化一个内存分配：分地址 → 拷字节 → 重定位（provenance 表逐项写真地址+addend）。
    fn materialize(&mut self, id: AllocId, alloc: ConstAllocation<'tcx>) -> Result<u64, String> {
        let a = alloc.inner();
        let size = a.size().bytes();
        let align = a.align.bytes();
        let base = self.frozen.alloc(size, align);
        self.alloc_addrs.insert(id, base); // 先分后填（环安全）
        let bytes = a.inspect_with_uninit_and_ptr_outside_interpreter(0..size as usize);
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), base as *mut u8, size as usize) };
        // 重定位：ptr 位置存的 8 字节 = 目标内偏移（addend）→ 换成目标真地址 + addend
        for (off, prov) in a.provenance().ptrs().iter() {
            let target = self.ensure_alloc(prov.alloc_id())?;
            let at = (base + off.bytes()) as *mut u64;
            unsafe {
                let addend = at.read_unaligned();
                at.write_unaligned(target.wrapping_add(addend));
            }
        }
        Ok(base)
    }

    /// instance → FuncId；首见分配 id 并入待降低队列（worklist 扩集的入口）。
    pub(crate) fn func_id(&mut self, inst: Instance<'tcx>) -> ir::FuncId {
        let next = self.ids.len() as ir::FuncId;
        match self.ids.entry(inst) {
            std::collections::hash_map::Entry::Occupied(e) => *e.get(),
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(next);
                self.queue.push_back((next, inst));
                next
            }
        }
    }

    /// 调用点的 callee 解析（Call 终止子用）。Err = 该块 Trap（带分期诊断）。
    pub(crate) fn resolve_call(&mut self, inst: Instance<'tcx>) -> Result<Callee, String> {
        // intrinsic（D5）：fallback body 按普通函数补收（collector 因 backend
        // replaced_intrinsics 跳过收集，解释视角必须自己收——构造同 collector 源码：
        // Instance::new_raw）；must_be_overridden 的等引擎内建表（M4.1 第 5 步）。
        if let InstanceKind::Intrinsic(def_id) = inst.def {
            let intrinsic = self
                .tcx
                .intrinsic(def_id)
                .expect("InstanceKind::Intrinsic 必有 IntrinsicDef");
            if intrinsic.must_be_overridden {
                return Err(format!(
                    "intrinsic `{}` 无 fallback（引擎内建表，M4.1）",
                    intrinsic.name
                ));
            }
            let item = Instance::new_raw(def_id, inst.args);
            return Ok(Callee::Func(self.func_id(item)));
        }
        if let InstanceKind::Virtual(..) = inst.def {
            return Err("dyn 虚调用派发（M4.1+）".into());
        }
        if self.tcx.is_foreign_item(inst.def_id()) {
            let link_name = Symbol::intern(self.tcx.symbol_name(inst).name);
            // ①引擎原语（alloc/unwind/stub/快路径直通）
            if let Some(&b) = self.builtins.get(&link_name) {
                return Ok(Callee::Builtin(b));
            }
            // ②链接仿真：按符号名在已链接 crate 的导出定义里找（tier-0
            // find_exported_symbol 同构；panic_impl→rust_begin_unwind、__rdl_* 走此路）
            let target = self.exported_defs().get(&link_name).copied();
            if let Some((target, is_weak)) = target {
                // native 链接器语义：weak 定义让位于动态库强符号（compiler-builtins
                // 的 weak sqrt/memcmp vs libc/libm）。Rust 内部 ABI 符号（__rust/
                // __rdl/rust_ 前缀）除外——宿主进程（librustc_driver）也导出它们，
                // 直通会打穿引擎的堆/panic 模型。
                let name = link_name.as_str();
                let rust_internal = name.starts_with("__rust")
                    || name.starts_with("__rdl")
                    || name.starts_with("rust_");
                if is_weak && !rust_internal {
                    let cname = std::ffi::CString::new(name).unwrap();
                    let strong = unsafe { libc::dlsym(std::ptr::null_mut(), cname.as_ptr()) };
                    if !strong.is_null() {
                        return self.freeze_foreign_sig(inst, name);
                    }
                }
                return Ok(Callee::Func(self.func_id(target)));
            }
            // ③os:: 直通（P7）：denylist 拒 → 其余 dlsym+libffi 按冻结签名直调
            let name = link_name.as_str();
            if DENY_EXACT.contains(&name) || DENY_PREFIX.iter().any(|p| name.starts_with(p)) {
                return Err(format!(
                    "foreign `{name}`（denylist：线程 M4.4 / 进程模型不直通）"
                ));
            }
            if name.starts_with("llvm.") {
                return Err(format!("foreign `{name}`（LLVM 内部符号，按需内建）"));
            }
            return self.freeze_foreign_sig(inst, name);
        }
        // naked fn（D8h）：函数体是裸机器码，无常规 MIR body。物化进 global-asm
        // `.so`（收集阶段已做），调用点按真 ABI 走 foreign 直调其 mangled 符号。
        if self
            .tcx
            .codegen_fn_attrs(inst.def_id())
            .flags
            .contains(rustc_middle::middle::codegen_fn_attrs::CodegenFnAttrFlags::NAKED)
        {
            let name = self.tcx.symbol_name(inst).name;
            return self.freeze_foreign_sig(inst, name);
        }
        // 普通函数：worklist 闭包扩集（跨 crate 非泛型函数不在 collector 种子集）
        Ok(Callee::Func(self.func_id(inst)))
    }

    /// os:: 直通签名冻结：foreign fn sig → FfiKind 列表（tier-0 ty_to_ffitype 同构）。
    /// fn-ptr 类型的参数（pthread_create 的 thread_start 等）额外冻结**内层签名**
    /// （M4.4 D1）：执行期该位若收到 fn 条目地址，thunk 工厂物化真机器码后再直传。
    fn freeze_foreign_sig(&mut self, inst: Instance<'tcx>, name: &str) -> Result<Callee, String> {
        let sig = self
            .tcx
            .fn_sig(inst.def_id())
            .instantiate(self.tcx, inst.args)
            .skip_binder();
        let env = TypingEnv::fully_monomorphized();
        let mut args = Vec::with_capacity(sig.inputs().len());
        let mut thunk_args = Vec::new();
        for (i, &t) in sig.inputs().iter().enumerate() {
            args.push(ffi_kind_of(self.tcx, env, t).map_err(|e| {
                format!("foreign `{name}` 参数 {t}: {e}（libffi 直通仅标量/指针）")
            })?);
            // fn ptr 参数位：裸 fn ptr + `Option<fn>`（可空回调——pthread_key_create 的
            // dtor 等；niche 布局下 None=0 原样直传）。内层签名不可冻结 = 整调用点
            // Trap（防静默错值：条目地址直传给 native 是静默崩溃）。
            let fnptr_ty = if t.is_fn_ptr() {
                Some(t)
            } else if let rustc_middle::ty::TyKind::Adt(def, sub) = t.kind()
                && self
                    .tcx
                    .is_diagnostic_item(rustc_span::sym::Option, def.did())
                && sub.type_at(0).is_fn_ptr()
            {
                Some(sub.type_at(0))
            } else {
                None
            };
            if let Some(t) = fnptr_ty {
                let inner = t.fn_sig(self.tcx).skip_binder();
                if inner.c_variadic() {
                    return Err(format!("foreign `{name}` 参数 {t}: 变参回调不支持 thunk"));
                }
                let mut in_args = Vec::with_capacity(inner.inputs().len());
                for &it in inner.inputs() {
                    let k = ffi_kind_of(self.tcx, env, it).map_err(|e| {
                        format!("foreign `{name}` 回调参数 {it}: {e}（thunk 仅标量/指针）")
                    })?;
                    if k == ir::FfiKind::Void {
                        return Err(format!(
                            "foreign `{name}` 回调参数 {it}: ZST 不可作 cif 参数"
                        ));
                    }
                    in_args.push(k);
                }
                let in_ret = ffi_kind_of(self.tcx, env, inner.output())
                    .map_err(|e| format!("foreign `{name}` 回调返回 {}: {e}", inner.output()))?;
                thunk_args.push((
                    i,
                    ir::ForeignSig {
                        args: in_args,
                        ret: in_ret,
                        fixed: None,
                        thunk_args: vec![],
                    },
                ));
            }
        }
        let ret = ffi_kind_of(self.tcx, env, sig.output())
            .map_err(|e| format!("foreign `{name}` 返回 {}: {e}", sig.output()))?;
        Ok(Callee::Foreign {
            sym: name.into(),
            args,
            ret,
            variadic: sig.c_variadic(),
            thunk_args,
        })
    }

    /// 导出符号表（②），惰性一次构建：遍历"最终二进制会链接到"的全部非泛型导出 def
    /// （tier-0 `for_each_linked_def` 同构），符号名 → mono instance，strong 覆盖 weak。
    fn exported_defs(&mut self) -> &FxHashMap<Symbol, (Instance<'tcx>, bool)> {
        let tcx = self.tcx;
        self.exports.get_or_insert_with(|| {
            use rustc_hir::def_id::LOCAL_CRATE;
            use rustc_middle::middle::codegen_fn_attrs::CodegenFnAttrFlags;
            use rustc_middle::middle::exported_symbols::ExportedSymbol;
            use rustc_session::config::CrateType;

            // (instance, is_weak)：非 weak 覆盖 weak
            let mut map: FxHashMap<Symbol, (Instance<'tcx>, bool)> = FxHashMap::default();
            let mut add = |def_id: rustc_hir::def_id::DefId| {
                if tcx.is_foreign_item(def_id)
                    || !matches!(tcx.def_kind(def_id), rustc_hir::def::DefKind::Fn)
                {
                    return;
                }
                let inst = Instance::mono(tcx, def_id);
                let name = Symbol::intern(tcx.symbol_name(inst).name);
                let is_weak = tcx.codegen_fn_attrs(def_id).linkage
                    == Some(rustc_hir::attrs::Linkage::WeakAny);
                match map.entry(name) {
                    std::collections::hash_map::Entry::Occupied(mut e) => {
                        if e.get().1 && !is_weak {
                            e.insert((inst, false));
                        }
                    }
                    std::collections::hash_map::Entry::Vacant(e) => {
                        e.insert((inst, is_weak));
                    }
                }
            };

            // 本地 crate：遍历 HIR（exported_symbols 会漏 #[used]）
            for def_id in tcx.hir_crate_items(()).definitions() {
                if !tcx.def_kind(def_id).has_codegen_attrs() {
                    continue;
                }
                let attrs = tcx.codegen_fn_attrs(def_id);
                let exported = attrs.contains_extern_indicator()
                    || attrs.flags.contains(CodegenFnAttrFlags::USED_COMPILER)
                    || attrs.flags.contains(CodegenFnAttrFlags::USED_LINKER);
                if !exported || tcx.generics_of(def_id).requires_monomorphization(tcx) {
                    continue;
                }
                add(def_id.into());
            }
            // 依赖 crate 的非泛型导出符号
            let dependency_formats = tcx.dependency_formats(());
            if let Some(format) = dependency_formats.get(&CrateType::Executable) {
                for (cnum, &linkage) in format.iter_enumerated() {
                    if cnum == LOCAL_CRATE
                        || linkage == rustc_middle::middle::dependency_format::Linkage::NotLinked
                    {
                        continue;
                    }
                    for &(symbol, _) in tcx.exported_non_generic_symbols(cnum) {
                        if let ExportedSymbol::NonGeneric(def_id) = symbol {
                            add(def_id);
                        }
                    }
                }
            }
            map
        })
    }
}

/// extern "C" 系 fn-ptr 类型 → 冻结 ForeignSig（M4.4 FFI 反方向之二：调用点带上，
/// 执行期条目反查未命中 = guest 持 native 真码 → libffi 按此直调）。
/// None = Rust ABI / 变参 / 参数不可类——该调用点只能派发 guest 条目（未命中即诊断）。
pub(crate) fn freeze_c_fnptr_sig<'tcx>(
    tcx: TyCtxt<'tcx>,
    env: TypingEnv<'tcx>,
    ty: rustc_middle::ty::Ty<'tcx>,
) -> Option<ir::ForeignSig> {
    use rustc_abi::ExternAbi;
    let sig = ty.fn_sig(tcx).skip_binder();
    if !matches!(sig.abi(), ExternAbi::C { .. } | ExternAbi::System { .. }) || sig.c_variadic() {
        return None;
    }
    let mut args = Vec::with_capacity(sig.inputs().len());
    for &t in sig.inputs() {
        let k = ffi_kind_of(tcx, env, t).ok()?;
        if k == ir::FfiKind::Void {
            return None; // ZST 不可作 cif 参数
        }
        args.push(k);
    }
    let ret = ffi_kind_of(tcx, env, sig.output()).ok()?;
    Some(ir::ForeignSig {
        args,
        ret,
        fixed: None,
        thunk_args: vec![],
    })
}

/// 类型 → libffi 直通类别（标量与指针；ZST=Void 仅返回位；聚合不支持）。
pub(crate) fn ffi_kind_of<'tcx>(
    tcx: TyCtxt<'tcx>,
    env: TypingEnv<'tcx>,
    ty: rustc_middle::ty::Ty<'tcx>,
) -> Result<ir::FfiKind, String> {
    use rustc_abi::{BackendRepr, Float, Integer, Primitive};
    let layout = tcx
        .layout_of(env.as_query_input(ty))
        .map_err(|e| format!("layout 失败: {e}"))?;
    if layout.is_zst() {
        return Ok(ir::FfiKind::Void);
    }
    if let BackendRepr::Scalar(s) = layout.backend_repr {
        return Ok(match s.primitive() {
            Primitive::Int(Integer::I8, true) => ir::FfiKind::I8,
            Primitive::Int(Integer::I16, true) => ir::FfiKind::I16,
            Primitive::Int(Integer::I32, true) => ir::FfiKind::I32,
            Primitive::Int(Integer::I64, true) => ir::FfiKind::I64,
            Primitive::Int(Integer::I8, false) => ir::FfiKind::U8,
            Primitive::Int(Integer::I16, false) => ir::FfiKind::U16,
            Primitive::Int(Integer::I32, false) => ir::FfiKind::U32,
            Primitive::Int(Integer::I64, false) => ir::FfiKind::U64,
            Primitive::Float(Float::F32) => ir::FfiKind::F32,
            Primitive::Float(Float::F64) => ir::FfiKind::F64,
            Primitive::Pointer(_) => ir::FfiKind::Ptr,
            other => return Err(format!("标量 {other:?} 不支持")),
        });
    }
    Err("非标量（按值聚合）".into())
}

/// ①引擎原语表：codegen 会为 allocator shim 生成的符号清单（tier-0/Miri 同款来源，
/// 符号是 mangled 的——`mangle_internal_symbol`）。Special = 默认分配器（引擎接管）；
/// 非 special（自定义 #[global_allocator] 的 __rust_* → 用户 __rg_* 转发、
/// __rust_alloc_error_handler）暂不注册 → 走 ③ Trap 诊断。
fn engine_builtins(tcx: TyCtxt<'_>) -> FxHashMap<Symbol, ir::Builtin> {
    use rustc_ast::expand::allocator::{self, SpecialAllocatorMethod as S};
    use rustc_symbol_mangling::mangle_internal_symbol;

    let mut out = FxHashMap::default();
    if let Some(kind) = tcx.allocator_kind(()) {
        for method in rustc_codegen_ssa::base::allocator_shim_contents(tcx, kind) {
            let Some(special) = method.special else {
                continue;
            };
            let b = match special {
                S::Alloc => ir::Builtin::RustAlloc,
                S::Dealloc => ir::Builtin::RustDealloc,
                S::Realloc => ir::Builtin::RustRealloc,
                S::AllocZeroed => ir::Builtin::RustAllocZeroed,
            };
            let sym = mangle_internal_symbol(tcx, &allocator::global_fn_name(method.name));
            out.insert(Symbol::intern(&sym), b);
        }
    }
    let sentinel =
        mangle_internal_symbol(tcx, rustc_ast::expand::allocator::NO_ALLOC_SHIM_IS_UNSTABLE);
    out.insert(Symbol::intern(&sentinel), ir::Builtin::NoAllocShim);
    // unwind 原语（M4.2）：panic_unwind 照常解释，引擎在平台 unwinder 符号层接管
    out.insert(
        Symbol::intern("_Unwind_RaiseException"),
        ir::Builtin::UnwindRaise,
    );
    // 快路径直通（panic 链高频；其余 foreign 走通用 dlsym+libffi 道）
    out.insert(Symbol::intern("getenv"), ir::Builtin::HostGetenv);
    out.insert(Symbol::intern("write"), ir::Builtin::HostWrite);
    out.insert(Symbol::intern("strlen"), ir::Builtin::HostStrlen);
    out.insert(Symbol::intern("abort"), ir::Builtin::HostAbort);
    // atexit 家族（D8g）：glibc 不导出 `atexit` 供 guest dlsym → builtin 接管。
    out.insert(Symbol::intern("atexit"), ir::Builtin::HostAtexit);
    out.insert(Symbol::intern("__cxa_atexit"), ir::Builtin::HostCxaAtexit);
    out.insert(Symbol::intern("on_exit"), ir::Builtin::HostOnExit);
    out.insert(Symbol::intern("syscall"), ir::Builtin::HostSyscall);
    // signal/sigaction 的 handler 藏在整数/结构体中，不能由通用 FFI fn-ptr 参数
    // thunk 化；而且 signal trampoline 必须异步信号安全，普通 libffi closure 不满足。
    // 明确 Trap，直到有专用实现。其余旧 StubZero 项改走 dlsym+libffi；
    // atexit/dl_iterate_phdr 的显式 fn-ptr 参数可由 M4.4 thunk 工厂处理。
    out.insert(Symbol::intern("signal"), ir::Builtin::HostSignal);
    out.insert(Symbol::intern("sigaction"), ir::Builtin::HostSigaction);
    // 宿主 unwinder 从 libffi/解释器的 native stack 取回 IP，无法代表
    // guest 的冻结函数条目。回调 thunk 只解决调用方向，不会翻译栈帧；
    // 所以在 guest-frame/IP 映射完成前必须明确拒绝，不能返回貌似成功
    // 的宿主 backtrace。
    // `_Unwind_RaiseException` / `_Unwind_DeleteException` 上面有 guest 专用语义；
    // 其余 libgcc context/stack API 若直通，看到的只会是宿主解释器帧。
    // 整组显式 deny，避免从 GetIPInfo/CFA/LSDA 等旁路重新引入静默错值。
    for name in [
        "_Unwind_Backtrace",
        "_Unwind_FindEnclosingFunction",
        "_Unwind_Find_FDE",
        "_Unwind_ForcedUnwind",
        "_Unwind_GetCFA",
        "_Unwind_GetDataRelBase",
        "_Unwind_GetGR",
        "_Unwind_GetIP",
        "_Unwind_GetIPInfo",
        "_Unwind_GetLanguageSpecificData",
        "_Unwind_GetRegionStart",
        "_Unwind_GetTextRelBase",
        "_Unwind_Resume",
        "_Unwind_Resume_or_Rethrow",
        "_Unwind_SetGR",
        "_Unwind_SetIP",
    ] {
        out.insert(Symbol::intern(name), ir::Builtin::Unsupported(name));
    }
    out.insert(
        Symbol::intern("_Unwind_DeleteException"),
        ir::Builtin::UnwindDeleteException,
    );
    out.insert(
        Symbol::intern("llvm.x86.sse2.pause"),
        ir::Builtin::CpuHintNop,
    );
    out.insert(
        Symbol::intern("llvm.x86.avx.vzeroupper"),
        ir::Builtin::CpuHintNop,
    );
    out.insert(
        Symbol::intern("llvm.x86.addcarry.64"),
        ir::Builtin::AddCarry64,
    );
    out.insert(
        Symbol::intern("llvm.x86.subborrow.64"),
        ir::Builtin::SubBorrow64,
    );
    out.insert(Symbol::intern("llvm.x86.xgetbv"), ir::Builtin::Xgetbv);
    out.insert(
        Symbol::intern("llvm.x86.ssse3.pshuf.b.128"),
        ir::Builtin::X86Pshufb128,
    );
    out.insert(
        Symbol::intern("llvm.x86.avx2.pshuf.b"),
        ir::Builtin::X86Pshufb256,
    );
    out.insert(
        Symbol::intern("llvm.x86.sha256msg1"),
        ir::Builtin::X86Sha256Msg1,
    );
    out.insert(
        Symbol::intern("llvm.x86.sha256msg2"),
        ir::Builtin::X86Sha256Msg2,
    );
    out.insert(
        Symbol::intern("llvm.x86.sha256rnds2"),
        ir::Builtin::X86Sha256Rnds2,
    );
    out
}

/// 整程序降低：种子收集 → worklist 闭包降低 → exports 表 + main 启动计划。
/// `argv` = guest 进程实参（argv[0]=脚本路径；布进冻结区的 C 串表）。
pub fn lower_program(tcx: TyCtxt<'_>, argv: &[String]) -> ir::Module {
    let typing_env = TypingEnv::fully_monomorphized();
    let mut linker = Linker::new(tcx);

    // 种子 = mono collector 集（D1：与 native codegen 同一起点，正确性白拿）
    for inst in collect::collect(tcx) {
        linker.func_id(inst);
    }

    // main 启动计划（cg_ssa create_entry_fn 同构）：
    // lang_start::<main_ret>(main fn-ptr, argc, argv, sigpipe) -> isize
    let entry = tcx.entry_fn(()).map(|(main_def, entry_ty)| {
        let rustc_session::config::EntryFnType::Main { sigpipe } = entry_ty;
        let main_inst = Instance::mono(tcx, main_def);
        let main_addr = linker.fn_entry_addr(main_inst);
        let main_ret = tcx
            .fn_sig(main_def)
            .no_bound_vars()
            .expect("main 无晚绑定区域")
            .output()
            .no_bound_vars()
            .expect("main 返回无晚绑定");
        let start_def = tcx.require_lang_item(rustc_hir::LangItem::Start, rustc_span::DUMMY_SP);
        let start_inst = Instance::expect_resolve(
            tcx,
            typing_env,
            start_def,
            tcx.mk_args(&[main_ret.into()]),
            rustc_span::DUMMY_SP,
        );
        let lang_start = linker.func_id(start_inst);
        // argv C 串表布进冻结区（tier-0 setup_process_memory 同构）
        let mut ptrs: Vec<u64> = Vec::with_capacity(argv.len());
        for a in argv {
            let bytes = a.as_bytes();
            let p = linker.frozen.alloc(bytes.len() as u64 + 1, 1);
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), p as *mut u8, bytes.len());
                *((p + bytes.len() as u64) as *mut u8) = 0;
            }
            ptrs.push(p);
        }
        let table = linker.frozen.alloc((ptrs.len() as u64 + 1) * 8, 8);
        for (i, &p) in ptrs.iter().enumerate() {
            unsafe { *((table + i as u64 * 8) as *mut u64) = p };
        }
        // 尾 NULL 由清零保证
        ir::EntryPlan {
            lang_start,
            main_addr,
            argc: argv.len() as u64,
            argv_ptr: table,
            sigpipe,
        }
    });

    let mut module = ir::Module::default();
    let mut funcs: Vec<Option<ir::FuncBody>> = Vec::new();
    while let Some((id, inst)) = linker.queue.pop_front() {
        let sym = tcx.symbol_name(inst).name.to_owned();
        let body = func::lower_instance(tcx, typing_env, inst, &mut linker)
            .unwrap_or_else(|reason| func::trap_body(&sym, &reason));
        if funcs.len() <= id as usize {
            funcs.resize_with(id as usize + 1, || None);
        }
        funcs[id as usize] = Some(body);
        module.exports.insert(sym.into_boxed_str(), id);
    }
    module.funcs = funcs
        .into_iter()
        .map(|f| f.expect("队列耗尽时每个 FuncId 必有产出"))
        .collect();

    // 入口别名（--vm-stats 从程序入口做可达分析用）
    if let Some((entry_def, _)) = tcx.entry_fn(())
        && let Some(&id) = linker.ids.get(&Instance::mono(tcx, entry_def))
    {
        module.exports.insert("@entry".into(), id);
    }
    // `-l` 链接指令 → dlopen 候选路径（tier-0 ensure_libs_loaded 同构）
    let sess = tcx.sess;
    for lib in &sess.opts.libs {
        // Static 只能走下方经过验证的必需 archive 路径；不能同时伪装成可选 `.so`
        // 候选，否则所需 archive dlopen 失败时可能静默命中系统同名库。
        if matches!(lib.kind, rustc_hir::attrs::NativeLibKind::Static { .. }) {
            continue;
        }
        let name = lib.name.as_str();
        for d in sess.opts.search_paths.iter().map(|sp| &sp.dir) {
            module
                .native_libs
                .push(d.join(format!("lib{name}.so")).display().to_string().into());
        }
        module.native_libs.push(format!("lib{name}.so").into());
        module.native_libs.push(format!("lib{name}.so.1").into());
    }
    // 上游 crate build.rs 的 Static native libraries（M5.1 D2）：从 rustc metadata +
    // native search paths 找到真实 `.a`，受约束地转换为内容寻址 `.so`。转换失败必须
    // 在加载相响亮终止；不能让执行相 dlsym 静默跳过后再伪装成普通符号缺失。
    module.required_native_libs.extend(
        crate::native_archive::materialize_static_libraries(tcx)
            .unwrap_or_else(|reason| panic!("Static native library 装载失败: {reason}")),
    );
    // global_asm! + naked fn 物化（M5.2 D8h）：模块级/函数级 asm → `.so` → required lib
    //（guest 引用的符号在任何 dlsym 前 RTLD_NOW 就位）。失败响亮终止。
    if let Some(so) = global_asm::materialize(tcx)
        .unwrap_or_else(|reason| panic!("global_asm/naked 物化失败: {reason}"))
    {
        module.required_native_libs.push(so);
    }
    // asm-stub 批量物化（M5.0）：全部 wrapper cc 汇编 + dlopen + dlsym → 真地址表
    module.asm_stub_addrs = asm::materialize(&linker.asm_sites);
    // 冻结区与 fn 条目反查表移交执行相
    module.frozen = Some(linker.frozen);
    module.fn_addrs = linker.fn_addrs.into_iter().collect();
    module.tls = linker.tls_slots;
    module.entry = entry;
    module
}
