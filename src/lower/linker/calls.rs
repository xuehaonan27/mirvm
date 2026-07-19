//! 调用解析（自 lower/mod.rs M5 calls 带整搬）：func_id（去重+入队）/
//! resolve_call（Callee 三形态）/freeze_foreign_sig/exported_defs（C2
//! 救援链的 rlib 符号集供给）。impl Linker 子块。

use super::*;

impl<'tcx> Linker<'tcx> {

    /// instance → FuncId；首见分配 id 并入待降低队列（worklist 扩集的入口）。
    /// S4：首见先查底座（v0 symbol_name 键）——命中即复用底座 id，不入队；
    /// symbol_name 只在首见且有底座时计算一次（无底座路径零额外成本）。
    /// A2 split：非底座实例按 purity 分轨——image 类（Pure）得标签 id 入 image
    /// 队列，delta 类（Local/Tainted）走今日 untagged 空间入 delta 队列。
    pub(crate) fn func_id(&mut self, inst: Instance<'tcx>) -> ir::FuncId {
        if let Some(&id) = self.ids.get(&inst) {
            return id;
        }
        if !self.base_fns.is_empty()
            && let Some(&bid) = self.base_fns.get(self.tcx.symbol_name(inst).name)
        {
            self.ids.insert(inst, bid);
            return bid;
        }
        if let Some(s) = &mut self.split {
            let id = if classify_purity(inst).is_image() {
                let j = s.image_fn_next;
                s.image_fn_next += 1;
                let id = IMAGE_TAG | j;
                s.image_queue.push_back((id, inst));
                if s.image_funcs.len() <= j as usize {
                    s.image_funcs.resize_with(j as usize + 1, || None);
                }
                s.image_insts.push(inst);
                id
            } else {
                let id = self.next_fn;
                self.next_fn += 1;
                self.queue.push_back((id, inst));
                id
            };
            self.ids.insert(inst, id);
            return id;
        }
        let id = self.next_fn;
        self.next_fn += 1;
        self.ids.insert(inst, id);
        self.queue.push_back((id, inst));
        id
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
            let link_name = Symbol::intern(canonical_link_name(self.tcx.symbol_name(inst).name));
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
                    let strong = crate::os::dll::sym(0, &cname);
                    if strong != 0 {
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
            let name = canonical_link_name(self.tcx.symbol_name(inst).name);
            return self.freeze_foreign_sig(inst, name);
        }
        // 普通函数：worklist 闭包扩集（跨 crate 非泛型函数不在 collector 种子集）
        Ok(Callee::Func(self.func_id(inst)))
    }

    /// os:: 直通签名冻结：foreign fn sig → FfiKind 列表（tier-0 ty_to_ffitype 同构）。
    /// fn-ptr 类型的参数（pthread_create 的 thread_start 等）额外冻结**内层签名**
    /// （M4.4 D1）：执行期该位若收到 fn 条目地址，thunk 工厂物化真机器码后再直传。
    pub(super) fn freeze_foreign_sig(&mut self, inst: Instance<'tcx>, name: &str) -> Result<Callee, String> {
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
    /// guest 导出符号（`#[no_mangle]`/`#[export_name]`/`#[used]` 非泛型）→
    /// （定义 Instance, is_weak）：② 链接仿真的权威表（与 native final link
    /// 的 `exported_non_generic_symbols` 集符集同源）；C2 native-archive
    /// 「符号在 rlib」闭包判定同表（名称 → 可物化 P1 条目）。
    pub(crate) fn exported_defs(&mut self) -> &FxHashMap<Symbol, (Instance<'tcx>, bool)> {
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
