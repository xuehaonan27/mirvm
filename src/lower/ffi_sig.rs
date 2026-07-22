//! FFI 签名推导自由函数（自 lower/mod.rs M6 整搬）：freeze_c_fnptr_sig/
//! ffi_kind_of/scalar_ffi_kind/ffi_agg_of/push_agg_field/canonical_link_name
//! （extern "C" 系 fn-ptr 类型 → 冻结 ForeignSig；不可派生 = None 的判据面）。

use super::*;

/// extern "C" 系 fn-ptr 类型 → 冻结 ForeignSig（M4.4 FFI 反方向之二：调用点带上，
/// 执行期条目反查未命中 = guest 持 native 真码 → libffi 按此直调）。
/// None = Rust ABI / 变参 / 参数不可类——该调用点只能派发 guest 条目（未命中即诊断）。
pub(crate) fn freeze_c_fnptr_sig<'tcx>(
    tcx: TyCtxt<'tcx>,
    env: TypingEnv<'tcx>,
    ty: rustc_middle::ty::Ty<'tcx>,
) -> Option<ir::ForeignSig> {
    use rustc_abi::ExternAbi;
    let sig = ty.fn_sig(tcx).skip_binder();
    // F-09（2026-07-22 实锤反转）：C/C-unwind 均收——unwind 属性保全进
    // ForeignSig.unwind（接受是「读过的」，不是「没看见」）。callback 形
    // panic 仍 abort 于 nounwind trampoline 边界（libffi 闭包无 unwind
    // info 原理阻塞，R18 记档）；longjmp 形机器层不受 ABI 属性影响。
    if !matches!(sig.abi(), ExternAbi::C { .. } | ExternAbi::System { .. }) || sig.c_variadic() {
        return None;
    }
    let unwind = matches!(
        sig.abi(),
        ExternAbi::C { unwind: true } | ExternAbi::System { unwind: true }
    );
    let mut args = Vec::with_capacity(sig.inputs().len());
    for &t in sig.inputs() {
        let k = ffi_kind_of(tcx, env, t).ok()?;
        if k == ir::FfiKind::Void {
            return None; // ZST 不可作 cif 参数
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

/// 类型 → libffi 直通类别（标量与指针；ZST=Void 仅返回位；C1：按值聚合放开）。
pub(crate) fn ffi_kind_of<'tcx>(
    tcx: TyCtxt<'tcx>,
    env: TypingEnv<'tcx>,
    ty: rustc_middle::ty::Ty<'tcx>,
) -> Result<ir::FfiKind, String> {
    use rustc_abi::BackendRepr;
    let layout = tcx
        .layout_of(env.as_query_input(ty))
        .map_err(|e| format!("layout 失败: {e}"))?;
    if layout.is_zst() {
        return Ok(ir::FfiKind::Void);
    }
    if let BackendRepr::Scalar(s) = layout.backend_repr {
        return match scalar_ffi_kind(s.primitive()) {
            Ok(k) => Ok(k),
            Err(other) => Err(format!("标量 {other:?} 不支持")),
        };
    }
    Ok(ir::FfiKind::Agg(ffi_agg_of(tcx, layout)?))
}

/// 标量 primitive → FfiKind（沿用 C1 前的映射表）。
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

/// C1：rustc layout → 冻结聚合（声明序字段 + 嵌套递归；数组 = 元素重复字段；
/// ZST 成员不入列但 padding 由 size/off 保持）。响亮拒绝边界：union、SIMD 向量、
/// unsized（Err 文案可鉴红分类）。
fn ffi_agg_of<'tcx>(
    tcx: TyCtxt<'tcx>,
    layout: rustc_middle::ty::layout::TyAndLayout<'tcx>,
) -> Result<ir::FfiAgg, String> {
    use rustc_abi::BackendRepr;
    let size = layout.layout.size().bytes() as u32;
    let align = layout.layout.align().abi.bytes() as u32;
    if align > 8 {
        return Err(format!("按值聚合 align={align} > 8（C1 边界）"));
    }
    let mut fields = Vec::new();
    // ScalarPair（{ptr,len} / 两标量字段形态）：两叶按 primitive 直出
    if let BackendRepr::ScalarPair(a, b) = layout.backend_repr {
        let (ao, bo) = (
            layout.fields.offset(0).bytes() as u32,
            layout.fields.offset(1).bytes() as u32,
        );
        fields.push(ir::FfiField {
            off: ao,
            leaf: ir::FfiLeaf::Scalar(
                scalar_ffi_kind(a.primitive()).map_err(|p| format!("标量 {p:?} 不支持"))?,
            ),
        });
        fields.push(ir::FfiField {
            off: bo,
            leaf: ir::FfiLeaf::Scalar(
                scalar_ffi_kind(b.primitive()).map_err(|p| format!("标量 {p:?} 不支持"))?,
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
            return Err(format!(
                "按值聚合布局形态 {:?} 不支持（C1 边界；SIMD 另轴）",
                layout.backend_repr
            ));
        }
    }
    if layout.ty.is_union() {
        return Err("按值聚合 union（C1 边界；SysV union 分类另规则）".into());
    }
    // 逐字段展开（Memory 布局的 Adt/tuple/array；slice fat 已在上行 ScalarPair 命中）
    let env = rustc_middle::ty::TypingEnv::fully_monomorphized();
    let layout_of_ty = |t: rustc_middle::ty::Ty<'tcx>| -> Result<_, String> {
        tcx.layout_of(env.as_query_input(t))
            .map_err(|e| format!("按值聚合字段 layout: {e}"))
    };
    match layout.ty.kind() {
        rustc_middle::ty::TyKind::Array(elem_ty, n) => {
            let n = n.try_to_target_usize(tcx).ok_or("按值聚合数组长度不求值")?;
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
                return Err(format!(
                    "按值聚合 {:?}（C1 边界；single-variant struct 外的 Adt）",
                    def.adt_kind()
                ));
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
        other => return Err(format!("按值聚合类型形态 {other:?} 不支持")),
    }
    let agg = ir::FfiAgg {
        size,
        align,
        fields,
    };
    validate_agg_natural(&agg)?;
    Ok(agg)
}

/// 收一个字段叶（Scalar → 叶；ScalarPair/Memory → 递归嵌套；ZST 跳过不占列）。
fn push_agg_field<'tcx>(
    tcx: TyCtxt<'tcx>,
    fields: &mut Vec<ir::FfiField>,
    off: u32,
    fl: rustc_middle::ty::layout::TyAndLayout<'tcx>,
) -> Result<(), String> {
    if fl.layout.is_zst() {
        return Ok(());
    }
    let leaf = if let rustc_abi::BackendRepr::Scalar(s) = fl.backend_repr {
        ir::FfiLeaf::Scalar(
            scalar_ffi_kind(s.primitive()).map_err(|p| format!("标量 {p:?} 不支持"))?,
        )
    } else {
        ir::FfiLeaf::Agg(ffi_agg_of(tcx, fl)?)
    };
    fields.push(ir::FfiField { off, leaf });
    Ok(())
}

/// LLVM verbatim 前缀 `\x01`（上游 crate 经 `#[link_name = "\u{1}..."]` 给符号
/// 加的分组标记；目标文件/动态符号表只存**去前缀**名——corpus 批8 c_aws_lc
/// 实锤的 aws-lc-sys BORINGSSL_PREFIX 全符号家族）。dlsym 查找口径必须同剥，
/// 否则查 `\x01aws_lc_...` 必然全域未命中。
pub(crate) fn canonical_link_name(name: &str) -> &str {
    name.strip_prefix('\x01').unwrap_or(name)
}

/// F-06：libffi 自然布局可表达性校验——ffi.rs 构造 structure 只喂字段类型，
/// 冻结的 off/size/align 不被消费；packed/align(N) 等非自然形态下 libffi
/// 算出的布局 ≠ 真实布局 = 静默 ABI 错调。实现全 padding 表达之前，冻结期
/// 对不可表达形态响亮拒绝（递归逐字段比对偏移 + 尾 padding 对齐复核）。
fn validate_agg_natural(agg: &ir::FfiAgg) -> Result<(), String> {
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
        return Err(
            "按值聚合非自然布局（packed/align(N)——libffi 类型系统不可表达，F-06 边界）".into(),
        );
    }
    Ok(())
}
