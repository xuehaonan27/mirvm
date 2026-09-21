//! FFI signature derivation free functions (moved whole from lower/mod.rs M6): freeze_c_fnptr_sig/
//! ffi_kind_of/scalar_ffi_kind/ffi_agg_of/push_agg_field/canonical_link_name
//! (extern "C" family fn-ptr types → frozen ForeignSig; criteria for non-derivable = None).

use crate::lower::Error;

use super::*;

/// Unwind bit of the C-family ABI. `None` means not a C/System ABI directly callable by libffi.
pub(super) fn c_abi_unwind(abi: rustc_abi::ExternAbi) -> Option<bool> {
    use rustc_abi::ExternAbi;
    match abi {
        ExternAbi::C { unwind } | ExternAbi::System { unwind } => Some(unwind),
        _ => None,
    }
}

/// extern "C" family fn-ptr type → frozen ForeignSig (M4.4 FFI reverse direction #2: carried at call site,
/// runtime entry reverse lookup miss = guest holds native real code → libffi calls directly per this).
/// None = Rust ABI / variadic / arg unclassifiable — this call site can only dispatch guest entries (diagnose on miss).
pub(crate) fn freeze_c_fnptr_sig<'tcx>(
    tcx: TyCtxt<'tcx>,
    env: TypingEnv<'tcx>,
    ty: rustc_middle::ty::Ty<'tcx>,
) -> Option<ir::ForeignSig> {
    let sig = ty.fn_sig(tcx).skip_binder();
    // F-09 (2026-07-22 confirmed reversal): C/C-unwind both accepted — unwind attribute preserved into
    // ForeignSig.unwind (accepted means "was read", not "unseen"). Runtime direct
    // foreign, callback thunk, and P1 entries all select C/C-unwind boundary by this bit.
    let unwind = c_abi_unwind(sig.abi())?;
    if sig.c_variadic() {
        return None;
    }
    let mut args = Vec::with_capacity(sig.inputs().len());
    for &t in sig.inputs() {
        let k = ffi_kind_of(tcx, env, t).ok()?;
        if k == ir::FfiKind::Void {
            return None; // ZST cannot be a cif arg
        }
        args.push(k);
    }
    let ret = ffi_kind_of(tcx, env, sig.output()).ok()?;
    Some(ir::ForeignSig {
        args,
        ret,
        fixed: None,
        thunk_args: vec![],
        unwind,
    })
}

/// Type → libffi direct-pass class (scalars and pointers; ZST=Void only for return; C1: by-value aggregate allowed).
pub(crate) fn ffi_kind_of<'tcx>(
    tcx: TyCtxt<'tcx>,
    env: TypingEnv<'tcx>,
    ty: rustc_middle::ty::Ty<'tcx>,
) -> Result<ir::FfiKind, Error> {
    use rustc_abi::BackendRepr;
    let layout = tcx
        .layout_of(env.as_query_input(ty))
        .map_err(|e| Error::internal(format!("layout failed: {e}")))?;
    if layout.is_zst() {
        return Ok(ir::FfiKind::Void);
    }
    if let BackendRepr::Scalar(s) = layout.backend_repr {
        return match scalar_ffi_kind(s.primitive()) {
            Ok(k) => Ok(k),
            Err(other) => Err(Error::unsupported(format!(
                "scalar {other:?} not supported"
            ))),
        };
    }
    Ok(ir::FfiKind::Agg(ffi_agg_of(tcx, layout)?))
}

/// Scalar primitive → FfiKind (uses pre-C1 mapping table).
fn scalar_ffi_kind(p: rustc_abi::Primitive) -> Result<ir::FfiKind, rustc_abi::Primitive> {
    use rustc_abi::{Float, Integer, Primitive};
    Ok(match p {
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
        other => return Err(other),
    })
}

/// C1: rustc layout → frozen aggregate (declaration-order fields + nested recursion; array = repeated element field;
/// ZST members omitted but padding preserved by size/off). Loud-reject boundaries: union, SIMD vector,
/// unsized (Err message shows red classification).
fn ffi_agg_of<'tcx>(
    tcx: TyCtxt<'tcx>,
    layout: rustc_middle::ty::layout::TyAndLayout<'tcx>,
) -> Result<ir::FfiAgg, Error> {
    use rustc_abi::BackendRepr;
    let size = layout.layout.size().bytes() as u32;
    let align = layout.layout.align().abi.bytes() as u32;
    if align > 8 {
        return Err(Error::unsupported(format!(
            "by-value aggregate align={align} > 8 (C1 boundary)"
        )));
    }
    let mut fields = Vec::new();
    // ScalarPair ({ptr,len} / two-scalar-field shape): both leaves emitted directly by primitive
    if let BackendRepr::ScalarPair(a, b) = layout.backend_repr {
        let (ao, bo) = (
            layout.fields.offset(0).bytes() as u32,
            layout.fields.offset(1).bytes() as u32,
        );
        fields.push(ir::FfiField {
            off: ao,
            leaf: ir::FfiLeaf::Scalar(
                scalar_ffi_kind(a.primitive())
                    .map_err(|p| Error::unsupported(format!("scalar {p:?} not supported")))?,
            ),
        });
        fields.push(ir::FfiField {
            off: bo,
            leaf: ir::FfiLeaf::Scalar(
                scalar_ffi_kind(b.primitive())
                    .map_err(|p| Error::unsupported(format!("scalar {p:?} not supported")))?,
            ),
        });
        let agg = ir::FfiAgg {
            size,
            align,
            fields,
        };
        validate_agg_natural(&agg)?;
        return Ok(agg);
    }
    match layout.backend_repr {
        BackendRepr::Memory { .. } => {}
        _ => {
            return Err(Error::unsupported(format!(
                "by-value aggregate layout shape {:?} not supported (C1 boundary; SIMD separate axis)",
                layout.backend_repr
            )));
        }
    }
    if layout.ty.is_union() {
        return Err(Error::unsupported(
            "by-value aggregate union (C1 boundary; SysV union classification separate rule)",
        ));
    }
    // expand field by field (Memory-layout Adt/tuple/array; slice fat already hit in ScalarPair branch above)
    let env = rustc_middle::ty::TypingEnv::fully_monomorphized();
    let layout_of_ty = |t: rustc_middle::ty::Ty<'tcx>| -> Result<_, Error> {
        tcx.layout_of(env.as_query_input(t))
            .map_err(|e| Error::internal(format!("by-value aggregate field layout: {e}")))
    };
    match layout.ty.kind() {
        rustc_middle::ty::TyKind::Array(elem_ty, n) => {
            let n = n.try_to_target_usize(tcx).ok_or(Error::internal(
                "by-value aggregate array length not evaluable",
            ))?;
            let elem_layout = layout_of_ty(*elem_ty)?;
            let stride = elem_layout.layout.size().bytes() as u32;
            for i in 0..n {
                let off = (i as u32).saturating_mul(stride);
                push_agg_field(tcx, &mut fields, off, elem_layout)?;
            }
        }
        rustc_middle::ty::TyKind::Tuple(ts) => {
            for (i, fty) in ts.iter().enumerate() {
                let off = layout.layout.fields.offset(i).bytes() as u32;
                push_agg_field(tcx, &mut fields, off, layout_of_ty(fty)?)?;
            }
        }
        rustc_middle::ty::TyKind::Adt(def, args) => {
            if !def.is_struct() {
                return Err(Error::unsupported(format!(
                    "by-value aggregate {:?} (C1 boundary; Adt other than single-variant struct)",
                    def.adt_kind()
                )));
            }
            let var = def.variant(rustc_abi::VariantIdx::ZERO);
            for (i, f) in var.fields.iter().enumerate() {
                let off = layout.layout.fields.offset(i).bytes() as u32;
                push_agg_field(
                    tcx,
                    &mut fields,
                    off,
                    layout_of_ty(tcx.normalize_erasing_regions(env, f.ty(tcx, args)))?,
                )?;
            }
        }
        other => {
            return Err(Error::unsupported(format!(
                "by-value aggregate type shape {other:?} not supported"
            )));
        }
    }
    let agg = ir::FfiAgg {
        size,
        align,
        fields,
    };
    validate_agg_natural(&agg)?;
    Ok(agg)
}

/// Accept one field leaf (Scalar → leaf; ScalarPair/Memory → recursive nesting; ZST skipped and not counted).
fn push_agg_field<'tcx>(
    tcx: TyCtxt<'tcx>,
    fields: &mut Vec<ir::FfiField>,
    off: u32,
    fl: rustc_middle::ty::layout::TyAndLayout<'tcx>,
) -> Result<(), Error> {
    if fl.layout.is_zst() {
        return Ok(());
    }
    let leaf = if let rustc_abi::BackendRepr::Scalar(s) = fl.backend_repr {
        ir::FfiLeaf::Scalar(
            scalar_ffi_kind(s.primitive())
                .map_err(|p| Error::unsupported(format!("scalar {p:?} not supported")))?,
        )
    } else {
        ir::FfiLeaf::Agg(ffi_agg_of(tcx, fl)?)
    };
    fields.push(ir::FfiField { off, leaf });
    Ok(())
}

/// LLVM verbatim prefix `\x01` (upstream crate adds grouping marker to symbol via `#[link_name = "\u{1}..."]`
/// grouping marker; object/dynamic symbol table stores only **stripped-prefix** name — corpus batch 8 c_aws_lc
/// confirmed aws-lc-sys BORINGSSL_PREFIX whole-symbol family). dlsym lookup must strip the same way,
/// otherwise lookup of `\x01aws_lc_...` will miss globally.
pub(crate) fn canonical_link_name(name: &str) -> &str {
    name.strip_prefix('\x01').unwrap_or(name)
}

/// F-06: libffi natural-layout expressibility check — ffi.rs constructs structure using only field types,
/// frozen off/size/align are not consumed; under non-natural shapes like packed/align(N), libffi
/// computed layout ≠ real layout = silent ABI mis-call. Until full-padding expression is implemented, freeze-time
/// loudly reject inexpressible shapes (recursive field-by-field offset comparison + tail-padding alignment recheck).
fn validate_agg_natural(agg: &ir::FfiAgg) -> Result<(), Error> {
    fn leaf_layout(l: &ir::FfiLeaf) -> Option<(u32, u32)> {
        match l {
            ir::FfiLeaf::Scalar(k) => {
                let n = match k {
                    ir::FfiKind::I8 | ir::FfiKind::U8 => 1,
                    ir::FfiKind::I16 | ir::FfiKind::U16 => 2,
                    ir::FfiKind::I32 | ir::FfiKind::U32 | ir::FfiKind::F32 => 4,
                    ir::FfiKind::I64 | ir::FfiKind::U64 | ir::FfiKind::F64 | ir::FfiKind::Ptr => 8,
                    ir::FfiKind::Void | ir::FfiKind::Agg(_) => return None,
                };
                Some((n, n))
            }
            ir::FfiLeaf::Agg(inner) => natural_layout(inner),
        }
    }
    fn natural_layout(agg: &ir::FfiAgg) -> Option<(u32, u32)> {
        let mut off = 0u32;
        let mut mal = 1u32;
        for f in &agg.fields {
            let (fsz, fal) = leaf_layout(&f.leaf)?;
            let at = off.next_multiple_of(fal);
            if at != f.off {
                return None;
            }
            off = at + fsz;
            mal = mal.max(fal);
        }
        Some((off.next_multiple_of(mal), mal))
    }
    if natural_layout(agg) != Some((agg.size, agg.align)) {
        return Err(Error::unsupported(
            "by-value aggregate non-natural layout (packed/align(N) — inexpressible in libffi type system, F-06 boundary)",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::c_abi_unwind;
    use rustc_abi::ExternAbi;

    #[test]
    fn c_abi_unwind_preserves_plain_and_unwind_variants() {
        assert_eq!(c_abi_unwind(ExternAbi::C { unwind: false }), Some(false));
        assert_eq!(c_abi_unwind(ExternAbi::C { unwind: true }), Some(true));
        assert_eq!(
            c_abi_unwind(ExternAbi::System { unwind: false }),
            Some(false)
        );
        assert_eq!(c_abi_unwind(ExternAbi::System { unwind: true }), Some(true));
        assert_eq!(c_abi_unwind(ExternAbi::Rust), None);
    }
}
