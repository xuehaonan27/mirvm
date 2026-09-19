//! The two non-scalar value channels: the float channel (a value is the bit
//! pattern in an I64 slot, matching the interpreter) and the wide-value channel
//! (a 16-byte place as a `(lo, hi)` pair / I128). `impl Translator` sub-block;
//! the struct is in `super`.

use super::*;

impl Translator<'_, '_> {
    // ===== Float channel: a value is the bit pattern in an I64 slot, the same
    // representation the interpreter uses =====

    pub(super) fn float_ty(w: ir::FloatW) -> cranelift_codegen::ir::Type {
        match w {
            ir::FloatW::F32 => types::F32,
            ir::FloatW::F64 => types::F64,
            ir::FloatW::F16 => unreachable!("f16 goes through a helper"),
        }
    }

    /// Slot bits to a float register value: a bitcast, with F32 ireduced first.
    pub(super) fn as_float(&mut self, v: Value, w: ir::FloatW) -> Value {
        match w {
            ir::FloatW::F32 => {
                let n = self.b.ins().ireduce(types::I32, v);
                self.b.ins().bitcast(types::F32, MemFlagsData::new(), n)
            }
            ir::FloatW::F64 => self.b.ins().bitcast(types::F64, MemFlagsData::new(), v),
            ir::FloatW::F16 => unreachable!("f16 goes through a helper"),
        }
    }

    /// Float register value back to slot bits: a bitcast back, with F32 uextended
    /// afterwards.
    pub(super) fn as_bits(&mut self, v: Value, w: ir::FloatW) -> Value {
        match w {
            ir::FloatW::F32 => {
                let n = self.b.ins().bitcast(types::I32, MemFlagsData::new(), v);
                self.b.ins().uextend(types::I64, n)
            }
            ir::FloatW::F64 => self.b.ins().bitcast(types::I64, MemFlagsData::new(), v),
            ir::FloatW::F16 => unreachable!("f16 goes through a helper"),
        }
    }

    /// Unary libm call; the width picks the f32/f64 symbol suffix, matching the
    /// interpreter's libm channel.
    pub(super) fn call_libm_un(&mut self, name: &str, a: Value, w: ir::FloatW) -> Value {
        let t = Self::float_ty(w);
        let fname = format!("{}{}", name, if t == types::F32 { "f" } else { "" });
        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(t));
        sig.returns.push(AbiParam::new(t));
        let fid = self
            .module
            .declare_function(&fname, Linkage::Import, &sig)
            .unwrap_or_else(|_| panic!("missing libm symbol: {fname}"));
        let fref = self.module.declare_func_in_func(fid, self.b.func);
        let call = self.b.ins().call(fref, &[a]);
        self.b.inst_results(call)[0]
    }

    pub(super) fn call_libm_bin(&mut self, name: &str, a: Value, b: Value, w: ir::FloatW) -> Value {
        let t = Self::float_ty(w);
        let fname = format!("{}{}", name, if t == types::F32 { "f" } else { "" });
        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(t));
        sig.params.push(AbiParam::new(t));
        sig.returns.push(AbiParam::new(t));
        let fid = self
            .module
            .declare_function(&fname, Linkage::Import, &sig)
            .unwrap_or_else(|_| panic!("missing libm symbol: {fname}"));
        let fref = self.module.declare_func_in_func(fid, self.b.func);
        let call = self.b.ins().call(fref, &[a, b]);
        self.b.inst_results(call)[0]
    }

    pub(super) fn call_powi(&mut self, a: Value, n_i32: Value, w: ir::FloatW) -> Value {
        let t = Self::float_ty(w);
        let fname = if t == types::F32 {
            "__powisf2"
        } else {
            "__powidf2"
        };
        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(t));
        sig.params.push(AbiParam::new(types::I32));
        sig.returns.push(AbiParam::new(t));
        let fid = self
            .module
            .declare_function(fname, Linkage::Import, &sig)
            .expect("missing compiler-builtins powi symbol");
        let fref = self.module.declare_func_in_func(fid, self.b.func);
        let call = self.b.ins().call(fref, &[a, n_i32]);
        self.b.inst_results(call)[0]
    }

    // ===== Wide-value channel: a 16-byte place as a (lo, hi) pair / I128 =====

    pub(super) fn read_wide(&mut self, pe: &ir::PlaceExpr) -> (Value, Value) {
        let a = self.place_addr(pe);
        let lo = self.b.ins().load(types::I64, MemFlagsData::trusted(), a, 0);
        let hi = self.b.ins().load(types::I64, MemFlagsData::trusted(), a, 8);
        (lo, hi)
    }

    pub(super) fn write_wide(&mut self, pe: &ir::PlaceExpr, lo: Value, hi: Value) {
        let a = self.place_addr(pe);
        self.b.ins().store(MemFlagsData::trusted(), lo, a, 0);
        self.b.ins().store(MemFlagsData::trusted(), hi, a, 8);
    }

    pub(super) fn i128_of(&mut self, lo: Value, hi: Value) -> Value {
        self.b.ins().iconcat(lo, hi)
    }

    pub(super) fn iconst128(&mut self, v: u128) -> Value {
        let lo = self.b.ins().iconst(types::I64, v as u64 as i64);
        let hi = self.b.ins().iconst(types::I64, (v >> 64) as u64 as i64);
        self.b.ins().iconcat(lo, hi)
    }
}
