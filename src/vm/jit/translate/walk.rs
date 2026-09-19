//! Statement, rvalue and terminator lowering: the three exhaustive matches that
//! lower one IR body, plus the shared Repeat skeleton. `impl Translator`
//! sub-block; the struct and `build` are in `super`.

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

    fn rvalue(&mut self, rv: &ir::Rvalue) -> Value {
        use crate::vm::ir::Rvalue as R;
        match rv {
            R::Use(a) => self.operand(a).0,
            R::IntBin { op, signed, a, b } => {
                let (av, w) = self.operand(a);
                let (bv, _) = self.operand(b);
                self.int_bin(*op, *signed, av, bv, w)
            }
            R::IntCmp { cc, signed, a, b } => {
                let (av, w) = self.operand(a);
                let (bv, _) = self.operand(b);
                let (x, y) = if *signed {
                    (self.sext_val(av, w), self.sext_val(bv, w))
                } else {
                    (av, bv) // The slot invariant already zero-extended these.
                };
                let c = match (cc, signed) {
                    (IntCc::Eq, _) => IntCC::Equal,
                    (IntCc::Ne, _) => IntCC::NotEqual,
                    (IntCc::Lt, true) => IntCC::SignedLessThan,
                    (IntCc::Le, true) => IntCC::SignedLessThanOrEqual,
                    (IntCc::Gt, true) => IntCC::SignedGreaterThan,
                    (IntCc::Ge, true) => IntCC::SignedGreaterThanOrEqual,
                    (IntCc::Lt, false) => IntCC::UnsignedLessThan,
                    (IntCc::Le, false) => IntCC::UnsignedLessThanOrEqual,
                    (IntCc::Gt, false) => IntCC::UnsignedGreaterThan,
                    (IntCc::Ge, false) => IntCC::UnsignedGreaterThanOrEqual,
                };
                let b1 = self.b.ins().icmp(c, x, y);
                self.b.ins().uextend(types::I64, b1)
            }
            R::IntCmp3 { signed, a, b } => {
                // Mirror of the interpreter's IntCmp3 rvalue: a three-way compare
                // yields the Ordering i8 bit pattern (-1 = 0xFF, which truncates to
                // the same value at W8).
                let (av, w) = self.operand(a);
                let (bv, _) = self.operand(b);
                let (x, y) = if *signed {
                    (self.sext_val(av, w), self.sext_val(bv, w))
                } else {
                    (av, bv)
                };
                let ltc = if *signed {
                    IntCC::SignedLessThan
                } else {
                    IntCC::UnsignedLessThan
                };
                let lt = self.b.ins().icmp(ltc, x, y);
                let eq = self.b.ins().icmp(IntCC::Equal, x, y);
                let neg1 = self.b.ins().iconst(types::I64, 0xFF);
                let one = self.b.ins().iconst(types::I64, 1);
                let zero = self.b.ins().iconst(types::I64, 0);
                let ge = self.b.ins().select(eq, zero, one);
                self.b.ins().select(lt, neg1, ge)
            }
            R::NicheDiscr {
                tag,
                niche_start,
                variants_start,
                variants_len,
                untagged,
            } => {
                // Mirror of the interpreter's NicheDiscr rvalue: rel = tag -
                // niche_start wraps at the tag's width; rel < len gives
                // variants_start + rel, otherwise untagged.
                let (tv, w) = self.operand(tag);
                let ns = self.b.ins().iconst(types::I64, *niche_start as i64);
                let rel = self.b.ins().isub(tv, ns);
                let rel = self.mask_val(rel, w);
                let hit = self
                    .b
                    .ins()
                    .icmp_imm(IntCC::UnsignedLessThan, rel, *variants_len as i64);
                let vs = self.b.ins().iconst(types::I64, *variants_start as i64);
                let tagged = self.b.ins().iadd(vs, rel);
                let un = self.b.ins().iconst(types::I64, *untagged as i64);
                self.b.ins().select(hit, tagged, un)
            }
            R::NotBits(a) => {
                let (v, w) = self.operand(a);
                let n = self.b.ins().bnot(v);
                self.mask_val(n, w)
            }
            R::NotBool(a) => {
                let (v, _) = self.operand(a);
                self.b.ins().bxor_imm(v, 1)
            }
            R::Neg(a) => {
                let (v, w) = self.operand(a);
                let n = self.b.ins().ineg(v);
                self.mask_val(n, w)
            }
            R::Cast { from, to, a } => {
                let (v, _) = self.operand(a);
                let x = if from.1 {
                    let s = self.sext_val(v, from.0);
                    // The I64 view after sextending; masked to the target width below.
                    s
                } else {
                    self.mask_val(v, from.0)
                };
                self.mask_val(x, *to)
            }
            // ===== Memory / address family =====
            R::Ref(expr) => self.place_addr(expr),
            R::PtrOffset { ptr, count, stride } => {
                // The true-address model passes the bit pattern through with wrapping, as the
                // interpreter does.
                let (p, _) = self.operand(ptr);
                let (c, _) = self.operand(count);
                let scaled = self.b.ins().imul_imm(c, *stride as i64);
                self.b.ins().iadd(p, scaled)
            }
            R::PtrDiff { a, b, stride } => {
                // (a - b) / stride as i64 division; stride is a frozen constant and admit
                // rejects 0.
                let (av, _) = self.operand(a);
                let (bv, _) = self.operand(b);
                let d = self.b.ins().isub(av, bv);
                let sv = self.b.ins().iconst(types::I64, *stride as i64);
                self.b.ins().sdiv(d, sv)
            }
            R::UMax { a, b } => {
                let (av, _) = self.operand(a);
                let (bv, _) = self.operand(b);
                self.b.ins().umax(av, bv)
            }
            // ===== Scalar additions =====
            R::IntSat { op, signed, a, b } => {
                // Mirror of the interpreter's int_saturating: int_ovf picks the direction,
                // then clamp.
                let (av, w) = self.operand(a);
                let (bv, _) = self.operand(b);
                let (val, ovf) = self.int_ovf(*op, *signed, av, bv, w);
                let ovf8 = self.b.ins().icmp_imm(IntCC::NotEqual, ovf, 0);
                let m = w.mask() as i64;
                let clamp = if *signed {
                    let (x, y) = (self.sext_val(av, w), self.sext_val(bv, w));
                    let toward_max = match op {
                        OvfOp::Add => self.b.ins().icmp_imm(IntCC::SignedGreaterThan, y, 0),
                        OvfOp::Sub => self.b.ins().icmp_imm(IntCC::SignedLessThan, y, 0),
                        OvfOp::Mul => {
                            let x0 = self.b.ins().icmp_imm(IntCC::SignedGreaterThan, x, 0);
                            let y0 = self.b.ins().icmp_imm(IntCC::SignedGreaterThan, y, 0);
                            self.b.ins().icmp(IntCC::Equal, x0, y0)
                        }
                    };
                    let maxv = self.b.ins().iconst(types::I64, m >> 1);
                    let minv = self
                        .b
                        .ins()
                        .iconst(types::I64, (((w.mask() >> 1) + 1) & w.mask()) as i64);
                    self.b.ins().select(toward_max, maxv, minv)
                } else {
                    self.b
                        .ins()
                        .iconst(types::I64, if matches!(op, OvfOp::Sub) { 0 } else { m })
                };
                let masked = self.mask_val(val, w);
                self.b.ins().select(ovf8, clamp, masked)
            }
            R::BitUn { op, a } => {
                let (v, w) = self.operand(a);
                use crate::vm::ir::BitUnOp as B;
                match op {
                    B::Popcount | B::Ctlz | B::Cttz => {
                        let n = if w == Width::W64 {
                            v
                        } else {
                            self.b.ins().ireduce(Self::narrow_ty(w), v)
                        };
                        let r = match op {
                            B::Popcount => self.b.ins().popcnt(n),
                            B::Ctlz => self.b.ins().clz(n),
                            B::Cttz => self.b.ins().ctz(n),
                            _ => unreachable!(),
                        };
                        if w == Width::W64 {
                            r
                        } else {
                            self.b.ins().uextend(types::I64, r)
                        }
                    }
                    B::Bswap => {
                        if w == Width::W8 {
                            // Interpreter: W8 is the identity (v & 0xff).
                            self.mask_val(v, w)
                        } else {
                            let n = self.b.ins().ireduce(Self::narrow_ty(w), v);
                            let r = self.b.ins().bswap(n);
                            self.b.ins().uextend(types::I64, r)
                        }
                    }
                    B::Bitreverse => {
                        let n = if w == Width::W64 {
                            v
                        } else {
                            self.b.ins().ireduce(Self::narrow_ty(w), v)
                        };
                        let r = self.b.ins().bitrev(n);
                        if w == Width::W64 {
                            r
                        } else {
                            self.b.ins().uextend(types::I64, r)
                        }
                    }
                }
            }
            R::MemCmp { a, b, n } => {
                // Host memcmp import; the i32 result is sign-extended, the same channel the
                // interpreter uses.
                let (pa, _) = self.operand(a);
                let (pb, _) = self.operand(b);
                let (nv, _) = self.operand(n);
                let fref = self.module.declare_func_in_func(self.memcmp, self.b.func);
                let call = self.b.ins().call(fref, &[pa, pb, nv]);
                let r32 = self.b.inst_results(call)[0];
                self.b.ins().sextend(types::I64, r32)
            }
            R::AtomicLoad { addr, width, order } => {
                // CLIF atomics are always SeqCst: Cranelift 0.133 offers no weaker ordering,
                // so the JIT side uniformly uses the strongest one. Weaker orderings exist
                // only on the interpreted path, and the stronger ordering here stays inside
                // the nondeterminism envelope the RAM model allows.
                let (p, _) = self.operand(addr);
                let _ = order;
                let v =
                    self.b
                        .ins()
                        .atomic_load(Self::narrow_ty(*width), MemFlagsData::trusted(), p);
                if *width == Width::W64 {
                    v
                } else {
                    self.b.ins().uextend(types::I64, v)
                }
            }
            // ===== f32/f64 floats and the Math family =====
            R::FloatBin { op, fw, a, b } => {
                let (av, _) = self.operand(a);
                let (bv, _) = self.operand(b);
                if matches!(fw, ir::FloatW::F16) {
                    // f16 goes through a helper, matching the interpreter's direct host
                    // computation; the op codes are f128_bin's.
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
                    let r = self.call_helper1("mirvm_f16_bin", &[oi, av, bv]);
                    self.mask_val(r, Width::W16)
                } else {
                    let fa = self.as_float(av, *fw);
                    let fb = self.as_float(bv, *fw);
                    let r = match op {
                        ir::FloatOp::Add => self.b.ins().fadd(fa, fb),
                        ir::FloatOp::Sub => self.b.ins().fsub(fa, fb),
                        ir::FloatOp::Mul => self.b.ins().fmul(fa, fb),
                        ir::FloatOp::Div => self.b.ins().fdiv(fa, fb),
                        // IEEE fmod, Rust's `%` on floats: the libm fmod channel, as in the
                        // interpreter.
                        ir::FloatOp::Rem => self.call_libm_bin("fmod", fa, fb, *fw),
                    };
                    self.as_bits(r, *fw)
                }
            }
            R::FloatCmp { cc, fw, a, b } => {
                // IEEE partial-order semantics (every comparison is false for NaN except Ne):
                // the CLIF ordered family, with Ne mapped to NotEqual.
                use cranelift_codegen::ir::condcodes::FloatCC;
                let (av, _) = self.operand(a);
                let (bv, _) = self.operand(b);
                if matches!(fw, ir::FloatW::F16) {
                    let ci = self.b.ins().iconst(
                        types::I64,
                        match cc {
                            IntCc::Eq => 0,
                            IntCc::Ne => 1,
                            IntCc::Lt => 2,
                            IntCc::Le => 3,
                            IntCc::Gt => 4,
                            IntCc::Ge => 5,
                        },
                    );
                    self.call_helper1("mirvm_f16_cmp", &[ci, av, bv])
                } else {
                    let fa = self.as_float(av, *fw);
                    let fb = self.as_float(bv, *fw);
                    let c = match cc {
                        IntCc::Eq => FloatCC::Equal,
                        IntCc::Ne => FloatCC::NotEqual,
                        IntCc::Lt => FloatCC::LessThan,
                        IntCc::Le => FloatCC::LessThanOrEqual,
                        IntCc::Gt => FloatCC::GreaterThan,
                        IntCc::Ge => FloatCC::GreaterThanOrEqual,
                    };
                    let b1 = self.b.ins().fcmp(c, fa, fb);
                    self.b.ins().uextend(types::I64, b1)
                }
            }
            R::FloatNeg { fw, a } => {
                let (av, _) = self.operand(a);
                if matches!(fw, ir::FloatW::F16) {
                    let r = self.call_helper1("mirvm_f16_neg", &[av]);
                    self.mask_val(r, Width::W16)
                } else {
                    let fa = self.as_float(av, *fw);
                    let r = self.b.ins().fneg(fa);
                    self.as_bits(r, *fw)
                }
            }
            R::FloatCast { from, to, a } => {
                let (av, _) = self.operand(a);
                if matches!(from, ir::FloatW::F16) || matches!(to, ir::FloatW::F16) {
                    // Any conversion touching f16 goes through a helper (kind: 1 = f16->f32,
                    // 2 = f16->f64, 3 = f32->f16, 4 = f64->f16).
                    let k = self.b.ins().iconst(
                        types::I64,
                        match (from, to) {
                            (ir::FloatW::F16, ir::FloatW::F32) => 1,
                            (ir::FloatW::F16, ir::FloatW::F64) => 2,
                            (ir::FloatW::F32, ir::FloatW::F16) => 3,
                            (ir::FloatW::F64, ir::FloatW::F16) => 4,
                            _ => unreachable!("no other pair involves f16"),
                        },
                    );
                    let r = self.call_helper1("mirvm_f16_cast", &[k, av]);
                    let w = match to {
                        ir::FloatW::F16 => Width::W16,
                        ir::FloatW::F32 => Width::W32,
                        ir::FloatW::F64 => Width::W64,
                    };
                    self.mask_val(r, w)
                } else if from == to {
                    self.mask_val(
                        av,
                        match to {
                            ir::FloatW::F32 => Width::W32,
                            ir::FloatW::F64 => Width::W64,
                            ir::FloatW::F16 => Width::W16,
                        },
                    )
                } else {
                    let fa = self.as_float(av, *from);
                    let r = match (from, to) {
                        (ir::FloatW::F32, ir::FloatW::F64) => self.b.ins().fpromote(types::F64, fa),
                        (ir::FloatW::F64, ir::FloatW::F32) => self.b.ins().fdemote(types::F32, fa),
                        _ => unreachable!("f16 conversions go through a helper"),
                    };
                    self.as_bits(r, *to)
                }
            }
            R::FloatToInt {
                from,
                to,
                signed,
                a,
            } => {
                // Rust `as` saturation semantics (NaN -> 0, out of range -> the boundary):
                // signed W32/64 uses fcvt_to_sint_sat directly; signed W8/16 saturates to I32
                // and is then clamped to the target range; unsigned uses fcvt_to_uint_sat to
                // I64 and then an unsigned min against the width mask.
                let (av, _) = self.operand(a);
                if matches!(from, ir::FloatW::F16) {
                    // f16 -> int through a helper (kind: 0 = i8, 1 = u8, 2 = i16, 3 = u16,
                    // 4 = i32, 5 = u32, 6 = i64, 7 = u64).
                    let k = self.b.ins().iconst(
                        types::I64,
                        match (to, signed) {
                            (Width::W8, true) => 0,
                            (Width::W8, false) => 1,
                            (Width::W16, true) => 2,
                            (Width::W16, false) => 3,
                            (Width::W32, true) => 4,
                            (Width::W32, false) => 5,
                            (Width::W64, true) => 6,
                            (Width::W64, false) => 7,
                        },
                    );
                    let r = self.call_helper1("mirvm_f16_to_int", &[k, av]);
                    self.mask_val(r, *to)
                } else {
                    let fa = self.as_float(av, *from);
                    if *signed {
                        let i64v = match to {
                            Width::W64 => self.b.ins().fcvt_to_sint_sat(types::I64, fa),
                            _ => {
                                let v32 = self.b.ins().fcvt_to_sint_sat(types::I32, fa);
                                self.b.ins().sextend(types::I64, v32)
                            }
                        };
                        match to {
                            Width::W64 => i64v,
                            Width::W32 => self.mask_val(i64v, *to),
                            _ => {
                                // W8/16: the I32 saturation is clamped again to [iN::MIN, iN::MAX].
                                let (lo, hi) = match to {
                                    Width::W8 => (i8::MIN as i64, i8::MAX as i64),
                                    Width::W16 => (i16::MIN as i64, i16::MAX as i64),
                                    _ => unreachable!(),
                                };
                                let hi_v = self.b.ins().iconst(types::I64, hi);
                                let lo_v = self.b.ins().iconst(types::I64, lo);
                                let c1 = self.b.ins().smin(i64v, hi_v);
                                let c2 = self.b.ins().smax(c1, lo_v);
                                self.mask_val(c2, *to)
                            }
                        }
                    } else {
                        let u64v = self.b.ins().fcvt_to_uint_sat(types::I64, fa);
                        let m = self.b.ins().iconst(types::I64, to.mask() as i64);
                        self.b.ins().umin(u64v, m)
                    }
                }
            }
            R::IntToFloat { from, to, a } => {
                let (av, _) = self.operand(a);
                if matches!(to, ir::FloatW::F16) {
                    // int -> f16 through a helper, with the same kind codes as to_int.
                    let (fw, signed) = *from;
                    let k = self.b.ins().iconst(
                        types::I64,
                        match (fw, signed) {
                            (Width::W8, true) => 0,
                            (Width::W8, false) => 1,
                            (Width::W16, true) => 2,
                            (Width::W16, false) => 3,
                            (Width::W32, true) => 4,
                            (Width::W32, false) => 5,
                            (Width::W64, true) => 6,
                            (Width::W64, false) => 7,
                        },
                    );
                    let r = self.call_helper1("mirvm_f16_from_int", &[k, av]);
                    self.mask_val(r, Width::W16)
                } else {
                    let (fw, signed) = *from;
                    let t = Self::float_ty(*to);
                    let x = if signed {
                        self.sext_val(av, fw)
                    } else {
                        self.mask_val(av, fw)
                    };
                    let src = if fw == Width::W64 {
                        x
                    } else {
                        self.b.ins().ireduce(types::I32, x)
                    };
                    let f = if signed {
                        self.b.ins().fcvt_from_sint(t, src)
                    } else {
                        self.b.ins().fcvt_from_uint(t, src)
                    };
                    self.as_bits(f, *to)
                }
            }
            R::MathUn { op, fw, a } => {
                use crate::vm::ir::MathUnOp as M;
                let (av, _) = self.operand(a);
                if matches!(fw, ir::FloatW::F16) {
                    // f16 math goes through helpers, the same host f16 methods the interpreter
                    // calls. This branch is also a guard: as_float(F16) cannot be represented and
                    // would panic.
                    let oi = self.b.ins().iconst(
                        types::I64,
                        match op {
                            M::Sqrt => 0,
                            M::Sin => 1,
                            M::Cos => 2,
                            M::Exp => 3,
                            M::Exp2 => 4,
                            M::Ln => 5,
                            M::Log2 => 6,
                            M::Log10 => 7,
                            M::Fabs => 8,
                            M::Floor => 9,
                            M::Ceil => 10,
                            M::Trunc => 11,
                            M::Round => 12,
                            M::RoundTiesEven => 13,
                        },
                    );
                    let r = self.call_helper1("mirvm_f16_math_un", &[oi, av]);
                    self.mask_val(r, Width::W16)
                } else {
                    let fa = self.as_float(av, *fw);
                    let r = match op {
                        M::Sqrt => self.call_libm_un("sqrt", fa, *fw),
                        M::Sin => self.call_libm_un("sin", fa, *fw),
                        M::Cos => self.call_libm_un("cos", fa, *fw),
                        M::Exp => self.call_libm_un("exp", fa, *fw),
                        M::Exp2 => self.call_libm_un("exp2", fa, *fw),
                        M::Ln => self.call_libm_un("log", fa, *fw),
                        M::Log2 => self.call_libm_un("log2", fa, *fw),
                        M::Log10 => self.call_libm_un("log10", fa, *fw),
                        M::Fabs => self.call_libm_un("fabs", fa, *fw),
                        M::Floor => self.call_libm_un("floor", fa, *fw),
                        M::Ceil => self.call_libm_un("ceil", fa, *fw),
                        M::Trunc => self.call_libm_un("trunc", fa, *fw),
                        M::Round => self.call_libm_un("round", fa, *fw),
                        // round_ties_even is C99 rint, which the interpreter and Rust both use.
                        M::RoundTiesEven => self.call_libm_un("rint", fa, *fw),
                    };
                    self.as_bits(r, *fw)
                }
            }
            R::MathBin { op, fw, a, b } => {
                use crate::vm::ir::MathBinOp as M;
                let (av, _) = self.operand(a);
                let (bv, _) = self.operand(b);
                if matches!(fw, ir::FloatW::F16) {
                    // Binary f16 math goes through a helper; powi passes the raw i32 bits of b.
                    let oi = self.b.ins().iconst(
                        types::I64,
                        match op {
                            M::Pow => 0,
                            M::Powi => 1,
                            M::Copysign => 2,
                            M::Minnum => 3,
                            M::Maxnum => 4,
                        },
                    );
                    let r = self.call_helper1("mirvm_f16_math_bin", &[oi, av, bv]);
                    self.mask_val(r, Width::W16)
                } else {
                    let fa = self.as_float(av, *fw);
                    let fb = self.as_float(bv, *fw);
                    let r = match op {
                        M::Pow => self.call_libm_bin("pow", fa, fb, *fw),
                        M::Powi => {
                            let n32 = self.b.ins().ireduce(types::I32, bv);
                            self.call_powi(fa, n32, *fw)
                        }
                        M::Copysign => self.call_libm_bin("copysign", fa, fb, *fw),
                        M::Minnum => self.call_libm_bin("fmin", fa, fb, *fw),
                        M::Maxnum => self.call_libm_bin("fmax", fa, fb, *fw),
                    };
                    self.as_bits(r, *fw)
                }
            }
            R::MathFma { fw, a, b, c } => {
                // fma rounds once, like the host mul_add. The fmuladd instruction may or may
                // not fuse, but fusing is always allowed, so this stays on the same side of the
                // allowed set as the interpreter's fused result.
                let (av, _) = self.operand(a);
                let (bv, _) = self.operand(b);
                let (cv, _) = self.operand(c);
                if matches!(fw, ir::FloatW::F16) {
                    let r = self.call_helper1("mirvm_f16_fma", &[av, bv, cv]);
                    self.mask_val(r, Width::W16)
                } else {
                    let fa = self.as_float(av, *fw);
                    let fb = self.as_float(bv, *fw);
                    let fc = self.as_float(cv, *fw);
                    let r = self.b.ins().fma(fa, fb, fc);
                    self.as_bits(r, *fw)
                }
            }
            // ===== f128 comparison in the rvalue wide channel =====
            R::F128Cmp { cc, a, b } => {
                let (alo, ahi) = self.read_wide(a);
                let (blo, bhi) = self.read_wide(b);
                let ci = self.b.ins().iconst(
                    types::I64,
                    match cc {
                        IntCc::Eq => 0,
                        IntCc::Ne => 1,
                        IntCc::Lt => 2,
                        IntCc::Le => 3,
                        IntCc::Gt => 4,
                        IntCc::Ge => 5,
                    },
                );
                self.call_helper1("mirvm_f128_cmp", &[ci, alo, ahi, blo, bhi])
            }
            // ===== 128-bit integer comparison on the rvalue side =====
            R::Cmp128 { cc, signed, a, b } => {
                let (alo, ahi) = self.read_wide(a);
                let (blo, bhi) = self.read_wide(b);
                let x = self.i128_of(alo, ahi);
                let y = self.i128_of(blo, bhi);
                let c = match (cc, signed) {
                    (IntCc::Eq, _) => IntCC::Equal,
                    (IntCc::Ne, _) => IntCC::NotEqual,
                    (IntCc::Lt, true) => IntCC::SignedLessThan,
                    (IntCc::Le, true) => IntCC::SignedLessThanOrEqual,
                    (IntCc::Gt, true) => IntCC::SignedGreaterThan,
                    (IntCc::Ge, true) => IntCC::SignedGreaterThanOrEqual,
                    (IntCc::Lt, false) => IntCC::UnsignedLessThan,
                    (IntCc::Le, false) => IntCC::UnsignedLessThanOrEqual,
                    (IntCc::Gt, false) => IntCC::UnsignedGreaterThan,
                    (IntCc::Ge, false) => IntCC::UnsignedGreaterThanOrEqual,
                };
                let b1 = self.b.ins().icmp(c, x, y);
                self.b.ins().uextend(types::I64, b1)
            }
            R::TlsRef(id) => {
                // The mirvm_tls_ref helper shares its body with interp::tls_addr: the
                // per-thread instance block is materialized lazily.
                let fref = self.module.declare_func_in_func(self.tls_ref, self.b.func);
                let i = self.b.ins().iconst(types::I64, i64::from(*id));
                let call = self.b.ins().call(fref, &[i]);
                self.b.inst_results(call)[0]
            }
            // ===== The three SIMD rvalues, through the mirvm_simd_rv helper, which shares
            // its body with the interpreter's simd_exec. It takes the rvalue's real address
            // and the vector place's address. =====
            R::SimdBitmask { a, .. } => {
                // Lane bitmask; at 64 bits or fewer no masking is needed, as in the interpreter
                // body.
                let pa = self.place_addr(a);
                let rp = self
                    .b
                    .ins()
                    .iconst(types::I64, rv as *const ir::Rvalue as i64);
                let fref = self.module.declare_func_in_func(self.simd_rv, self.b.func);
                let call = self.b.ins().call(fref, &[rp, pa]);
                self.b.inst_results(call)[0]
            }
            R::SimdReduce { a, .. } => {
                // bool 0/1, the same as the interpreter body's `acc as u64`.
                let pa = self.place_addr(a);
                let rp = self
                    .b
                    .ins()
                    .iconst(types::I64, rv as *const ir::Rvalue as i64);
                let fref = self.module.declare_func_in_func(self.simd_rv, self.b.func);
                let call = self.b.ins().call(fref, &[rp, pa]);
                self.b.inst_results(call)[0]
            }
            R::SimdReduceArith { a, lane_bytes, .. } => {
                // Scalar bit pattern at the lane width; the interpreter body already masks each
                // op back to lw, and this masks to the same width.
                let pa = self.place_addr(a);
                let rp = self
                    .b
                    .ins()
                    .iconst(types::I64, rv as *const ir::Rvalue as i64);
                let fref = self.module.declare_func_in_func(self.simd_rv, self.b.func);
                let call = self.b.ins().call(fref, &[rp, pa]);
                let r = self.b.inst_results(call)[0];
                self.mask_val(
                    r,
                    Width::from_bytes(u64::from(*lane_bytes)).expect("lane width"),
                )
            }
        }
    }

    pub(super) fn term(
        &mut self,
        func: u32,
        body: &ir::FuncBody,
        t: &Terminator,
        blocks: &[cranelift_codegen::ir::Block],
    ) {
        match t {
            Terminator::Goto(bb) => {
                self.b.ins().jump(blocks[*bb as usize], &[]);
            }
            Terminator::SwitchInt {
                discr,
                targets,
                otherwise,
            } => match discr {
                SwitchDiscr::Scalar(op) => {
                    let (v, _) = self.operand(op);
                    // An icmp+brif chain; the values are sparse, so a br_table is left as a
                    // possible optimization.
                    for (val, bb) in targets {
                        let hit = self.b.ins().icmp_imm(IntCC::Equal, v, *val as u64 as i64);
                        let next = self.b.create_block();
                        self.b.ins().brif(hit, blocks[*bb as usize], &[], next, &[]);
                        self.b.switch_to_block(next);
                    }
                    self.b.ins().jump(blocks[*otherwise as usize], &[]);
                }
                SwitchDiscr::Wide(pe) => {
                    // 128-bit discriminant: read the whole place at once (iconcat) and compare
                    // against each target as an I128 constant, so both the targets and the
                    // discriminant keep their full 128 bits.
                    let (lo, hi) = self.read_wide(pe);
                    let v = self.i128_of(lo, hi);
                    for (val, bb) in targets {
                        let c = self.iconst128(*val);
                        let hit = self.b.ins().icmp(IntCC::Equal, v, c);
                        let next = self.b.create_block();
                        self.b.ins().brif(hit, blocks[*bb as usize], &[], next, &[]);
                        self.b.switch_to_block(next);
                    }
                    self.b.ins().jump(blocks[*otherwise as usize], &[]);
                }
            },
            Terminator::Call {
                callee,
                args,
                ret,
                target,
                unwind,
                role,
            } => {
                // Flatten the arguments in the interpreter's Call order: for RetDest::Indirect
                // the destination's real address comes first, then each argument. Lowering has
                // already split Pair arguments into two slots and appended the phantom tail
                // argument.
                let mut av: Vec<Value> = Vec::with_capacity(args.len() + 1);
                if let RetDest::Indirect(dst) = ret {
                    let a = self.place_addr(dst);
                    av.push(a);
                }
                for a in args {
                    av.push(self.operand(a).0);
                }
                // Write-back template, the same shape for PLT results and the c2i ret_ss.
                macro_rules! write_back {
                    ($lo:expr, $hi:expr) => {
                        match ret {
                            RetDest::Ignore | RetDest::Indirect(_) => {}
                            RetDest::Scalar(ScalarPlace::Slot(s)) => {
                                self.def_slot(*s, $lo);
                            }
                            RetDest::Pair(ScalarPlace::Slot(pl), ScalarPlace::Slot(ph)) => {
                                self.def_slot(*pl, $lo);
                                self.def_slot(*ph, $hi);
                            }
                            _ => unreachable!("admit screens the ret shapes"),
                        }
                    };
                }
                match unwind {
                    UnwindAction::Cleanup(bb) => {
                        // try_call: `normal` is the ok block (write back, then jump to target) and
                        // exception-table tag 0 goes to pad(TryCallExn(0)). Every case goes through
                        // c2i-try_call because that is the one authoritative implementation; a PLT
                        // try_call_indirect is left as a possible optimization.
                        let args_ss = self.b.create_sized_stack_slot(StackSlotData::new(
                            StackSlotKind::ExplicitSlot,
                            (av.len().max(1) * 8) as u32,
                            3,
                        ));
                        for (i, v) in av.iter().enumerate() {
                            self.b.ins().stack_store(*v, args_ss, (i * 8) as i32);
                        }
                        let ret_ss = self.b.create_sized_stack_slot(StackSlotData::new(
                            StackSlotKind::ExplicitSlot,
                            16,
                            3,
                        ));
                        let mut sig0 = self.module.make_signature();
                        for _ in 0..4 {
                            sig0.params.push(AbiParam::new(types::I64));
                        }
                        let (et, ok, pad) = self.prepare_cleanup(sig0);
                        let fref = self.module.declare_func_in_func(self.c2i, self.b.func);
                        let fv = self.b.ins().iconst(types::I64, *callee as i64);
                        let ap = self.b.ins().stack_addr(types::I64, args_ss, 0);
                        let nv = self.b.ins().iconst(types::I64, av.len() as i64);
                        let rp = self.b.ins().stack_addr(types::I64, ret_ss, 0);
                        self.b.ins().try_call(fref, &[fv, ap, nv, rp], et);
                        self.enter_cleanup_continuation(pad, *bb, ok, blocks);
                        let lo = self.b.ins().stack_load(types::I64, ret_ss, 0);
                        let hi = self.b.ins().stack_load(types::I64, ret_ss, 8);
                        write_back!(lo, hi);
                        self.b.ins().jump(blocks[*target as usize], &[]);
                    }
                    UnwindAction::Terminate => {
                        // The Terminate boundary is mirvm_call_terminate, a c2i-shaped wrapper with
                        // the same semantics as the interpreter's call_guarding_terminate.
                        let args_ss = self.b.create_sized_stack_slot(StackSlotData::new(
                            StackSlotKind::ExplicitSlot,
                            (av.len().max(1) * 8) as u32,
                            3,
                        ));
                        for (i, v) in av.iter().enumerate() {
                            self.b.ins().stack_store(*v, args_ss, (i * 8) as i32);
                        }
                        let ret_ss = self.b.create_sized_stack_slot(StackSlotData::new(
                            StackSlotKind::ExplicitSlot,
                            16,
                            3,
                        ));
                        let fref = self
                            .module
                            .declare_func_in_func(self.call_terminate, self.b.func);
                        let fv = self.b.ins().iconst(types::I64, *callee as i64);
                        let ap = self.b.ins().stack_addr(types::I64, args_ss, 0);
                        let nv = self.b.ins().iconst(types::I64, av.len() as i64);
                        let rp = self.b.ins().stack_addr(types::I64, ret_ss, 0);
                        self.b.ins().call(fref, &[fv, ap, nv, rp]);
                        let lo = self.b.ins().stack_load(types::I64, ret_ss, 0);
                        let hi = self.b.ins().stack_load(types::I64, ret_ss, 8);
                        write_back!(lo, hi);
                        self.b.ins().jump(blocks[*target as usize], &[]);
                    }
                    UnwindAction::Continue => {
                        if matches!(role, ir::CallRole::MainPanicBoundary) {
                            let args_ss = self.b.create_sized_stack_slot(StackSlotData::new(
                                StackSlotKind::ExplicitSlot,
                                (av.len().max(1) * 8) as u32,
                                3,
                            ));
                            for (i, v) in av.iter().enumerate() {
                                self.b.ins().stack_store(*v, args_ss, (i * 8) as i32);
                            }
                            let ret_ss = self.b.create_sized_stack_slot(StackSlotData::new(
                                StackSlotKind::ExplicitSlot,
                                16,
                                3,
                            ));
                            let fref = self
                                .module
                                .declare_func_in_func(self.call_main_catch, self.b.func);
                            let fv = self.b.ins().iconst(types::I64, *callee as i64);
                            let ap = self.b.ins().stack_addr(types::I64, args_ss, 0);
                            let nv = self.b.ins().iconst(types::I64, av.len() as i64);
                            let rp = self.b.ins().stack_addr(types::I64, ret_ss, 0);
                            self.b.ins().call(fref, &[fv, ap, nv, rp]);
                            let lo = self.b.ins().stack_load(types::I64, ret_ss, 0);
                            let hi = self.b.ins().stack_load(types::I64, ret_ss, 8);
                            write_back!(lo, hi);
                        } else {
                            let cb = &self.shared.module.funcs[*callee as usize];
                            let plt = callee_abi(cb).filter(|cabi| cabi.nparams == av.len());
                            if let Some(cabi) = plt {
                                // Hot path: indirect through the PLT's memory -- load
                                // slots_fast[callee], then call_indirect. The shape is
                                // constant, and the trampoline-to-fast upgrade is invisible to
                                // the call site. The slot array is the one for this body's
                                // domain: a trace body only ever resolves trace slots, a plain
                                // body only plain slots.
                                let slot_addr = &self.shared.jit.slots_for(self.domain).slots_fast
                                    [*callee as usize]
                                    as *const std::sync::atomic::AtomicU64
                                    as i64;
                                let ap = self.b.ins().iconst(types::I64, slot_addr);
                                let fp =
                                    self.b
                                        .ins()
                                        .load(types::I64, MemFlagsData::trusted(), ap, 0);
                                let sig = {
                                    let mut s = self.module.make_signature();
                                    for _ in 0..av.len() {
                                        s.params.push(AbiParam::new(types::I64));
                                    }
                                    for _ in 0..cabi.nrets {
                                        s.returns.push(AbiParam::new(types::I64));
                                    }
                                    s
                                };
                                let sigref = self.b.import_signature(sig);
                                let call = self.b.ins().call_indirect(sigref, fp, &av);
                                let lo = if cabi.nrets >= 1 {
                                    self.b.inst_results(call)[0]
                                } else {
                                    self.b.ins().iconst(types::I64, 0)
                                };
                                let hi = if cabi.nrets >= 2 {
                                    self.b.inst_results(call)[1]
                                } else {
                                    self.b.ins().iconst(types::I64, 0)
                                };
                                write_back!(lo, hi);
                            } else {
                                // Cold path: call c2i directly from the call site, packing the
                                // flattened arguments back to the interpreter. The interpreter
                                // already consumes a flattened argument vector, so any callee
                                // ABI agrees; this is also where panic-like branches end up.
                                let args_ss = self.b.create_sized_stack_slot(StackSlotData::new(
                                    StackSlotKind::ExplicitSlot,
                                    (av.len().max(1) * 8) as u32,
                                    3,
                                ));
                                for (i, v) in av.iter().enumerate() {
                                    self.b.ins().stack_store(*v, args_ss, (i * 8) as i32);
                                }
                                let ret_ss = self.b.create_sized_stack_slot(StackSlotData::new(
                                    StackSlotKind::ExplicitSlot,
                                    16,
                                    3,
                                ));
                                let fref = self.module.declare_func_in_func(self.c2i, self.b.func);
                                let fv = self.b.ins().iconst(types::I64, *callee as i64);
                                let ap = self.b.ins().stack_addr(types::I64, args_ss, 0);
                                let nv = self.b.ins().iconst(types::I64, av.len() as i64);
                                let rp = self.b.ins().stack_addr(types::I64, ret_ss, 0);
                                self.b.ins().call(fref, &[fv, ap, nv, rp]);
                                let lo = self.b.ins().stack_load(types::I64, ret_ss, 0);
                                let hi = self.b.ins().stack_load(types::I64, ret_ss, 8);
                                write_back!(lo, hi);
                            }
                        }
                        self.b.ins().jump(blocks[*target as usize], &[]);
                    }
                }
            }
            Terminator::Return => {
                self.poll_signals();
                match body.ret {
                    RetAbi::Zst => {
                        self.b.ins().return_(&[]);
                    }
                    RetAbi::Scalar(s) => {
                        let v = self.read_slot(s);
                        self.b.ins().return_(&[v]);
                    }
                    RetAbi::Pair(lo_s, hi_s) => {
                        let lo = self.read_slot(lo_s);
                        let hi = self.read_slot(hi_s);
                        self.b.ins().return_(&[lo, hi]);
                    }
                    RetAbi::Indirect {
                        ret_off,
                        size,
                        sret_off,
                    } => {
                        // The same semantics as the interpreter's Return: read the destination
                        // address from the sret slot, memcpy _0 -> dst for `size` bytes, and
                        // return no values.
                        let dst = self.read_slot(Slot {
                            off: sret_off,
                            width: Width::W64,
                        });
                        let src = self.addr_of_local(ret_off);
                        let fref = self.module.declare_func_in_func(self.memmove, self.b.func);
                        let n = self.b.ins().iconst(types::I64, i64::from(size));
                        self.b.ins().call(fref, &[dst, src, n]);
                        self.b.ins().return_(&[]);
                    }
                }
            }
            Terminator::CallIndirect {
                callee,
                args,
                ret,
                target,
                unwind,
                null_ok,
                native_sig,
            } => {
                // The mirvm_call_indirect helper dispatches as the interpreter's CallIndirect
                // arm does: look the address up in fn_addrs and call_guest, or on a miss with a
                // native_sig, ffi::call_addr.
                let (addr, _) = self.operand(callee);
                // Flatten the arguments in the interpreter's order: for RetDest::Indirect the
                // destination address first, then each argument.
                let mut av: Vec<Value> = Vec::with_capacity(args.len() + 1);
                if let RetDest::Indirect(dst) = ret {
                    let a = self.place_addr(dst);
                    av.push(a);
                }
                for a in args {
                    av.push(self.operand(a).0);
                }
                let args_ss = self.b.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    (av.len().max(1) * 8) as u32,
                    3,
                ));
                for (i, v) in av.iter().enumerate() {
                    self.b.ins().stack_store(*v, args_ss, (i * 8) as i32);
                }
                let ret_ss = self.b.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    16,
                    3,
                ));
                let fref = self
                    .module
                    .declare_func_in_func(self.call_indirect, self.b.func);
                let ap = self.b.ins().stack_addr(types::I64, args_ss, 0);
                let nv = self.b.ins().iconst(types::I64, av.len() as i64);
                let rp = self.b.ins().stack_addr(types::I64, ret_ss, 0);
                let nok = self.b.ins().iconst(types::I64, i64::from(*null_ok));
                let nsig = self.b.ins().iconst(
                    types::I64,
                    native_sig
                        .as_ref()
                        .map_or(0, |s| s as *const ir::ForeignSig as i64),
                );
                let fv = self.b.ins().iconst(types::I64, func as i64);
                if let UnwindAction::Cleanup(bb) = unwind {
                    // try_call: the ok block writes back and then jumps to target; the pad jumps to
                    // the IR cleanup block.
                    let mut sig0 = self.module.make_signature();
                    for _ in 0..8 {
                        sig0.params.push(AbiParam::new(types::I64));
                    }
                    let (et, ok, pad) = self.prepare_cleanup(sig0);
                    let z = self.b.ins().iconst(types::I64, 0);
                    self.b
                        .ins()
                        .try_call(fref, &[addr, ap, nv, rp, nok, nsig, fv, z], et);
                    self.enter_cleanup_continuation(pad, *bb, ok, blocks);
                    let lo = self.b.ins().stack_load(types::I64, ret_ss, 0);
                    let hi = self.b.ins().stack_load(types::I64, ret_ss, 8);
                    self.write_ret(ret, lo, hi);
                    self.b.ins().jump(blocks[*target as usize], &[]);
                } else {
                    // Continue / Terminate are distinguished by a flag; the Terminate flag means
                    // the call is wrapped in catch_unwind + abort, the same semantics as
                    // call_guarding_terminate.
                    let term = self.b.ins().iconst(
                        types::I64,
                        i64::from(matches!(unwind, UnwindAction::Terminate)),
                    );
                    self.b
                        .ins()
                        .call(fref, &[addr, ap, nv, rp, nok, nsig, fv, term]);
                    let lo = self.b.ins().stack_load(types::I64, ret_ss, 0);
                    let hi = self.b.ins().stack_load(types::I64, ret_ss, 8);
                    self.write_ret(ret, lo, hi);
                    self.b.ins().jump(blocks[*target as usize], &[]);
                }
            }
            Terminator::InlineAsm {
                stub,
                buf_size,
                ins,
                outs,
                target,
            } => {
                // Call the asm stub's real address directly, using the same slot ABI as the
                // interpreter's InlineAsm arm: a stack buffer; scalar `ins` occupy the low 8
                // bytes of a slot while VecBytes are copied at full width; call fn(*mut u8);
                // then read `outs` back.
                let buf = self.b.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    (*buf_size).max(1),
                    4,
                ));
                let base = self.b.ins().stack_addr(types::I64, buf, 0);
                for (off, v) in ins {
                    match v {
                        ir::AsmIoVal::Scalar(o) => {
                            let (x, _) = self.operand(o);
                            self.b.ins().stack_store(x, buf, *off as i32);
                        }
                        ir::AsmIoVal::VecBytes(pe, size) => {
                            let src = self.place_addr(pe);
                            let dst = self.b.ins().stack_addr(types::I64, buf, *off as i32);
                            let n = self.b.ins().iconst(types::I64, i64::from(*size));
                            let fref = self.module.declare_func_in_func(self.memmove, self.b.func);
                            self.b.ins().call(fref, &[dst, src, n]);
                        }
                    }
                }
                let stub_addr = self.b.ins().iconst(
                    types::I64,
                    self.shared.module.asm_stub_addrs[*stub as usize] as i64,
                );
                let mut s = self.module.make_signature();
                s.params.push(AbiParam::new(types::I64));
                let sigref = self.b.import_signature(s);
                self.b.ins().call_indirect(sigref, stub_addr, &[base]);
                for (off, d) in outs {
                    match d {
                        ir::AsmIoDst::Scalar(sp) => {
                            let v = self.b.ins().stack_load(types::I64, buf, *off as i32);
                            self.write_scalar_place(sp, v);
                        }
                        ir::AsmIoDst::VecBytes(pe, size) => {
                            let dst = self.place_addr(pe);
                            let src = self.b.ins().stack_addr(types::I64, buf, *off as i32);
                            let n = self.b.ins().iconst(types::I64, i64::from(*size));
                            let fref = self.module.declare_func_in_func(self.memmove, self.b.func);
                            self.b.ins().call(fref, &[dst, src, n]);
                        }
                    }
                }
                self.b.ins().jump(blocks[*target as usize], &[]);
            }
            Terminator::CallForeign {
                sym,
                sig,
                args,
                ret,
                target,
                unwind,
            } => {
                // The mirvm_call_foreign helper mirrors the interpreter's CallForeign arm:
                // materialize thunk_args, resolve the C1 Indirect destination, restore the
                // enlarged pthread stack, and call ffi::call itself.
                let mut av: Vec<Value> = Vec::with_capacity(args.len());
                for a in args {
                    av.push(self.operand(a).0);
                }
                // An aggregate returned by value uses the Indirect destination: the ffi layer
                // memcpys to the destination's real address.
                let ret_dst = if let RetDest::Indirect(dst) = ret {
                    self.place_addr(dst)
                } else {
                    self.b.ins().iconst(types::I64, 0)
                };
                let args_ss = self.b.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    (av.len().max(1) * 8) as u32,
                    3,
                ));
                for (i, v) in av.iter().enumerate() {
                    self.b.ins().stack_store(*v, args_ss, (i * 8) as i32);
                }
                let fref = self
                    .module
                    .declare_func_in_func(self.call_foreign, self.b.func);
                let sp = self.b.ins().iconst(types::I64, sym.as_ptr() as i64);
                let sl = self.b.ins().iconst(types::I64, sym.len() as i64);
                let sg = self
                    .b
                    .ins()
                    .iconst(types::I64, sig as *const ir::ForeignSig as i64);
                let ap = self.b.ins().stack_addr(types::I64, args_ss, 0);
                let nv = self.b.ins().iconst(types::I64, av.len() as i64);
                let fv = self.b.ins().iconst(types::I64, func as i64);
                macro_rules! foreign_write_back {
                    ($r:expr) => {
                        let r = $r;
                        match ret {
                            RetDest::Ignore => {}
                            RetDest::Scalar(p) => {
                                self.write_scalar_place(p, r);
                            }
                            // The by-value aggregate bytes have already been memcpy'd to dst by the
                            // ffi layer.
                            RetDest::Indirect(_) => {}
                            _ => unreachable!("admit screens the foreign return shapes"),
                        }
                    };
                }
                if let UnwindAction::Cleanup(bb) = unwind {
                    // try_call: the ok block writes back and then jumps to target; the pad jumps to
                    // the IR cleanup block. The eight-parameter signature matches
                    // mirvm_call_foreign's actual arguments: seven slots plus the terminate flag.
                    let mut sig0 = self.module.make_signature();
                    for _ in 0..8 {
                        sig0.params.push(AbiParam::new(types::I64));
                    }
                    sig0.returns.push(AbiParam::new(types::I64));
                    let (et, ok, pad) = self.prepare_cleanup(sig0);
                    let z = self.b.ins().iconst(types::I64, 0);
                    self.b
                        .ins()
                        .try_call(fref, &[sp, sl, sg, ap, nv, ret_dst, fv, z], et);
                    self.enter_cleanup_continuation(pad, *bb, ok, blocks);
                    foreign_write_back!(self.b.block_params(ok)[0]);
                    self.b.ins().jump(blocks[*target as usize], &[]);
                } else {
                    let term = self.b.ins().iconst(
                        types::I64,
                        i64::from(matches!(unwind, UnwindAction::Terminate)),
                    );
                    let call = self
                        .b
                        .ins()
                        .call(fref, &[sp, sl, sg, ap, nv, ret_dst, fv, term]);
                    foreign_write_back!(self.b.inst_results(call)[0]);
                    self.b.ins().jump(blocks[*target as usize], &[]);
                }
            }
            Terminator::CallBuiltin {
                builtin,
                args,
                ret,
                target,
                unwind,
                role,
            } => {
                // The trace domain has its own syscall site. The recorder is read from the
                // register the activation boundary pinned; the site itself consults neither TLS
                // nor the session, and the syscall plus its paired Enter/Exit records all happen
                // inside the helper.
                if self.domain == CodeDomain::Trace
                    && matches!(builtin, ir::Builtin::HostSyscallTrace)
                    && matches!(unwind, UnwindAction::Continue)
                {
                    self.trace_syscall_site(args, ret, *target, blocks);
                    return;
                }
                // The four allocation builtins take the mirvm_alloc fast path -- the engine
                // heap's single entry point, dispatched by tag. Everything else goes through
                // mirvm_call_builtin, which shares its body with exec_builtin.
                let alloc_tag = match builtin {
                    ir::Builtin::RustAlloc => Some(0i64),
                    ir::Builtin::RustAllocZeroed => Some(1),
                    ir::Builtin::RustRealloc => Some(2),
                    ir::Builtin::RustDealloc => Some(3),
                    _ => None,
                };
                if let UnwindAction::Cleanup(bb) = unwind {
                    // Everything goes through the generic mirvm_call_builtin path with try_call;
                    // the allocation builtins live in that same body, so they must not be routed
                    // around the exception table via the fast path.
                    let mut av: Vec<Value> = Vec::with_capacity(args.len());
                    for a in args {
                        av.push(self.operand(a).0);
                    }
                    let ret_dst = if let RetDest::Indirect(dst) = ret {
                        self.place_addr(dst)
                    } else {
                        self.b.ins().iconst(types::I64, 0)
                    };
                    let args_ss = self.b.create_sized_stack_slot(StackSlotData::new(
                        StackSlotKind::ExplicitSlot,
                        (av.len().max(1) * 8) as u32,
                        3,
                    ));
                    for (i, v) in av.iter().enumerate() {
                        self.b.ins().stack_store(*v, args_ss, (i * 8) as i32);
                    }
                    let ret_ss = self.b.create_sized_stack_slot(StackSlotData::new(
                        StackSlotKind::ExplicitSlot,
                        16,
                        3,
                    ));
                    let fref = self
                        .module
                        .declare_func_in_func(self.call_builtin, self.b.func);
                    let bp = self
                        .b
                        .ins()
                        .iconst(types::I64, builtin as *const ir::Builtin as i64);
                    let ap = self.b.ins().stack_addr(types::I64, args_ss, 0);
                    let nv = self.b.ins().iconst(types::I64, av.len() as i64);
                    let rp = self.b.ins().stack_addr(types::I64, ret_ss, 0);
                    let fv = self.b.ins().iconst(types::I64, func as i64);
                    let mut sig0 = self.module.make_signature();
                    for _ in 0..8 {
                        sig0.params.push(AbiParam::new(types::I64));
                    }
                    let (et, ok, pad) = self.prepare_cleanup(sig0);
                    let z = self.b.ins().iconst(types::I64, 0);
                    let role = self.b.ins().iconst(types::I64, *role as i64);
                    self.b
                        .ins()
                        .try_call(fref, &[bp, ap, nv, ret_dst, fv, rp, z, role], et);
                    self.enter_cleanup_continuation(pad, *bb, ok, blocks);
                    let lo = self.b.ins().stack_load(types::I64, ret_ss, 0);
                    let hi = self.b.ins().stack_load(types::I64, ret_ss, 8);
                    self.write_ret(ret, lo, hi);
                    self.b.ins().jump(blocks[*target as usize], &[]);
                } else if matches!(unwind, UnwindAction::Continue)
                    && let Some(tag) = alloc_tag
                    && matches!(ret, RetDest::Ignore | RetDest::Scalar(ScalarPlace::Slot(_)))
                {
                    // A fixed four-slot argument vector: realloc uses all four, while
                    // alloc/dealloc pad with 0 and the helper consumes them by tag, since
                    // exec_builtin's body only reads the a(i) it needs.
                    let mut av: Vec<Value> = Vec::with_capacity(4);
                    for i in 0..4 {
                        av.push(match args.get(i) {
                            Some(o) => self.operand(o).0,
                            None => self.b.ins().iconst(types::I64, 0),
                        });
                    }
                    let fref = self.module.declare_func_in_func(self.alloc, self.b.func);
                    let tv = self.b.ins().iconst(types::I64, tag);
                    let fv = self.b.ins().iconst(types::I64, func as i64);
                    let call = self
                        .b
                        .ins()
                        .call(fref, &[tv, av[0], av[1], av[2], av[3], fv]);
                    let r = self.b.inst_results(call)[0];
                    match ret {
                        RetDest::Ignore => {}
                        RetDest::Scalar(ScalarPlace::Slot(s)) => self.def_slot(*s, r),
                        _ => unreachable!("this branch screens the ret shapes"),
                    }
                    self.b.ins().jump(blocks[*target as usize], &[]);
                } else {
                    // Flatten the arguments with no sret prepended -- a builtin's Indirect
                    // destination is evaluated separately, in the interpreter's thin-arm order.
                    // ret_dst is the Indirect destination's real address, otherwise 0.
                    let mut av: Vec<Value> = Vec::with_capacity(args.len());
                    for a in args {
                        av.push(self.operand(a).0);
                    }
                    let ret_dst = if let RetDest::Indirect(dst) = ret {
                        self.place_addr(dst)
                    } else {
                        self.b.ins().iconst(types::I64, 0)
                    };
                    let args_ss = self.b.create_sized_stack_slot(StackSlotData::new(
                        StackSlotKind::ExplicitSlot,
                        (av.len().max(1) * 8) as u32,
                        3,
                    ));
                    for (i, v) in av.iter().enumerate() {
                        self.b.ins().stack_store(*v, args_ss, (i * 8) as i32);
                    }
                    let ret_ss = self.b.create_sized_stack_slot(StackSlotData::new(
                        StackSlotKind::ExplicitSlot,
                        16,
                        3,
                    ));
                    let fref = self
                        .module
                        .declare_func_in_func(self.call_builtin, self.b.func);
                    let bp = self
                        .b
                        .ins()
                        .iconst(types::I64, builtin as *const ir::Builtin as i64);
                    let ap = self.b.ins().stack_addr(types::I64, args_ss, 0);
                    let nv = self.b.ins().iconst(types::I64, av.len() as i64);
                    let rp = self.b.ins().stack_addr(types::I64, ret_ss, 0);
                    let fv = self.b.ins().iconst(types::I64, func as i64);
                    // The Terminate flag, with the same catch_unwind + abort semantics as
                    // elsewhere.
                    let term = self.b.ins().iconst(
                        types::I64,
                        i64::from(matches!(unwind, UnwindAction::Terminate)),
                    );
                    let role = self.b.ins().iconst(types::I64, *role as i64);
                    self.b
                        .ins()
                        .call(fref, &[bp, ap, nv, ret_dst, fv, rp, term, role]);
                    let lo = self.b.ins().stack_load(types::I64, ret_ss, 0);
                    let hi = self.b.ins().stack_load(types::I64, ret_ss, 8);
                    self.write_ret(ret, lo, hi);
                    self.b.ins().jump(blocks[*target as usize], &[]);
                }
            }
            Terminator::Resume => {
                // Read the exception pointer from exception_var -- defined by the pad's
                // TryCallExn(0), or 0 from the entry when no pad reached it, since build
                // pre-declares it for any function containing Resume -- and call _Unwind_Resume
                // to continue unwinding, as cg_clif's Resume does.
                let ev = self.exception_var();
                let exn = self.b.use_var(ev);
                let fref = self
                    .module
                    .declare_func_in_func(self.unwind_resume, self.b.func);
                self.b.ins().call(fref, &[exn]);
                self.b.ins().trap(TrapCode::user(1).unwrap());
            }
            Terminator::TerminateAbort => {
                // mirvm_jit_terminate_abort, with the message and code of the interpreter's
                // TerminateAbort arm: an UnwindTerminate (double panic or ABI boundary) aborts.
                let fref = self
                    .module
                    .declare_func_in_func(self.terminate_abort, self.b.func);
                self.b.ins().call(fref, &[]);
                self.b.ins().trap(TrapCode::user(1).unwrap());
            }
            Terminator::Unreachable => {
                let fref = self
                    .module
                    .declare_func_in_func(self.unreachable, self.b.func);
                let fv = self.b.ins().iconst(types::I64, func as i64);
                self.b.ins().call(fref, &[fv]);
                self.b.ins().trap(TrapCode::user(1).unwrap());
            }
            // The Trap-stub terminator: the terminator form of mirvm_jit_trap, which takes
            // the function name and exits with the interpreter's runblocks-arm message and
            // error code 70.
            Terminator::Trap(reason) => {
                let fref = self.module.declare_func_in_func(self.trap, self.b.func);
                let p = self.b.ins().iconst(types::I64, reason.as_ptr() as i64);
                let n = self.b.ins().iconst(types::I64, reason.len() as i64);
                let fv = self.b.ins().iconst(types::I64, func as i64);
                self.b.ins().call(fref, &[p, n, fv]);
                self.b.ins().trap(TrapCode::user(1).unwrap());
            }
        }
    }
}
