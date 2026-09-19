//! Bit-for-bit mirrors of the interpreter's integer arithmetic: `int_bin` for
//! the plain binary ops and `int_ovf` for the checked family.
//! `impl Translator` sub-block; the struct is in `super`.

use super::*;

impl Translator<'_, '_> {
    /// Bit-for-bit mirror of `interp::int_bin`. Add/Sub/Mul are signature-agnostic
    /// once masked; shifts use the same mod-64 amount as `wrapping_shl` and
    /// `wrapping_shr`; a signed Shr shifts the sextended view arithmetically, and
    /// `b & 63` agrees with CLIF's mod-64 shift amount.
    pub(super) fn int_bin(
        &mut self,
        op: IntBinOp,
        signed: bool,
        a: Value,
        b: Value,
        w: Width,
    ) -> Value {
        let r = match op {
            IntBinOp::Add => self.b.ins().iadd(a, b),
            IntBinOp::Sub => self.b.ins().isub(a, b),
            IntBinOp::Mul => self.b.ins().imul(a, b),
            IntBinOp::BitAnd => return self.b.ins().band(a, b),
            IntBinOp::BitOr => return self.b.ins().bor(a, b),
            IntBinOp::BitXor => return self.b.ins().bxor(a, b),
            IntBinOp::Shl => {
                // In the signed case the sextended high bits shift out of the masked region, so
                // the shape matches the interpreter's.
                let s = self.b.ins().ishl(a, b);
                return self.mask_val(s, w);
            }
            IntBinOp::Shr => {
                let s = if signed {
                    let x = self.sext_val(a, w);
                    self.b.ins().sshr(x, b)
                } else {
                    self.b.ins().ushr(a, b)
                };
                return self.mask_val(s, w);
            }
            IntBinOp::Div | IntBinOp::Rem => {
                // A zero check goes to mirvm_jit_div_zero, with the interpreter's message and
                // code. The signed MIN/-1 case is special-cased per branch: x86 idiv faults
                // with #DE and CLIF sdiv emits idiv directly.
                let is_rem = matches!(op, IntBinOp::Rem);
                let zero = self.b.ins().icmp_imm(IntCC::Equal, b, 0);
                self.div_zero_if(zero, is_rem, false);
                if signed {
                    // The slot invariant is a zero-extended I64, so signed semantics must sext
                    // to 64 bits first, matching int_bin's wrapping_div/rem; otherwise a negative
                    // value would be divided as a large positive.
                    let x = self.sext_val(a, w);
                    let y = self.sext_val(b, w);
                    let neg1 = self.b.ins().icmp_imm(IntCC::Equal, y, -1);
                    let triv_blk = self.b.create_block();
                    let norm_blk = self.b.create_block();
                    let join_blk = self.b.create_block();
                    self.b.ins().brif(neg1, triv_blk, &[], norm_blk, &[]);
                    self.b.switch_to_block(triv_blk);
                    let tv = if is_rem {
                        self.b.ins().iconst(types::I64, 0)
                    } else {
                        // wrapping_div(x, -1) = -x: MIN wraps around, and ineg has the same shape.
                        self.b.ins().ineg(x)
                    };
                    self.b.ins().jump(join_blk, &[tv.into()]);
                    self.b.switch_to_block(norm_blk);
                    let nv = if is_rem {
                        self.b.ins().srem(x, y)
                    } else {
                        self.b.ins().sdiv(x, y)
                    };
                    self.b.ins().jump(join_blk, &[nv.into()]);
                    self.b.switch_to_block(join_blk);
                    let r = self.b.append_block_param(join_blk, types::I64);
                    self.mask_val(r, w)
                } else {
                    let r = if is_rem {
                        self.b.ins().urem(a, b)
                    } else {
                        self.b.ins().udiv(a, b)
                    };
                    self.mask_val(r, w)
                }
            }
        };
        self.mask_val(r, w)
    }

    /// Bit-for-bit mirror of `interp::int_ovf`, as 64-bit identities lifted to 128
    /// bits:
    /// - unsigned w<64: the sum/product is exact in 64 bits, so ovf = exact > mask;
    ///   Sub ovf = a < b.
    /// - unsigned W64: Add ovf = wrapped (r < a); Mul ovf = umulhi != 0; Sub as above.
    /// - signed w<64: exact in 64 bits after sextending, so compare against [lo, hi].
    /// - signed W64: the standard sign identities for Add/Sub; Mul ovf is
    ///   smulhi != (r >> 63).
    pub(super) fn int_ovf(
        &mut self,
        op: OvfOp,
        signed: bool,
        a: Value,
        b: Value,
        w: Width,
    ) -> (Value, Value) {
        let bb = &mut *self.b;
        if !signed {
            match (op, w) {
                (OvfOp::Sub, _) => {
                    let r = bb.ins().isub(a, b);
                    let f8 = bb.ins().icmp(IntCC::UnsignedLessThan, a, b);
                    let f = bb.ins().uextend(types::I64, f8);
                    (self.mask_val(r, w), f)
                }
                (_, Width::W64) => match op {
                    OvfOp::Add => {
                        let r = bb.ins().iadd(a, b);
                        let f8 = bb.ins().icmp(IntCC::UnsignedLessThan, r, a);
                        let f = bb.ins().uextend(types::I64, f8);
                        (r, f)
                    }
                    OvfOp::Mul => {
                        let r = bb.ins().imul(a, b);
                        let hi = bb.ins().umulhi(a, b);
                        let f8 = bb.ins().icmp_imm(IntCC::NotEqual, hi, 0);
                        let f = bb.ins().uextend(types::I64, f8);
                        (r, f)
                    }
                    OvfOp::Sub => unreachable!(),
                },
                (_, _) => {
                    // w<64: exact within 64 bits.
                    let exact = match op {
                        OvfOp::Add => bb.ins().iadd(a, b),
                        OvfOp::Mul => bb.ins().imul(a, b),
                        OvfOp::Sub => unreachable!(),
                    };
                    let f8 = bb
                        .ins()
                        .icmp_imm(IntCC::UnsignedGreaterThan, exact, w.mask() as i64);
                    let f = bb.ins().uextend(types::I64, f8);
                    (self.mask_val(exact, w), f)
                }
            }
        } else {
            let x = self.sext_val(a, w);
            let y = self.sext_val(b, w);
            let bb = &mut *self.b;
            if w == Width::W64 {
                let r = match op {
                    OvfOp::Add => bb.ins().iadd(x, y),
                    OvfOp::Sub => bb.ins().isub(x, y),
                    OvfOp::Mul => bb.ins().imul(x, y),
                };
                let f8 = match op {
                    // add overflows when both inputs share a sign and the result does not; sub when
                    // the inputs differ and the result differs from the minuend.
                    OvfOp::Add => {
                        let t1 = bb.ins().bxor(r, x);
                        let t2 = bb.ins().bxor(r, y);
                        let t = bb.ins().band(t1, t2);
                        bb.ins().icmp_imm(IntCC::SignedLessThan, t, 0)
                    }
                    OvfOp::Sub => {
                        let t1 = bb.ins().bxor(x, y);
                        let t2 = bb.ins().bxor(r, x);
                        let t = bb.ins().band(t1, t2);
                        bb.ins().icmp_imm(IntCC::SignedLessThan, t, 0)
                    }
                    OvfOp::Mul => {
                        let hi = bb.ins().smulhi(x, y);
                        let sgn = bb.ins().sshr_imm(r, 63);
                        bb.ins().icmp(IntCC::NotEqual, hi, sgn)
                    }
                };
                let f = bb.ins().uextend(types::I64, f8);
                (self.mask_val(r, w), f)
            } else {
                let r = match op {
                    OvfOp::Add => bb.ins().iadd(x, y),
                    OvfOp::Sub => bb.ins().isub(x, y),
                    OvfOp::Mul => bb.ins().imul(x, y),
                };
                let (lo, hi) = match w {
                    Width::W8 => (i8::MIN as i64, i8::MAX as i64),
                    Width::W16 => (i16::MIN as i64, i16::MAX as i64),
                    Width::W32 => (i32::MIN as i64, i32::MAX as i64),
                    Width::W64 => unreachable!(),
                };
                let under = bb.ins().icmp_imm(IntCC::SignedLessThan, r, lo);
                let over = bb.ins().icmp_imm(IntCC::SignedGreaterThan, r, hi);
                let f8 = bb.ins().bor(under, over);
                let f = bb.ins().uextend(types::I64, f8);
                (self.mask_val(r, w), f)
            }
        }
    }
}
