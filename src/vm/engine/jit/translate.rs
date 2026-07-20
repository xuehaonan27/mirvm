//! Translator（自 jit_compile.rs J8+J10 整搬）：槽 SSA（I64 零扩到宽
//! 不变量）+ place 求值 + stmt/rvalue/term 三个大 match + 调用助手族 +
//! clif_rmw_op/collect_ssa_offs。语义 = 与 interp 逐位一致（恒等式镜像）。

use super::*;
use super::frame::FrameMap;
use super::admit::callee_abi;

pub(super) struct Translator<'a, 'b> {
    pub(super) shared: &'static Shared,
    pub(super) module: &'a mut JITModule,
    pub(super) b: &'a mut FunctionBuilder<'b>,
    pub(super) vars: std::collections::HashMap<u32, Variable>,
    /// 落帧 offset 集（analyze_frame 产出，区间模型）
    pub(super) frame_offs: FrameMap,
    /// guest 帧栈槽（frame_offs 非空时创建；frame_size 字节、frame_align 对齐）
    pub(super) frame_ss: Option<StackSlot>,
    pub(super) unreachable: ClifFuncId,
    pub(super) c2i: ClifFuncId,
    pub(super) memmove: ClifFuncId,
    pub(super) memset: ClifFuncId,
    pub(super) memcmp: ClifFuncId,
    pub(super) div_zero: ClifFuncId,
    pub(super) volatile_load: ClifFuncId,
    pub(super) volatile_store: ClifFuncId,
    pub(super) call_indirect: ClifFuncId,
    pub(super) tls_ref: ClifFuncId,
    pub(super) call_foreign: ClifFuncId,
    pub(super) call_builtin: ClifFuncId,
    pub(super) alloc: ClifFuncId,
    /// T1-c unwind 产品化：TerminateAbort 纯助手 / Terminate 边界直接调用 /
    /// _Unwind_Resume 导入 / try_call pad 的异常指针槽 / 本函数是否含 try_call
    /// （LSDA 注册判定用）
    pub(super) terminate_abort: ClifFuncId,
    pub(super) call_terminate: ClifFuncId,
    pub(super) unwind_resume: ClifFuncId,
    /// T1-d：Trap 占位助手（语句级/终止子同口）
    pub(super) trap: ClifFuncId,
    /// T1-d：SIMD/宽 stmt 与 SIMD rvalue 三件的统一助手（interp simd_exec 共享本体）
    pub(super) simd_stmt: ClifFuncId,
    pub(super) simd_rv: ClifFuncId,
    pub(super) exception_var: Option<Variable>,
    pub(super) has_try_call: bool,
}

impl Translator<'_, '_> {
    /// T1-c：try_call pad 的异常指针槽（TryCallExn(0) 落点；Resume 从本槽读）。
    fn exception_var(&mut self) -> Variable {
        if let Some(v) = self.exception_var {
            return v;
        }
        let v = self.b.declare_var(types::I64);
        self.exception_var = Some(v);
        v
    }

    fn var(&mut self, off: u32) -> Variable {
        if let Some(&v) = self.vars.get(&off) {
            return v;
        }
        let v = self.b.declare_var(types::I64);
        self.vars.insert(off, v);
        v
    }

    fn mask_val(&mut self, v: Value, w: Width) -> Value {
        if w == Width::W64 {
            return v;
        }
        self.b.ins().band_imm(v, w.mask() as i64)
    }

    /// 槽不变量下的符号扩展视图（i64）：w=64 原样；否则 ireduce→sextend。
    fn sext_val(&mut self, v: Value, w: Width) -> Value {
        let t = match w {
            Width::W8 => types::I8,
            Width::W16 => types::I16,
            Width::W32 => types::I32,
            Width::W64 => return v,
        };
        let narrow = self.b.ins().ireduce(t, v);
        self.b.ins().sextend(types::I64, narrow)
    }

    fn narrow_ty(w: Width) -> cranelift_codegen::ir::Type {
        match w {
            Width::W8 => types::I8,
            Width::W16 => types::I16,
            Width::W32 => types::I32,
            Width::W64 => types::I64,
        }
    }

    /// 读槽（分派：落帧 → 栈槽 load + 零扩；SSA → use_var）
    fn read_slot(&mut self, s: Slot) -> Value {
        if self.frame_offs.contains(s.off) {
            let ss = self.frame_ss.expect("落帧 offset 必有帧槽");
            let v = self
                .b
                .ins()
                .stack_load(Self::narrow_ty(s.width), ss, s.off as i32);
            if s.width == Width::W64 {
                v
            } else {
                self.b.ins().uextend(types::I64, v)
            }
        } else {
            let v = self.var(s.off);
            self.b.use_var(v)
        }
    }

    /// 写槽（分派：落帧 → 掩宽 + 窄化 + 栈槽 store；SSA → 掩宽 def_var）
    fn write_slot(&mut self, s: Slot, v: Value) {
        let masked = self.mask_val(v, s.width);
        if self.frame_offs.contains(s.off) {
            let ss = self.frame_ss.expect("落帧 offset 必有帧槽");
            let n = if s.width == Width::W64 {
                masked
            } else {
                self.b.ins().ireduce(Self::narrow_ty(s.width), masked)
            };
            self.b.ins().stack_store(n, ss, s.off as i32);
        } else {
            let var = self.var(s.off);
            self.b.def_var(var, masked);
        }
    }

    /// 取帧内 offset 的真地址（取址分析已保证其落帧）
    fn addr_of_local(&mut self, off: u32) -> Value {
        let ss = self
            .frame_ss
            .expect("取址 offset 必落帧（analyze_frame 全集）");
        self.b.ins().stack_addr(types::I64, ss, off as i32)
    }

    /// PlaceExpr 求值（interp::eval_place_addr 逐位镜像；Deref/Offset 为 wrapping 语义）
    fn place_addr(&mut self, pe: &ir::PlaceExpr) -> Value {
        let mut addr = match pe.base {
            ir::PlaceBase::Local(off) => self.addr_of_local(off),
            ir::PlaceBase::Static(a) => self.b.ins().iconst(types::I64, a as i64),
        };
        for step in pe.steps.iter() {
            match step {
                ir::PlaceStep::Deref => {
                    addr = self
                        .b
                        .ins()
                        .load(types::I64, MemFlagsData::trusted(), addr, 0)
                }
                ir::PlaceStep::Offset(d) => addr = self.b.ins().iadd_imm(addr, i64::from(*d)),
                ir::PlaceStep::IndexScaled { idx, stride } => {
                    let i = self.read_slot(*idx);
                    let scaled = self.b.ins().imul_imm(i, *stride as i64);
                    addr = self.b.ins().iadd(addr, scaled);
                }
                ir::PlaceStep::VTableAlignOffset {
                    meta,
                    unaligned,
                    packed,
                } => {
                    // interp 恒等式：align = *(vtable+16)；packed 取 min；非 2 幂/溢出即
                    // abort（JIT 侧 = mirvm_jit_trap 诊断退出，与 interp engine_abort 同口径）
                    let (vtable, _) = self.operand(meta);
                    let mut align =
                        self.b
                            .ins()
                            .load(types::I64, MemFlagsData::trusted(), vtable, 16);
                    if let Some(p) = packed {
                        let p = self.b.ins().iconst(types::I64, *p as i64);
                        align = self.b.ins().umin(align, p);
                    }
                    // 2 幂检查：align != 0 && (align & (align-1)) == 0，否则 trap
                    let is_zero = self.b.ins().icmp_imm(IntCC::Equal, align, 0);
                    let am1 = self.b.ins().iadd_imm(align, -1);
                    let pow2 = self.b.ins().band(align, am1);
                    let not_pow2 = self.b.ins().icmp_imm(IntCC::NotEqual, pow2, 0);
                    let bad = self.b.ins().bor(is_zero, not_pow2);
                    self.trap_if(bad, "dyn vtable alignment 非 2 的幂");
                    // (unaligned + align-1) & !(align-1)；checked_add 溢出 → trap
                    let uv = self.b.ins().iconst(types::I64, *unaligned as i64);
                    let sum = self
                        .b
                        .ins()
                        .uadd_overflow_trap(uv, am1, TrapCode::user(2).unwrap());
                    let off = self.b.ins().band_not(sum, am1);
                    addr = self.b.ins().iadd(addr, off);
                }
            }
        }
        addr
    }

    /// 条件即诊断退出（与 interp engine_abort 同口径的 JIT 形态）。
    fn trap_if(&mut self, cond: Value, _msg: &'static str) {
        let t_blk = self.b.create_block();
        let f_blk = self.b.create_block();
        self.b.ins().brif(cond, t_blk, &[], f_blk, &[]);
        self.b.switch_to_block(t_blk);
        let fref = self
            .module
            .declare_func_in_func(self.unreachable, self.b.func);
        let fv = self.b.ins().iconst(types::I64, 0);
        self.b.ins().call(fref, &[fv]);
        self.b.ins().trap(TrapCode::user(1).unwrap());
        self.b.switch_to_block(f_blk);
    }

    /// M5.4b-1 除零分支：cond 真 → 调 mirvm_jit_div_zero（interp 同文案同码退出）。
    /// wide=false 64 位（kind 0/1），wide=true 128 位（kind 2/3）。
    fn div_zero_if(&mut self, cond: Value, is_rem: bool, wide: bool) {
        let t_blk = self.b.create_block();
        let f_blk = self.b.create_block();
        self.b.ins().brif(cond, t_blk, &[], f_blk, &[]);
        self.b.switch_to_block(t_blk);
        let fref = self.module.declare_func_in_func(self.div_zero, self.b.func);
        let kind = match (wide, is_rem) {
            (false, false) => 0,
            (false, true) => 1,
            (true, false) => 2,
            (true, true) => 3,
        };
        let kv = self.b.ins().iconst(types::I64, kind);
        self.b.ins().call(fref, &[kv]);
        self.b.ins().trap(TrapCode::user(1).unwrap());
        self.b.switch_to_block(f_blk);
    }

    /// 标量落点写（Slot → write_slot；Mem → 掩宽窄化 store）。
    fn write_scalar_place(&mut self, sp: &ScalarPlace, v: Value) {
        match sp {
            ScalarPlace::Slot(s) => {
                let s = *s;
                self.write_slot(s, v);
            }
            ScalarPlace::Mem { expr, width } => {
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

    // ===== M5.4b-2 浮点通道（值 = I64 槽里的位型，与 interp 同一表示）=====

    fn float_ty(w: ir::FloatW) -> cranelift_codegen::ir::Type {
        match w {
            ir::FloatW::F32 => types::F32,
            ir::FloatW::F64 => types::F64,
            ir::FloatW::F16 => unreachable!("f16 走助手（M5.4b-3）"),
        }
    }

    /// 槽位型 → 浮点寄存器值（bitcast；F32 先 ireduce）
    fn as_float(&mut self, v: Value, w: ir::FloatW) -> Value {
        match w {
            ir::FloatW::F32 => {
                let n = self.b.ins().ireduce(types::I32, v);
                self.b.ins().bitcast(types::F32, MemFlagsData::trusted(), n)
            }
            ir::FloatW::F64 => self.b.ins().bitcast(types::F64, MemFlagsData::trusted(), v),
            ir::FloatW::F16 => unreachable!("f16 走助手（M5.4b-3）"),
        }
    }

    /// 浮点寄存器值 → 槽位型（bitcast 回来；F32 再 uextend）
    fn as_bits(&mut self, v: Value, w: ir::FloatW) -> Value {
        match w {
            ir::FloatW::F32 => {
                let n = self.b.ins().bitcast(types::I32, MemFlagsData::trusted(), v);
                self.b.ins().uextend(types::I64, n)
            }
            ir::FloatW::F64 => self.b.ins().bitcast(types::I64, MemFlagsData::trusted(), v),
            ir::FloatW::F16 => unreachable!("f16 走助手（M5.4b-3）"),
        }
    }

    /// 一元/二元 libm 调用（按宽选 f32/f64 后缀符号；interp 的 libm 通道同源）
    fn call_libm_un(&mut self, name: &str, a: Value, w: ir::FloatW) -> Value {
        let t = Self::float_ty(w);
        let fname = format!("{}{}", name, if t == types::F32 { "f" } else { "" });
        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(t));
        sig.returns.push(AbiParam::new(t));
        let fid = self
            .module
            .declare_function(&fname, Linkage::Import, &sig)
            .unwrap_or_else(|_| panic!("libm 符号缺失: {fname}"));
        let fref = self.module.declare_func_in_func(fid, self.b.func);
        let call = self.b.ins().call(fref, &[a]);
        self.b.inst_results(call)[0]
    }

    fn call_libm_bin(&mut self, name: &str, a: Value, b: Value, w: ir::FloatW) -> Value {
        let t = Self::float_ty(w);
        let fname = format!("{}{}", name, if t == types::F32 { "f" } else { "" });
        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(t));
        sig.params.push(AbiParam::new(t));
        sig.returns.push(AbiParam::new(t));
        let fid = self
            .module
            .declare_function(&fname, Linkage::Import, &sig)
            .unwrap_or_else(|_| panic!("libm 符号缺失: {fname}"));
        let fref = self.module.declare_func_in_func(fid, self.b.func);
        let call = self.b.ins().call(fref, &[a, b]);
        self.b.inst_results(call)[0]
    }

    fn call_powi(&mut self, a: Value, n_i32: Value, w: ir::FloatW) -> Value {
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
            .expect("compiler-builtins powi 符号缺失");
        let fref = self.module.declare_func_in_func(fid, self.b.func);
        let call = self.b.ins().call(fref, &[a, n_i32]);
        self.b.inst_results(call)[0]
    }

    // ===== M5.4b-3 宽值通道（16 字节 place ↔ (lo,hi) 对/I128）=====

    fn read_wide(&mut self, pe: &ir::PlaceExpr) -> (Value, Value) {
        let a = self.place_addr(pe);
        let lo = self.b.ins().load(types::I64, MemFlagsData::trusted(), a, 0);
        let hi = self.b.ins().load(types::I64, MemFlagsData::trusted(), a, 8);
        (lo, hi)
    }

    fn write_wide(&mut self, pe: &ir::PlaceExpr, lo: Value, hi: Value) {
        let a = self.place_addr(pe);
        self.b.ins().store(MemFlagsData::trusted(), lo, a, 0);
        self.b.ins().store(MemFlagsData::trusted(), hi, a, 8);
    }

    fn i128_of(&mut self, lo: Value, hi: Value) -> Value {
        self.b.ins().iconcat(lo, hi)
    }

    fn iconst128(&mut self, v: u128) -> Value {
        let lo = self.b.ins().iconst(types::I64, v as u64 as i64);
        let hi = self.b.ins().iconst(types::I64, (v >> 64) as u64 as i64);
        self.b.ins().iconcat(lo, hi)
    }

    /// 16 字节 out 型助手调用：栈槽接 (lo,hi) 结果并写回 place。
    fn call_out128(&mut self, name: &str, args: &[Value], dst: &ir::PlaceExpr) {
        let ss =
            self.b
                .create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 16, 4));
        let outp = self.b.ins().stack_addr(types::I64, ss, 0);
        let mut a: Vec<Value> = args.to_vec();
        a.push(outp);
        let mut sig = self.module.make_signature();
        for _ in &a {
            sig.params.push(AbiParam::new(types::I64));
        }
        let fid = self
            .module
            .declare_function(name, Linkage::Import, &sig)
            .unwrap_or_else(|_| panic!("助手符号缺失: {name}"));
        let fref = self.module.declare_func_in_func(fid, self.b.func);
        self.b.ins().call(fref, &a);
        let lo = self.b.ins().stack_load(types::I64, ss, 0);
        let hi = self.b.ins().stack_load(types::I64, ss, 8);
        self.write_wide(dst, lo, hi);
    }

    /// 单返回 u64 的助手调用。
    fn call_helper1(&mut self, name: &str, args: &[Value]) -> Value {
        let mut sig = self.module.make_signature();
        for _ in args {
            sig.params.push(AbiParam::new(types::I64));
        }
        sig.returns.push(AbiParam::new(types::I64));
        let fid = self
            .module
            .declare_function(name, Linkage::Import, &sig)
            .unwrap_or_else(|_| panic!("助手符号缺失: {name}"));
        let fref = self.module.declare_func_in_func(fid, self.b.func);
        let call = self.b.ins().call(fref, args);
        self.b.inst_results(call)[0]
    }

    fn operand(&mut self, op: &Operand) -> (Value, Width) {
        match op {
            Operand::Slot(s) => (self.read_slot(*s), s.width),
            Operand::Imm { bits, width } => {
                let v = self
                    .b
                    .ins()
                    .iconst(types::I64, (*bits & width.mask()) as i64);
                (v, *width)
            }
            Operand::Mem { expr, width } => {
                let a = self.place_addr(expr);
                let v = self
                    .b
                    .ins()
                    .load(Self::narrow_ty(*width), MemFlagsData::trusted(), a, 0);
                let v = if *width == Width::W64 {
                    v
                } else {
                    self.b.ins().uextend(types::I64, v)
                };
                (v, *width)
            }
            Operand::AddrOf(expr) => {
                let a = self.place_addr(expr);
                (a, Width::W64)
            }
            Operand::SubImm { base, sub } => {
                let (v, w) = self.operand(base);
                (self.b.ins().iadd_imm(v, (*sub as i64).wrapping_neg()), w)
            }
        }
    }

    fn def_slot(&mut self, s: Slot, v: Value) {
        self.write_slot(s, v);
    }

    pub(super) fn build(&mut self, func: u32, body: &ir::FuncBody) {
        let entry = self.b.create_block();
        self.b.append_block_params_for_function_params(entry);
        let blocks: Vec<_> = (0..body.blocks.len())
            .map(|_| self.b.create_block())
            .collect();

        self.b.switch_to_block(entry);
        // 槽变量 def 0（确定化；interp 帧不清零，但有效 MIR 无读前未写路径）。
        // 帧内存同步清零（v1 确定化纪律的延伸：JIT-on/off 差分对任何 MIR 形状确定）。
        let mut offs: Vec<u32> = Vec::new();
        collect_ssa_offs(body, &self.frame_offs, &mut offs);
        let zero = self.b.ins().iconst(types::I64, 0);
        for off in offs {
            let var = self.var(off);
            self.b.def_var(var, zero);
        }
        if let Some(ss) = self.frame_ss {
            let fref = self.module.declare_func_in_func(self.memset, self.b.func);
            let dst = self.b.ins().stack_addr(types::I64, ss, 0);
            let c0 = self.b.ins().iconst(types::I64, 0);
            let n = self.b.ins().iconst(types::I64, i64::from(body.frame_size));
            self.b.ins().call(fref, &[dst, c0, n]);
        }
        // T1-c：Resume 是 cleanup 链尾，经链内正常边落入（f156 实证：bb 序可
        // 先于其 pad）——exception_var 在入口预声明并 def 0 兜底，pad 的 def
        // 经支配关系覆盖真用点（cranelift 变量要求 use 时可解析到 def）。
        if body
            .blocks
            .iter()
            .any(|bl| matches!(bl.term, ir::Terminator::Resume))
        {
            let ev = self.exception_var();
            self.b.def_var(ev, zero);
        }
        // 参数落槽（T1-a：interp ABI v2 展平序全形态——sret 前插 / Scalar /
        // Pair / Indirect(memmove) / track_caller 幻影尾参，packed/interp 同序）
        let params = self.b.block_params(entry).to_vec();
        let mut pi = 0usize;
        if let RetAbi::Indirect { sret_off, .. } = body.ret {
            self.def_slot(
                Slot {
                    off: sret_off,
                    width: Width::W64,
                },
                params[pi],
            );
            pi += 1;
        }
        for p in &body.params {
            match p {
                ParamAbi::Zst => {}
                ParamAbi::Scalar(s) => {
                    self.def_slot(*s, params[pi]);
                    pi += 1;
                }
                ParamAbi::Pair(lo, hi) => {
                    self.def_slot(*lo, params[pi]);
                    self.def_slot(*hi, params[pi + 1]);
                    pi += 2;
                }
                ParamAbi::Indirect { off, size } => {
                    // 帧内 off 必落帧（analyze_frame 的 ABI 展平面已强制）
                    let dst = self.addr_of_local(*off);
                    let fref = self.module.declare_func_in_func(self.memmove, self.b.func);
                    let n = self.b.ins().iconst(types::I64, i64::from(*size));
                    self.b.ins().call(fref, &[dst, params[pi], n]);
                    pi += 1;
                }
            }
        }
        if let Some(off) = body.caller_loc_off {
            self.def_slot(
                Slot {
                    off,
                    width: Width::W64,
                },
                params[pi],
            );
        }
        self.b.ins().jump(blocks[0], &[]);

        for (bi, blk) in body.blocks.iter().enumerate() {
            self.b.switch_to_block(blocks[bi]);
            for st in &blk.stmts {
                self.stmt(st);
            }
            self.term(func, body, &blk.term, &blocks, bi);
        }
    }

    fn stmt(&mut self, st: &Stmt) {
        match st {
            Stmt::Assign { dst, rv } => {
                let v = self.rvalue(rv);
                match dst {
                    ScalarPlace::Slot(s) => {
                        let s = *s;
                        self.def_slot(s, v);
                    }
                    ScalarPlace::Mem { expr, width } => {
                        // 内存落点：掩宽 + 窄化 + store（与 interp mem_write 同口径）
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
                    _ => unreachable!("admit 已排除"),
                };
                self.def_slot(sv, val);
                self.def_slot(sf, flag);
            }
            Stmt::Copy { dst, src, size } => {
                // memmove 语义（interp std::ptr::copy 同源：guest 侧重叠是 UB，
                // 引擎不因此崩——防御性一致）
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
                // interp 循环镜像：for i in 0..count { mem_write(d + i*elem, w, v) }
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
                // interp 镜像：for i in 1..count { 逐元素 memmove（元素 0 不变 ⇒
                // 与 interp 的 copy_nonoverlapping 逐元素结果一致） }
                let src = self.place_addr(first);
                self.repeat_loop(src, src, *count, *elem_size, true);
            }
            // ===== M5.4b-1 内存/原子补面 =====
            Stmt::MemCopy {
                dst,
                src,
                count,
                elem_size,
                overlap,
            } => {
                // intrinsic copy/copy_nonoverlapping：memmove 通道（overlap 为真时
                // 与 interp 的 ptr::copy 同义；非重叠场景 memcpy 结果相同）
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
                let _ = order; // CLIF 原子恒 SeqCst（合规强化，见 R::AtomicLoad 注）
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
                // CLIF atomic_cas = strong CAS（weak 用 strong 合规：weak 允许假失败
                // 但不禁止成功）；succ/fail 序 → SeqCst（合规强化）
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
                // ok = (old == expected)（按宽掩后比较，与 interp 的 compare_exchange 同口径）
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
                // single_thread = compiler fence（无指令，编译屏障在 JIT 码内天然成立）
            }
            // ===== M5.4b-3 128 位整族 =====
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
                // with_overflow 的 Add/Sub/Mul：helper（Rust overflowing_* 精确语义）
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
                    // 旗标写 dst+16（interp 同布局：(u128, bool) 旗标在 +16）
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
                        let y = self.i128_of(blo, bhi);
                        let is_rem = matches!(op, IntBinOp::Rem);
                        let zero = self.b.ins().icmp_imm(IntCC::Equal, y, 0);
                        self.div_zero_if(zero, is_rem, true);
                        if *signed {
                            let neg1 = self.b.ins().icmp_imm(IntCC::Equal, y, -1);
                            let triv_blk = self.b.create_block();
                            let norm_blk = self.b.create_block();
                            let join_blk = self.b.create_block();
                            self.b.ins().brif(neg1, triv_blk, &[], norm_blk, &[]);
                            self.b.switch_to_block(triv_blk);
                            let tv = if is_rem {
                                self.b.ins().iconst(types::I128, 0)
                            } else {
                                // wrapping_div(x, -1) = -x（MIN 回绕，ineg 同形）
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
                            self.b.append_block_param(join_blk, types::I128)
                        } else if is_rem {
                            self.b.ins().urem(x, y)
                        } else {
                            self.b.ins().udiv(x, y)
                        }
                    }
                };
                let (lo, hi) = {
                    let pair = self.b.ins().isplit(r);
                    (pair.0, pair.1)
                };
                self.write_wide(dst, lo, hi);
            }
            Stmt::Bit128 { op, src, dst } => {
                use crate::vm::engine::ir::BitUnOp as B;
                let (lo, hi) = self.read_wide(src);
                let (rlo, rhi) = match op {
                    B::Bswap => {
                        // u128::swap_bytes = 半字互换 + 各自 bswap
                        let a = self.b.ins().bswap(hi);
                        let b = self.b.ins().bswap(lo);
                        (a, b)
                    }
                    B::Bitreverse => {
                        // u128::reverse_bits = 半字互换 + 各自 bitrev
                        let a = self.b.ins().bitrev(hi);
                        let b = self.b.ins().bitrev(lo);
                        (a, b)
                    }
                    _ => unreachable!("Bit128 只 bswap/bitreverse"),
                };
                self.write_wide(dst, rlo, rhi);
            }
            Stmt::Bit128Count { op, src, dst } => {
                use crate::vm::engine::ir::BitUnOp as B;
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
                    _ => unreachable!("Bit128Count 只 popcount/ctlz/cttz"),
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
                // interp 恒等式：rel = tag - niche_start（u128 wrapping）；rel < len →
                // variants_start + rel，否则 untagged
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
                // i128/u128 → f16/f32/f64：f32/f64 走 compiler-builtins float*ti* 族
                // （Rust i128 as f32/f64 的同一批符号）；f16 走 mirvm_wide_to_f16
                let (lo, hi) = self.read_wide(src);
                let s = self.b.ins().iconst(types::I64, *signed as i64);
                let f = match to {
                    ir::FloatW::F16 => {
                        let bits = self.call_helper1("mirvm_wide_to_f16", &[lo, hi, s]);
                        self.mask_val(bits, Width::W16)
                    }
                    ir::FloatW::F32 => {
                        let fname = if *signed {
                            "__floattisf"
                        } else {
                            "__floatuntisf"
                        };
                        let r = self.call_helper1(fname, &[lo, hi]);
                        let n = self.b.ins().ireduce(types::I32, r);
                        let f32v = self.b.ins().bitcast(types::F32, MemFlagsData::trusted(), n);
                        self.as_bits(f32v, ir::FloatW::F32)
                    }
                    ir::FloatW::F64 => {
                        let fname = if *signed {
                            "__floattidf"
                        } else {
                            "__floatuntidf"
                        };
                        let r = self.call_helper1(fname, &[lo, hi]);
                        self.as_bits(r, ir::FloatW::F64)
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
                self.call_out128("mirvm_float_to_wide", &[bits, kind, s], dst);
            }
            // ===== M5.4b-3 f128 宽通道（全走助手）=====
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
            // ===== T1-d SIMD 15 件 + Sat128（mirvm_simd_stmt 助手，interp
            // simd_exec 共享本体；参数序 (stmt, a, b, c, dst, v0, v1)，缺位补 0）=====
            Stmt::SimdBin { dst, a, b, .. } => {
                let pd = self.place_addr(dst);
                let pa = self.place_addr(a);
                let pb = self.place_addr(b);
                let sp = self.b.ins().iconst(types::I64, st as *const ir::Stmt as i64);
                let z = self.b.ins().iconst(types::I64, 0);
                let fref = self.module.declare_func_in_func(self.simd_stmt, self.b.func);
                self.b.ins().call(fref, &[sp, pa, pb, z, pd, z, z]);
            }
            Stmt::SimdUn { dst, a, .. } => {
                let pd = self.place_addr(dst);
                let pa = self.place_addr(a);
                let sp = self.b.ins().iconst(types::I64, st as *const ir::Stmt as i64);
                let z = self.b.ins().iconst(types::I64, 0);
                let fref = self.module.declare_func_in_func(self.simd_stmt, self.b.func);
                self.b.ins().call(fref, &[sp, pa, z, z, pd, z, z]);
            }
            Stmt::SimdFma { dst, a, b, c, .. } => {
                let pd = self.place_addr(dst);
                let pa = self.place_addr(a);
                let pb = self.place_addr(b);
                let pc = self.place_addr(c);
                let sp = self.b.ins().iconst(types::I64, st as *const ir::Stmt as i64);
                let z = self.b.ins().iconst(types::I64, 0);
                let fref = self.module.declare_func_in_func(self.simd_stmt, self.b.func);
                self.b.ins().call(fref, &[sp, pa, pb, pc, pd, z, z]);
            }
            Stmt::SimdFunnel {
                dst, a, b, shift, ..
            } => {
                let pd = self.place_addr(dst);
                let pa = self.place_addr(a);
                let pb = self.place_addr(b);
                let ps = self.place_addr(shift);
                let sp = self.b.ins().iconst(types::I64, st as *const ir::Stmt as i64);
                let z = self.b.ins().iconst(types::I64, 0);
                let fref = self.module.declare_func_in_func(self.simd_stmt, self.b.func);
                self.b.ins().call(fref, &[sp, pa, pb, ps, pd, z, z]);
            }
            Stmt::SimdCast { dst, src, .. } => {
                let pd = self.place_addr(dst);
                let ps = self.place_addr(src);
                let sp = self.b.ins().iconst(types::I64, st as *const ir::Stmt as i64);
                let z = self.b.ins().iconst(types::I64, 0);
                let fref = self.module.declare_func_in_func(self.simd_stmt, self.b.func);
                self.b.ins().call(fref, &[sp, ps, z, z, pd, z, z]);
            }
            Stmt::SimdSelect {
                mask, a, b, dst, ..
            } => {
                let pm = self.place_addr(mask);
                let pa = self.place_addr(a);
                let pb = self.place_addr(b);
                let pd = self.place_addr(dst);
                let sp = self.b.ins().iconst(types::I64, st as *const ir::Stmt as i64);
                let z = self.b.ins().iconst(types::I64, 0);
                let fref = self.module.declare_func_in_func(self.simd_stmt, self.b.func);
                self.b.ins().call(fref, &[sp, pm, pa, pb, pd, z, z]);
            }
            Stmt::SimdSelectBitmask {
                mask, a, b, dst, ..
            } => {
                let (m, _) = self.operand(mask);
                let pa = self.place_addr(a);
                let pb = self.place_addr(b);
                let pd = self.place_addr(dst);
                let sp = self.b.ins().iconst(types::I64, st as *const ir::Stmt as i64);
                let z = self.b.ins().iconst(types::I64, 0);
                let fref = self.module.declare_func_in_func(self.simd_stmt, self.b.func);
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
                let sp = self.b.ins().iconst(types::I64, st as *const ir::Stmt as i64);
                let z = self.b.ins().iconst(types::I64, 0);
                let fref = self.module.declare_func_in_func(self.simd_stmt, self.b.func);
                self.b.ins().call(fref, &[sp, pv, pp, pm, pd, z, z]);
            }
            Stmt::SimdScatter {
                values,
                ptrs,
                mask,
                ..
            } => {
                let pv = self.place_addr(values);
                let pp = self.place_addr(ptrs);
                let pm = self.place_addr(mask);
                let sp = self.b.ins().iconst(types::I64, st as *const ir::Stmt as i64);
                let z = self.b.ins().iconst(types::I64, 0);
                let fref = self.module.declare_func_in_func(self.simd_stmt, self.b.func);
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
                let sp = self.b.ins().iconst(types::I64, st as *const ir::Stmt as i64);
                let z = self.b.ins().iconst(types::I64, 0);
                let fref = self.module.declare_func_in_func(self.simd_stmt, self.b.func);
                self.b.ins().call(fref, &[sp, pm, pv, z, pd, pbase, z]);
            }
            Stmt::SimdMaskedStore {
                mask,
                base,
                values,
                ..
            } => {
                let pm = self.place_addr(mask);
                let (pbase, _) = self.operand(base);
                let pv = self.place_addr(values);
                let sp = self.b.ins().iconst(types::I64, st as *const ir::Stmt as i64);
                let z = self.b.ins().iconst(types::I64, 0);
                let fref = self.module.declare_func_in_func(self.simd_stmt, self.b.func);
                self.b.ins().call(fref, &[sp, pm, pv, z, z, pbase, z]);
            }
            Stmt::SimdExtractDyn {
                src, idx, dst, ..
            } => {
                let ps = self.place_addr(src);
                let (i, _) = self.operand(idx);
                let sp = self.b.ins().iconst(types::I64, st as *const ir::Stmt as i64);
                let z = self.b.ins().iconst(types::I64, 0);
                let fref = self.module.declare_func_in_func(self.simd_stmt, self.b.func);
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
                let sp = self.b.ins().iconst(types::I64, st as *const ir::Stmt as i64);
                let z = self.b.ins().iconst(types::I64, 0);
                let fref = self.module.declare_func_in_func(self.simd_stmt, self.b.func);
                self.b.ins().call(fref, &[sp, ps, z, z, pd, i, v]);
            }
            Stmt::SimdArithOffset {
                ptrs,
                offsets,
                dst,
                ..
            } => {
                let pp = self.place_addr(ptrs);
                let po = self.place_addr(offsets);
                let pd = self.place_addr(dst);
                let sp = self.b.ins().iconst(types::I64, st as *const ir::Stmt as i64);
                let z = self.b.ins().iconst(types::I64, 0);
                let fref = self.module.declare_func_in_func(self.simd_stmt, self.b.func);
                self.b.ins().call(fref, &[sp, pp, po, z, pd, z, z]);
            }
            Stmt::SimdSplat { dst, val, .. } => {
                let pd = self.place_addr(dst);
                let (v, _) = self.operand(val);
                let sp = self.b.ins().iconst(types::I64, st as *const ir::Stmt as i64);
                let z = self.b.ins().iconst(types::I64, 0);
                let fref = self.module.declare_func_in_func(self.simd_stmt, self.b.func);
                self.b.ins().call(fref, &[sp, z, z, z, pd, v, z]);
            }
            Stmt::Sat128 { a, b, dst, .. } => {
                let pa = self.place_addr(a);
                let pb = self.place_addr(b);
                let pd = self.place_addr(dst);
                let sp = self.b.ins().iconst(types::I64, st as *const ir::Stmt as i64);
                let z = self.b.ins().iconst(types::I64, 0);
                let fref = self.module.declare_func_in_func(self.simd_stmt, self.b.func);
                self.b.ins().call(fref, &[sp, pa, pb, z, pd, z, z]);
            }
            // T1-d：Trap/Nop（语句级 Trap = mirvm_jit_trap stmt 形，interp
            // engine_abort 同文案同 exit(70)；call 后补 trap 保底——助手不返回）
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

    /// Repeat 两族的共用循环骨架：memmove_elem=true 时逐元素 memmove（RepeatBytes，
    /// 起始 i=1）；否则按标量存（RepeatScalar，起始 i=0）。
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
        use crate::vm::engine::ir::Rvalue as R;
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
                    (av, bv) // 槽不变量已 zext
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
                // interp rvalue IntCmp3 镜像：三路比较 → Ordering i8 位型
                //（-1 = 0xFF；dst W8 截断同值）
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
                // interp rvalue NicheDiscr 镜像：rel = (tag - niche_start) 按 tag
                // 宽 wrapping；rel < len → variants_start+rel，否则 untagged
                let (tv, w) = self.operand(tag);
                let ns = self.b.ins().iconst(types::I64, *niche_start as i64);
                let rel = self.b.ins().isub(tv, ns);
                let rel = self.mask_val(rel, w);
                let hit = self.b.ins().icmp_imm(
                    IntCC::UnsignedLessThan,
                    rel,
                    *variants_len as i64,
                );
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
                    // sext 后按 64 位视图，再掩到目标宽
                    s
                } else {
                    self.mask_val(v, from.0)
                };
                self.mask_val(x, *to)
            }
            // ===== M5.4a 内存/地址族 =====
            R::Ref(expr) => self.place_addr(expr),
            R::PtrOffset { ptr, count, stride } => {
                // 真实地址模型位透传（wrapping；与 interp 同）
                let (p, _) = self.operand(ptr);
                let (c, _) = self.operand(count);
                let scaled = self.b.ins().imul_imm(c, *stride as i64);
                self.b.ins().iadd(p, scaled)
            }
            R::PtrDiff { a, b, stride } => {
                // (a - b) / stride（i64 除法；stride 为冻结常量，admit 已拒 0）
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
            // ===== M5.4b-1 标量补面 =====
            R::IntSat { op, signed, a, b } => {
                // interp int_saturating 镜像：int_ovf 判方向后取 clamp
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
                use crate::vm::engine::ir::BitUnOp as B;
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
                            // interp：W8 恒等（v & 0xff）
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
                // 宿主 memcmp import（i32 结果符号扩展；interp 同通道）
                let (pa, _) = self.operand(a);
                let (pb, _) = self.operand(b);
                let (nv, _) = self.operand(n);
                let fref = self.module.declare_func_in_func(self.memcmp, self.b.func);
                let call = self.b.ins().call(fref, &[pa, pb, nv]);
                let r32 = self.b.inst_results(call)[0];
                self.b.ins().sextend(types::I64, r32)
            }
            R::AtomicLoad { addr, width, order } => {
                // CLIF 原子 = SeqCst（0.133 无弱序；合规强化——D8j 弱序恢复目前只在
                // interp，JIT 侧统一最强序，RAM non-det 包络内，记账 m5-log）
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
            // ===== M5.4b-2 浮点 f32/f64 + Math 系 =====
            R::FloatBin { op, fw, a, b } => {
                let (av, _) = self.operand(a);
                let (bv, _) = self.operand(b);
                if matches!(fw, ir::FloatW::F16) {
                    // f16 走助手（interp 宿主直算通道；op 码表同 f128_bin）
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
                        // IEEE fmod（Rust % 浮点语义）：libm fmod 通道（interp 同源）
                        ir::FloatOp::Rem => self.call_libm_bin("fmod", fa, fb, *fw),
                    };
                    self.as_bits(r, *fw)
                }
            }
            R::FloatCmp { cc, fw, a, b } => {
                // IEEE 偏序语义（NaN 全 false 除 Ne）：CLIF ordered 族 + Ne=NotEqual
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
                    // f16 参与的互转走助手（kind: 1=f16→f32 2=f16→f64 3=f32→f16 4=f64→f16）
                    let k = self.b.ins().iconst(
                        types::I64,
                        match (from, to) {
                            (ir::FloatW::F16, ir::FloatW::F32) => 1,
                            (ir::FloatW::F16, ir::FloatW::F64) => 2,
                            (ir::FloatW::F32, ir::FloatW::F16) => 3,
                            (ir::FloatW::F64, ir::FloatW::F16) => 4,
                            _ => unreachable!("f16 互转组合外无此类"),
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
                        _ => unreachable!("f16 互转走助手"),
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
                // Rust `as` 饱和语义（NaN→0、越界→边界）：
                // signed W32/64 = fcvt_to_sint_sat 直达；signed W8/16 = I32 饱和后
                // 再按目标域钳；unsigned = fcvt_to_uint_sat(I64) 后按 mask 钳（u32
                // 域 ⊂ u64，须先钳到 u32::MAX 再掩，Rust 语义）
                let (av, _) = self.operand(a);
                if matches!(from, ir::FloatW::F16) {
                    // f16 → int：助手（kind: 0=i8 1=u8 2=i16 3=u16 4=i32 5=u32 6=i64 7=u64）
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
                                // W8/16：I32 饱和值再钳到 [iN::MIN, iN::MAX]
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
                    // int → f16：助手（kind 同 to_int 码表）
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
                use crate::vm::engine::ir::MathUnOp as M;
                let (av, _) = self.operand(a);
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
                    // round_ties_even = C99 rint（与 interp/Rust 同源）
                    M::RoundTiesEven => self.call_libm_un("rint", fa, *fw),
                };
                self.as_bits(r, *fw)
            }
            R::MathBin { op, fw, a, b } => {
                use crate::vm::engine::ir::MathBinOp as M;
                let (av, _) = self.operand(a);
                let (bv, _) = self.operand(b);
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
            R::MathFma { fw, a, b, c } => {
                // fma 单次舍入（宿主 mul_add 同源；fmuladd 允许融合/不融合两结果，
                // 融合恒在允许集合内——与 interp 取融合同侧）
                let (av, _) = self.operand(a);
                let (bv, _) = self.operand(b);
                let (cv, _) = self.operand(c);
                let fa = self.as_float(av, *fw);
                let fb = self.as_float(bv, *fw);
                let fc = self.as_float(cv, *fw);
                let r = self.b.ins().fma(fa, fb, fc);
                self.as_bits(r, *fw)
            }
            // ===== M5.4b-3 f128 比较（Rvalue 侧的宽通道）=====
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
            // ===== M5.4b-3 128 位整数比较（Rvalue 侧）=====
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
                // T1-b：mirvm_tls_ref 助手（interp::tls_addr 同本体——每线程
                // 实例块惰性物化）
                let fref = self.module.declare_func_in_func(self.tls_ref, self.b.func);
                let i = self.b.ins().iconst(types::I64, i64::from(*id));
                let call = self.b.ins().call(fref, &[i]);
                self.b.inst_results(call)[0]
            }
            // ===== T1-d SIMD rvalue 三件（mirvm_simd_rv 助手，interp simd_exec
            // 共享本体；rv 真地址 + 向量 place 地址两参）=====
            R::SimdBitmask { a, .. } => {
                // lanes 位掩码（≤64 位 u64 无需 mask——interp 本体同口径）
                let pa = self.place_addr(a);
                let rp = self.b.ins().iconst(types::I64, rv as *const ir::Rvalue as i64);
                let fref = self.module.declare_func_in_func(self.simd_rv, self.b.func);
                let call = self.b.ins().call(fref, &[rp, pa]);
                self.b.inst_results(call)[0]
            }
            R::SimdReduce { a, .. } => {
                // bool 0/1（interp 本体 acc as u64 同口径）
                let pa = self.place_addr(a);
                let rp = self.b.ins().iconst(types::I64, rv as *const ir::Rvalue as i64);
                let fref = self.module.declare_func_in_func(self.simd_rv, self.b.func);
                let call = self.b.ins().call(fref, &[rp, pa]);
                self.b.inst_results(call)[0]
            }
            R::SimdReduceArith {
                a, lane_bytes, ..
            } => {
                // lane 宽标量位型（interp 本体各 op 已按 lw 掩回，此处同宽掩齐）
                let pa = self.place_addr(a);
                let rp = self.b.ins().iconst(types::I64, rv as *const ir::Rvalue as i64);
                let fref = self.module.declare_func_in_func(self.simd_rv, self.b.func);
                let call = self.b.ins().call(fref, &[rp, pa]);
                let r = self.b.inst_results(call)[0];
                self.mask_val(r, Width::from_bytes(u64::from(*lane_bytes)).expect("lane 宽度"))
            }
        }
    }

    /// interp::int_bin 的逐位镜像（Add/Sub/Mul 掩后签名无关；移位 mod-64 同
    /// wrapping_shl/shr；符号 Shr 用 sext 视图算术右移，b&63 与 CLIF mod-64 一致）。
    fn int_bin(&mut self, op: IntBinOp, signed: bool, a: Value, b: Value, w: Width) -> Value {
        let r = match op {
            IntBinOp::Add => self.b.ins().iadd(a, b),
            IntBinOp::Sub => self.b.ins().isub(a, b),
            IntBinOp::Mul => self.b.ins().imul(a, b),
            IntBinOp::BitAnd => return self.b.ins().band(a, b),
            IntBinOp::BitOr => return self.b.ins().bor(a, b),
            IntBinOp::BitXor => return self.b.ins().bxor(a, b),
            IntBinOp::Shl => {
                // signed 分支的 sext 高位左移后必然溢出掩区（见 interp 注释），同型
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
                // M5.4b-1：零检 → mirvm_jit_div_zero（interp 同文案同码）；
                // signed 的 MIN/-1 用分支特判（x86 idiv #DE，CLIF sdiv 直接发 idiv）。
                let is_rem = matches!(op, IntBinOp::Rem);
                let zero = self.b.ins().icmp_imm(IntCC::Equal, b, 0);
                self.div_zero_if(zero, is_rem, false);
                if signed {
                    // 槽不变量为零扩到宽：signed 语义先 sext 到 64 位（interp
                    // int_bin: sext 后 wrapping_div/rem；否则负值被当大正数除）。
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
                        // wrapping_div(x, -1) = -x（MIN 回绕，ineg 同形）
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

    /// interp::int_ovf 的逐位镜像（128 位提升的 64 位恒等式）：
    /// - unsigned w<64：和/积在 64 位内精确 ⇒ ovf = 精确值 > mask；Sub ovf = a<b。
    /// - unsigned W64：Add ovf = 回绕（r<a）；Mul ovf = umulhi≠0；Sub 同上。
    /// - signed w<64：sext 后 64 位精确 ⇒ 与 [lo,hi] 比界。
    /// - signed W64：Add/Sub 标准符号恒等式；Mul ovf = smulhi ≠ (r>>63)。
    fn int_ovf(&mut self, op: OvfOp, signed: bool, a: Value, b: Value, w: Width) -> (Value, Value) {
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
                    // w<64：64 位内精确
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
                    // add：符号同入异出；sub：入异且出与被减数异
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

    /// T1-c：try_call 的异常表发射器——异常表 tag0 → pad 块（TryCallExn(0)
    /// 块参 = 异常指针落点，def exception_var 后跳 IR cleanup 块）；normal
    /// 指向新建 ok 块（调用方在其上做 ret 写回再跳 IR target）。
    /// 返回 (异常表, ok 块)；调用方在本 IR 块位发 try_call 后 switch 到 ok 块。
    fn emit_cleanup(
        &mut self,
        target: ir::Bb,
        cleanup: ir::Bb,
        sig: cranelift_codegen::ir::Signature,
        blocks: &[cranelift_codegen::ir::Block],
        bi: usize,
    ) -> (cranelift_codegen::ir::ExceptionTable, cranelift_codegen::ir::Block) {
        use cranelift_codegen::ir::{
            BlockArg, BlockCall, ExceptionTableData, ExceptionTableItem, ExceptionTag,
        };
        self.has_try_call = true;
        let pad = self.b.create_block();
        self.b.append_block_param(pad, types::I64);
        let ok = self.b.create_block();
        let normal = BlockCall::new(ok, [], &mut self.b.func.dfg.value_lists);
        let pad_call = self
            .b
            .func
            .dfg
            .block_call(pad, &[BlockArg::TryCallExn(0)]);
        let sigref = self.b.func.import_signature(sig);
        let et = self.b.func.dfg.exception_tables.push(ExceptionTableData::new(
            sigref,
            normal,
            [ExceptionTableItem::Tag(
                ExceptionTag::with_number(0).unwrap(),
                pad_call,
            )],
        ));
        self.b.switch_to_block(pad);
        let exn = self.b.block_params(pad)[0];
        let ev = self.exception_var();
        self.b.def_var(ev, exn);
        self.b.ins().jump(blocks[cleanup as usize], &[]);
        self.b.switch_to_block(blocks[bi]);
        let _ = target;
        (et, ok)
    }

    /// 调用写回（interp 同形：Ignore/Indirect 不写，Scalar=lo，Pair=(lo,hi)）。
    fn write_ret(&mut self, ret: &RetDest, lo: Value, hi: Value) {
        match ret {
            RetDest::Ignore | RetDest::Indirect(_) => {}
            RetDest::Scalar(ScalarPlace::Slot(s)) => {
                let s = *s;
                self.def_slot(s, lo);
            }
            RetDest::Pair(ScalarPlace::Slot(pl), ScalarPlace::Slot(ph)) => {
                let (pl, ph) = (*pl, *ph);
                self.def_slot(pl, lo);
                self.def_slot(ph, hi);
            }
            _ => unreachable!("admit 已筛 ret 形态"),
        }
    }

    fn term(
        &mut self,
        func: u32,
        body: &ir::FuncBody,
        t: &Terminator,
        blocks: &[cranelift_codegen::ir::Block],
        bi: usize,
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
                    // icmp+brif 链（v1；值稀疏，br_table 留优化项）
                    for (val, bb) in targets {
                        let hit = self.b.ins().icmp_imm(IntCC::Equal, v, *val as u64 as i64);
                        let next = self.b.create_block();
                        self.b.ins().brif(hit, blocks[*bb as usize], &[], next, &[]);
                        self.b.switch_to_block(next);
                    }
                    self.b.ins().jump(blocks[*otherwise as usize], &[]);
                }
                SwitchDiscr::Wide(pe) => {
                    // M5.4b-3：128 位判别——place 一次读全 128 位（iconcat），逐目标
                    // I128 常量比较（D8k：targets 与 discriminator 都保完整 128 位）
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
            } => {
                // 展平 av（interp Call 臂同序：RetDest::Indirect 前插目的真地址 +
                // 逐实参；lower 已把 Pair 实参展开为两槽、幻影尾参附加在末）
                let mut av: Vec<Value> = Vec::with_capacity(args.len() + 1);
                if let RetDest::Indirect(dst) = ret {
                    let a = self.place_addr(dst);
                    av.push(a);
                }
                for a in args {
                    av.push(self.operand(a).0);
                }
                // 写回模板（PLT 结果/c2i ret_ss 同形）
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
                            _ => unreachable!("admit 已筛 ret 形态"),
                        }
                    };
                }
                match unwind {
                    UnwindAction::Cleanup(bb) => {
                        // T1-c：try_call（normal=ok 块（写回后进 target），异常表
                        // tag0→pad(TryCallExn(0))；v1 统一走 c2i-try_call——语义唯一
                        // 权威，PLT try_call_indirect 留优化项）
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
                        let (et, ok) = self.emit_cleanup(*target, *bb, sig0, blocks, bi);
                        let fref = self.module.declare_func_in_func(self.c2i, self.b.func);
                        let fv = self.b.ins().iconst(types::I64, *callee as i64);
                        let ap = self.b.ins().stack_addr(types::I64, args_ss, 0);
                        let nv = self.b.ins().iconst(types::I64, av.len() as i64);
                        let rp = self.b.ins().stack_addr(types::I64, ret_ss, 0);
                        self.b.ins().try_call(fref, &[fv, ap, nv, rp], et);
                        self.b.switch_to_block(ok);
                        let lo = self.b.ins().stack_load(types::I64, ret_ss, 0);
                        let hi = self.b.ins().stack_load(types::I64, ret_ss, 8);
                        write_back!(lo, hi);
                        self.b.ins().jump(blocks[*target as usize], &[]);
                    }
                    UnwindAction::Terminate => {
                        // T1-c：Terminate 边界 = mirvm_call_terminate（c2i 形包装，
                        // interp call_guarding_terminate 同语义）
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
                        let cb = &self.shared.module.funcs[*callee as usize];
                        let plt = callee_abi(cb).filter(|cabi| cabi.nparams == av.len());
                        if let Some(cabi) = plt {
                            // 热路：PLT 内存间接——load slots_fast[callee] + call_indirect
                            //（恒定形状；蹦床→fast 的升级对调用点透明）
                            let slot_addr = &self.shared.jit.slots_fast[*callee as usize]
                                as *const std::sync::atomic::AtomicU64
                                as i64;
                            let ap = self.b.ins().iconst(types::I64, slot_addr);
                            let fp = self
                                .b
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
                            // 冷路：调用点直接 c2i（打包展平实参回解释器——interp 本就吃
                            // 展平 av，callee 任意 ABI 语义一致；panic 类分支的归宿）
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
                        self.b.ins().jump(blocks[*target as usize], &[]);
                    }
                }
            }
            Terminator::Return => {
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
                        // interp Return 同语义：从 sret 槽读目的地址，
                        // memcpy(_0 → dst, size)，(lo,hi) 返回 (0,0)
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
                // T1-b：mirvm_call_indirect 助手（interp CallIndirect 臂同派发：
                // fn_addrs 反查 → call_guest；未命中 + native_sig → ffi::call_addr）
                let (addr, _) = self.operand(callee);
                // 展平 av（RetDest::Indirect 前插目的地址 + 逐实参，interp 同序）
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
                    // T1-c：try_call（ok 块写回后先进 target；pad 跳 IR cleanup）
                    let mut sig0 = self.module.make_signature();
                    for _ in 0..8 {
                        sig0.params.push(AbiParam::new(types::I64));
                    }
                    let (et, ok) = self.emit_cleanup(*target, *bb, sig0, blocks, bi);
                    let z = self.b.ins().iconst(types::I64, 0);
                    self.b
                        .ins()
                        .try_call(fref, &[addr, ap, nv, rp, nok, nsig, fv, z], et);
                    self.b.switch_to_block(ok);
                    let lo = self.b.ins().stack_load(types::I64, ret_ss, 0);
                    let hi = self.b.ins().stack_load(types::I64, ret_ss, 8);
                    self.write_ret(ret, lo, hi);
                    self.b.ins().jump(blocks[*target as usize], &[]);
                } else {
                    // Continue / Terminate：旗标区分（Terminate 旗 = 外包
                    // catch_unwind+abort，call_guarding_terminate 同语义）
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
                // T1-b：asm-stub 真地址直调（interp InlineAsm 臂同槽 ABI：
                // 栈缓冲、ins 标量 8B 槽低位/VecBytes 全宽拷、call fn(*mut u8)、
                // outs 读回）
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
                // T1-b：mirvm_call_foreign 助手（interp CallForeign 臂同构——
                // thunk_args 物化/C1 Indirect 落点/pthread 栈放大还原/ffi::call 本体）
                let mut av: Vec<Value> = Vec::with_capacity(args.len());
                for a in args {
                    av.push(self.operand(a).0);
                }
                // C1：按值聚合返回 = Indirect 落点（ffi 层 memcpy 至目的真地址）
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
                let sp = self
                    .b
                    .ins()
                    .iconst(types::I64, sym.as_ptr() as i64);
                let sl = self.b.ins().iconst(types::I64, sym.len() as i64);
                let sg = self.b.ins().iconst(
                    types::I64,
                    sig as *const ir::ForeignSig as i64,
                );
                let ap = self.b.ins().stack_addr(types::I64, args_ss, 0);
                let nv = self.b.ins().iconst(types::I64, av.len() as i64);
                let fv = self.b.ins().iconst(types::I64, func as i64);
                macro_rules! foreign_write_back {
                    ($call:expr) => {
                        let r = self.b.inst_results($call)[0];
                        match ret {
                            RetDest::Ignore => {}
                            RetDest::Scalar(p) => {
                                self.write_scalar_place(p, r);
                            }
                            // C1：按值聚合字节已由 ffi 层 memcpy 至 dst
                            RetDest::Indirect(_) => {}
                            _ => unreachable!("admit 已筛 foreign 返回形态"),
                        }
                    };
                }
                if let UnwindAction::Cleanup(bb) = unwind {
                    // T1-c：try_call（ok 块写回后先进 target；pad 跳 IR cleanup）
                    let mut sig0 = self.module.make_signature();
                    for _ in 0..7 {
                        sig0.params.push(AbiParam::new(types::I64));
                    }
                    sig0.returns.push(AbiParam::new(types::I64));
                    let (et, ok) = self.emit_cleanup(*target, *bb, sig0, blocks, bi);
                    let z = self.b.ins().iconst(types::I64, 0);
                    let call =
                        self.b
                            .ins()
                            .try_call(fref, &[sp, sl, sg, ap, nv, ret_dst, fv, z], et);
                    self.b.switch_to_block(ok);
                    foreign_write_back!(call);
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
                    foreign_write_back!(call);
                    self.b.ins().jump(blocks[*target as usize], &[]);
                }
            }
            Terminator::CallBuiltin {
                builtin,
                args,
                ret,
                target,
                unwind,
            } => {
                // T1-b：分配系四件走 mirvm_alloc 快路（引擎堆同一入口，tag
                // 分派）；其余走 mirvm_call_builtin（exec_builtin 同一本体）
                let alloc_tag = match builtin {
                    ir::Builtin::RustAlloc => Some(0i64),
                    ir::Builtin::RustAllocZeroed => Some(1),
                    ir::Builtin::RustRealloc => Some(2),
                    ir::Builtin::RustDealloc => Some(3),
                    _ => None,
                };
                if let UnwindAction::Cleanup(bb) = unwind {
                    // T1-c：统一走 mirvm_call_builtin 通用路 + try_call（分配系
                    // 同在本体内，勿快路绕行异常表）
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
                    for _ in 0..7 {
                        sig0.params.push(AbiParam::new(types::I64));
                    }
                    let (et, ok) = self.emit_cleanup(*target, *bb, sig0, blocks, bi);
                    let z = self.b.ins().iconst(types::I64, 0);
                    self.b
                        .ins()
                        .try_call(fref, &[bp, ap, nv, ret_dst, fv, rp, z], et);
                    self.b.switch_to_block(ok);
                    let lo = self.b.ins().stack_load(types::I64, ret_ss, 0);
                    let hi = self.b.ins().stack_load(types::I64, ret_ss, 8);
                    self.write_ret(ret, lo, hi);
                    self.b.ins().jump(blocks[*target as usize], &[]);
                } else if matches!(unwind, UnwindAction::Continue)
                    && let Some(tag) = alloc_tag
                    && matches!(
                        ret,
                        RetDest::Ignore | RetDest::Scalar(ScalarPlace::Slot(_))
                    )
                {
                    // 实参定长四槽（realloc 用满；alloc/dealloc 缺位补 0，助手
                    // 按 tag 消费——exec_builtin 本体内 a(i) 只取所需）
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
                        _ => unreachable!("本支已筛 ret 形态"),
                    }
                    self.b.ins().jump(blocks[*target as usize], &[]);
                } else {
                    // 展平 av（无 sret 前插——builtin 的 Indirect 落点独立求值，
                    // interp 薄臂同序）；ret_dst = Indirect 目的真地址否则 0
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
                    // Terminate 旗（T1-c：外包 catch_unwind+abort 同语义）
                    let term = self.b.ins().iconst(
                        types::I64,
                        i64::from(matches!(unwind, UnwindAction::Terminate)),
                    );
                    self.b
                        .ins()
                        .call(fref, &[bp, ap, nv, ret_dst, fv, rp, term]);
                    let lo = self.b.ins().stack_load(types::I64, ret_ss, 0);
                    let hi = self.b.ins().stack_load(types::I64, ret_ss, 8);
                    self.write_ret(ret, lo, hi);
                    self.b.ins().jump(blocks[*target as usize], &[]);
                }
            }
            Terminator::Resume => {
                // T1-c：从 exception_var（pad 的 TryCallExn(0) def；入口预 def 0
                // 兜底，build 已按函数含 Resume 预声明）读异常指针 →
                // call _Unwind_Resume 续传（cg_clif Resume 同构）
                let ev = self.exception_var();
                let exn = self.b.use_var(ev);
                let fref = self
                    .module
                    .declare_func_in_func(self.unwind_resume, self.b.func);
                self.b.ins().call(fref, &[exn]);
                self.b.ins().trap(TrapCode::user(1).unwrap());
            }
            Terminator::TerminateAbort => {
                // T1-c：mirvm_jit_terminate_abort（interp TerminateAbort 臂同
                // 文案同码：UnwindTerminate（double panic/ABI 边界）→ abort）
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
            // T1-d：Trap-stub 终止子（mirvm_jit_trap 终止子形带 fn 名，interp
            // runblocks 臂同文案同 exit(70)）
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

pub(super) fn clif_rmw_op(op: ir::RmwOp) -> cranelift_codegen::ir::AtomicRmwOp {
    use cranelift_codegen::ir::AtomicRmwOp as C;
    match op {
        ir::RmwOp::Xchg => C::Xchg,
        ir::RmwOp::Add => C::Add,
        ir::RmwOp::Sub => C::Sub,
        ir::RmwOp::And => C::And,
        ir::RmwOp::Or => C::Or,
        ir::RmwOp::Xor => C::Xor,
        ir::RmwOp::Nand => C::Nand,
        ir::RmwOp::Max => C::Smax,
        ir::RmwOp::Min => C::Smin,
        ir::RmwOp::UMax => C::Umax,
        ir::RmwOp::UMin => C::Umin,
    }
}

/// 收集 SSA 候选槽偏移（def 0 初始化用）= 全部 Slot 引用减去落帧集。
pub(super) fn collect_ssa_offs(body: &ir::FuncBody, frame_offs: &FrameMap, out: &mut Vec<u32>) {
    let mut push = |s: &Slot| {
        if !frame_offs.contains(s.off) && !out.contains(&s.off) {
            out.push(s.off);
        }
    };
    let op = |o: &Operand, push: &mut dyn FnMut(&Slot)| {
        if let Operand::Slot(s) = o {
            push(s);
        }
    };
    if let RetAbi::Scalar(s) = &body.ret {
        push(s);
    }
    if let RetAbi::Pair(lo, hi) = &body.ret {
        push(lo);
        push(hi);
    }
    for p in &body.params {
        match p {
            ParamAbi::Scalar(s) => push(s),
            ParamAbi::Pair(lo, hi) => {
                push(lo);
                push(hi);
            }
            _ => {}
        }
    }
    for blk in &body.blocks {
        for st in &blk.stmts {
            match st {
                Stmt::Assign { dst, rv } => {
                    if let ScalarPlace::Slot(s) = dst {
                        push(s);
                    }
                    use ir::Rvalue as R;
                    match rv {
                        R::Use(a)
                        | R::NotBits(a)
                        | R::NotBool(a)
                        | R::Neg(a)
                        | R::Cast { a, .. }
                        | R::BitUn { a, .. } => op(a, &mut push),
                        R::IntBin { a, b, .. }
                        | R::IntCmp { a, b, .. }
                        | R::IntCmp3 { a, b, .. }
                        | R::PtrDiff { a, b, .. }
                        | R::UMax { a, b } => {
                            op(a, &mut push);
                            op(b, &mut push);
                        }
                        R::NicheDiscr { tag, .. } => op(tag, &mut push),
                        R::PtrOffset { ptr, count, .. } => {
                            op(ptr, &mut push);
                            op(count, &mut push);
                        }
                        _ => {}
                    }
                }
                Stmt::AssignOverflow {
                    a,
                    b,
                    dst_val,
                    dst_flag,
                    ..
                } => {
                    op(a, &mut push);
                    op(b, &mut push);
                    if let ScalarPlace::Slot(s) = dst_val {
                        push(s);
                    }
                    if let ScalarPlace::Slot(s) = dst_flag {
                        push(s);
                    }
                }
                // T1-d：SIMD 族的标量 Operand 槽（向量 place 走帧，不在此列）
                Stmt::SimdSplat { val, .. } => op(val, &mut push),
                Stmt::SimdExtractDyn { idx, dst, .. } => {
                    op(idx, &mut push);
                    if let ScalarPlace::Slot(s) = dst {
                        push(s);
                    }
                }
                Stmt::SimdInsertDyn { idx, val, .. } => {
                    op(idx, &mut push);
                    op(val, &mut push);
                }
                Stmt::SimdSelectBitmask { mask, .. } => op(mask, &mut push),
                Stmt::SimdMaskedLoad { base, .. } | Stmt::SimdMaskedStore { base, .. } => {
                    op(base, &mut push)
                }
                _ => {}
            }
        }
        match &blk.term {
            Terminator::SwitchInt {
                discr: SwitchDiscr::Scalar(o),
                ..
            } => op(o, &mut push),
            Terminator::Call { args, ret, .. } => {
                for a in args {
                    op(a, &mut push);
                }
                if let RetDest::Scalar(ScalarPlace::Slot(s)) = ret {
                    push(s);
                }
            }
            _ => {}
        }
    }
}

