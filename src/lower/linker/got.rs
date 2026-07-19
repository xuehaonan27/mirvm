//! GOT/foreign 槽（自 lower/mod.rs M5 got 带整搬）：got_intern/
//! got_fixup_push/foreign_slot/foreign_const_operand/foreign_fn_slot/
//! materialize_in（P2 启动相统一重填的素材生产）。impl Linker 子块。

use super::*;

impl<'tcx> Linker<'tcx> {

    /// P2：本侧符号表 idx（名字首现才登记；image 侧在 Split 三表）
    pub(super) fn got_intern(&mut self, name: &str, weak: bool, image: bool) -> u32 {
        let (syms, idx_map) = if image {
            let s = self.split.as_mut().expect("image 侧 got 表必在 split");
            (&mut s.image_got_syms, &mut s.image_got_idx)
        } else {
            (&mut self.got_syms, &mut self.got_idx)
        };
        if let Some(&i) = idx_map.get(name) {
            return i;
        }
        let i = syms.len() as u32;
        syms.push(ir::GotSym {
            name: name.into(),
            weak,
        });
        idx_map.insert(name.into(), i);
        i
    }

    /// P2：本侧修补点登记（image 侧入 Split 表）
    pub(super) fn got_fixup_push(&mut self, image: bool, f: ir::GotFixup) {
        if image {
            self.split
                .as_mut()
                .expect("image 侧 got 表必在 split")
                .image_got_fixups
                .push(f);
        } else {
            self.got_fixups.push(f);
        }
    }

    /// foreign 符号在当前上下文的 GOT 槽（P2，decision-history §7.5c）：槽 =
    /// 本侧冻结区普通 8 字节格（固定基域 ⇒ 槽址可序列化，内容启动相重填）；
    /// 无则开格、初填 init、以 addend=0 登记槽位修补点。
    pub(super) fn foreign_slot(&mut self, name: &str, init: u64, weak: bool) -> u64 {
        let ctx_image = self.split.as_ref().is_some_and(|s| s.current_image);
        if let Some(&a) = self.foreign_slots.get(&(name.into(), ctx_image)) {
            return a;
        }
        let idx = self.got_intern(name, weak, ctx_image);
        let addr = if ctx_image {
            self.split
                .as_mut()
                .expect("image 上下文必在 split")
                .image_frozen
                .alloc(8, 8)
        } else {
            self.frozen.alloc(8, 8)
        };
        unsafe { *(addr as *mut u64) = init };
        self.got_fixup_push(
            ctx_image,
            ir::GotFixup {
                addr,
                sym: idx,
                addend: 0,
            },
        );
        self.foreign_slots.insert((name.into(), ctx_image), addr);
        addr
    }

    /// foreign 分配的常量操作数（P2）：值 = 本侧 GOT 槽内容；非零 addend 经
    /// SubImm 精确等价改写（`(槽) − (0 − addend)` ≡ `(槽) + addend`，mod 2^64
    /// 算术恒等）。非 foreign → None（调用方按原样发 Imm）。
    pub(crate) fn foreign_const_operand(
        &mut self,
        id: AllocId,
        init: u64,
        addend: u64,
    ) -> Option<ir::Operand> {
        let (name, weak) = self.foreign_alloc_sym.get(&id)?.clone();
        let slot = self.foreign_slot(&name, init, weak);
        let mem = ir::Operand::Mem {
            expr: ir::PlaceExpr {
                base: ir::PlaceBase::Static(slot),
                steps: Box::new([]),
            },
            width: ir::Width::W64,
        };
        Some(if addend == 0 {
            mem
        } else {
            ir::Operand::SubImm {
                base: Box::new(mem),
                sub: 0u64.wrapping_sub(addend),
            }
        })
    }

    /// extern fn 条目（fn-ptr）在当前上下文的 GOT 槽址（P2）：bake 已保证本
    /// 上下文槽在场；非 foreign → None（func.rs Reify/Closure 发码点用）。
    pub(crate) fn foreign_fn_slot(&mut self, inst: Instance<'tcx>) -> Option<u64> {
        if !self.tcx.is_foreign_item(inst.def_id()) {
            return None;
        }
        let name = canonical_link_name(self.tcx.symbol_name(inst).name);
        let ctx_image = self.split.as_ref().is_some_and(|s| s.current_image);
        self.foreign_slots.get(&(name.into(), ctx_image)).copied()
    }

    /// 物化一个内存分配：分地址 → 拷字节 → 重定位（provenance 表逐项写真地址+addend）。
    /// image=true 落 image 域并登记 image 表（split 专用）；false 落 delta 域（今日路径）。
    pub(super) fn materialize_in(
        &mut self,
        id: AllocId,
        alloc: ConstAllocation<'tcx>,
        image: bool,
    ) -> Result<u64, String> {
        let a = alloc.inner();
        let size = a.size().bytes();
        let align = a.align.bytes();
        let base = if image {
            let s = self.split.as_mut().expect("image 物化仅 split 模式");
            let base = s.image_frozen.alloc(size, align);
            s.image_alloc_addrs.insert(id, base); // 先分后填（环安全）
            base
        } else {
            let base = self.frozen.alloc(size, align);
            self.alloc_addrs.insert(id, base); // 先分后填（环安全）
            base
        };
        let bytes = a.inspect_with_uninit_and_ptr_outside_interpreter(0..size as usize);
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), base as *mut u8, size as usize) };
        // 重定位：ptr 位置存的 8 字节 = 目标内偏移（addend）→ 换成目标真地址 + addend
        for (off, prov) in a.provenance().ptrs().iter() {
            let target = self.ensure_alloc(prov.alloc_id())?;
            let at = (base + off.bytes()) as *mut u64;
            let addend = unsafe {
                let addend = at.read_unaligned();
                at.write_unaligned(target.wrapping_add(addend));
                addend
            };
            // P2：目标是 foreign 分配（非 weak extern static / extern fn 取址）⇒
            // 本字节点登记修补点（启动相按名重填；初填 = 本进程解析，冷路径不变）
            if let Some((name, weak)) = self.foreign_alloc_sym.get(&prov.alloc_id()).cloned() {
                let idx = self.got_intern(&name, weak, image);
                self.got_fixup_push(
                    image,
                    ir::GotFixup {
                        addr: base + off.bytes(),
                        sym: idx,
                        addend,
                    },
                );
            }
        }
        Ok(base)
    }
}
