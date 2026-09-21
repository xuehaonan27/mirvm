//! Unsize family: unsize_meta_of/dyn_unsize_tails/fresh_unsize_meta derive the
//! fat-pointer meta and transform the vtable on dyn upcasting (isomorphic to
//! cg_ssa unsized_info). Sole entry = cast.rs's CoerceUnsized arm.

use crate::lower::Error;

use super::*;

/// Meta derivation for an unsizing coercion: recurse on types (isomorphic to
/// cg_ssa coerce_unsized_into/unsize_ptr). Pointer (including Box) -> take the
/// pointee pair's meta; same-def struct -> recurse into the one field whose type
/// differs (custom CoerceUnsized such as Arc/Rc/Pin drills down the
/// Arc{NonNull{*const ArcInner<T>}} chain).
impl<'tcx> LowerCx<'tcx, '_> {
    pub(super) fn unsize_meta_of(
        &mut self,
        src: Ty<'tcx>,
        dst: Ty<'tcx>,
    ) -> Result<Operand, Error> {
        if let (Some(sp), Some(dp)) = (src.builtin_deref(true), dst.builtin_deref(true)) {
            return self.fresh_unsize_meta(sp, dp);
        }
        // pattern type (on this nightly NonNull is internally `*const T is !null`): peel and recurse into base
        if let (ty::Pat(ba, _), ty::Pat(bb, _)) = (src.kind(), dst.kind()) {
            return self.unsize_meta_of(*ba, *bb);
        }
        if let (ty::Adt(da, sa), ty::Adt(db, sb)) = (src.kind(), dst.kind())
            && da.did() == db.did()
            && da.is_struct()
        {
            // Isomorphic to cg_ssa unsize_ptr's Adt arm: skip 1-ZST fields (PhantomData/
            // Global -- type may differ but carry no payload), recurse into the **only** non-ZST field.
            let mut found = None;
            for f in &da.non_enum_variant().fields {
                // On this nightly FieldDef::ty returns an Unnormalized wrapper;
                // normalize_erasing_regions accepts it directly (normalization under monomorphization)
                let norm = |t: rustc_middle::ty::Unnormalized<'tcx, Ty<'tcx>>| {
                    self.tcx.normalize_erasing_regions(self.typing_env, t)
                };
                let (fa, fb) = (norm(f.ty(self.tcx, sa)), norm(f.ty(self.tcx, sb)));
                if self.layout_of(fa)?.is_1zst() {
                    continue;
                }
                if found.is_some() {
                    return Err(Error::internal(format!(
                        "CoerceUnsized has multiple non-ZST fields ({src})"
                    )));
                }
                found = Some((fa, fb));
            }
            let Some((fa, fb)) = found else {
                return Err(Error::internal(format!(
                    "CoerceUnsized has no non-ZST field ({src} -> {dst})"
                )));
            };
            return self.unsize_meta_of(fa, fb);
        }
        Err(Error::internal(format!(
            "unknown Unsize shape ({src} -> {dst})"
        )))
    }

    /// Unified recursive criterion for a dyn tail pair. Four routes per level: (1)
    /// builtin_deref straight to a Dynamic pair (Ref/RawPtr/Box/DerefPure); (2)
    /// struct_lockstep straight to a Dynamic pair (nested tail pair); (3) Pat shell
    /// (NonNull = `*const T is !null`) peeled and recursed; (4) Adt same-def struct
    /// recursed into its only non-ZST field (the whole Arc -> NonNull -> `*const`
    /// ArcInner` -> data Arc-wrapped dyn chain). A hit is a (source tail, target tail).
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
            // Re-check at the dereferenced pointee (`*const ArcInner` -> ArcInner ->
            // struct_lockstep -> data Arc-wrapped dyn chain; ArcInner is an Adt, so take the Adt arm)
            let (lst, ldt) = self
                .tcx
                .struct_lockstep_tails_for_codegen(sp, dp, self.typing_env);
            if both_dyn(lst, ldt) {
                return Some((lst, ldt));
            }
            if let Some(r) = self.dyn_unsize_tails(sp, dp) {
                return Some(r);
            }
        }
        let (st, dt) = self
            .tcx
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
            // (1) Recurse into the only non-ZST field (wrapper drill-down: Arc -> NonNull<ArcInner>)
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
            // (2) Recurse into the tail_opt field (the ArcInner -> data dyn tail pair path --
            // the only drill-down direction in a multi-non-ZST struct such as strong/weak/data)
            if let Some(f) = da.non_enum_variant().tail_opt()
                && let Some(r) =
                    self.dyn_unsize_tails(norm(f.ty(self.tcx, sa)), norm(f.ty(self.tcx, sb)))
            {
                return Some(r);
            }
        }
        None
    }

    /// Synthesize meta for a pointee pair: an Array->Slice lockstep tail pair yields the
    /// length immediate; sized->dyn materializes the real vtable address. dyn->dyn
    /// (reuse the source's second half) is left to the caller (Unsize arm).
    pub(super) fn fresh_unsize_meta(
        &mut self,
        src_pointee: Ty<'tcx>,
        dst_pointee: Ty<'tcx>,
    ) -> Result<Operand, Error> {
        let (st, dt) =
            self.tcx
                .struct_lockstep_tails_for_codegen(src_pointee, dst_pointee, self.typing_env);
        match (st.kind(), dt.kind()) {
            (ty::Array(_, n), ty::Slice(_)) => {
                let n = n
                    .try_to_target_usize(self.tcx)
                    .ok_or(Error::internal("array length is not constant"))?;
                Ok(Operand::Imm {
                    bits: n,
                    width: Width::W64,
                })
            }
            (_, ty::Dynamic(preds, _)) if !matches!(st.kind(), ty::Dynamic(..)) => {
                if self.layout_of(st)?.is_unsized() {
                    return Err(Error::internal(format!("unsized->dyn ({st} -> {dt})")));
                }
                let principal = preds
                    .principal()
                    .map(|b| self.tcx.instantiate_bound_regions_with_erased(b));
                let vt_id = self.tcx.vtable_allocation((st, principal));
                Ok(Operand::AddrImm(ir::LinkAddr(
                    self.linker.ensure_alloc(vt_id)?,
                )))
            }
            _ => Err(Error::internal(format!("unsize tail pair {st} -> {dt}"))),
        }
    }
}
