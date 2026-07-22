//! P1 fn 条目 + FFI 签名（自 lower/mod.rs M5 entries 带整搬）：fn_entry_addr
//! （FFI 可派生条目可执行化 = 本域 stub 码址）/entry_ffi_sig/alloc_entry_stub/
//! foreign_fn_entry_addr。impl Linker 子块；字段在 mod.rs 的 Linker 结构。

use super::*;

impl<'tcx> Linker<'tcx> {
    /// fn-ptr 条目地址（D4）：每 instance 一个真地址身份。
    /// P1（decision-history §7.6）：FFI 可派生条目**可执行化**——值 = 本域 stub
    /// 码址（任何姿势流给 native 都落可执行入口，thunk 盲区结构性消失）；其余
    /// 保持数据槽（内装 FuncId，调试用；Rust ABI/聚合/变参无 native 合法调用面）。
    /// S4：底座函数已有条目则复用（单一地址身份；底座 vtable 与 delta 取址一致）；
    /// 底座函数无条目（构建时没被取址）则在 delta 区补一个——总量仍恰一份。
    /// A2 split：条目按 instance 类定域（image 类 → image 区，单一地址身份不变）；
    /// image 上下文遇 delta 类 = purity 封闭被破坏（分类器 bug），响亮拒绝。
    /// extern fn（fn 体内 extern 块声明的内核函数被当 fn-ptr 用，ring 的派发模式）：
    /// 无 MIR 可降，走 foreign_fn_entry_addr——值 = native 链接器解析出的真符号地址。
    pub(crate) fn fn_entry_addr(&mut self, inst: Instance<'tcx>) -> Result<u64, String> {
        if let Some(&a) = self.fn_entries.get(&inst) {
            return Ok(a);
        }
        if self.tcx.is_foreign_item(inst.def_id()) {
            return self.foreign_fn_entry_addr(inst);
        }
        let fid = self.func_id(inst);
        if fid < self.delta_first_fn
            && let Some(&a) = self.base_fn_entries.get(self.tcx.symbol_name(inst).name)
        {
            self.fn_entries.insert(inst, a);
            return Ok(a);
        }
        // P1：FFI 可派生 ⇒ 可执行条目（值 = 本域 stub 码址）
        if let Some(sig) = self.entry_ffi_sig(inst) {
            let addr = self.alloc_entry_stub(inst, fid, sig);
            self.fn_entries.insert(inst, addr);
            self.fn_addrs.insert(addr, fid);
            return Ok(addr);
        }
        let addr = if let Some(s) = &mut self.split {
            if fid & IMAGE_TAG != 0 || fid < self.delta_first_fn {
                // image 类，或底座命中但底座无条目（S4 补建条目的 split 变体）：
                // image 区——单一地址身份（delta 引用 image 域恒稳定；delta 区对
                // image 字节码是跨运行不稳定域，绝不能去）。
                let a = s.image_frozen.alloc(8, 16);
                s.image_fn_entries.insert(inst, a);
                a
            } else {
                if s.current_image {
                    panic!(
                        "A2 closure violation：image 实例引用 delta 类 fn 条目（分类器漏判）: {}",
                        self.tcx.symbol_name(inst).name
                    );
                }
                self.frozen.alloc(8, 16)
            }
        } else {
            self.frozen.alloc(8, 16)
        };
        unsafe { (addr as *mut u64).write(fid as u64) };
        self.fn_entries.insert(inst, addr);
        self.fn_addrs.insert(addr, fid);
        Ok(addr)
    }

    /// P1：instance 的冻结 cif 签名（FnDef 且 freeze_c_fnptr_sig 可派生）；
    /// 结果缓存（含 None——不可派生者恒走数据槽，无重复试探成本）。
    /// native_archive C2 救援链判定「rlib fn 可物化条目」同此判据。
    pub(crate) fn entry_ffi_sig(&mut self, inst: Instance<'tcx>) -> Option<ir::ForeignSig> {
        if let Some(sig) = self.entry_sig_cache.get(&inst) {
            return sig.clone();
        }
        let env = TypingEnv::fully_monomorphized();
        let ty = inst.ty(self.tcx, env);
        let sig = if matches!(ty.kind(), rustc_middle::ty::FnDef(..)) {
            freeze_c_fnptr_sig(self.tcx, env, ty)
        } else {
            None
        };
        self.entry_sig_cache.insert(inst, sig.clone());
        sig
    }

    /// P1 可执行条目发放（§7.6）：值 = 本域 stub 码址（域由实例类定——image 类
    /// 恒 image 域，跨运行稳定；配方位序 = stub 偏移，启动相按同序物化复现）。
    pub(super) fn alloc_entry_stub(
        &mut self,
        inst: Instance<'tcx>,
        fid: ir::FuncId,
        sig: ir::ForeignSig,
    ) -> u64 {
        let image_side =
            self.split.is_some() && (fid & IMAGE_TAG != 0 || fid < self.delta_first_fn);
        if let Some(&i) = self.entry_stub_ids.get(&inst) {
            return if image_side {
                self.split
                    .as_ref()
                    .expect("split")
                    .image_code_arena
                    .addr_of(i as u64)
            } else {
                self.code_arena.addr_of(i as u64)
            };
        }
        if image_side {
            let s = self.split.as_mut().expect("split");
            let i = s.image_stub_sites.len() as u32;
            s.image_stub_sites
                .push(ir::EntryStubSite { func: fid, sig });
            let addr = s.image_code_arena.addr_of(i as u64);
            s.image_fn_entries.insert(inst, addr);
            self.entry_stub_ids.insert(inst, i);
            addr
        } else {
            if self.split.as_ref().is_some_and(|s| s.current_image) {
                panic!(
                    "A2 closure violation：image 实例引用 delta 类 fn 条目（分类器漏判）: {}",
                    self.tcx.symbol_name(inst).name
                );
            }
            let i = self.entry_stub_sites.len() as u32;
            self.entry_stub_sites
                .push(ir::EntryStubSite { func: fid, sig });
            let addr = self.code_arena.addr_of(i as u64);
            self.entry_stub_ids.insert(inst, i);
            addr
        }
    }

    /// extern fn 条目地址（fn-ptr 取址）：无 MIR 的 foreign item 不能入 worklist
    /// （instance_mir = rustc query panic）；其 fn-ptr 值语义 = native 链接器解析
    /// 出的真符号地址。解析序与 resolve_call 同构：①引擎内建 ②导出符号仿真
    /// ③denylist/llvm/rust-internal ④归档 hidden 兜底表 → dlsym 全域。
    /// 值本体仍初填宿主真码址，但消费面经 GOT 槽读（P2，decision-history §7.5c）：
    /// 槽随模块序列化、启动相按名重填，模块对 ASLR 位置无关。
    pub(super) fn foreign_fn_entry_addr(&mut self, inst: Instance<'tcx>) -> Result<u64, String> {
        let name = canonical_link_name(self.tcx.symbol_name(inst).name);
        // extern weak 缺席取址 = NULL（native 同语义）；weak 标记供 GOT 启动相
        // 未命中时写 0 而非终止
        let weak = self.tcx.codegen_fn_attrs(inst.def_id()).import_linkage
            == Some(rustc_hir::attrs::Linkage::ExternalWeak);
        if let Some(&a) = self.foreign_fn_entries.get(&inst) {
            // P2：缓存命中同样要保证【当前上下文】槽位在场（槽按 (名, 上下文) 分侧）
            let _ = self.foreign_slot(name, a, weak);
            return Ok(a);
        }
        let bake = |this: &mut Self, addr: u64| {
            // P2 GOT：槽初填本进程解析值，启动相按名重填
            let _ = this.foreign_slot(name, addr, weak);
            this.foreign_fn_entries.insert(inst, addr);
            addr
        };
        let link_name = Symbol::intern(name);
        // ①引擎内建：纯直通快路径（语义与通用 dlsym+libffi 道逐位一致）可给真地址；
        // 其余内建（alloc/unwind/fork/atexit/signal/backtrace 系）是引擎接管的语义，
        // 无地址可物化——响亮 Trap（宿主进程也导出 __rust/_Unwind 系符号，直取 =
        // 打穿引擎的堆/panic/unwind 模型）。
        if let Some(&b) = self.builtins.get(&link_name) {
            use ir::Builtin as B;
            if !matches!(
                b,
                B::HostGetenv | B::HostWrite | B::HostStrlen | B::HostAbort
            ) {
                return Err(format!(
                    "extern fn `{name}` 被当作值取址（fn-ptr），但它是引擎内建语义符号，无地址可物化"
                ));
            }
        }
        let rust_internal =
            name.starts_with("__rust") || name.starts_with("__rdl") || name.starts_with("rust_");
        // ②链接仿真：符号由已链接 crate 的导出定义提供 → 值 = 该 guest 定义的
        // 条目地址；weak 定义让位于动态库强符号（Rust 内部符号除外——见①注）。
        let exported = self.exported_defs().get(&link_name).copied();
        if let Some((target, is_weak)) = exported {
            if is_weak && !rust_internal {
                let cname = std::ffi::CString::new(name).map_err(|_| "符号名含 NUL".to_string())?;
                let strong = crate::os::dll::sym(0, &cname);
                if strong != 0 {
                    return Ok(bake(self, strong as u64));
                }
            }
            return self.fn_entry_addr(target);
        }
        // ③与 resolve_call 同一纪律的绝不直通清单
        if DENY_EXACT.contains(&name) || DENY_PREFIX.iter().any(|p| name.starts_with(p)) {
            return Err(format!(
                "foreign `{name}` 被当作值取址（denylist：线程 M4.4 / 进程模型不直通）"
            ));
        }
        if name.starts_with("llvm.") {
            return Err(format!(
                "foreign `{name}` 被当作值取址（LLVM 内部符号，按需内建）"
            ));
        }
        if rust_internal {
            return Err(format!(
                "foreign `{name}` 被当作值取址（Rust 内部 ABI 符号，宿主进程亦有导出，不能直取）"
            ));
        }
        // ④解析序：hidden 兜底表 → 归档句柄（链接序）→ dlsym 全域（native 链接
        // 期绑定——guest 自己链进来的对象（hidden 或 dynsym 可见）恒胜宿主进程
        // 同名库：libLLVM 的 ZSTD_*（c_zstd_stream）与 librustc_driver 的
        // rust_psm_on_stack（c_polars_frame）两实锤）。归档 `.so` 已在排干
        // worklist 前 RTLD_NOW|RTLD_GLOBAL 加载（lower_inner 头部），句柄在
        // self.archive_handles；全域兜底真系统库。
        let cname = std::ffi::CString::new(name).map_err(|_| "符号名含 NUL".to_string())?;
        let mut p = 0u64;
        for (bias, syms) in &self.archive_fallbacks {
            if let Some(&v) = syms.get(name) {
                p = bias + v;
                break;
            }
        }
        if p == 0 {
            for &h in &self.archive_handles {
                p = crate::os::dll::sym(h, &cname) as u64;
                if p != 0 {
                    break;
                }
            }
        }
        if p == 0 {
            p = crate::os::dll::sym(0, &cname) as u64;
        }
        if p == 0 {
            // weak 符号缺席 = NULL（native 未定义弱符号的取址语义）；经它间接调用
            // 在执行相响亮终止（CallIndirect 的空指针诊断）
            if weak {
                return Ok(bake(self, 0));
            }
            return Err(format!(
                "extern fn `{name}` 被当作值取址，但符号未命中（归档兜底表 / dlsym 全域均无）"
            ));
        }
        Ok(bake(self, p))
    }
}
