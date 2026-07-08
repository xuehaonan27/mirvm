//! 帧布局冻结（M4.0 设计 §2）：逐 local 取单态化类型的 layout，对齐 bump 分配帧内偏移。
//! 产物只含 (offset, size, align, 值分类)——执行相零 tcx。
//!
//! M4.1：`ValKind` 值分类（ABI v2 与 place 求值共用）——Zst / 标量 / 标量对 / 聚合。

use rustc_abi::{BackendRepr, HasDataLayout};
use rustc_middle::mir::Body;
use rustc_middle::ty::{Ty, TyCtxt, TypingEnv};

use crate::vm::engine::ir::Width;

/// 值分类：决定访问路径与调用约定（frame-abi §3 / m4.1-design F3）。
#[derive(Clone, Copy, Debug)]
pub enum ValKind {
    Zst,
    /// ≤64 位标量（int/ptr/bool/char/float——float 位搬运当标量，算术另有通道）
    Scalar(Width),
    /// 标量对：两半的（相对本值起始的偏移，宽度）
    Pair((u32, Width), (u32, Width)),
    /// 聚合/大标量（u128、SIMD 向量、struct…）：memcpy 通道，带尺寸
    Other { size: u64 },
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

/// 一个 local 的冻结信息。
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
) -> Result<rustc_middle::ty::layout::TyAndLayout<'tcx>, String> {
    tcx.layout_of(typing_env.as_query_input(ty)).map_err(|e| format!("layout 失败: {e}"))
}

/// layout → 值分类。ScalarPair 两半偏移按 codegen 同款公式
/// （b_off = a.size.align_to(b.align)，rustc_codegen_ssa operand.rs）。
pub fn classify(tcx: TyCtxt<'_>, layout: &rustc_middle::ty::layout::TyAndLayout<'_>) -> ValKind {
    if layout.is_zst() {
        return ValKind::Zst;
    }
    match layout.backend_repr {
        BackendRepr::Scalar(_) => match Width::from_bytes(layout.size.bytes()) {
            Some(w) => ValKind::Scalar(w),
            // u128/i128：memcpy 通道位搬运；算术是第 3 步
            None => ValKind::Other { size: layout.size.bytes() },
        },
        BackendRepr::ScalarPair(a, b) => {
            let dl = tcx.data_layout();
            let a_size = a.size(dl);
            let b_off = a_size.align_to(b.default_align(dl).abi);
            let (Some(aw), Some(bw)) =
                (Width::from_bytes(a_size.bytes()), Width::from_bytes(b.size(dl).bytes()))
            else {
                return ValKind::Other { size: layout.size.bytes() };
            };
            ValKind::Pair((0, aw), (b_off.bytes() as u32, bw))
        }
        _ => ValKind::Other { size: layout.size.bytes() },
    }
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
        locals.push(LocalInfo { off: aligned, ty, size, kind: classify(tcx, &layout) });
        off = aligned + size as u32;
    }
    Ok(FrameLayout { locals, size: off, align: max_align })
}
