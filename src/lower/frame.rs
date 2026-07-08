//! 帧布局冻结（M4.0 设计 §2）：逐 local 取单态化类型的 layout，对齐 bump 分配帧内偏移。
//! 产物只含 (offset, size, align, 标量性/符号性)——执行相零 tcx。

use rustc_abi::BackendRepr;
use rustc_middle::mir::Body;
use rustc_middle::ty::{Ty, TyCtxt, TypingEnv};

use crate::vm::engine::ir::Width;

/// 一个 local 的冻结信息。
pub struct LocalInfo<'tcx> {
    pub off: u32,
    pub ty: Ty<'tcx>,
    pub size: u64,
    /// 标量宽度（backend_repr 为 Scalar 且 size∈{1,2,4,8} 时 Some）
    pub scalar: Option<Width>,
    pub zst: bool,
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
) -> Result<rustc_middle::ty::layout::TyAndLayout<'tcx>, String> {
    tcx.layout_of(typing_env.as_query_input(ty)).map_err(|e| format!("layout 失败: {e}"))
}

/// 类型的标量宽度（若是 ≤64 位标量）。
pub fn scalar_width(layout: &rustc_middle::ty::layout::TyAndLayout<'_>) -> Option<Width> {
    if !matches!(layout.backend_repr, BackendRepr::Scalar(_)) {
        return None;
    }
    Width::from_bytes(layout.size.bytes())
}

pub fn ty_signed(ty: Ty<'_>) -> bool {
    matches!(ty.kind(), rustc_middle::ty::Int(_))
}

/// 冻结整个函数帧。任一 local 无法布局（不应发生于单态化后）→ 整函数 Trap。
pub fn freeze<'tcx>(
    tcx: TyCtxt<'tcx>,
    typing_env: TypingEnv<'tcx>,
    body: &Body<'tcx>,
) -> Result<FrameLayout<'tcx>, String> {
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
            scalar: scalar_width(&layout),
            zst: layout.is_zst(),
        });
        off = aligned + size as u32;
    }
    Ok(FrameLayout { locals, size: off, align: max_align })
}
