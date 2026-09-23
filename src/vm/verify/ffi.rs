//! What a foreign call signature must satisfy, and the libffi aggregate and scalar layout that a
//! marshalled argument is handed over under.

use super::*;

impl<'a> Verifier<'a> {
    pub(super) fn foreign_sig(&self, sig: &ForeignSig) -> Result<(), String> {
        if sig.fixed.is_some_and(|n| n > sig.args.len()) {
            return Err(format!(
                "variadic fixed count exceeds argument count {}",
                sig.args.len()
            ));
        }
        for (i, kind) in sig.args.iter().enumerate() {
            if matches!(kind, FfiKind::Void) {
                return Err(format!("argument {i} has void type"));
            }
            self.ffi_kind(kind)
                .map_err(|e| format!("argument {i}: {e}"))?;
        }
        self.ffi_kind(&sig.ret)
            .map_err(|e| format!("return type: {e}"))?;
        for (i, (arg, nested)) in sig.thunk_args.iter().enumerate() {
            if *arg >= sig.args.len() {
                return Err(format!(
                    "callback entry {i} refers to missing argument {arg}"
                ));
            }
            if !matches!(sig.args[*arg], FfiKind::Ptr) {
                return Err(format!("callback argument {arg} is not a pointer"));
            }
            if !nested.thunk_args.is_empty() {
                return Err(format!("callback argument {arg} contains nested callbacks"));
            }
            self.foreign_sig(nested)
                .map_err(|e| format!("callback argument {arg}: {e}"))?;
        }
        Ok(())
    }

    pub(super) fn ffi_kind(&self, kind: &FfiKind) -> Result<(), String> {
        match kind {
            FfiKind::Agg(agg) => ffi_agg(agg).map(|_| ()),
            FfiKind::I8
            | FfiKind::I16
            | FfiKind::I32
            | FfiKind::I64
            | FfiKind::U8
            | FfiKind::U16
            | FfiKind::U32
            | FfiKind::U64
            | FfiKind::F32
            | FfiKind::F64
            | FfiKind::Ptr
            | FfiKind::Void => Ok(()),
        }
    }
}

pub(super) fn ffi_agg(agg: &FfiAgg) -> Result<(u32, u32), String> {
    if agg.align == 0 || agg.align > 8 || !agg.align.is_power_of_two() {
        return Err(format!("aggregate has invalid alignment {}", agg.align));
    }
    let mut off = 0u32;
    let mut align = 1u32;
    for (i, field) in agg.fields.iter().enumerate() {
        let (size, field_align) = match &field.leaf {
            FfiLeaf::Scalar(kind) => ffi_scalar_layout(kind)
                .ok_or_else(|| format!("aggregate field {i} is not a scalar FFI value"))?,
            FfiLeaf::Agg(inner) => {
                ffi_agg(inner).map_err(|e| format!("aggregate field {i}: {e}"))?
            }
        };
        let expected = off
            .checked_add(field_align - 1)
            .map(|n| n & !(field_align - 1))
            .ok_or_else(|| format!("aggregate field {i} alignment overflows"))?;
        if field.off != expected {
            return Err(format!(
                "aggregate field {i} offset {} is not natural offset {expected}",
                field.off
            ));
        }
        off = field
            .off
            .checked_add(size)
            .ok_or_else(|| format!("aggregate field {i} range overflows"))?;
        align = align.max(field_align);
    }
    let size = off
        .checked_add(align - 1)
        .map(|n| n & !(align - 1))
        .ok_or("aggregate size overflows")?;
    if (size, align) != (agg.size, agg.align) {
        return Err(format!(
            "aggregate layout computes as size/alignment {size}/{align}, encoded as {}/{}",
            agg.size, agg.align
        ));
    }
    Ok((size, align))
}

pub(super) fn ffi_scalar_layout(kind: &FfiKind) -> Option<(u32, u32)> {
    let n = match kind {
        FfiKind::I8 | FfiKind::U8 => 1,
        FfiKind::I16 | FfiKind::U16 => 2,
        FfiKind::I32 | FfiKind::U32 | FfiKind::F32 => 4,
        FfiKind::I64 | FfiKind::U64 | FfiKind::F64 | FfiKind::Ptr => 8,
        FfiKind::Void | FfiKind::Agg(_) => return None,
    };
    Some((n, n))
}
