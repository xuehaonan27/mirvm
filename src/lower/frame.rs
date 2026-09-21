//! Frame layout freezing: lay out each local's monomorphized type and bump-allocate its
//! frame offset with alignment. The result carries only (offset, size, align, value
//! class), so the execution phase needs no `tcx`.
//!
//! `ValKind` classifies a value for the call ABI and for place evaluation:
//! Zst / scalar / scalar pair / aggregate.

use crate::lower::Error;

use rustc_abi::{BackendRepr, HasDataLayout};
use rustc_middle::mir::Body;
use rustc_middle::ty::{Ty, TyCtxt, TypingEnv};

use crate::vm::ir::Width;

/// Value class: decides the access path and the calling convention.
#[derive(Clone, Copy, Debug)]
pub enum ValKind {
    Zst,
    /// Scalar of at most 64 bits (int/ptr/bool/char/float). A float travels as bits here;
    /// float arithmetic has its own lane.
    Scalar(Width),
    /// Scalar pair: each half's (offset from the value start, width).
    Pair((u32, Width), (u32, Width)),
    /// Aggregate or oversized scalar (u128, SIMD vector, struct, ...): the memcpy lane,
    /// carrying its size.
    Other {
        size: u64,
    },
}

impl ValKind {
    pub fn is_zst(&self) -> bool {
        matches!(self, ValKind::Zst)
    }
    pub fn scalar(&self) -> Option<Width> {
        match self {
            ValKind::Scalar(w) => Some(*w),
            _ => None,
        }
    }
}

/// One local's frozen layout.
pub struct LocalInfo<'tcx> {
    pub off: u32,
    pub ty: Ty<'tcx>,
    pub size: u64,
    pub kind: ValKind,
}

pub struct FrameLayout<'tcx> {
    pub locals: Vec<LocalInfo<'tcx>>,
    pub size: u32,
    pub align: u32,
}

pub fn layout_of<'tcx>(
    tcx: TyCtxt<'tcx>,
    typing_env: TypingEnv<'tcx>,
    ty: Ty<'tcx>,
) -> Result<rustc_middle::ty::layout::TyAndLayout<'tcx>, Error> {
    tcx.layout_of(typing_env.as_query_input(ty))
        .map_err(|e| Error::internal(format!("layout failed: {e}")))
}

/// Derive the value class from a layout. The scalar-pair half offset uses the same
/// formula as codegen: `b_off = a.size.align_to(b.align)`.
pub fn classify(tcx: TyCtxt<'_>, layout: &rustc_middle::ty::layout::TyAndLayout<'_>) -> ValKind {
    if layout.is_zst() {
        return ValKind::Zst;
    }
    match layout.backend_repr {
        BackendRepr::Scalar(_) => match Width::from_bytes(layout.size.bytes()) {
            Some(w) => ValKind::Scalar(w),
            // u128/i128: moved as bits through the memcpy lane; arithmetic is separate
            None => ValKind::Other {
                size: layout.size.bytes(),
            },
        },
        BackendRepr::ScalarPair(a, b) => {
            let dl = tcx.data_layout();
            let a_size = a.size(dl);
            let b_off = a_size.align_to(b.default_align(dl).abi);
            let (Some(aw), Some(bw)) = (
                Width::from_bytes(a_size.bytes()),
                Width::from_bytes(b.size(dl).bytes()),
            ) else {
                return ValKind::Other {
                    size: layout.size.bytes(),
                };
            };
            ValKind::Pair((0, aw), (b_off.bytes() as u32, bw))
        }
        _ => ValKind::Other {
            size: layout.size.bytes(),
        },
    }
}

/// The type's scalar width, when it is a scalar of at most 64 bits.
pub fn scalar_width(layout: &rustc_middle::ty::layout::TyAndLayout<'_>) -> Option<Width> {
    if !matches!(layout.backend_repr, BackendRepr::Scalar(_)) {
        return None;
    }
    Width::from_bytes(layout.size.bytes())
}

pub fn ty_signed(ty: Ty<'_>) -> bool {
    matches!(ty.kind(), rustc_middle::ty::Int(_))
}

/// Freeze the whole function frame. A local that cannot be laid out (which should not
/// happen after monomorphization) traps the whole function.
pub fn freeze<'tcx>(
    tcx: TyCtxt<'tcx>,
    typing_env: TypingEnv<'tcx>,
    body: &Body<'tcx>,
) -> Result<FrameLayout<'tcx>, Error> {
    let mut locals = Vec::with_capacity(body.local_decls.len());
    let mut off = 0u32;
    let mut max_align = 1u32;
    for decl in body.local_decls.iter() {
        let ty = decl.ty;
        let layout = layout_of(tcx, typing_env, ty)?;
        let size = layout.size.bytes();
        let align = layout.align.abi.bytes() as u32;
        max_align = max_align.max(align);
        let aligned = (off + align.max(1) - 1) & !(align.max(1) - 1);
        locals.push(LocalInfo {
            off: aligned,
            ty,
            size,
            kind: classify(tcx, &layout),
        });
        off = aligned + size as u32;
    }
    Ok(FrameLayout {
        locals,
        size: off,
        align: max_align,
    })
}
