//! What a guest function body must satisfy: its parameter and return conventions, and every
//! statement, rvalue and terminator it is built from.

use super::*;

impl<'a> Verifier<'a> {
    pub(super) fn body(&self, body: &FuncBody) -> Result<(), String> {
        if body.frame_align == 0 || !body.frame_align.is_power_of_two() {
            return Err(format!("invalid frame alignment {}", body.frame_align));
        }
        if u64::from(body.frame_size) > REGION_CAP || u64::from(body.frame_align) > REGION_CAP {
            return Err(format!(
                "frame size/alignment ({}/{}) exceeds the 1 GiB operand region",
                body.frame_size, body.frame_align
            ));
        }
        if body.blocks.is_empty() {
            return Err("has no basic blocks".into());
        }
        self.ret_abi(body, &body.ret)?;
        for (i, param) in body.params.iter().enumerate() {
            self.param_abi(body, param)
                .map_err(|e| format!("parameter {i}: {e}"))?;
        }
        if let Some(off) = body.caller_loc_off {
            self.span(body, off, 8, "track_caller slot")?;
        }
        for (bb, block) in body.blocks.iter().enumerate() {
            for (si, stmt) in block.stmts.iter().enumerate() {
                self.stmt(body, stmt)
                    .map_err(|e| format!("bb{bb} statement {si}: {e}"))?;
            }
            self.term(body, &block.term)
                .map_err(|e| format!("bb{bb} terminator: {e}"))?;
        }
        Ok(())
    }

    pub(super) fn param_abi(&self, body: &FuncBody, abi: &ParamAbi) -> Result<(), String> {
        match abi {
            ParamAbi::Zst => Ok(()),
            ParamAbi::Scalar(slot) => self.slot(body, *slot),
            ParamAbi::Pair(a, b) => {
                self.slot(body, *a)?;
                self.slot(body, *b)
            }
            ParamAbi::Indirect { off, size } => self.span(body, *off, *size, "indirect parameter"),
        }
    }

    pub(super) fn ret_abi(&self, body: &FuncBody, abi: &RetAbi) -> Result<(), String> {
        match abi {
            RetAbi::Zst => Ok(()),
            RetAbi::Scalar(slot) => self.slot(body, *slot),
            RetAbi::Pair(a, b) => {
                self.slot(body, *a)?;
                self.slot(body, *b)
            }
            RetAbi::Indirect {
                ret_off,
                size,
                sret_off,
            } => {
                self.span(body, *ret_off, *size, "indirect return value")?;
                self.span(body, *sret_off, 8, "indirect return pointer")
            }
        }
    }

    pub(super) fn stmt(&self, body: &FuncBody, stmt: &Stmt) -> Result<(), String> {
        match stmt {
            Stmt::Assign { dst, rv } => {
                self.scalar_place(body, dst)?;
                self.rvalue(body, rv)
            }
            Stmt::AssignOverflow {
                a,
                b,
                dst_val,
                dst_flag,
                ..
            } => {
                self.operands(body, [a, b])?;
                self.scalar_places(body, [dst_val, dst_flag])
            }
            Stmt::Copy { dst, src, .. } => self.places(body, [dst, src]),
            Stmt::RepeatScalar { dst, val, .. } => {
                self.place(body, dst)?;
                self.operand(body, val)
            }
            Stmt::AtomicStore { addr, val, .. } => self.operands(body, [addr, val]),
            Stmt::VolatileLoad { addr, dst, .. } => {
                self.operand(body, addr)?;
                self.place(body, dst)
            }
            Stmt::VolatileStore { addr, src, .. } => {
                self.operand(body, addr)?;
                self.place(body, src)
            }
            Stmt::AtomicCxchg {
                addr,
                expected,
                new,
                dst_val,
                dst_ok,
                ..
            } => {
                self.operands(body, [addr, expected, new])?;
                self.scalar_places(body, [dst_val, dst_ok])
            }
            Stmt::AtomicRmw { addr, val, dst, .. } => {
                self.operands(body, [addr, val])?;
                self.scalar_place(body, dst)
            }
            Stmt::MemCopy {
                dst, src, count, ..
            } => self.operands(body, [dst, src, count]),
            Stmt::MemSet {
                dst, val, count, ..
            } => self.operands(body, [dst, val, count]),
            Stmt::SimdBin {
                dst,
                a,
                b,
                lanes,
                lane_bytes,
                ..
            } => {
                vector(*lanes, *lane_bytes)?;
                self.places(body, [dst, a, b])
            }
            Stmt::SimdUn {
                dst,
                a,
                lanes,
                lane_bytes,
                ..
            } => {
                vector(*lanes, *lane_bytes)?;
                self.places(body, [dst, a])
            }
            Stmt::SimdFma {
                dst,
                a,
                b,
                c,
                lanes,
                lane_bytes,
            }
            | Stmt::SimdFunnel {
                dst,
                a,
                b,
                shift: c,
                lanes,
                lane_bytes,
                ..
            } => {
                vector(*lanes, *lane_bytes)?;
                self.places(body, [dst, a, b, c])
            }
            Stmt::SimdCast {
                dst,
                src,
                lanes,
                src_bytes,
                dst_bytes,
                ..
            } => {
                vector(*lanes, *src_bytes)?;
                vector(*lanes, *dst_bytes)?;
                self.places(body, [dst, src])
            }
            Stmt::SimdSelect {
                mask,
                a,
                b,
                dst,
                lanes,
                lane_bytes,
                mask_bytes,
            } => {
                vector(*lanes, *lane_bytes)?;
                vector(*lanes, *mask_bytes)?;
                self.places(body, [mask, a, b, dst])
            }
            Stmt::SimdSelectBitmask {
                mask,
                a,
                b,
                dst,
                lanes,
                lane_bytes,
            } => {
                vector(*lanes, *lane_bytes)?;
                self.operand(body, mask)?;
                self.places(body, [a, b, dst])
            }
            Stmt::SimdGather {
                passthru,
                ptrs,
                mask,
                dst,
                lanes,
                lane_bytes,
                mask_bytes,
            } => {
                vector(*lanes, *lane_bytes)?;
                vector(*lanes, *mask_bytes)?;
                self.places(body, [passthru, ptrs, mask, dst])
            }
            Stmt::SimdScatter {
                values,
                ptrs,
                mask,
                lanes,
                lane_bytes,
                mask_bytes,
            } => {
                vector(*lanes, *lane_bytes)?;
                vector(*lanes, *mask_bytes)?;
                self.places(body, [values, ptrs, mask])
            }
            Stmt::SimdMaskedLoad {
                mask,
                base,
                passthru,
                dst,
                lanes,
                lane_bytes,
                mask_bytes,
            } => {
                vector(*lanes, *lane_bytes)?;
                vector(*lanes, *mask_bytes)?;
                self.operand(body, base)?;
                self.places(body, [mask, passthru, dst])
            }
            Stmt::SimdMaskedStore {
                mask,
                base,
                values,
                lanes,
                lane_bytes,
                mask_bytes,
            } => {
                vector(*lanes, *lane_bytes)?;
                vector(*lanes, *mask_bytes)?;
                self.operand(body, base)?;
                self.places(body, [mask, values])
            }
            Stmt::SimdExtractDyn {
                src,
                idx,
                dst,
                lanes,
                lane_bytes,
            } => {
                vector(*lanes, *lane_bytes)?;
                self.place(body, src)?;
                self.operand(body, idx)?;
                self.scalar_place(body, dst)
            }
            Stmt::SimdInsertDyn {
                src,
                idx,
                val,
                dst,
                lanes,
                lane_bytes,
            } => {
                vector(*lanes, *lane_bytes)?;
                self.places(body, [src, dst])?;
                self.operands(body, [idx, val])
            }
            Stmt::SimdArithOffset {
                ptrs,
                offsets,
                dst,
                lanes,
                ..
            } => {
                if *lanes == 0 {
                    return Err("SIMD lane count is zero".into());
                }
                self.places(body, [ptrs, offsets, dst])
            }
            Stmt::SimdSplat {
                dst,
                val,
                lanes,
                lane_bytes,
            } => {
                vector(*lanes, *lane_bytes)?;
                self.place(body, dst)?;
                self.operand(body, val)
            }
            Stmt::Bin128 { a, b, dst, .. } => {
                self.places(body, [a, dst])?;
                self.bin128_rhs(body, b)
            }
            Stmt::Sat128 { a, b, dst, .. }
            | Stmt::F128Bin { a, b, dst, .. }
            | Stmt::F128MathBin {
                a,
                b: F128Rhs::Wide(b),
                dst,
                ..
            } => self.places(body, [a, b, dst]),
            Stmt::F128MathBin {
                a,
                b: F128Rhs::Scalar(b),
                dst,
                ..
            } => {
                self.places(body, [a, dst])?;
                self.operand(body, b)
            }
            Stmt::Wide128ToFloat { src, dst, .. }
            | Stmt::Bit128Count { src, dst, .. }
            | Stmt::F128ToScalar { src, dst, .. } => {
                self.place(body, src)?;
                self.scalar_place(body, dst)
            }
            Stmt::FloatToWide128 { src, dst, .. } | Stmt::F128FromScalar { src, dst, .. } => {
                self.operand(body, src)?;
                self.place(body, dst)
            }
            Stmt::Bit128 { src, dst, .. }
            | Stmt::F128Un { a: src, dst, .. }
            | Stmt::F128FromWideInt { src, dst, .. }
            | Stmt::F128ToWideInt { src, dst, .. } => self.places(body, [src, dst]),
            Stmt::F128Fma { a, b, c, dst } => self.places(body, [a, b, c, dst]),
            Stmt::NicheDiscr128 { tag, dst, .. } => {
                self.place(body, tag)?;
                self.scalar_place(body, dst)
            }
            Stmt::RepeatBytes { first, .. } => self.place(body, first),
            Stmt::Trap(_) | Stmt::Nop | Stmt::Fence { .. } => Ok(()),
        }
    }

    pub(super) fn rvalue(&self, body: &FuncBody, rv: &Rvalue) -> Result<(), String> {
        match rv {
            Rvalue::Use(a)
            | Rvalue::NotBits(a)
            | Rvalue::NotBool(a)
            | Rvalue::Neg(a)
            | Rvalue::Cast { a, .. }
            | Rvalue::MathUn { a, .. }
            | Rvalue::FloatNeg { a, .. }
            | Rvalue::FloatCast { a, .. }
            | Rvalue::FloatToInt { a, .. }
            | Rvalue::IntToFloat { a, .. }
            | Rvalue::BitUn { a, .. }
            | Rvalue::AtomicLoad { addr: a, .. } => self.operand(body, a),
            Rvalue::TlsRef(id) => self.tls(*id),
            Rvalue::IntBin { a, b, .. }
            | Rvalue::IntCmp { a, b, .. }
            | Rvalue::PtrOffset {
                ptr: a, count: b, ..
            }
            | Rvalue::IntCmp3 { a, b, .. }
            | Rvalue::FloatBin { a, b, .. }
            | Rvalue::MathBin { a, b, .. }
            | Rvalue::UMax { a, b }
            | Rvalue::FloatCmp { a, b, .. }
            | Rvalue::PtrDiff { a, b, .. }
            | Rvalue::IntSat { a, b, .. } => self.operands(body, [a, b]),
            Rvalue::NicheDiscr { tag, .. } => self.operand(body, tag),
            Rvalue::MathFma { a, b, c, .. } | Rvalue::MemCmp { a, b, n: c } => {
                self.operands(body, [a, b, c])
            }
            Rvalue::Ref(p) => self.place(body, p),
            Rvalue::F128Cmp { a, b, .. } | Rvalue::Cmp128 { a, b, .. } => self.places(body, [a, b]),
            Rvalue::SimdBitmask {
                a,
                lanes,
                lane_bytes,
            }
            | Rvalue::SimdReduce {
                a,
                lanes,
                lane_bytes,
                ..
            }
            | Rvalue::SimdReduceArith {
                a,
                lanes,
                lane_bytes,
                ..
            } => {
                vector(*lanes, *lane_bytes)?;
                self.place(body, a)
            }
        }
    }

    pub(super) fn term(&self, body: &FuncBody, term: &Terminator) -> Result<(), String> {
        match term {
            Terminator::Goto(bb) => self.bb(body, *bb),
            Terminator::SwitchInt {
                discr,
                targets,
                otherwise,
            } => {
                self.switch_discr(body, discr)?;
                for (_, bb) in targets {
                    self.bb(body, *bb)?;
                }
                self.bb(body, *otherwise)
            }
            Terminator::Call {
                callee,
                args,
                ret,
                target,
                unwind,
                role,
            } => {
                self.func(*callee)?;
                self.operands(body, args)?;
                self.ret_dest(body, ret)?;
                self.bb(body, *target)?;
                self.unwind(body, *unwind)?;
                if matches!(role, crate::vm::ir::CallRole::MainPanicBoundary)
                    && !matches!(unwind, UnwindAction::Continue)
                {
                    return Err("main panic boundary call must use Continue unwind action".into());
                }
                Ok(())
            }
            Terminator::CallBuiltin {
                builtin,
                args,
                ret,
                target,
                unwind,
                role,
            } => {
                self.operands(body, args)?;
                self.ret_dest(body, ret)?;
                self.bb(body, *target)?;
                self.unwind(body, *unwind)?;
                if matches!(role, BuiltinCallRole::MainPanicCatcher) {
                    let byte_ret = matches!(
                        ret,
                        RetDest::Scalar(ScalarPlace::Slot(Slot {
                            width: Width::W8,
                            ..
                        })) | RetDest::Scalar(ScalarPlace::Mem {
                            width: Width::W8,
                            ..
                        })
                    );
                    if !matches!(builtin, Builtin::CatchUnwind)
                        || !matches!(unwind, UnwindAction::Continue)
                        || args.len() != 3
                        || !args.iter().all(|arg| arg.width() == Width::W64)
                        || !byte_ret
                    {
                        return Err(
                            "main panic catcher must be CatchUnwind with three pointer-width \
                             arguments, a byte scalar return, and Continue unwind action"
                                .into(),
                        );
                    }
                }
                Ok(())
            }
            Terminator::CallForeign {
                sig,
                args,
                ret,
                target,
                unwind,
                ..
            } => {
                self.foreign_sig(sig)?;
                self.operands(body, args)?;
                self.ret_dest(body, ret)?;
                self.bb(body, *target)?;
                self.unwind(body, *unwind)
            }
            Terminator::CallIndirect {
                callee,
                args,
                ret,
                target,
                unwind,
                native_sig,
                ..
            } => {
                self.operand(body, callee)?;
                self.operands(body, args)?;
                self.ret_dest(body, ret)?;
                if let Some(sig) = native_sig {
                    self.foreign_sig(sig)?;
                }
                self.bb(body, *target)?;
                self.unwind(body, *unwind)
            }
            Terminator::InlineAsm {
                stub,
                buf_size,
                ins,
                outs,
                target,
            } => {
                self.asm(*stub)?;
                for (i, (off, val)) in ins.iter().enumerate() {
                    let width = match val {
                        AsmIoVal::Scalar(v) => {
                            self.operand(body, v)?;
                            8
                        }
                        AsmIoVal::VecBytes(p, n) => {
                            self.place(body, p)?;
                            *n
                        }
                    };
                    buffer_span(*buf_size, *off, width).map_err(|e| format!("input {i}: {e}"))?;
                }
                for (i, (off, dst)) in outs.iter().enumerate() {
                    let width = match dst {
                        AsmIoDst::Scalar(p) => {
                            self.scalar_place(body, p)?;
                            8
                        }
                        AsmIoDst::VecBytes(p, n) => {
                            self.place(body, p)?;
                            *n
                        }
                    };
                    buffer_span(*buf_size, *off, width).map_err(|e| format!("output {i}: {e}"))?;
                }
                self.bb(body, *target)
            }
            Terminator::Return
            | Terminator::Unreachable
            | Terminator::Resume
            | Terminator::TerminateAbort
            | Terminator::Trap(_) => Ok(()),
        }
    }
}
