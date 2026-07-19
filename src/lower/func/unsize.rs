use super::*;

    /// Unsize 胖化的 meta 推导（类型递归，cg_ssa coerce_unsized_into/unsize_ptr 同构）：
    /// 指针（含 Box）→ pointee 对取 meta；同 def 结构体 → 唯一类型不同的字段对递归
    /// （Arc/Rc/Pin 等自定义 CoerceUnsized：Arc{NonNull{*const ArcInner<T>}} 一路下钻）。
impl<'tcx> LowerCx<'tcx, '_> {
    pub(super) fn unsize_meta_of(&mut self, src: Ty<'tcx>, dst: Ty<'tcx>) -> Result<Operand, String> {
        if let (Some(sp), Some(dp)) = (src.builtin_deref(true), dst.builtin_deref(true)) {
            return self.fresh_unsize_meta(sp, dp);
        }
        // pattern type（本 nightly NonNull 内部 = `*const T is !null`）：剥壳递归 base
        if let (ty::Pat(ba, _), ty::Pat(bb, _)) = (src.kind(), dst.kind()) {
            return self.unsize_meta_of(*ba, *bb);
        }
        if let (ty::Adt(da, sa), ty::Adt(db, sb)) = (src.kind(), dst.kind())
            && da.did() == db.did()
            && da.is_struct()
        {
            // cg_ssa unsize_ptr 的 Adt 臂同构：跳过 1-ZST 字段（PhantomData/Global
            // ——类型可不同但无载荷），递归**唯一**非 ZST 字段。
            let mut found = None;
            for f in &da.non_enum_variant().fields {
                // 本 nightly：FieldDef::ty 返回 Unnormalized 包装——
                // normalize_erasing_regions 直接吃包装（单态化环境下规范化）
                let norm = |t: rustc_middle::ty::Unnormalized<'tcx, Ty<'tcx>>| {
                    self.tcx.normalize_erasing_regions(self.typing_env, t)
                };
                let (fa, fb) = (norm(f.ty(self.tcx, sa)), norm(f.ty(self.tcx, sb)));
                if self.layout_of(fa)?.is_1zst() {
                    continue;
                }
                if found.is_some() {
                    return Err(format!("CoerceUnsized 多非 ZST 字段（{src}）"));
                }
                found = Some((fa, fb));
            }
            let Some((fa, fb)) = found else {
                return Err(format!("CoerceUnsized 无非 ZST 字段（{src} → {dst}）"));
            };
            return self.unsize_meta_of(fa, fb);
        }
        Err(format!("Unsize 形态未知（{src} → {dst}）"))
    }

    /// C5 dyn 尾对统一递归判据（批10，datafusion/typst 双供养）：
    /// 每级四路——① builtin_deref 直达双 Dynamic（Ref/RawPtr/Box/DerefPure）；
    /// ② struct_lockstep 直达双 Dynamic（嵌套尾对）；③ Pat 壳（NonNull =
    /// `*const T is !null`）剥壳递归；④ Adt 同构结构体唯一非 ZST 字段递归
    /// （Arc → NonNull → `*const ArcInner` → data 的 Arc-wrapped dyn 全链）。
    /// 命中 = 双 Dynamic 的（source tail, target tail）。
    pub(super) fn dyn_unsize_tails(
        &mut self,
        src: Ty<'tcx>,
        dst: Ty<'tcx>,
    ) -> Option<(Ty<'tcx>, Ty<'tcx>)> {
        let both_dyn = |x: Ty<'tcx>, y: Ty<'tcx>| {
            matches!(x.kind(), ty::Dynamic(..)) && matches!(y.kind(), ty::Dynamic(..))
        };
        if let (Some(sp), Some(dp)) = (src.builtin_deref(true), dst.builtin_deref(true)) {
            if both_dyn(sp, dp) {
                return Some((sp, dp));
            }
            // 解引用落点再判（`*const ArcInner` → ArcInner → struct_lockstep →
            // data 的 Arc-wrapped dyn 链；ArcInner 是 Adt，继续走 Adt 臂）
            let (lst, ldt) =
                self.tcx
                    .struct_lockstep_tails_for_codegen(sp, dp, self.typing_env);
            if both_dyn(lst, ldt) {
                return Some((lst, ldt));
            }
            if let Some(r) = self.dyn_unsize_tails(sp, dp) {
                return Some(r);
            }
        }
        let (st, dt) =
            self.tcx
                .struct_lockstep_tails_for_codegen(src, dst, self.typing_env);
        if both_dyn(st, dt) {
            return Some((st, dt));
        }
        if let (ty::Pat(ba, _), ty::Pat(bb, _)) = (src.kind(), dst.kind())
            && let Some(r) = self.dyn_unsize_tails(*ba, *bb)
        {
            return Some(r);
        }
        if let (ty::Adt(da, sa), ty::Adt(db, sb)) = (src.kind(), dst.kind())
            && da.did() == db.did()
            && da.is_struct()
        {
            let tcx = self.tcx;
            let env = self.typing_env;
            let norm = move |t: rustc_middle::ty::Unnormalized<'tcx, Ty<'tcx>>| {
                tcx.normalize_erasing_regions(env, t)
            };
            // ① 唯一非 ZST 字段递归（包装下钻：Arc → NonNull<ArcInner>）
            let mut found = None;
            let mut multi = false;
            for f in &da.non_enum_variant().fields {
                let (fa, fb) = (norm(f.ty(self.tcx, sa)), norm(f.ty(self.tcx, sb)));
                if self.layout_of(fa).ok()?.is_1zst() {
                    continue;
                }
                if found.is_some() {
                    multi = true;
                    break;
                }
                found = Some((fa, fb));
            }
            if !multi
                && let Some((fa, fb)) = found
                && let Some(r) = self.dyn_unsize_tails(fa, fb)
            {
                return Some(r);
            }
            // ② tail_opt 尾字段递归（ArcInner → data 的 dyn 尾对路径——
            // 多非 ZST 结构（strong/weak/data）唯一可下钻向）
            if let Some(f) = da.non_enum_variant().tail_opt()
                && let Some(r) = self.dyn_unsize_tails(
                    norm(f.ty(self.tcx, sa)),
                    norm(f.ty(self.tcx, sb)),
                )
            {
                return Some(r);
            }
        }
        None
    }

    /// pointee 对 → 新造 meta：lockstep 尾对为 Array→Slice = 长度立即数、
    /// sized→dyn = 物化 vtable 真地址（cg_ssa unsized_info 同构）。
    /// dyn→dyn（meta 沿用源第二半）不在此路——调用方（Unsize 臂）特例处理。
    pub(super) fn fresh_unsize_meta(
        &mut self,
        src_pointee: Ty<'tcx>,
        dst_pointee: Ty<'tcx>,
    ) -> Result<Operand, String> {
        let (st, dt) =
            self.tcx
                .struct_lockstep_tails_for_codegen(src_pointee, dst_pointee, self.typing_env);
        match (st.kind(), dt.kind()) {
            (ty::Array(_, n), ty::Slice(_)) => {
                let n = n.try_to_target_usize(self.tcx).ok_or("数组长度非常量")?;
                Ok(Operand::Imm {
                    bits: n,
                    width: Width::W64,
                })
            }
            (_, ty::Dynamic(preds, _)) if !matches!(st.kind(), ty::Dynamic(..)) => {
                if self.layout_of(st)?.is_unsized() {
                    return Err(format!("unsized→dyn（{st} → {dt}，M4.1+）"));
                }
                let principal = preds
                    .principal()
                    .map(|b| self.tcx.instantiate_bound_regions_with_erased(b));
                let vt_id = self.tcx.vtable_allocation((st, principal));
                Ok(Operand::Imm {
                    bits: self.linker.ensure_alloc(vt_id)?,
                    width: Width::W64,
                })
            }
            _ => Err(format!("unsize 尾对 {st} → {dt}（M4.4+）")),
        }
    }

}
