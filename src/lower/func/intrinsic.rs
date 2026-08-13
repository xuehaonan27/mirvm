//! intrinsic 就地展开（自 func.rs F13+F14+F16 整搬）：try_expand_intrinsic
//! ~60 名分派（atomic/volatile/copy/ct*/math/fma/fast-float/ptr/size_of/
//! saturating/caller_location/compare_bytes 族）+ float 路由/atomic_ord +
//! F16 自由小件（elem_of/float_w/LayoutCxAt 等，pub(super) 供全树）。

use super::*;

impl<'tcx> LowerCx<'tcx, '_> {
    pub(super) fn resolve_float_route(
        &self,
        suffix: Option<FloatSuffix>,
        inst: &Instance<'tcx>,
        n: &str,
    ) -> Result<FloatRoute, String> {
        let suffix = match suffix {
            Some(sfx) => sfx,
            None => {
                let t = inst.args.type_at(0);
                match t.kind() {
                    ty::Float(ty::FloatTy::F16) => FloatSuffix::F16,
                    ty::Float(ty::FloatTy::F32) => FloatSuffix::F32,
                    ty::Float(ty::FloatTy::F64) => FloatSuffix::F64,
                    ty::Float(ty::FloatTy::F128) => FloatSuffix::F128,
                    _ => return Err(format!("intrinsic `{n}` 泛型参数非浮点（{t}）")),
                }
            }
        };
        Ok(match suffix {
            FloatSuffix::F16 => FloatRoute::Scalar(ir::FloatW::F16),
            FloatSuffix::F32 => FloatRoute::Scalar(ir::FloatW::F32),
            FloatSuffix::F64 => FloatRoute::Scalar(ir::FloatW::F64),
            FloatSuffix::F128 => FloatRoute::Wide128,
        })
    }

    /// atomic intrinsic 的 const 泛型序 → 冻结 MemOrd（cg_ssa parse_atomic_ordering
    /// 同构：valtree 分支[0] 是判别式叶；D8j）。**按位置收集全部 const 泛参**而非
    /// 硬编码下标——本 nightly 各 atomic intrinsic 的类型参数量异构（xadd<T,U,ORD>
    /// vs load<T,ORD>），序参数是其中唯一的 const（cxchg 两个：succ, fail）。
    pub(super) fn atomic_ord(
        &self,
        inst: &Instance<'tcx>,
        nth: usize,
    ) -> Result<ir::MemOrd, String> {
        use rustc_middle::ty::AtomicOrdering as A;
        let c = inst
            .args
            .iter()
            .filter_map(|a| a.as_const())
            .nth(nth)
            .ok_or_else(|| format!("atomic intrinsic 缺第 {nth} 个 const 序参数"))?;
        let discr = c.to_value().to_branch()[0].to_leaf();
        Ok(match discr.to_atomic_ordering() {
            A::Relaxed => ir::MemOrd::Relaxed,
            A::Acquire => ir::MemOrd::Acquire,
            A::Release => ir::MemOrd::Release,
            A::AcqRel => ir::MemOrd::AcqRel,
            A::SeqCst => ir::MemOrd::SeqCst,
        })
    }

    pub(super) fn try_expand_intrinsic(
        &mut self,
        inst: &Instance<'tcx>,
        args: &[rustc_span::Spanned<mir::Operand<'tcx>>],
        destination: &mir::Place<'tcx>,
        target: Option<mir::BasicBlock>,
        unwind: mir::UnwindAction,
        span: rustc_span::Span,
    ) -> Result<Option<(Vec<Stmt>, Terminator)>, String> {
        let InstanceKind::Intrinsic(def_id) = inst.def else {
            return Ok(None);
        };
        let name = self.tcx.item_name(def_id);
        // 引擎级 intrinsic（非纯值展开）：catch_unwind 走 Builtin 通道
        if name.as_str() == "catch_unwind" {
            let (dst_p, w) = self.place_scalar(destination)?;
            let tgt = target.ok_or("catch_unwind 发散？")?.as_u32();
            let role = self
                .linker
                .main_catch_site
                .filter(|site| {
                    site.catcher_caller == self.instance && site.catcher_intrinsic == *inst
                })
                .map_or(ir::BuiltinCallRole::Normal, |_| {
                    ir::BuiltinCallRole::MainPanicCatcher
                });
            return Ok(Some((
                vec![],
                Terminator::CallBuiltin {
                    builtin: ir::Builtin::CatchUnwind,
                    args: vec![
                        self.lower_operand_scalar(&args[0].node)?,
                        self.lower_operand_scalar(&args[1].node)?,
                        self.lower_operand_scalar(&args[2].node)?,
                    ],
                    ret: RetDest::Scalar(dst_p.scalar_place(w)),
                    target: tgt,
                    unwind: self.lower_unwind(unwind),
                    role,
                },
            )));
        }
        // 泛型元素尺寸（copy/write_bytes/offset 的 T）
        let elem_size = |cx: &Self| -> Result<u64, String> {
            let t = inst.args.type_at(0);
            Ok(cx.layout_of(t)?.size.bytes())
        };
        let stmts = match name.as_str() {
            "offset" | "arith_offset" => {
                // fn offset<Ptr, Delta>(ptr: Ptr, count: Delta) -> Ptr
                let ptr_ty = self.op_ty(&args[0].node)?;
                let pointee = ptr_ty
                    .builtin_deref(true)
                    .ok_or_else(|| format!("offset 非指针（ty={ptr_ty}）"))?;
                let stride = self.layout_of(pointee)?.size.bytes();
                let (dst_p, w) = self.place_scalar(destination)?;
                vec![Stmt::Assign {
                    dst: dst_p.scalar_place(w),
                    rv: Rvalue::PtrOffset {
                        ptr: self.lower_operand_scalar(&args[0].node)?,
                        count: self.lower_operand_scalar(&args[1].node)?,
                        stride,
                    },
                }]
            }
            "ctpop" | "ctlz" | "cttz" | "ctlz_nonzero" | "cttz_nonzero" | "bswap"
            | "bitreverse" => {
                use ir::BitUnOp as B;
                let op = match name.as_str() {
                    "ctpop" => B::Popcount,
                    "ctlz" | "ctlz_nonzero" => B::Ctlz,
                    "cttz" | "cttz_nonzero" => B::Cttz,
                    "bswap" => B::Bswap,
                    _ => B::Bitreverse,
                };
                // 128 位（D8k）：ctpop/ctlz/cttz 结果是 u32（≤128 装 u32）；bswap/
                // bitreverse 结果仍是 128 位。走宽通道 Bit128（源 16 字节 place）。
                let a_ty = self.op_ty(&args[0].node)?;
                if self.layout_of(a_ty)?.size.bytes() == 16 {
                    let src = self.wide_place(&args[0].node)?;
                    return Ok(Some((
                        if matches!(op, B::Bswap | B::Bitreverse) {
                            vec![Stmt::Bit128 {
                                op,
                                src,
                                dst: self.resolve_place(destination)?.expr(),
                            }]
                        } else {
                            // 计数类：结果标量（u32），走 Bit128Count
                            let (dst_p, w) = self.place_scalar(destination)?;
                            vec![Stmt::Bit128Count {
                                op,
                                src,
                                dst: dst_p.scalar_place(w),
                            }]
                        },
                        Terminator::Goto(target.ok_or("bit intrinsic 发散？")?.as_u32()),
                    )));
                }
                let (dst_p, w) = self.place_scalar(destination)?;
                vec![Stmt::Assign {
                    dst: dst_p.scalar_place(w),
                    rv: Rvalue::BitUn {
                        op,
                        a: self.lower_operand_scalar(&args[0].node)?,
                    },
                }]
            }
            "exact_div" => {
                let a_ty = self.op_ty(&args[0].node)?;
                let (dst_p, w) = self.place_scalar(destination)?;
                vec![Stmt::Assign {
                    dst: dst_p.scalar_place(w),
                    rv: Rvalue::IntBin {
                        op: IntBinOp::Div,
                        signed: frame::ty_signed(a_ty),
                        a: self.lower_operand_scalar(&args[0].node)?,
                        b: self.lower_operand_scalar(&args[1].node)?,
                    },
                }]
            }
            "atomic_load" => {
                let order = self.atomic_ord(inst, 0)?;
                if matches!(order, ir::MemOrd::Release | ir::MemOrd::AcqRel) {
                    return Err(format!("atomic_load 非法序 {order:?}"));
                }
                let (dst_p, w) = self.place_scalar(destination)?;
                vec![Stmt::Assign {
                    dst: dst_p.scalar_place(w),
                    rv: Rvalue::AtomicLoad {
                        addr: self.lower_operand_scalar(&args[0].node)?,
                        width: w,
                        order,
                    },
                }]
            }
            "atomic_store" => {
                let order = self.atomic_ord(inst, 0)?;
                if matches!(order, ir::MemOrd::Acquire | ir::MemOrd::AcqRel) {
                    return Err(format!("atomic_store 非法序 {order:?}"));
                }
                vec![Stmt::AtomicStore {
                    addr: self.lower_operand_scalar(&args[0].node)?,
                    val: self.lower_operand_scalar(&args[1].node)?,
                    order,
                }]
            }
            "atomic_cxchg" | "atomic_cxchgweak" => {
                // (ptr, expected, new) -> (T, bool)
                let dst_p = self.resolve_place(destination)?;
                let ValKind::Pair((vo, vw), (fo, fw)) = self.classify(dst_p.ty)? else {
                    return Err("cxchg 目标非 pair".into());
                };
                let succ = self.atomic_ord(inst, 0)?;
                let fail = self.atomic_ord(inst, 1)?;
                if matches!(fail, ir::MemOrd::Release | ir::MemOrd::AcqRel) {
                    return Err(format!("cxchg 失败序非法 {fail:?}"));
                }
                vec![Stmt::AtomicCxchg {
                    addr: self.lower_operand_scalar(&args[0].node)?,
                    expected: self.lower_operand_scalar(&args[1].node)?,
                    new: self.lower_operand_scalar(&args[2].node)?,
                    dst_val: dst_p.half_place(vo, vw),
                    dst_ok: dst_p.half_place(fo, fw),
                    weak: name.as_str() == "atomic_cxchgweak",
                    succ,
                    fail,
                }]
            }
            "atomic_xchg" | "atomic_xadd" | "atomic_xsub" | "atomic_and" | "atomic_or"
            | "atomic_xor" | "atomic_nand" | "atomic_max" | "atomic_min" | "atomic_umax"
            | "atomic_umin" => {
                use ir::RmwOp as R;
                let op = match name.as_str() {
                    "atomic_xchg" => R::Xchg,
                    "atomic_xadd" => R::Add,
                    "atomic_xsub" => R::Sub,
                    "atomic_and" => R::And,
                    "atomic_or" => R::Or,
                    "atomic_xor" => R::Xor,
                    // fetch_max/min（D8i）：有符号性由 intrinsic 名冻结进变体
                    "atomic_max" => R::Max,
                    "atomic_min" => R::Min,
                    "atomic_umax" => R::UMax,
                    "atomic_umin" => R::UMin,
                    _ => R::Nand,
                };
                let (dst_p, w) = self.place_scalar(destination)?;
                vec![Stmt::AtomicRmw {
                    op,
                    addr: self.lower_operand_scalar(&args[0].node)?,
                    val: self.lower_operand_scalar(&args[1].node)?,
                    dst: dst_p.scalar_place(w),
                    order: self.atomic_ord(inst, 0)?,
                }]
            }
            // fence（M4.4 D4 补真；D8j 序贯通）
            "atomic_fence" => vec![Stmt::Fence {
                single_thread: false,
                order: self.atomic_ord(inst, 0)?,
            }],
            "atomic_singlethreadfence" => vec![Stmt::Fence {
                single_thread: true,
                order: self.atomic_ord(inst, 0)?,
            }],
            // volatile 是 RAM 可观察行为，必须一路保留到执行器。值始终使用
            // alignment=1 的 opaque MaybeUninit 字节载体：既不对 `[u8; N]`
            // 施加错误的整数对齐，也不把聚合值的未初始化 padding 读成宿主值。
            // rustc 对 memory-repr store 本身会发 volatile memcpy；宽值由执行器
            // 按后端可承载的块分解，标量常用宽度仍保持一个 volatile 事件。
            "volatile_load" | "unaligned_volatile_load" => {
                let t = inst.args.type_at(0);
                let size = u32::try_from(self.layout_of(t)?.size.bytes())
                    .map_err(|_| format!("{name} 类型 {t} 大小超出 u32"))?;
                if size == 0 {
                    vec![Stmt::Nop]
                } else {
                    vec![Stmt::VolatileLoad {
                        addr: self.lower_operand_scalar(&args[0].node)?,
                        dst: self.resolve_place(destination)?.expr(),
                        size,
                    }]
                }
            }
            "volatile_store" | "unaligned_volatile_store" => {
                let t = inst.args.type_at(0);
                let size = u32::try_from(self.layout_of(t)?.size.bytes())
                    .map_err(|_| format!("{name} 类型 {t} 大小超出 u32"))?;
                if size == 0 {
                    vec![Stmt::Nop]
                } else {
                    let addr = self.lower_operand_scalar(&args[0].node)?;
                    let mut stmts = Vec::new();
                    let src = if let Some(place) = args[1].node.place() {
                        self.resolve_place(&place)?.expr()
                    } else {
                        let value = self.lower_operand(&args[1].node)?;
                        let kind = self.classify(t)?;
                        let scratch = self.scratch_place(t)?;
                        stmts.extend(self.assign_lowered(&scratch, kind, value)?);
                        scratch.expr()
                    };
                    stmts.push(Stmt::VolatileStore { addr, src, size });
                    stmts
                }
            }
            "copy_nonoverlapping" | "copy" => {
                // (src, dst, count)——注意顺序与 C memcpy 相反
                vec![Stmt::MemCopy {
                    src: self.lower_operand_scalar(&args[0].node)?,
                    dst: self.lower_operand_scalar(&args[1].node)?,
                    count: self.lower_operand_scalar(&args[2].node)?,
                    elem_size: elem_size(self)?,
                    overlap: name.as_str() == "copy",
                }]
            }
            // volatile 批量访存（D8i）：LLVM volatile memcpy/memset 对访问宽度/次数
            // 本就无承诺，volatile 只保证"不可省略/不可重排合并"——解释器逐条执行
            // 从不省略，故与 copy/write_bytes 同一执行通道即为忠实实现。
            // 注意实参顺序：这族是 (dst, src, count)，与 copy 的 (src, dst, count) 相反。
            "volatile_copy_memory" | "volatile_copy_nonoverlapping_memory" => {
                vec![Stmt::MemCopy {
                    dst: self.lower_operand_scalar(&args[0].node)?,
                    src: self.lower_operand_scalar(&args[1].node)?,
                    count: self.lower_operand_scalar(&args[2].node)?,
                    elem_size: elem_size(self)?,
                    overlap: name.as_str() == "volatile_copy_memory",
                }]
            }
            "volatile_set_memory" => {
                vec![Stmt::MemSet {
                    dst: self.lower_operand_scalar(&args[0].node)?,
                    val: self.lower_operand_scalar(&args[1].node)?,
                    count: self.lower_operand_scalar(&args[2].node)?,
                    elem_size: elem_size(self)?,
                }]
            }
            // 非临时 store（D8i，stdarch _mm_stream_* 汇入）：NT 是绕缓存的性能 hint，
            // 值语义 = 普通 store；其与 fence 的弱序注意事项是 guest 的既有义务。
            // 走 volatile store 通道（防省略的超集保证，宽值分块规则一致）。
            "nontemporal_store" => {
                let t = inst.args.type_at(0);
                let size = u32::try_from(self.layout_of(t)?.size.bytes())
                    .map_err(|_| format!("{name} 类型 {t} 大小超出 u32"))?;
                if size == 0 {
                    vec![Stmt::Nop]
                } else {
                    let addr = self.lower_operand_scalar(&args[0].node)?;
                    let mut stmts = Vec::new();
                    let src = if let Some(place) = args[1].node.place() {
                        self.resolve_place(&place)?.expr()
                    } else {
                        let value = self.lower_operand(&args[1].node)?;
                        let kind = self.classify(t)?;
                        let scratch = self.scratch_place(t)?;
                        stmts.extend(self.assign_lowered(&scratch, kind, value)?);
                        scratch.expr()
                    };
                    stmts.push(Stmt::VolatileStore { addr, src, size });
                    stmts
                }
            }
            // ptr.mask(m)（D8i）：地址位与，provenance 不变（真实地址下位与即语义）。
            "ptr_mask" => {
                let (dst_p, w) = self.place_scalar(destination)?;
                vec![Stmt::Assign {
                    dst: dst_p.scalar_place(w),
                    rv: Rvalue::IntBin {
                        op: IntBinOp::BitAnd,
                        signed: false,
                        a: self.lower_operand_scalar(&args[0].node)?,
                        b: self.lower_operand_scalar(&args[1].node)?,
                    },
                }]
            }
            // vtable 槽直读（D8i）：实参是裸 vtable 指针（DynMetadata::size_of/align_of）。
            // 槽布局 [drop, size, align, ...] 与 size_of_val 的 dyn 臂同一来源。
            "vtable_size" | "vtable_align" => {
                let vt = self.lower_operand_scalar(&args[0].node)?;
                let slot_off = if name.as_str() == "vtable_size" {
                    8
                } else {
                    16
                };
                let (dst_p, w) = self.place_scalar(destination)?;
                vec![Stmt::Assign {
                    dst: dst_p.scalar_place(w),
                    rv: Rvalue::Use(operand_deref_at(vt, slot_off)?),
                }]
            }
            // nullary 类型查询保险臂（D8i）：正常被 GVN 常量折叠消解，残留时在
            // lower 期以 tcx 折成常量（与 rustc eval_nullary_intrinsic 同构）。
            // type_id/type_name/offset_of/field_offset 不在此列：TypeId 本 nightly 是
            // vtable 身份结构、type_name 需字符串物化——残留仍响亮 Trap（重开条件：
            // 真实程序在非默认 mir-opt 下撞到）。
            "size_of" | "align_of" | "min_align_of" => {
                let t = inst.args.type_at(0);
                let l = self.layout_of(t)?;
                let v = if name.as_str() == "size_of" {
                    l.size.bytes()
                } else {
                    l.align.abi.bytes()
                };
                let (dst_p, w) = self.place_scalar(destination)?;
                vec![Stmt::Assign {
                    dst: dst_p.scalar_place(w),
                    rv: Rvalue::Use(Operand::Imm { bits: v, width: w }),
                }]
            }
            "variant_count" => {
                // rustc eval_nullary_intrinsic 同构：Pat 剥到 base；Adt=变体数；
                // 其余具体类型=0
                let mut t = inst.args.type_at(0);
                while let ty::Pat(base, _) = t.kind() {
                    t = *base;
                }
                let v = match t.kind() {
                    ty::Adt(adt, _) => adt.variants().len() as u64,
                    _ => 0,
                };
                let (dst_p, w) = self.place_scalar(destination)?;
                vec![Stmt::Assign {
                    dst: dst_p.scalar_place(w),
                    rv: Rvalue::Use(Operand::Imm { bits: v, width: w }),
                }]
            }
            "needs_drop" => {
                let t = inst.args.type_at(0);
                let v = t.needs_drop(self.tcx, self.typing_env) as u64;
                let (dst_p, w) = self.place_scalar(destination)?;
                vec![Stmt::Assign {
                    dst: dst_p.scalar_place(w),
                    rv: Rvalue::Use(Operand::Imm { bits: v, width: w }),
                }]
            }
            "write_bytes" => {
                vec![Stmt::MemSet {
                    dst: self.lower_operand_scalar(&args[0].node)?,
                    val: self.lower_operand_scalar(&args[1].node)?,
                    count: self.lower_operand_scalar(&args[2].node)?,
                    elem_size: elem_size(self)?,
                }]
            }
            "black_box" | "transmute" => {
                // 值透传（transmute 调用形态 = 位重解释；black_box = copy）
                let dst_p = self.resolve_place(destination)?;
                let dst_kind = self.classify(dst_p.ty)?;
                let src = self.lower_operand(&args[0].node)?;
                self.assign_lowered(&dst_p, dst_kind, src)?
            }
            "assume" => vec![Stmt::Nop],
            "abort" => {
                // core::intrinsics::abort：进程级中止（native=SIGILL trap，引擎=SIGABRT
                // ——信号差异记 m4-log，差分若较信号级再对齐）
                let tgt = (self.mir_block_count + self.extra_blocks.len()) as Bb;
                self.extra_blocks.push(ir::Block {
                    stmts: vec![],
                    term: Terminator::Unreachable,
                });
                return Ok(Some((
                    vec![],
                    Terminator::CallBuiltin {
                        builtin: ir::Builtin::HostAbort,
                        args: vec![],
                        ret: RetDest::Ignore,
                        target: tgt,
                        unwind: ir::UnwindAction::Continue,
                        role: ir::BuiltinCallRole::Normal,
                    },
                )));
            }
            "breakpoint" => {
                // 真 int3（D8i）：与 native 同为 SIGTRAP 可观测行为；正常续行到 target
                let tgt = target.ok_or("breakpoint 发散？")?.as_u32();
                return Ok(Some((
                    vec![],
                    Terminator::CallBuiltin {
                        builtin: ir::Builtin::Breakpoint,
                        args: vec![],
                        ret: RetDest::Ignore,
                        target: tgt,
                        unwind: ir::UnwindAction::Continue,
                        role: ir::BuiltinCallRole::Normal,
                    },
                )));
            }
            "assert_inhabited" | "assert_zero_valid" | "assert_mem_uninitialized_valid" => {
                // lower 期判定（collector 同构）：合法 → nop；违反 → 占位
                //（native 展开为 panic_nounwind；此路径本就是防御性死路）
                let req = rustc_middle::ty::layout::ValidityRequirement::from_intrinsic(name)
                    .expect("validity intrinsic 名");
                let t = inst.args.type_at(0);
                let ok = self
                    .tcx
                    .check_validity_requirement((req, self.typing_env.as_query_input(t)))
                    .map_err(|e| format!("validity 判定失败: {e}"))?;
                if ok {
                    vec![Stmt::Nop]
                } else {
                    vec![Stmt::Trap(
                        format!("{name} 违反（ty={t}——native panic_nounwind）").into_boxed_str(),
                    )]
                }
            }
            "saturating_add" | "saturating_sub" => {
                let a_ty = self.op_ty(&args[0].node)?;
                let op = if name.as_str() == "saturating_add" {
                    OvfOp::Add
                } else {
                    OvfOp::Sub
                };
                // 128 位：宽形态走 Sat128（宿主 u128/i128 直算；标量 IntSat 只到 64 位）
                if matches!(
                    a_ty.kind(),
                    ty::Int(ty::IntTy::I128) | ty::Uint(ty::UintTy::U128)
                ) {
                    vec![Stmt::Sat128 {
                        op,
                        signed: frame::ty_signed(a_ty),
                        a: self.wide_place(&args[0].node)?,
                        b: self.wide_place(&args[1].node)?,
                        dst: self.resolve_place(destination)?.expr(),
                    }]
                } else {
                    let (dst_p, w) = self.place_scalar(destination)?;
                    vec![Stmt::Assign {
                        dst: dst_p.scalar_place(w),
                        rv: Rvalue::IntSat {
                            op,
                            signed: frame::ty_signed(a_ty),
                            a: self.lower_operand_scalar(&args[0].node)?,
                            b: self.lower_operand_scalar(&args[1].node)?,
                        },
                    }]
                }
            }
            "caller_location" => {
                // Location::caller()：本函数 track_caller → 读隐藏尾实参槽；
                // 否则按 intrinsic 调用点合成（罕见——caller 链通常 track 到底）
                let (dst_p, w) = self.place_scalar(destination)?;
                let op = match self.caller_loc_off {
                    Some(off) => Operand::Slot(Slot {
                        off,
                        width: Width::W64,
                    }),
                    None => self.caller_location_imm(span)?,
                };
                vec![Stmt::Assign {
                    dst: dst_p.scalar_place(w),
                    rv: Rvalue::Use(op),
                }]
            }
            "compare_bytes" => {
                let (dst_p, w) = self.place_scalar(destination)?;
                vec![Stmt::Assign {
                    dst: dst_p.scalar_place(w),
                    rv: Rvalue::MemCmp {
                        a: self.lower_operand_scalar(&args[0].node)?,
                        b: self.lower_operand_scalar(&args[1].node)?,
                        n: self.lower_operand_scalar(&args[2].node)?,
                    },
                }]
            }
            // 字节级相等（<[u8;N]>::eq 的 spec 路径，csv 逼出）：memcmp == 0
            "raw_eq" => {
                let t = inst.args.type_at(0);
                let size = self.layout_of(t)?.size.bytes();
                let (dst_p, w) = self.place_scalar(destination)?;
                let s = self.scratch64();
                let s32 = Slot {
                    off: s.off,
                    width: Width::W32,
                };
                vec![
                    Stmt::Assign {
                        dst: ScalarPlace::Slot(s32),
                        rv: Rvalue::MemCmp {
                            a: self.lower_operand_scalar(&args[0].node)?,
                            b: self.lower_operand_scalar(&args[1].node)?,
                            n: Operand::Imm {
                                bits: size,
                                width: Width::W64,
                            },
                        },
                    },
                    Stmt::Assign {
                        dst: dst_p.scalar_place(w),
                        rv: Rvalue::IntCmp {
                            cc: IntCc::Eq,
                            signed: false,
                            a: Operand::Slot(s32),
                            b: Operand::Imm {
                                bits: 0,
                                width: Width::W32,
                            },
                        },
                    },
                ]
            }
            // 不检查的浮点→整数（fast 不检 UB：与 `as` 同一实现，numbigint 逼出）
            "float_to_int_unchecked" => {
                let fty = inst.args.type_at(0);
                let ity = inst.args.type_at(1);
                let signed = frame::ty_signed(ity);
                // f128 源：≤64 整数经 F128ToScalar；i128/u128 经 F128ToWideInt
                if matches!(fty.kind(), ty::Float(ty::FloatTy::F128)) {
                    let pa = self.wide_place(&args[0].node)?;
                    let dst = self.resolve_place(destination)?;
                    match frame::scalar_width(&self.layout_of(ity)?) {
                        Some(to_w) => {
                            vec![Stmt::F128ToScalar {
                                src: pa,
                                kind: ir::F128Scalar::Int { signed },
                                w: to_w,
                                dst: dst.scalar_place(to_w),
                            }]
                        }
                        None => vec![Stmt::F128ToWideInt {
                            src: pa,
                            signed,
                            dst: dst.expr(),
                        }],
                    }
                } else if let Some(to_w) = frame::scalar_width(&self.layout_of(ity)?) {
                    let (dst_p, w) = self.place_scalar(destination)?;
                    vec![Stmt::Assign {
                        dst: dst_p.scalar_place(w),
                        rv: Rvalue::FloatToInt {
                            from: float_w(fty)?,
                            to: to_w,
                            signed,
                            a: self.lower_operand_scalar(&args[0].node)?,
                        },
                    }]
                } else {
                    // 标量浮点 → i128/u128（D8k）
                    vec![Stmt::FloatToWide128 {
                        src: self.lower_operand_scalar(&args[0].node)?,
                        from: float_w(fty)?,
                        signed,
                        dst: self.resolve_place(destination)?.expr(),
                    }]
                }
            }
            // 数学面（must_be_overridden float intrinsic）：宿主直算（合成处置，P7）。
            // 宽度：后缀名（sqrtf64）定死；裸泛型名（fabs<T>，本 nightly 漂移）看类型参数。
            n if math_un_of(n).is_some() => {
                let (op, sfx) = math_un_of(n).unwrap();
                match self.resolve_float_route(sfx, inst, n)? {
                    FloatRoute::Scalar(fw) => {
                        let (dst_p, w) = self.place_scalar(destination)?;
                        vec![Stmt::Assign {
                            dst: dst_p.scalar_place(w),
                            rv: Rvalue::MathUn {
                                op,
                                fw,
                                a: self.lower_operand_scalar(&args[0].node)?,
                            },
                        }]
                    }
                    FloatRoute::Wide128 => vec![Stmt::F128Un {
                        op: ir::F128UnOp::Math(op),
                        a: self.wide_place(&args[0].node)?,
                        dst: self.resolve_place(destination)?.expr(),
                    }],
                }
            }
            n if math_bin_of(n).is_some() => {
                let (op, sfx) = math_bin_of(n).unwrap();
                match self.resolve_float_route(sfx, inst, n)? {
                    FloatRoute::Scalar(fw) => {
                        let (dst_p, w) = self.place_scalar(destination)?;
                        vec![Stmt::Assign {
                            dst: dst_p.scalar_place(w),
                            rv: Rvalue::MathBin {
                                op,
                                fw,
                                a: self.lower_operand_scalar(&args[0].node)?,
                                b: self.lower_operand_scalar(&args[1].node)?,
                            },
                        }]
                    }
                    FloatRoute::Wide128 => {
                        // powi 的 rhs 是 i32 标量，其余 f128 wide
                        let b = if matches!(op, ir::MathBinOp::Powi) {
                            ir::F128Rhs::Scalar(self.lower_operand_scalar(&args[1].node)?)
                        } else {
                            ir::F128Rhs::Wide(self.wide_place(&args[1].node)?)
                        };
                        vec![Stmt::F128MathBin {
                            op,
                            a: self.wide_place(&args[0].node)?,
                            b,
                            dst: self.resolve_place(destination)?.expr(),
                        }]
                    }
                }
            }
            // 融合乘加（D8i/D8c）：a*b+c 单次舍入。fmuladd 允许融合/不融合，融合恒在
            // 允许集合内。f16 经宿主 f16::mul_add（数学上正确舍入）。
            "fmaf16" | "fmaf32" | "fmaf64" | "fmuladdf16" | "fmuladdf32" | "fmuladdf64" => {
                let fw = match split_float_suffix(name.as_str()).1 {
                    Some(FloatSuffix::F16) => ir::FloatW::F16,
                    Some(FloatSuffix::F32) => ir::FloatW::F32,
                    _ => ir::FloatW::F64,
                };
                let (dst_p, w) = self.place_scalar(destination)?;
                vec![Stmt::Assign {
                    dst: dst_p.scalar_place(w),
                    rv: Rvalue::MathFma {
                        fw,
                        a: self.lower_operand_scalar(&args[0].node)?,
                        b: self.lower_operand_scalar(&args[1].node)?,
                        c: self.lower_operand_scalar(&args[2].node)?,
                    },
                }]
            }
            "fmaf128" | "fmuladdf128" => vec![Stmt::F128Fma {
                a: self.wide_place(&args[0].node)?,
                b: self.wide_place(&args[1].node)?,
                c: self.wide_place(&args[2].node)?,
                dst: self.resolve_place(destination)?.expr(),
            }],
            // fast/algebraic 浮点（D8i）：fast-math 标记是"允许重结合/收缩"的自由授权，
            // 按精确 IEEE 语义执行的结果恒在允许集合内（与关掉 fast-math 的 native 同值）。
            "fadd_fast" | "fsub_fast" | "fmul_fast" | "fdiv_fast" | "frem_fast"
            | "fadd_algebraic" | "fsub_algebraic" | "fmul_algebraic" | "fdiv_algebraic"
            | "frem_algebraic" => {
                use ir::FloatOp as F;
                let op = match name.as_str().split('_').next().unwrap() {
                    "fadd" => F::Add,
                    "fsub" => F::Sub,
                    "fmul" => F::Mul,
                    "fdiv" => F::Div,
                    _ => F::Rem,
                };
                match self.resolve_float_route(None, inst, name.as_str())? {
                    FloatRoute::Scalar(fw) => {
                        let (dst_p, w) = self.place_scalar(destination)?;
                        vec![Stmt::Assign {
                            dst: dst_p.scalar_place(w),
                            rv: Rvalue::FloatBin {
                                op,
                                fw,
                                a: self.lower_operand_scalar(&args[0].node)?,
                                b: self.lower_operand_scalar(&args[1].node)?,
                            },
                        }]
                    }
                    FloatRoute::Wide128 => vec![Stmt::F128Bin {
                        op,
                        a: self.wide_place(&args[0].node)?,
                        b: self.wide_place(&args[1].node)?,
                        dst: self.resolve_place(destination)?.expr(),
                    }],
                }
            }
            n if n.starts_with("simd_") => self.expand_simd(n, inst, args, destination)?,
            "ptr_offset_from" | "ptr_offset_from_unsigned" => {
                let ptr_ty = self.op_ty(&args[0].node)?;
                let pointee = ptr_ty.builtin_deref(true).ok_or("ptr_offset_from 非指针")?;
                let stride = self.layout_of(pointee)?.size.bytes().max(1);
                let (dst_p, w) = self.place_scalar(destination)?;
                vec![Stmt::Assign {
                    dst: dst_p.scalar_place(w),
                    rv: Rvalue::PtrDiff {
                        a: self.lower_operand_scalar(&args[0].node)?,
                        b: self.lower_operand_scalar(&args[1].node)?,
                        stride,
                    },
                }]
            }
            "size_of_val" | "min_align_of_val" | "align_of_val" => {
                // *const T 实参；T sized → 常量；[E]/str → meta 折算；dyn → vtable（M4.1+）
                let t = inst.args.type_at(0);
                let (dst_p, w) = self.place_scalar(destination)?;
                let is_size = name.as_str() == "size_of_val";
                let t_layout = self.layout_of(t)?;
                if t_layout.is_sized() {
                    let v = if is_size {
                        t_layout.size.bytes()
                    } else {
                        t_layout.align.abi.bytes()
                    };
                    vec![Stmt::Assign {
                        dst: dst_p.scalar_place(w),
                        rv: Rvalue::Use(Operand::Imm { bits: v, width: w }),
                    }]
                } else {
                    match t.kind() {
                        ty::Slice(e) | ty::Array(e, _) => {
                            let el = self.layout_of(*e)?;
                            if is_size {
                                // meta（元素数）× elem size
                                let LoweredOp::Pair(_, meta) = self.lower_operand(&args[0].node)?
                                else {
                                    return Err("size_of_val 实参非胖指针".into());
                                };
                                vec![Stmt::Assign {
                                    dst: dst_p.scalar_place(w),
                                    rv: Rvalue::IntBin {
                                        op: IntBinOp::Mul,
                                        signed: false,
                                        a: meta,
                                        b: Operand::Imm {
                                            bits: el.size.bytes(),
                                            width: Width::W64,
                                        },
                                    },
                                }]
                            } else {
                                vec![Stmt::Assign {
                                    dst: dst_p.scalar_place(w),
                                    rv: Rvalue::Use(Operand::Imm {
                                        bits: el.align.abi.bytes(),
                                        width: w,
                                    }),
                                }]
                            }
                        }
                        ty::Str => {
                            if is_size {
                                let LoweredOp::Pair(_, meta) = self.lower_operand(&args[0].node)?
                                else {
                                    return Err("size_of_val 实参非胖指针".into());
                                };
                                vec![Stmt::Assign {
                                    dst: dst_p.scalar_place(w),
                                    rv: Rvalue::Use(meta),
                                }]
                            } else {
                                vec![Stmt::Assign {
                                    dst: dst_p.scalar_place(w),
                                    rv: Rvalue::Use(Operand::Imm { bits: 1, width: w }),
                                }]
                            }
                        }
                        ty::Dynamic(..) => {
                            // vtable 布局：[drop, size, align, ...]（COMMON_VTABLE_ENTRIES）
                            let LoweredOp::Pair(_, vt) = self.lower_operand(&args[0].node)? else {
                                return Err(format!("{name} 实参非 dyn 胖指针"));
                            };
                            let slot_off = if is_size { 8 } else { 16 };
                            vec![Stmt::Assign {
                                dst: dst_p.scalar_place(w),
                                rv: Rvalue::Use(operand_deref_at(vt, slot_off)?),
                            }]
                        }
                        // unsized 尾字段结构体（Path/OsStr/RcInner<dyn>，M4.5）：
                        // cg_ssa glue size_and_align_of_dst 同构——
                        // full_align = max(sized_align, tail_align)；
                        // full_size = align_to(sized_size + tail_size, full_align)。
                        ty::Adt(..) | ty::Tuple(..) => {
                            let LoweredOp::Pair(_, meta) = self.lower_operand(&args[0].node)?
                            else {
                                return Err(format!("{name} 实参非胖指针"));
                            };
                            let tail = self.tcx.struct_tail_for_codegen(t, self.typing_env);
                            let sized_size = t_layout.size.bytes();
                            let sized_align = t_layout.align.abi.bytes();
                            let dst = dst_p.scalar_place(w);
                            let assign = |rv| Stmt::Assign {
                                dst: dst.clone(),
                                rv,
                            };
                            let imm = |v: u64| Operand::Imm {
                                bits: v,
                                width: Width::W64,
                            };
                            let dst_op = dst_p.scalar_operand(w);
                            match tail.kind() {
                                ty::Slice(..) | ty::Str => {
                                    let (esz, eal) = match tail.kind() {
                                        ty::Slice(e) => {
                                            let l = self.layout_of(*e)?;
                                            (l.size.bytes(), l.align.abi.bytes())
                                        }
                                        _ => (1, 1),
                                    };
                                    let fa = sized_align.max(eal); // 编译期常量
                                    if !is_size {
                                        vec![assign(Rvalue::Use(imm(fa)))]
                                    } else {
                                        let mut v = vec![
                                            assign(Rvalue::IntBin {
                                                op: IntBinOp::Mul,
                                                signed: false,
                                                a: meta,
                                                b: imm(esz),
                                            }),
                                            assign(Rvalue::IntBin {
                                                op: IntBinOp::Add,
                                                signed: false,
                                                a: dst_op.clone(),
                                                b: imm(sized_size),
                                            }),
                                        ];
                                        if fa > 1 {
                                            v.push(assign(Rvalue::IntBin {
                                                op: IntBinOp::Add,
                                                signed: false,
                                                a: dst_op.clone(),
                                                b: imm(fa - 1),
                                            }));
                                            v.push(assign(Rvalue::IntBin {
                                                op: IntBinOp::BitAnd,
                                                signed: false,
                                                a: dst_op.clone(),
                                                b: imm(!(fa - 1)),
                                            }));
                                        }
                                        v
                                    }
                                }
                                ty::Dynamic(..) => {
                                    // meta = vtable：tail size@+8 / align@+16（运行时读）。
                                    // full_align = max(sized_align, tail_align)；
                                    // full_size  = align_to(sized_size + tail_size, full_align)
                                    //            = (s + a − 1) & !(a − 1)（cg_ssa 同式）。
                                    let sl = |s: Slot| Operand::Slot(s);
                                    let sp = |s: Slot| ScalarPlace::Slot(s);
                                    let umax_align = Rvalue::UMax {
                                        a: operand_deref_at(meta.clone(), 16)?,
                                        b: imm(sized_align),
                                    };
                                    if !is_size {
                                        vec![assign(umax_align)]
                                    } else {
                                        let fa = self.scratch64();
                                        let m = self.scratch64();
                                        vec![
                                            Stmt::Assign {
                                                dst: sp(fa),
                                                rv: umax_align,
                                            },
                                            // dst = sized_size + tail_size
                                            assign(Rvalue::IntBin {
                                                op: IntBinOp::Add,
                                                signed: false,
                                                a: operand_deref_at(meta, 8)?,
                                                b: imm(sized_size),
                                            }),
                                            // m = fa − 1
                                            Stmt::Assign {
                                                dst: sp(m),
                                                rv: Rvalue::IntBin {
                                                    op: IntBinOp::Sub,
                                                    signed: false,
                                                    a: sl(fa),
                                                    b: imm(1),
                                                },
                                            },
                                            // dst += m
                                            assign(Rvalue::IntBin {
                                                op: IntBinOp::Add,
                                                signed: false,
                                                a: dst_op.clone(),
                                                b: sl(m),
                                            }),
                                            // m = !m
                                            Stmt::Assign {
                                                dst: sp(m),
                                                rv: Rvalue::NotBits(sl(m)),
                                            },
                                            // dst &= m
                                            assign(Rvalue::IntBin {
                                                op: IntBinOp::BitAnd,
                                                signed: false,
                                                a: dst_op.clone(),
                                                b: sl(m),
                                            }),
                                        ]
                                    }
                                }
                                _ => {
                                    return Err(format!("{name} 尾类型 {tail} 未支持（M4.5+）"));
                                }
                            }
                        }
                        _ => return Err(format!("{name} on unsized {t}（M4.1+）")),
                    }
                }
            }
            _ => return Ok(None),
        };
        let tgt = target.ok_or("intrinsic 展开：发散 intrinsic？")?.as_u32();
        Ok(Some((stmts, Terminator::Goto(tgt))))
    }
}

pub(super) fn elem_of(ty: Ty<'_>) -> Option<Ty<'_>> {
    match ty.kind() {
        ty::Array(t, _) | ty::Slice(t) => Some(*t),
        _ => None,
    }
}

pub(super) fn u128_to_u64(v: u128) -> Result<u64, String> {
    u64::try_from(v).map_err(|_| "128 位判别式（M4.1+）".to_string())
}

/// 数学 intrinsic 名 → (op, 宽度)。宽度 `Some(is64)` 来自 f32/f64 后缀；`None` =
/// 裸泛型名（本 nightly `fabs<T: FloatPrimitive>` 已去后缀——M5.2 D8i 实证漂移），
/// 由调用点按类型参数解析。**全表都做泛型兜底**：后缀剥离对 nightly 漂移脆弱，
/// 任一名字将来去后缀化时走同一条泛型道而不是 Trap。f16/f128 由调用点按 D8c 处置。
pub(super) fn math_un_of(n: &str) -> Option<(ir::MathUnOp, Option<FloatSuffix>)> {
    let (stem, sfx) = split_float_suffix(n);
    math_un_stem(stem).map(|op| (op, sfx))
}

fn math_un_stem(stem: &str) -> Option<ir::MathUnOp> {
    use ir::MathUnOp as M;
    Some(match stem {
        "sqrt" => M::Sqrt,
        "sin" => M::Sin,
        "cos" => M::Cos,
        "exp" => M::Exp,
        "exp2" => M::Exp2,
        "log" => M::Ln,
        "log2" => M::Log2,
        "log10" => M::Log10,
        "fabs" => M::Fabs,
        "floor" => M::Floor,
        "ceil" => M::Ceil,
        "trunc" => M::Trunc,
        "round" => M::Round,
        "round_ties_even" => M::RoundTiesEven,
        _ => return None,
    })
}

pub(super) fn math_bin_of(n: &str) -> Option<(ir::MathBinOp, Option<FloatSuffix>)> {
    use ir::MathBinOp as M;
    let (stem, sfx) = split_float_suffix(n);
    Some((
        match stem {
            "pow" => M::Pow,
            "powi" => M::Powi,
            "copysign" => M::Copysign,
            "minnum" => M::Minnum,
            "maxnum" => M::Maxnum,
            _ => return None,
        },
        sfx,
    ))
}

/// 浮点 intrinsic 名后缀（sqrtf16/f32/f64/f128）。
#[derive(Clone, Copy)]
pub(super) enum FloatSuffix {
    F16,
    F32,
    F64,
    F128,
}

/// 标量/宽通道路由。
#[derive(Clone, Copy)]
pub(super) enum FloatRoute {
    Scalar(ir::FloatW),
    Wide128,
}

/// 标量浮点宽度（f128 不在此——16 字节走宽通道，调用点先分流）。
pub(super) fn float_w(t: Ty<'_>) -> Result<ir::FloatW, String> {
    match t.kind() {
        ty::Float(ty::FloatTy::F16) => Ok(ir::FloatW::F16),
        ty::Float(ty::FloatTy::F32) => Ok(ir::FloatW::F32),
        ty::Float(ty::FloatTy::F64) => Ok(ir::FloatW::F64),
        _ => Err(format!("非标量浮点宽度 {t}")),
    }
}

/// 剥 f16/f32/f64/f128 后缀；无后缀返回原名 + None（泛型 intrinsic，宽度看类型参数）。
pub(super) fn split_float_suffix(n: &str) -> (&str, Option<FloatSuffix>) {
    for (sfx, tag) in [
        ("f128", FloatSuffix::F128),
        ("f64", FloatSuffix::F64),
        ("f32", FloatSuffix::F32),
        ("f16", FloatSuffix::F16),
    ] {
        if let Some(stem) = n.strip_suffix(sfx) {
            return (stem.trim_end_matches('_'), Some(tag));
        }
    }
    (n, None)
}

/// 在 operand 的值（指针）上再间接一层：*(op + off)。vtable 槽读取用。
pub(super) fn operand_deref_at(op: Operand, off: u32) -> Result<Operand, String> {
    let deref_steps = |mut steps: Vec<PlaceStep>| {
        steps.push(PlaceStep::Deref);
        if off != 0 {
            steps.push(PlaceStep::Offset(off as i32));
        }
        steps.into_boxed_slice()
    };
    Ok(match op {
        Operand::Slot(s) => Operand::Mem {
            expr: PlaceExpr {
                base: PlaceBase::Local(s.off),
                steps: deref_steps(Vec::new()),
            },
            width: Width::W64,
        },
        Operand::Mem { expr, .. } => Operand::Mem {
            expr: PlaceExpr {
                base: expr.base,
                steps: deref_steps(expr.steps.into_vec()),
            },
            width: Width::W64,
        },
        // 常量 vtable 地址（常量 dyn 引用）：运行期从冻结区读槽
        Operand::Imm { bits, .. } => Operand::Mem {
            expr: PlaceExpr {
                base: PlaceBase::Static(ir::LinkAddr(bits.wrapping_add(off as u64))),
                steps: Box::new([]),
            },
            width: Width::W64,
        },
        Operand::AddrImm(addr) => Operand::Mem {
            expr: PlaceExpr {
                base: PlaceBase::Static(ir::LinkAddr(addr.0.wrapping_add(off as u64))),
                steps: Box::new([]),
            },
            width: Width::W64,
        },
        Operand::AddrOf(_) | Operand::SubImm { .. } => {
            return Err("vtable operand 形态异常".into());
        }
    })
}

/// `TyAndLayout::for_variant` 需要一个 LayoutCx；用 (tcx, typing_env) 现造一个。
pub(super) struct LayoutCxAt<'tcx>(pub(super) TyCtxt<'tcx>, pub(super) TypingEnv<'tcx>);

impl<'tcx> rustc_abi::HasDataLayout for LayoutCxAt<'tcx> {
    fn data_layout(&self) -> &rustc_abi::TargetDataLayout {
        self.0.data_layout()
    }
}
impl<'tcx> rustc_middle::ty::layout::HasTyCtxt<'tcx> for LayoutCxAt<'tcx> {
    fn tcx(&self) -> TyCtxt<'tcx> {
        self.0
    }
}
impl<'tcx> rustc_middle::ty::layout::HasTypingEnv<'tcx> for LayoutCxAt<'tcx> {
    fn typing_env(&self) -> TypingEnv<'tcx> {
        self.1
    }
}
