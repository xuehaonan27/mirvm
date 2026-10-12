//! Stack headroom checks emitted before entering a compiled body.

use super::*;

pub(super) fn required_headroom(body: &ir::FuncBody) -> u64 {
    u64::from(body.frame_size)
        .saturating_add(u64::from(body.frame_align.saturating_sub(16)))
        .saturating_add(helpers::STACK_GUARD_MARGIN as u64)
}

pub(super) fn emit(
    module: &mut JITModule,
    b: &mut FunctionBuilder<'_>,
    domain: CodeDomain,
    func: Value,
    need: Value,
    stack_guard: ClifFuncId,
    stack_overflow: ClifFuncId,
) {
    if domain == CodeDomain::Plain && crate::os_arch::PLAIN_CODE_PINS_CTX {
        // Check before the callee's prologue. A zero floor leaves the limit below any real stack.
        // The offset is build-specific, as are the persisted entries that contain it.
        let ctx = b.ins().get_pinned_reg(types::I64);
        let floor = b.ins().load(
            types::I64,
            MemFlagsData::trusted(),
            ctx,
            std::mem::offset_of!(crate::vm::ctx::Ctx, stack_floor) as i32,
        );
        let sp = b.ins().get_stack_pointer(types::I64);
        let limit = b.ins().iadd(floor, need);
        let crossing = b.ins().icmp(IntCC::UnsignedLessThanOrEqual, sp, limit);
        let overflow = b.create_block();
        let cont = b.create_block();
        b.ins().brif(crossing, overflow, &[], cont, &[]);
        b.switch_to_block(overflow);
        let report = module.declare_func_in_func(stack_overflow, b.func);
        b.ins().call(report, &[func]);
        b.ins().trap(TrapCode::STACK_OVERFLOW);
        b.switch_to_block(cont);
    } else {
        // Trace pins the recorder; pairs without a pinned context also recover it through TLS.
        let guard = module.declare_func_in_func(stack_guard, b.func);
        b.ins().call(guard, &[func, need]);
    }
}
