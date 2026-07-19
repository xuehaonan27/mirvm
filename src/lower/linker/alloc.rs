//! 冻结区物化（自 lower/mod.rs M5 alloc 带整搬）：ensure_alloc（常量/
//! static/vtable/函数字节 → FrozenArena 定域）+ frozen_alloc_bytes +
//! record_addr/record_both（A2 split 双侧记账）。impl Linker 子块。

use super::*;
use crate::lower::purity::arg_mentions_local;

impl<'tcx> Linker<'tcx> {

    /// 裸字节物化进冻结区（128 位常量等小常量的通用道）。
    /// A2 split：纯字节无指针无 locality，按当前类定域即可。
    pub(crate) fn frozen_alloc_bytes(&mut self, bytes: &[u8]) -> u64 {
        let arena: &mut FrozenArena = match &mut self.split {
            Some(s) if s.current_image => &mut s.image_frozen,
            _ => &mut self.frozen,
        };
        let p = arena.alloc(bytes.len() as u64, 16);
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), p as *mut u8, bytes.len()) };
        p
    }

    /// alloc → 冻结区真地址（按需递归物化；先分后填 ⇒ 指针环安全）。
    /// A2 split 路由（s3b-a2-design §4.3）：
    /// - image 上下文只接受 image 域地址（跨运行稳定域）；delta 上下文两域皆可
    ///   （delta map 优先，image map 兜底复用——delta→image 向下稳定）。
    /// - 身份必需通道（static/weak cell/fn 条目）按 krate/类定域并**双表登记**
    ///   （防双份精神分裂）；Memory/vtable 身份 unspecified，按上下文定域，
    ///   image 上下文对 delta 区已有者**提升**（双份物化，常量只读安全）。
    pub(crate) fn ensure_alloc(&mut self, id: AllocId) -> Result<u64, String> {
        let ctx_image = self.split.as_ref().is_some_and(|s| s.current_image);
        if ctx_image {
            if let Some(&a) = self
                .split
                .as_ref()
                .expect("split")
                .image_alloc_addrs
                .get(&id)
            {
                return Ok(a);
            }
        } else {
            if let Some(&a) = self.alloc_addrs.get(&id) {
                return Ok(a);
            }
            if let Some(s) = &self.split
                && let Some(&a) = s.image_alloc_addrs.get(&id)
            {
                return Ok(a);
            }
        }
        match self.tcx.global_alloc(id) {
            GlobalAlloc::Memory(alloc) => self.materialize_in(id, alloc, ctx_image),
            GlobalAlloc::Static(def_id) => {
                // extern static = 真符号（os:: 直通）：
                // - weak（gettid 等 fn 符号判空模式）：判空 cell 写 0（缺席）——weak
                //   **fn** 符号即便存在也不能给真地址（guest 拿去调用 = 跳 native 代码，
                //   条目反查失败；M4.4 thunk 前统一走 fallback 路径）
                // - 非 weak（environ 等数据符号）：alloc 基址 = dlsym 真地址
                if self.tcx.is_foreign_item(def_id) {
                    // dlsym 用链接符号名（#[link_name] 前缀——ring 的 prefixed_extern
                    // 静态量；item_name 会丢掉前缀）与 LLVM verbatim `\x01` 剥除
                    // （aws-lc-sys 一族，canonical_link_name），与 resolve_call 的 fn 路径同源
                    let name = canonical_link_name(
                        self.tcx.symbol_name(Instance::mono(self.tcx, def_id)).name,
                    );
                    // extern block 内 item 的 linkage 在 import_linkage 字段
                    let weak = self.tcx.codegen_fn_attrs(def_id).import_linkage
                        == Some(rustc_hir::attrs::Linkage::ExternalWeak);
                    if weak {
                        // native extern weak 语义（P2 GOT 启动相重填）：命中 = 真
                        // 符号地址、缺席 = 0。例外 = 引擎接管语义的符号强制缺席
                        // （M4 判空 cell 纪律延续）：非纯直通内建 / denylist /
                        // 引擎模型符号（TLS-dtor 一族）——std 对它们走回退路径，
                        // 引擎接管不被真符号绕开（E27 闭合契约，2026-07-18）。
                        const FORCE_ABSENT_WEAK: &[&str] = &["__cxa_thread_atexit_impl"];
                        let engine_owned = self
                            .builtins
                            .get(&Symbol::intern(name))
                            .is_some_and(|b| {
                                !matches!(
                                    b,
                                    ir::Builtin::HostGetenv
                                        | ir::Builtin::HostWrite
                                        | ir::Builtin::HostStrlen
                                        | ir::Builtin::HostAbort
                                )
                            })
                            || DENY_EXACT.contains(&name)
                            || DENY_PREFIX.iter().any(|p| name.starts_with(p))
                            || FORCE_ABSENT_WEAK.contains(&name);
                        let cell = if engine_owned {
                            // 判空 cell = 符号缺席（krate 定域 + 双表登记同前）
                            if let Some(s) = &mut self.split
                                && def_id.krate != rustc_hir::def_id::LOCAL_CRATE
                            {
                                s.image_frozen.alloc(8, 8)
                            } else {
                                self.frozen.alloc(8, 8)
                            }
                        } else {
                            // GOT 槽（初填 0；启动相按名重填命中值或 0）
                            self.foreign_slot(name, 0, true)
                        };
                        self.record_both(id, cell);
                        return Ok(cell);
                    }
                    // 解析序同 fn 取址④：hidden 兜底表 → 归档句柄（链接序）→
                    // dlsym 全域（native 链接期绑定：归档内定义恒胜全局同名）
                    let cname =
                        std::ffi::CString::new(name).map_err(|_| "符号名含 NUL".to_string())?;
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
                        return Err(format!(
                            "extern static `{name}` 未命中（归档兜底表 / dlsym 全域均无）"
                        ));
                    }
                    // P2 GOT（decision-history §7.5c）：值仍初填本进程解析（冷路径
                    // 逐位不变），另登记槽位与 foreign 分配——常量发码改槽读、冻结
                    // 字节重定位登记修补点，启动相按名重填。
                    let _ = self.foreign_slot(name, p, false);
                    self.record_both(id, p);
                    self.foreign_alloc_sym.insert(id, (name.into(), false));
                    return Ok(p);
                }
                // S4 底座静态去重：同一 static 双份物化 = static mut/内部可变性的
                // 精神分裂（两处地址各自演化），命中必须复用底座地址。
                if !self.base_statics.is_empty() {
                    let sym = self.tcx.symbol_name(Instance::mono(self.tcx, def_id)).name;
                    if let Some(&addr) = self.base_statics.get(sym) {
                        self.record_both(id, addr);
                        return Ok(addr);
                    }
                }
                // A2 split：static 身份必需（static mut/内部可变性/&static 相等性），
                // 一律按 def_id.krate 定域（非本地 → image 区）；image 上下文遇本地
                // static = purity 封闭被破坏（分类器 bug），响亮拒绝。
                let to_image = if let Some(s) = &self.split {
                    if def_id.krate == rustc_hir::def_id::LOCAL_CRATE {
                        if s.current_image {
                            panic!("A2 closure violation：image 实例引用本地 static（分类器漏判）");
                        }
                        false
                    } else {
                        true
                    }
                } else {
                    false
                };
                // static 的字节 = 初始化器求值产物；可写（static mut/内部可变性）
                let alloc = self
                    .tcx
                    .eval_static_initializer(def_id)
                    .map_err(|e| format!("static 初始化器求值失败: {e:?}"))?;
                let addr = self.materialize_in(id, alloc, to_image)?;
                if to_image {
                    self.record_both(id, addr);
                }
                self.static_defs.push((def_id, addr)); // 底座/image 导出素材
                Ok(addr)
            }
            GlobalAlloc::Function { instance } => {
                let addr = self.fn_entry_addr(instance)?;
                self.record_both(id, addr);
                // P2：extern fn 取址（fn-ptr 值 = dlsym 宿主码址）登记 foreign
                // 分配——常量发码经 foreign_const_operand 出槽读、冻结字节经
                // materialize_in 重定位登记修补点（槽本体 bake 已开）。
                if self.tcx.is_foreign_item(instance.def_id()) {
                    let name = canonical_link_name(self.tcx.symbol_name(instance).name);
                    let weak = self.tcx.codegen_fn_attrs(instance.def_id()).import_linkage
                        == Some(rustc_hir::attrs::Linkage::ExternalWeak);
                    self.foreign_alloc_sym.insert(id, (name.into(), weak));
                }
                Ok(addr)
            }
            GlobalAlloc::VTable(ty, dyn_ty) => {
                // A2 split 护栏：image 上下文遇本地 Self 类型的 vtable = 封闭破坏。
                // vtable 地址身份 unspecified（rustc 自身 per-CGU 复制）⇒ 按上下文
                // 定域（提升双份合规），无需 krate 定域。
                if self.split.as_ref().is_some_and(|s| s.current_image)
                    && ty.walk().any(arg_mentions_local)
                {
                    panic!("A2 closure violation：image 实例引用本地类型 vtable（分类器漏判）");
                }
                // 现成的 vtable 分配（F5）——递归走 Memory 路径（含 fn 条目重定位）
                let principal = dyn_ty
                    .principal()
                    .map(|b| self.tcx.instantiate_bound_regions_with_erased(b));
                let vt_id = self.tcx.vtable_allocation((ty, principal));
                let addr = self.ensure_alloc(vt_id)?;
                self.record_addr(id, addr, ctx_image);
                Ok(addr)
            }
            GlobalAlloc::TypeId { .. } => {
                // TypeId"分配"：基址 0——重定位 base+addend 后值 = 128 位类型哈希的
                // 指针宽片段本身（tier-0 resolve_addr/Miri 同款）
                self.record_addr(id, 0, ctx_image);
                Ok(0)
            }
        }
    }

    /// 按上下文登记去重表（split；非 split 恒 delta 表）
    pub(super) fn record_addr(&mut self, id: AllocId, addr: u64, ctx_image: bool) {
        match &mut self.split {
            Some(s) if ctx_image => {
                s.image_alloc_addrs.insert(id, addr);
            }
            _ => {
                self.alloc_addrs.insert(id, addr);
            }
        }
    }

    /// 身份必需通道的双表登记（split：两上下文都能以同一地址复现 = 单一身份）
    pub(super) fn record_both(&mut self, id: AllocId, addr: u64) {
        self.alloc_addrs.insert(id, addr);
        if let Some(s) = &mut self.split {
            s.image_alloc_addrs.insert(id, addr);
        }
    }
}
