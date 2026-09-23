//! Statement lowering: the exhaustive `ir::Stmt` match, plus the Repeat skeleton it shares
//! between a copy and a fill. `impl Translator` sub-block; the struct is in `super`.

use super::*;

impl Translator<'_, '_> {
    pub(super) fn stmt(&mut self, st: &Stmt) {
        match st {
            Stmt::Assign { dst, rv } => {
                let v = self.rvalue(rv);
                match dst {
                    ScalarPlace::Slot(s) => {
                        let s = *s;
                        self.def_slot(s, v);
                    }
                    ScalarPlace::Mem { expr, width } => {
                        // Memory destination: mask, narrow, store -- the same as the
                        // interpreter's mem_write.
                        let a = self.place_addr(expr);
                        let masked = self.mask_val(v, *width);
                        let n = if *width == Width::W64 {
                            masked
                        } else {
                            self.b.ins().ireduce(Self::narrow_ty(*width), masked)
                        };
                        self.b.ins().store(MemFlagsData::trusted(), n, a, 0);
                    }
                }
            }
            Stmt::AssignOverflow {
                op,
                signed,
                a,
                b,
                dst_val,
                dst_flag,
            } => {
                let (av, w) = self.operand(a);
                let (bv, _) = self.operand(b);
                let (val, flag) = self.int_ovf(*op, *signed, av, bv, w);
                let (sv, sf) = match (dst_val, dst_flag) {
                    (ScalarPlace::Slot(v), ScalarPlace::Slot(f)) => (*v, *f),
                    _ => unreachable!("admit rejects this shape"),
                };
                self.def_slot(sv, val);
                self.def_slot(sf, flag);
            }
            Stmt::Copy { dst, src, size } => {
                // memmove semantics, matching the interpreter's std::ptr::copy:
                // overlap is UB on the guest side, but the engine must not crash over
                // it, and both sides stay defensive in the same way.
                let d = self.place_addr(dst);
                let s = self.place_addr(src);
                let n = self.b.ins().iconst(types::I64, i64::from(*size));
                let fref = self.module.declare_func_in_func(self.memmove, self.b.func);
                self.b.ins().call(fref, &[d, s, n]);
            }
            Stmt::RepeatScalar {
                dst,
                val,
                count,
                elem_size,
            } => {
                // Interpreter loop mirror: for i in 0..count { mem_write(d + i*elem, w, v) }
                let d = self.place_addr(dst);
                let (v, w) = self.operand(val);
                debug_assert_eq!(w.bytes(), *elem_size);
                let masked = self.mask_val(v, w);
                let n = if w == Width::W64 {
                    masked
                } else {
                    self.b.ins().ireduce(Self::narrow_ty(w), masked)
                };
                self.repeat_loop(d, n, *count, u64::from(*elem_size), false);
            }
            Stmt::RepeatBytes {
                first,
                count,
                elem_size,
            } => {
                // Interpreter mirror: for i in 1..count { memmove one element },
                // leaving element 0 in place -- element-wise the same result as the
                // interpreter's copy_nonoverlapping.
                let src = self.place_addr(first);
                self.repeat_loop(src, src, *count, *elem_size, true);
            }
            // ===== Memory / atomic statements =====
            Stmt::MemCopy {
                dst,
                src,
                count,
                elem_size,
                overlap,
            } => {
                // The copy/copy_nonoverlapping intrinsics go through memmove: with
                // overlap that means the same as the interpreter's ptr::copy, and
                // without overlap the result equals memcpy.
                let _ = overlap;
                let (d, _) = self.operand(dst);
                let (s, _) = self.operand(src);
                let (c, _) = self.operand(count);
                let es = self.b.ins().iconst(types::I64, *elem_size as i64);
                let n = self.b.ins().imul(c, es);
                let fref = self.module.declare_func_in_func(self.memmove, self.b.func);
                self.b.ins().call(fref, &[d, s, n]);
            }
            Stmt::MemSet {
                dst,
                val,
                count,
                elem_size,
            } => {
                let (d, _) = self.operand(dst);
                let (v, _) = self.operand(val);
                let (c, _) = self.operand(count);
                let es = self.b.ins().iconst(types::I64, *elem_size as i64);
                let n = self.b.ins().imul(c, es);
                let fref = self.module.declare_func_in_func(self.memset, self.b.func);
                self.b.ins().call(fref, &[d, v, n]);
            }
            Stmt::VolatileLoad { addr, dst, size } => {
                let (p, _) = self.operand(addr);
                let d = self.place_addr(dst);
                let n = self.b.ins().iconst(types::I64, i64::from(*size));
                let fref = self
                    .module
                    .declare_func_in_func(self.volatile_load, self.b.func);
                self.b.ins().call(fref, &[p, d, n]);
            }
            Stmt::VolatileStore { addr, src, size } => {
                let (p, _) = self.operand(addr);
                let s = self.place_addr(src);
                let n = self.b.ins().iconst(types::I64, i64::from(*size));
                let fref = self
                    .module
                    .declare_func_in_func(self.volatile_store, self.b.func);
                self.b.ins().call(fref, &[p, s, n]);
            }
            Stmt::AtomicStore { addr, val, order } => {
                let (p, _) = self.operand(addr);
                let (v, w) = self.operand(val);
                let masked = self.mask_val(v, w);
                let n = if w == Width::W64 {
                    masked
                } else {
                    self.b.ins().ireduce(Self::narrow_ty(w), masked)
                };
                let _ = order; // CLIF atomics are always SeqCst; see the R::AtomicLoad note.
                self.b.ins().atomic_store(MemFlagsData::trusted(), n, p);
            }
            Stmt::AtomicRmw {
                op,
                addr,
                val,
                dst,
                order,
            } => {
                let (p, _) = self.operand(addr);
                let (v, w) = self.operand(val);
                let masked = self.mask_val(v, w);
                let n = if w == Width::W64 {
                    masked
                } else {
                    self.b.ins().ireduce(Self::narrow_ty(w), masked)
                };
                let _ = order;
                let old = self.b.ins().atomic_rmw(
                    Self::narrow_ty(w),
                    MemFlagsData::trusted(),
                    clif_rmw_op(*op),
                    p,
                    n,
                );
                let old = if w == Width::W64 {
                    old
                } else {
                    self.b.ins().uextend(types::I64, old)
                };
                self.write_scalar_place(dst, old);
            }
            Stmt::AtomicCxchg {
                addr,
                expected,
                new,
                dst_val,
                dst_ok,
                weak,
                succ,
                fail,
            } => {
                // CLIF atomic_cas is a strong CAS, and using it for a weak CAS is
                // conforming: weak permits spurious failure but does not require it.
                // The succ/fail orderings are ignored in favour of SeqCst.
                let (p, _) = self.operand(addr);
                let (e, w) = self.operand(expected);
                let (n, _) = self.operand(new);
                let e_masked = self.mask_val(e, w);
                let n_masked = self.mask_val(n, w);
                let (e_n, n_n) = if w == Width::W64 {
                    (e_masked, n_masked)
                } else {
                    (
                        self.b.ins().ireduce(Self::narrow_ty(w), e_masked),
                        self.b.ins().ireduce(Self::narrow_ty(w), n_masked),
                    )
                };
                let _ = (weak, succ, fail);
                let old = self
                    .b
                    .ins()
                    .atomic_cas(MemFlagsData::trusted(), p, e_n, n_n);
                let old_ext = if w == Width::W64 {
                    old
                } else {
                    self.b.ins().uextend(types::I64, old)
                };
                // ok = (old == expected), compared after masking to the width, the
                // same as the interpreter's compare_exchange.
                let ok8 = self.b.ins().icmp(IntCC::Equal, old_ext, e_masked);
                let ok = self.b.ins().uextend(types::I64, ok8);
                self.write_scalar_place(dst_val, old_ext);
                self.write_scalar_place(dst_ok, ok);
            }
            Stmt::Fence {
                single_thread,
                order,
                ..
            } => {
                if !single_thread {
                    let _ = order;
                    self.b.ins().fence();
                }
                // A single-thread fence is a compiler fence and emits no instruction;
                // the compiler barrier already holds inside JIT-generated code.
            }
            // ===== 128-bit integer family =====
            Stmt::Bin128 {
                op,
                signed,
                a,
                b,
                dst,
                with_overflow,
            } => {
                let (alo, ahi) = self.read_wide(a);
                let (blo, bhi) = match b {
                    ir::Bin128Rhs::Wide(w) => self.read_wide(w),
                    ir::Bin128Rhs::Scalar(o) => {
                        let (v, _) = self.operand(o);
                        let z = self.b.ins().iconst(types::I64, 0);
                        (v, z)
                    }
                };
                // Add/Sub/Mul with overflow go to a helper so the result matches
                // Rust's overflowing_* exactly.
                if *with_overflow && matches!(op, IntBinOp::Add | IntBinOp::Sub | IntBinOp::Mul) {
                    let op_idx = match op {
                        IntBinOp::Add => 0,
                        IntBinOp::Sub => 1,
                        _ => 2,
                    };
                    let s = self.b.ins().iconst(types::I64, *signed as i64);
                    let oi = self.b.ins().iconst(types::I64, op_idx);
                    let ss = self.b.create_sized_stack_slot(StackSlotData::new(
                        StackSlotKind::ExplicitSlot,
                        16,
                        4,
                    ));
                    let outp = self.b.ins().stack_addr(types::I64, ss, 0);
                    let flag =
                        self.call_helper1("mirvm_bin128_ovf", &[oi, s, alo, ahi, blo, bhi, outp]);
                    let lo = self.b.ins().stack_load(types::I64, ss, 0);
                    let hi = self.b.ins().stack_load(types::I64, ss, 8);
                    self.write_wide(dst, lo, hi);
                    // The flag goes at dst + 16: the interpreter lays out a
                    // (u128, bool) with the flag at +16.
                    let da = self.place_addr(dst);
                    let f8 = self.b.ins().ireduce(types::I8, flag);
                    self.b.ins().store(MemFlagsData::trusted(), f8, da, 16);
                    return;
                }
                let x = self.i128_of(alo, ahi);
                let y = self.i128_of(blo, bhi);
                let r = match op {
                    IntBinOp::Add => self.b.ins().iadd(x, y),
                    IntBinOp::Sub => self.b.ins().isub(x, y),
                    IntBinOp::Mul => self.b.ins().imul(x, y),
                    IntBinOp::BitAnd => self.b.ins().band(x, y),
                    IntBinOp::BitOr => self.b.ins().bor(x, y),
                    IntBinOp::BitXor => self.b.ins().bxor(x, y),
                    IntBinOp::Shl => self.b.ins().ishl(x, blo),
                    IntBinOp::Shr => {
                        if *signed {
                            self.b.ins().sshr(x, blo)
                        } else {
                            self.b.ins().ushr(x, blo)
                        }
                    }
                    IntBinOp::Div | IntBinOp::Rem => {
                        // Cranelift's ISLE has no I128 division (udiv.i128 is
                        // unimplemented), so this goes to the mirvm_bin128_divrem
                        // helper: host wrapping operators, and division by zero exits
                        // with div_zero's message, as the interpreter does.
                        let ir_ = self
                            .b
                            .ins()
                            .iconst(types::I64, i64::from(matches!(op, IntBinOp::Rem)));
                        let s = self.b.ins().iconst(types::I64, *signed as i64);
                        self.call_out128("mirvm_bin128_divrem", &[ir_, s, alo, ahi, blo, bhi], dst);
                        return;
                    }
                };
                let (lo, hi) = {
                    let pair = self.b.ins().isplit(r);
                    (pair.0, pair.1)
                };
                self.write_wide(dst, lo, hi);
            }
            Stmt::Bit128 { op, src, dst } => {
                use crate::vm::ir::BitUnOp as B;
                let (lo, hi) = self.read_wide(src);
                let (rlo, rhi) = match op {
                    B::Bswap => {
                        // u128::swap_bytes = swap the halves, then bswap each.
                        let a = self.b.ins().bswap(hi);
                        let b = self.b.ins().bswap(lo);
                        (a, b)
                    }
                    B::Bitreverse => {
                        // u128::reverse_bits = swap the halves, then bitrev each.
                        let a = self.b.ins().bitrev(hi);
                        let b = self.b.ins().bitrev(lo);
                        (a, b)
                    }
                    _ => unreachable!("Bit128 only carries bswap/bitreverse"),
                };
                self.write_wide(dst, rlo, rhi);
            }
            Stmt::Bit128Count { op, src, dst } => {
                use crate::vm::ir::BitUnOp as B;
                let (lo, hi) = self.read_wide(src);
                let r = match op {
                    B::Popcount => {
                        let a = self.b.ins().popcnt(lo);
                        let b = self.b.ins().popcnt(hi);
                        self.b.ins().iadd(a, b)
                    }
                    B::Ctlz => {
                        let hz = self.b.ins().icmp_imm(IntCC::Equal, hi, 0);
                        let c_lo = self.b.ins().clz(lo);
                        let c64 = self.b.ins().iadd_imm(c_lo, 64);
                        let c_hi = self.b.ins().clz(hi);
                        self.b.ins().select(hz, c64, c_hi)
                    }
                    B::Cttz => {
                        let lz = self.b.ins().icmp_imm(IntCC::Equal, lo, 0);
                        let c_hi = self.b.ins().ctz(hi);
                        let c64 = self.b.ins().iadd_imm(c_hi, 64);
                        let c_lo = self.b.ins().ctz(lo);
                        self.b.ins().select(lz, c64, c_lo)
                    }
                    _ => unreachable!("Bit128Count only carries popcount/ctlz/cttz"),
                };
                self.write_scalar_place(dst, r);
            }
            Stmt::NicheDiscr128 {
                tag,
                niche_start,
                variants_start,
                variants_len,
                untagged,
                dst,
            } => {
                // Interpreter identity: rel = tag - niche_start with wrapping u128
                // arithmetic; rel < len gives variants_start + rel, otherwise
                // untagged.
                let (tlo, thi) = self.read_wide(tag);
                let t = self.i128_of(tlo, thi);
                let ns = self.iconst128(*niche_start);
                let rel = self.b.ins().isub(t, ns);
                let len = self.iconst128(*variants_len as u128);
                let hit = self.b.ins().icmp(IntCC::UnsignedLessThan, rel, len);
                let (rlo, _) = {
                    let pair = self.b.ins().isplit(rel);
                    (pair.0, pair.1)
                };
                let vs = self.b.ins().iconst(types::I64, *variants_start as i64);
                let hit_v = self.b.ins().iadd(vs, rlo);
                let un_v = self.b.ins().iconst(types::I64, *untagged as i64);
                let r = self.b.ins().select(hit, hit_v, un_v);
                self.write_scalar_place(dst, r);
            }
            Stmt::Wide128ToFloat {
                src,
                signed,
                to,
                dst,
            } => {
                // i128/u128 -> f16/f32/f64 always goes through a helper that uses the
                // host `as` cast: round to nearest, the same semantics as
                // compiler-builtins __float*ti*. Calling compiler-builtins directly is
                // wrong -- it returns the value in XMM0 while reading the argument
                // from I64/RAX.
                let (lo, hi) = self.read_wide(src);
                let s = self.b.ins().iconst(types::I64, *signed as i64);
                let f = match to {
                    ir::FloatW::F16 => {
                        let bits = self.call_helper1("mirvm_wide_to_f16", &[lo, hi, s]);
                        self.mask_val(bits, Width::W16)
                    }
                    ir::FloatW::F32 => {
                        let bits = self.call_helper1("mirvm_wide_to_f32", &[lo, hi, s]);
                        self.mask_val(bits, Width::W32)
                    }
                    ir::FloatW::F64 => {
                        // The f64 bit pattern is the slot value itself, so there is
                        // nothing to convert.
                        self.call_helper1("mirvm_wide_to_f64", &[lo, hi, s])
                    }
                };
                self.write_scalar_place(dst, f);
            }
            Stmt::FloatToWide128 {
                src,
                from,
                signed,
                dst,
            } => {
                let (v, _) = self.operand(src);
                let bits = match from {
                    ir::FloatW::F16 => self.mask_val(v, Width::W16),
                    ir::FloatW::F32 => self.mask_val(v, Width::W32),
                    ir::FloatW::F64 => v,
                };
                let kind = self.b.ins().iconst(
                    types::I64,
                    match from {
                        ir::FloatW::F16 => 0,
                        ir::FloatW::F32 => 1,
                        ir::FloatW::F64 => 2,
                    },
                );
                let s = self.b.ins().iconst(types::I64, *signed as i64);
                // The helper signature is (kind, v, signed, out): kind comes first.
                self.call_out128("mirvm_float_to_wide", &[kind, bits, s], dst);
            }
            // ===== f128 wide channel: everything goes through helpers =====
            Stmt::F128Bin { op, a, b, dst } => {
                let (alo, ahi) = self.read_wide(a);
                let (blo, bhi) = self.read_wide(b);
                let oi = self.b.ins().iconst(
                    types::I64,
                    match op {
                        ir::FloatOp::Add => 0,
                        ir::FloatOp::Sub => 1,
                        ir::FloatOp::Mul => 2,
                        ir::FloatOp::Rem => 3,
                        ir::FloatOp::Div => 4,
                    },
                );
                self.call_out128("mirvm_f128_bin", &[oi, alo, ahi, blo, bhi], dst);
            }
            Stmt::F128MathBin { op, a, b, dst } => {
                let (alo, ahi) = self.read_wide(a);
                let (blo, bhi) = match b {
                    ir::F128Rhs::Wide(w) => self.read_wide(w),
                    ir::F128Rhs::Scalar(o) => {
                        let (v, _) = self.operand(o);
                        let z = self.b.ins().iconst(types::I64, 0);
                        (v, z)
                    }
                };
                let oi = self.b.ins().iconst(
                    types::I64,
                    match op {
                        ir::MathBinOp::Pow => 0,
                        ir::MathBinOp::Powi => 1,
                        ir::MathBinOp::Copysign => 2,
                        ir::MathBinOp::Minnum => 3,
                        ir::MathBinOp::Maxnum => 4,
                    },
                );
                let z = self.b.ins().iconst(types::I64, 0);
                self.call_out128("mirvm_f128_math", &[oi, alo, ahi, blo, bhi, z, z], dst);
            }
            Stmt::F128Un { op, a, dst } => {
                let (alo, ahi) = self.read_wide(a);
                let oi = match op {
                    ir::F128UnOp::Neg => 0,
                    ir::F128UnOp::Math(m) => {
                        (match m {
                            ir::MathUnOp::Sqrt => 1,
                            ir::MathUnOp::Sin => 2,
                            ir::MathUnOp::Cos => 3,
                            ir::MathUnOp::Exp => 4,
                            ir::MathUnOp::Exp2 => 5,
                            ir::MathUnOp::Ln => 6,
                            ir::MathUnOp::Log2 => 7,
                            ir::MathUnOp::Log10 => 8,
                            ir::MathUnOp::Fabs => 9,
                            ir::MathUnOp::Floor => 10,
                            ir::MathUnOp::Ceil => 11,
                            ir::MathUnOp::Trunc => 12,
                            ir::MathUnOp::Round => 13,
                            ir::MathUnOp::RoundTiesEven => 14,
                        }) as i64
                    }
                };
                let oiv = self.b.ins().iconst(types::I64, oi);
                self.call_out128("mirvm_f128_un", &[oiv, alo, ahi], dst);
            }
            Stmt::F128Fma { a, b, c, dst } => {
                let (alo, ahi) = self.read_wide(a);
                let (blo, bhi) = self.read_wide(b);
                let (clo, chi) = self.read_wide(c);
                let oi = self.b.ins().iconst(types::I64, 5);
                self.call_out128("mirvm_f128_math", &[oi, alo, ahi, blo, bhi, clo, chi], dst);
            }
            Stmt::F128FromScalar { src, kind, dst } => {
                let (v, _) = self.operand(src);
                let k = self.b.ins().iconst(
                    types::I64,
                    match kind {
                        ir::F128Scalar::F(ir::FloatW::F16) => 0,
                        ir::F128Scalar::F(ir::FloatW::F32) => 1,
                        ir::F128Scalar::F(ir::FloatW::F64) => 2,
                        ir::F128Scalar::Int { signed: true } => 3,
                        ir::F128Scalar::Int { signed: false } => 4,
                    },
                );
                self.call_out128("mirvm_f128_from_scalar", &[k, v], dst);
            }
            Stmt::F128ToScalar { src, kind, w, dst } => {
                let (alo, ahi) = self.read_wide(src);
                let k = self.b.ins().iconst(
                    types::I64,
                    match kind {
                        ir::F128Scalar::F(ir::FloatW::F16) => 0,
                        ir::F128Scalar::F(ir::FloatW::F32) => 1,
                        ir::F128Scalar::F(ir::FloatW::F64) => 2,
                        ir::F128Scalar::Int { signed: true } => 3,
                        ir::F128Scalar::Int { signed: false } => 4,
                    },
                );
                let r = self.call_helper1("mirvm_f128_to_scalar", &[k, alo, ahi]);
                let r = self.mask_val(r, *w);
                self.write_scalar_place(dst, r);
            }
            Stmt::F128FromWideInt { src, signed, dst } => {
                let (lo, hi) = self.read_wide(src);
                let s = self.b.ins().iconst(types::I64, *signed as i64);
                self.call_out128("mirvm_f128_from_wide", &[s, lo, hi], dst);
            }
            Stmt::F128ToWideInt { src, signed, dst } => {
                let (alo, ahi) = self.read_wide(src);
                let s = self.b.ins().iconst(types::I64, *signed as i64);
                self.call_out128("mirvm_f128_to_wide", &[s, alo, ahi], dst);
            }
            // ===== The 15 SIMD statements plus Sat128, all through the
            // mirvm_simd_stmt helper, which shares its body with the interpreter's
            // simd_exec. Argument order is (stmt, a, b, c, dst, v0, v1) and unused
            // operands are passed as 0. =====
            Stmt::SimdBin { dst, a, b, .. } => {
                let pd = self.place_addr(dst);
                let pa = self.place_addr(a);
                let pb = self.place_addr(b);
                let sp = self
                    .b
                    .ins()
                    .iconst(types::I64, st as *const ir::Stmt as i64);
                let z = self.b.ins().iconst(types::I64, 0);
                let fref = self
                    .module
                    .declare_func_in_func(self.simd_stmt, self.b.func);
                self.b.ins().call(fref, &[sp, pa, pb, z, pd, z, z]);
            }
            Stmt::SimdUn { dst, a, .. } => {
                let pd = self.place_addr(dst);
                let pa = self.place_addr(a);
                let sp = self
                    .b
                    .ins()
                    .iconst(types::I64, st as *const ir::Stmt as i64);
                let z = self.b.ins().iconst(types::I64, 0);
                let fref = self
                    .module
                    .declare_func_in_func(self.simd_stmt, self.b.func);
                self.b.ins().call(fref, &[sp, pa, z, z, pd, z, z]);
            }
            Stmt::SimdFma { dst, a, b, c, .. } => {
                let pd = self.place_addr(dst);
                let pa = self.place_addr(a);
                let pb = self.place_addr(b);
                let pc = self.place_addr(c);
                let sp = self
                    .b
                    .ins()
                    .iconst(types::I64, st as *const ir::Stmt as i64);
                let z = self.b.ins().iconst(types::I64, 0);
                let fref = self
                    .module
                    .declare_func_in_func(self.simd_stmt, self.b.func);
                self.b.ins().call(fref, &[sp, pa, pb, pc, pd, z, z]);
            }
            Stmt::SimdFunnel {
                dst, a, b, shift, ..
            } => {
                let pd = self.place_addr(dst);
                let pa = self.place_addr(a);
                let pb = self.place_addr(b);
                let ps = self.place_addr(shift);
                let sp = self
                    .b
                    .ins()
                    .iconst(types::I64, st as *const ir::Stmt as i64);
                let z = self.b.ins().iconst(types::I64, 0);
                let fref = self
                    .module
                    .declare_func_in_func(self.simd_stmt, self.b.func);
                self.b.ins().call(fref, &[sp, pa, pb, ps, pd, z, z]);
            }
            Stmt::SimdCast { dst, src, .. } => {
                let pd = self.place_addr(dst);
                let ps = self.place_addr(src);
                let sp = self
                    .b
                    .ins()
                    .iconst(types::I64, st as *const ir::Stmt as i64);
                let z = self.b.ins().iconst(types::I64, 0);
                let fref = self
                    .module
                    .declare_func_in_func(self.simd_stmt, self.b.func);
                self.b.ins().call(fref, &[sp, ps, z, z, pd, z, z]);
            }
            Stmt::SimdSelect {
                mask, a, b, dst, ..
            } => {
                let pm = self.place_addr(mask);
                let pa = self.place_addr(a);
                let pb = self.place_addr(b);
                let pd = self.place_addr(dst);
                let sp = self
                    .b
                    .ins()
                    .iconst(types::I64, st as *const ir::Stmt as i64);
                let z = self.b.ins().iconst(types::I64, 0);
                let fref = self
                    .module
                    .declare_func_in_func(self.simd_stmt, self.b.func);
                self.b.ins().call(fref, &[sp, pm, pa, pb, pd, z, z]);
            }
            Stmt::SimdSelectBitmask {
                mask, a, b, dst, ..
            } => {
                let (m, _) = self.operand(mask);
                let pa = self.place_addr(a);
                let pb = self.place_addr(b);
                let pd = self.place_addr(dst);
                let sp = self
                    .b
                    .ins()
                    .iconst(types::I64, st as *const ir::Stmt as i64);
                let z = self.b.ins().iconst(types::I64, 0);
                let fref = self
                    .module
                    .declare_func_in_func(self.simd_stmt, self.b.func);
                self.b.ins().call(fref, &[sp, pa, pb, z, pd, m, z]);
            }
            Stmt::SimdGather {
                passthru,
                ptrs,
                mask,
                dst,
                ..
            } => {
                let pv = self.place_addr(passthru);
                let pp = self.place_addr(ptrs);
                let pm = self.place_addr(mask);
                let pd = self.place_addr(dst);
                let sp = self
                    .b
                    .ins()
                    .iconst(types::I64, st as *const ir::Stmt as i64);
                let z = self.b.ins().iconst(types::I64, 0);
                let fref = self
                    .module
                    .declare_func_in_func(self.simd_stmt, self.b.func);
                self.b.ins().call(fref, &[sp, pv, pp, pm, pd, z, z]);
            }
            Stmt::SimdScatter {
                values, ptrs, mask, ..
            } => {
                let pv = self.place_addr(values);
                let pp = self.place_addr(ptrs);
                let pm = self.place_addr(mask);
                let sp = self
                    .b
                    .ins()
                    .iconst(types::I64, st as *const ir::Stmt as i64);
                let z = self.b.ins().iconst(types::I64, 0);
                let fref = self
                    .module
                    .declare_func_in_func(self.simd_stmt, self.b.func);
                self.b.ins().call(fref, &[sp, pv, pp, pm, z, z, z]);
            }
            Stmt::SimdMaskedLoad {
                mask,
                base,
                passthru,
                dst,
                ..
            } => {
                let pm = self.place_addr(mask);
                let (pbase, _) = self.operand(base);
                let pv = self.place_addr(passthru);
                let pd = self.place_addr(dst);
                let sp = self
                    .b
                    .ins()
                    .iconst(types::I64, st as *const ir::Stmt as i64);
                let z = self.b.ins().iconst(types::I64, 0);
                let fref = self
                    .module
                    .declare_func_in_func(self.simd_stmt, self.b.func);
                self.b.ins().call(fref, &[sp, pm, pv, z, pd, pbase, z]);
            }
            Stmt::SimdMaskedStore {
                mask, base, values, ..
            } => {
                let pm = self.place_addr(mask);
                let (pbase, _) = self.operand(base);
                let pv = self.place_addr(values);
                let sp = self
                    .b
                    .ins()
                    .iconst(types::I64, st as *const ir::Stmt as i64);
                let z = self.b.ins().iconst(types::I64, 0);
                let fref = self
                    .module
                    .declare_func_in_func(self.simd_stmt, self.b.func);
                self.b.ins().call(fref, &[sp, pm, pv, z, z, pbase, z]);
            }
            Stmt::SimdExtractDyn { src, idx, dst, .. } => {
                let ps = self.place_addr(src);
                let (i, _) = self.operand(idx);
                let sp = self
                    .b
                    .ins()
                    .iconst(types::I64, st as *const ir::Stmt as i64);
                let z = self.b.ins().iconst(types::I64, 0);
                let fref = self
                    .module
                    .declare_func_in_func(self.simd_stmt, self.b.func);
                let call = self.b.ins().call(fref, &[sp, ps, z, z, z, i, z]);
                let r = self.b.inst_results(call)[0];
                self.write_scalar_place(dst, r);
            }
            Stmt::SimdInsertDyn {
                src, idx, val, dst, ..
            } => {
                let ps = self.place_addr(src);
                let pd = self.place_addr(dst);
                let (i, _) = self.operand(idx);
                let (v, _) = self.operand(val);
                let sp = self
                    .b
                    .ins()
                    .iconst(types::I64, st as *const ir::Stmt as i64);
                let z = self.b.ins().iconst(types::I64, 0);
                let fref = self
                    .module
                    .declare_func_in_func(self.simd_stmt, self.b.func);
                self.b.ins().call(fref, &[sp, ps, z, z, pd, i, v]);
            }
            Stmt::SimdArithOffset {
                ptrs, offsets, dst, ..
            } => {
                let pp = self.place_addr(ptrs);
                let po = self.place_addr(offsets);
                let pd = self.place_addr(dst);
                let sp = self
                    .b
                    .ins()
                    .iconst(types::I64, st as *const ir::Stmt as i64);
                let z = self.b.ins().iconst(types::I64, 0);
                let fref = self
                    .module
                    .declare_func_in_func(self.simd_stmt, self.b.func);
                self.b.ins().call(fref, &[sp, pp, po, z, pd, z, z]);
            }
            Stmt::SimdSplat { dst, val, .. } => {
                let pd = self.place_addr(dst);
                let (v, _) = self.operand(val);
                let sp = self
                    .b
                    .ins()
                    .iconst(types::I64, st as *const ir::Stmt as i64);
                let z = self.b.ins().iconst(types::I64, 0);
                let fref = self
                    .module
                    .declare_func_in_func(self.simd_stmt, self.b.func);
                self.b.ins().call(fref, &[sp, z, z, z, pd, v, z]);
            }
            Stmt::Sat128 { a, b, dst, .. } => {
                let pa = self.place_addr(a);
                let pb = self.place_addr(b);
                let pd = self.place_addr(dst);
                let sp = self
                    .b
                    .ins()
                    .iconst(types::I64, st as *const ir::Stmt as i64);
                let z = self.b.ins().iconst(types::I64, 0);
                let fref = self
                    .module
                    .declare_func_in_func(self.simd_stmt, self.b.func);
                self.b.ins().call(fref, &[sp, pa, pb, z, pd, z, z]);
            }
            // Trap/Nop. A statement-level Trap is the stmt form of mirvm_jit_trap,
            // which exits with engine_abort's message and error code 70. The trailing
            // trap is a fallback, because the helper does not return.
            Stmt::Trap(reason) => {
                let fref = self.module.declare_func_in_func(self.trap, self.b.func);
                let p = self.b.ins().iconst(types::I64, reason.as_ptr() as i64);
                let n = self.b.ins().iconst(types::I64, reason.len() as i64);
                let f = self.b.ins().iconst(types::I64, -1);
                self.b.ins().call(fref, &[p, n, f]);
                self.b.ins().trap(TrapCode::user(1).unwrap());
            }
            Stmt::Nop => {}
        }
    }

    /// Shared loop skeleton for both Repeat families: with `memmove_elem` each
    /// element is memmoved (RepeatBytes, starting at i = 1), otherwise a scalar is
    /// stored (RepeatScalar, starting at i = 0).
    fn repeat_loop(
        &mut self,
        base: Value,
        val_or_src: Value,
        count: u64,
        elem_size: u64,
        memmove_elem: bool,
    ) {
        let count_v = self.b.ins().iconst(types::I64, count as i64);
        let elem_v = self.b.ins().iconst(types::I64, elem_size as i64);
        let head = self.b.create_block();
        let body_blk = self.b.create_block();
        let tail = self.b.create_block();
        let ivar = self.b.declare_var(types::I64);
        let start = self
            .b
            .ins()
            .iconst(types::I64, if memmove_elem { 1 } else { 0 });
        self.b.def_var(ivar, start);
        self.b.ins().jump(head, &[]);
        self.b.switch_to_block(head);
        let iv = self.b.use_var(ivar);
        let done = self
            .b
            .ins()
            .icmp(IntCC::UnsignedGreaterThanOrEqual, iv, count_v);
        self.b.ins().brif(done, tail, &[], body_blk, &[]);
        self.b.switch_to_block(body_blk);
        let off = self.b.ins().imul(iv, elem_v);
        let p = self.b.ins().iadd(base, off);
        if memmove_elem {
            let fref = self.module.declare_func_in_func(self.memmove, self.b.func);
            self.b.ins().call(fref, &[p, val_or_src, elem_v]);
        } else {
            self.b
                .ins()
                .store(MemFlagsData::trusted(), val_or_src, p, 0);
        }
        let iv2 = self.b.use_var(ivar);
        let inc = self.b.ins().iadd_imm(iv2, 1);
        self.b.def_var(ivar, inc);
        self.b.ins().jump(head, &[]);
        self.b.switch_to_block(tail);
    }
}
