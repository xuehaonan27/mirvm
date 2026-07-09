//! 加载相：MIR → M4 引擎字节码的降低（rustc_private 域，tcx 关在这里）。
//!
//! 编排（M4.0 设计 §3 + D1 修正，debt-map §2-A）：mono 收集给**种子**（collector 是
//! codegen/链接视角，跨 crate 非泛型函数不收）→ **worklist 闭包扩集**（lower 遇到不在表
//! 里的 callee 就分配 FuncId 入队——解释视角没有"链接 libstd.so"可言，一切 MIR 自己降）
//! → 产出纯 Rust 的 `ir::Module`。
//!
//! **Trap-stub 全覆盖**：对全集 lowering 是全量的——不认识的构造绝不中止，
//! 就地降为 `Trap(诊断)`；只有被执行到的路径必须 trap-free（M4 增量协议）。

pub mod collect;
pub mod frame;
pub mod func;

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
    /// 引擎原语①（std runtime extern 边界：alloc 系等）
    Builtin(ir::Builtin),
}

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
    /// ②链接仿真：导出符号名 → 定义 instance（strong 覆盖 weak；惰性一次构建）
    exports: Option<FxHashMap<Symbol, Instance<'tcx>>>,
    /// 冻结区（statics/常量池/fn 条目）——lower 期物化，结束移交 Module
    frozen: FrozenArena,
    /// 已物化的 alloc → 冻结区真地址（去重 + 先分后填破指针环）
    alloc_addrs: FxHashMap<AllocId, u64>,
    /// fn-ptr 条目：instance → 条目真地址（D4 每 instance 一个真地址身份）
    fn_entries: FxHashMap<Instance<'tcx>, u64>,
    /// 反查：条目真地址 → FuncId（间接调用派发用，移交 Module）
    fn_addrs: FxHashMap<u64, ir::FuncId>,
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
        }
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

    /// alloc → 冻结区真地址（按需递归物化；先分后填 ⇒ 指针环安全）。
    pub(crate) fn ensure_alloc(&mut self, id: AllocId) -> Result<u64, String> {
        if let Some(&a) = self.alloc_addrs.get(&id) {
            return Ok(a);
        }
        match self.tcx.global_alloc(id) {
            GlobalAlloc::Memory(alloc) => self.materialize(id, alloc),
            GlobalAlloc::Static(def_id) => {
                // extern static：weak 符号判空 cell（如 gettid）——M4.2 写 0（宿主"无此
                // 符号"，guest 走 syscall fallback 直通）；M4.3 起 dlsym 真地址。
                // 非 weak 的 extern static（environ 等）仍归 os:: M4.3。
                if self.tcx.is_foreign_item(def_id) {
                    // extern block 内 item 的 linkage 在 import_linkage 字段
                    let weak = self.tcx.codegen_fn_attrs(def_id).import_linkage
                        == Some(rustc_hir::attrs::Linkage::ExternalWeak);
                    if weak {
                        let cell = self.frozen.alloc(8, 8); // 清零 cell = 符号缺席
                        self.alloc_addrs.insert(id, cell);
                        return Ok(cell);
                    }
                    return Err(format!(
                        "extern static `{}`（真符号地址，os:: M4.3）",
                        self.tcx.item_name(def_id)
                    ));
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
            let intrinsic =
                self.tcx.intrinsic(def_id).expect("InstanceKind::Intrinsic 必有 IntrinsicDef");
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
            // ①引擎原语
            if let Some(&b) = self.builtins.get(&link_name) {
                return Ok(Callee::Builtin(b));
            }
            // ②链接仿真：按符号名在已链接 crate 的导出定义里找（tier-0
            // find_exported_symbol 同构；panic_impl→rust_begin_unwind、__rdl_* 走此路）
            let target = self.exported_defs().get(&link_name).copied();
            if let Some(target) = target {
                return Ok(Callee::Func(self.func_id(target)));
            }
            // ③未知 foreign：os:: 注册表 M4.3
            return Err(format!("foreign `{link_name}`（os:: 直通/内建路由，M4.3）"));
        }
        // 普通函数：worklist 闭包扩集（跨 crate 非泛型函数不在 collector 种子集）
        Ok(Callee::Func(self.func_id(inst)))
    }

    /// 导出符号表（②），惰性一次构建：遍历"最终二进制会链接到"的全部非泛型导出 def
    /// （tier-0 `for_each_linked_def` 同构），符号名 → mono instance，strong 覆盖 weak。
    fn exported_defs(&mut self) -> &FxHashMap<Symbol, Instance<'tcx>> {
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
            map.into_iter().map(|(k, (inst, _))| (k, inst)).collect()
        })
    }
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
            let Some(special) = method.special else { continue };
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
    let sentinel = mangle_internal_symbol(
        tcx,
        rustc_ast::expand::allocator::NO_ALLOC_SHIM_IS_UNSTABLE,
    );
    out.insert(Symbol::intern(&sentinel), ir::Builtin::NoAllocShim);
    // unwind 原语（M4.2）：panic_unwind 照常解释，引擎在平台 unwinder 符号层接管
    out.insert(Symbol::intern("_Unwind_RaiseException"), ir::Builtin::UnwindRaise);
    // os:: 最小直通（panic 链需要；M4.3 换正式注册表）
    out.insert(Symbol::intern("getenv"), ir::Builtin::HostGetenv);
    out.insert(Symbol::intern("write"), ir::Builtin::HostWrite);
    out.insert(Symbol::intern("strlen"), ir::Builtin::HostStrlen);
    out.insert(Symbol::intern("abort"), ir::Builtin::HostAbort);
    out.insert(Symbol::intern("syscall"), ir::Builtin::HostSyscall);
    out
}

/// 整程序降低：种子收集 → worklist 闭包降低 → exports 表。
pub fn lower_program(tcx: TyCtxt<'_>) -> ir::Module {
    let typing_env = TypingEnv::fully_monomorphized();
    let mut linker = Linker::new(tcx);

    // 种子 = mono collector 集（D1：与 native codegen 同一起点，正确性白拿）
    for inst in collect::collect(tcx) {
        linker.func_id(inst);
    }

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
    module.funcs =
        funcs.into_iter().map(|f| f.expect("队列耗尽时每个 FuncId 必有产出")).collect();

    // 入口别名（--vm-stats 从程序入口做可达分析用）
    if let Some((entry_def, _)) = tcx.entry_fn(())
        && let Some(&id) = linker.ids.get(&Instance::mono(tcx, entry_def))
    {
        module.exports.insert("@entry".into(), id);
    }
    // 冻结区与 fn 条目反查表移交执行相
    module.frozen = Some(linker.frozen);
    module.fn_addrs = linker.fn_addrs.into_iter().collect();
    module
}
