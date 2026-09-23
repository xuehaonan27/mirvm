//! Rvalue lowering: the exhaustive `ir::Rvalue` match, from the scalar reads through the float
//! and wide channels to the SIMD family. `impl Translator` sub-block; the struct is in `super`.

use super::*;

impl Translator<'_, '_> {
    pub(super) fn rvalue(&mut self, rv: &ir::Rvalue) -> Value {
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
                // The mirvm_tls_ref helper shares its body with semantics::tls::tls_addr: the
                // per-thread instance block is materialized lazily.
                let fref = self.module.declare_func_in_func(self.tls_ref, self.b.func);
                let i = self.b.ins().iconst(types::I64, i64::from(*id));
                let call = self.b.ins().call(fref, &[i]);
                self.b.inst_results(call)[0]
            }
            // ===== The three SIMD rvalues, through the mirvm_simd_rv helper, which shares
            // its body with semantics::simd. It takes the rvalue's real address
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
}
