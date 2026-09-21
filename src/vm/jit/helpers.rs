//! `mirvm_*` runtime helpers called by compiled code through import symbols: the c2i
//! universal wrapper, the unreachable/trap/div_zero/volatile entries, host direct-eval of
//! 128-bit/f128/f16 arithmetic, and the libm symbol table. `compiler.rs` registers them.

use super::*;

mod floats;
mod stats;
/// Re-exported at the parent so `compiler.rs` keeps resolving these helper symbols
/// through `helpers::*`, exactly as it did when they were defined here.
pub(crate) use floats::*;
pub(crate) use stats::*;

fn active() -> (*mut crate::vm::ctx::Ctx, &'static Shared) {
    let ctx = crate::vm::ctx::current();
    (ctx, unsafe { &*(*ctx).shared })
}

fn active_shared() -> &'static Shared {
    active().1
}

/// JIT landing pads classify the exception they actually received. A separate
/// EngineFault may be suspended by a native catch on the same thread, so TLS
/// state cannot answer whether this particular unwind should skip cleanup.
pub(super) extern "C" fn mirvm_exception_is_engine_fault(exception: u64) -> u64 {
    u64::from(unsafe { crate::vm::unwind::raw_is_engine_fault(exception as *mut u8) })
}

/// Called by the published fast-entry wrapper before entering a compiled body.
/// The wrapper has no explicit stack slots, so this check runs before the
/// body's Cranelift prologue reserves its guest frame.
pub extern "C-unwind" fn mirvm_jit_stack_guard(func: u64, frame_bytes: u64) {
    let (ctx, shared) = active();
    let floor = unsafe { (*ctx).stack_floor };
    if floor == 0 {
        return;
    }
    let marker = 0u8;
    let sp = &marker as *const u8 as usize;
    // Keep room for backend spills and the diagnostic path itself in addition
    // to the explicit guest frame represented in the bytecode.
    let need = (frame_bytes as usize).saturating_add(256 << 10);
    if sp <= floor.saturating_add(need) {
        let name = shared
            .module
            .funcs
            .get(func as usize)
            .map(|body| &*body.name)
            .unwrap_or("<unknown>");
        crate::vm::interp::engine_abort(&format!(
            "guest stack overflow (JIT compiled frame hit safety margin before entry; fn {name})"
        ));
    }
}

/// Compiled-code safe point. This runs in ordinary VM state, never in the
/// kernel signal frame, so pthread TLS lookup and guest execution are allowed.
pub(super) extern "C-unwind" fn mirvm_poll_signals() {
    let (ctx, _shared) = active();
    crate::vm::ctx::drain_pending_signals(ctx);
}
use std::cell::Cell;

pub(super) extern "C-unwind" fn mirvm_c2i(func: u64, args: *const u64, n: u64, ret: *mut u64) {
    stat(S_C2I);
    let (ctx, _shared) = active();
    let a = unsafe { std::slice::from_raw_parts(args, n as usize) };
    let (lo, hi) = crate::vm::interp::call_guest(ctx, func as u32, a);
    unsafe {
        *ret = lo;
        *ret.add(1) = hi;
    }
}

/// The catch call that wraps user `main` in the fixed std startup chain. Same signature
/// as c2i, but it establishes the main panic scope around the call; the callee and the
/// intrinsics inside it may still enter the JIT.
pub(super) extern "C-unwind" fn mirvm_call_main_catch(
    func: u64,
    args: *const u64,
    n: u64,
    ret: *mut u64,
) {
    stat(S_C2I);
    let (ctx, _shared) = active();
    crate::vm::ctx::call_main_panic_boundary(ctx, || {
        let a = unsafe { std::slice::from_raw_parts(args, n as usize) };
        let (lo, hi) = crate::vm::interp::call_guest(ctx, func as u32, a);
        unsafe {
            *ret = lo;
            *ret.add(1) = hi;
        }
    });
}

/// TerminateAbort helper: same message and exit code as the interpreter's
/// `TerminateAbort` arm -- an `UnwindTerminate` (double panic / ABI boundary) aborts.
pub(super) extern "C-unwind" fn mirvm_jit_terminate_abort() -> ! {
    eprintln!("mirvm[m4-engine]: UnwindTerminate (double panic/ABI boundary) -- abort");
    std::process::abort()
}

/// Direct-call helper for the Terminate boundary. Interpreter and JIT share the raw
/// exception classification: an EngineFault unwinds on to its owning Engine, any other
/// unwind reaching a guest Terminate aborts. The c2i-shaped (callee, args, n, ret)
/// wrapper exists because a Call with a Terminate edge bypasses the PLT and re-enters the
/// body here.
pub(super) extern "C-unwind" fn mirvm_call_terminate(
    callee: u64,
    args: *const u64,
    n: u64,
    ret: *mut u64,
) {
    stat(S_CTERM);
    let (ctx, _shared) = active();
    let f = || {
        let a = unsafe { std::slice::from_raw_parts(args, n as usize) };
        crate::vm::interp::call_guest(ctx, callee as u32, a)
    };
    let (lo, hi) = crate::vm::unwind::guard_terminate(f);
    unsafe {
        *ret = lo;
        *ret.add(1) = hi;
    }
}

/// CallIndirect helper: the same dispatch as the interpreter's `CallIndirect` arm --
/// reverse lookup in `fn_addrs` into `call_guest`; on a miss with `native_sig` set,
/// `ffi::call_addr`; an empty slot with `null_ok` is a no-op. The null-pointer and
/// unknown-target diagnostics match the interpreter. When `terminate` is set, exceptions
/// go to the shared raw classifier instead.
pub(super) extern "C-unwind" fn mirvm_call_indirect(
    addr: u64,
    args: *const u64,
    n: u64,
    ret: *mut u64,
    null_ok: u64,
    native_sig: u64,
    caller: u64,
    terminate: u64,
) {
    stat(S_INDIR);
    if terminate != 0 {
        crate::vm::unwind::guard_terminate(|| {
            mirvm_call_indirect(addr, args, n, ret, null_ok, native_sig, caller, 0)
        });
        return;
    }
    let (ctx, shared) = active();
    let module = &shared.module;
    if null_ok != 0 && addr == 0 {
        return; // empty slot for a dyn virtual drop: no-op (same as the interpreter)
    }
    let caller_name = &module.funcs[caller as usize].name;
    if addr == 0 {
        crate::vm::interp::engine_abort(&format!(
            "indirect call through a null fn pointer (caller {caller_name})"
        ));
    }
    let av = unsafe { std::slice::from_raw_parts(args, n as usize) };
    let (lo, hi) = if let Some(&fid) = module.fn_addrs.get(&addr) {
        crate::vm::interp::call_guest(ctx, fid, av)
    } else if native_sig != 0 {
        // The guest holds a native code pointer obtained from dlsym at run time: call it
        // directly through the frozen signature. For an Agg return the first slot is the
        // destination, since libffi's sret slot is not an argument slot; same as the
        // interpreter.
        let nsig = unsafe { &*(native_sig as *const crate::vm::ir::ForeignSig) };
        let (ret_dst, arg_slice) = if matches!(nsig.ret, crate::vm::ir::FfiKind::Agg(_)) {
            (av.first().copied(), &av[1..])
        } else {
            (None, av)
        };
        (
            crate::vm::ffi::call_addr(addr as usize, nsig, arg_slice, ret_dst),
            0,
        )
    } else {
        crate::vm::interp::engine_abort(&format!(
            "indirect call target {addr:#x} is not a known fn entry (caller {caller_name})"
        ));
    };
    unsafe {
        *ret = lo;
        *ret.add(1) = hi;
    }
}

/// TlsRef helper: lazily materializes the per-thread instance block through
/// `interp::tls_addr`.
pub(super) extern "C-unwind" fn mirvm_tls_ref(id: u64) -> u64 {
    stat(S_TLS);
    let (ctx, _shared) = active();
    crate::vm::interp::tls_addr(ctx, id as u32)
}

/// CallForeign helper: the same shape as the interpreter's `CallForeign` arm --
/// materialize the thunk args, take the C1 Indirect landing, grow and restore the pthread
/// stack, then call `ffi::call`; the diagnostics match the interpreter.
pub(super) extern "C-unwind" fn mirvm_call_foreign(
    sym_ptr: *const u8,
    sym_len: u64,
    sig: u64,
    args: *const u64,
    n: u64,
    ret_dst: u64,
    caller: u64,
    terminate: u64,
) -> u64 {
    stat(S_FOREIGN);
    if terminate != 0 {
        return crate::vm::unwind::guard_terminate(|| {
            mirvm_call_foreign(sym_ptr, sym_len, sig, args, n, ret_dst, caller, 0)
        });
    }
    let (ctx, shared) = active();
    let module = &shared.module;
    let sym = unsafe {
        std::str::from_utf8_unchecked(std::slice::from_raw_parts(sym_ptr, sym_len as usize))
    };
    let sig = unsafe { &*(sig as *const crate::vm::ir::ForeignSig) };
    let mut av: Vec<u64> = unsafe { std::slice::from_raw_parts(args, n as usize) }.to_vec();
    let callbacks = crate::vm::thunks::prepare_foreign_callbacks(shared, sym, sig, &mut av);
    let ret_dst = (ret_dst != 0).then_some(ret_dst);
    // Temporarily raise the pthread stack size for the call and restore it afterwards; a
    // self-supplied stack is left alone.
    let stack_restore = crate::vm::ffi::amplify_pthread_stack(sym, &av);
    // Native code may synchronously call back into the guest, and that callback may call
    // foreign again in the same Ctx. The FFI cache is borrowed only for symbol resolution:
    // end the borrow once the address is in hand, then hand control to native code.
    let resolved = {
        let ffi = unsafe { &mut (*ctx).ffi };
        ffi.resolve(
            sym,
            &module.native_libs,
            &module.required_native_libs,
            &module.native_images,
            &module.mc_images,
        )
    };
    let r = resolved
        .map(|resolved| resolved.map(|fnptr| crate::vm::ffi::call_addr(fnptr, sig, &av, ret_dst)));
    if let Some((attr, orig)) = stack_restore {
        crate::os::thread::attr_set_stack_size(attr, orig);
    }
    let caller_name = &module.funcs[caller as usize].name;
    let r = r.unwrap_or_else(|reason| {
        crate::vm::interp::engine_abort(&format!(
            "failed to load a required native library for foreign `{sym}` (fn {caller_name}): {reason}"
        ))
    });
    let Some(r) = r else {
        crate::vm::interp::engine_abort(&format!(
            "foreign `{sym}` symbol not found (neither the archive fallback table nor a full-domain dlsym matched; fn {caller_name})"
        ));
    };
    callbacks.complete(r);
    r
}

/// CallBuiltin helper: shares `interp::exec_builtin`'s body, which owns the x86 vector
/// sret / pair / main-scalar lanes and their diagnostics. A JIT frame has no edge
/// semantics, so the edge cell is a dummy: with `unwind` set to `Continue` nothing reads
/// it.
pub(super) extern "C-unwind" fn mirvm_call_builtin(
    builtin: u64, // *const ir::Builtin
    args: *const u64,
    n: u64,
    ret_dst: u64,
    caller: u64,
    ret: *mut u64,
    terminate: u64,
    role: u64,
) {
    stat(S_BUILTIN);
    if terminate != 0 {
        crate::vm::unwind::guard_terminate(|| {
            mirvm_call_builtin(builtin, args, n, ret_dst, caller, ret, 0, role)
        });
        return;
    }
    let (ctx, shared) = active();
    let av = unsafe { std::slice::from_raw_parts(args, n as usize) };
    let (lo, hi) = crate::vm::interp::exec_builtin(
        ctx,
        &shared.module.funcs[caller as usize],
        &Cell::new(None),
        unsafe { &*(builtin as *const ir::Builtin) },
        av,
        (ret_dst != 0).then_some(ret_dst),
        &ir::UnwindAction::Continue,
        match role {
            0 => ir::BuiltinCallRole::Normal,
            1 => ir::BuiltinCallRole::MainPanicCatcher,
            _ => crate::vm::interp::engine_abort("invalid JIT builtin call role encoding"),
        },
    );
    unsafe {
        *ret = lo;
        *ret.add(1) = hi;
    }
}

/// Fast path for the allocation family: dispatch by tag into the same `exec_builtin`
/// body the interpreter uses, so the allocation semantics and the custom
/// `#[global_allocator]` shim routing all live in one place. Tags: 0=RustAlloc,
/// 1=RustAllocZeroed, 2=RustRealloc, 3=RustDealloc. Arguments occupy four fixed slots:
/// realloc uses them all, and the missing slots of the others are 0 and never read.
pub(super) extern "C-unwind" fn mirvm_alloc(
    tag: u64,
    a0: u64,
    a1: u64,
    a2: u64,
    a3: u64,
    caller: u64,
) -> u64 {
    stat(S_ALLOC);
    let (ctx, shared) = active();
    let builtin = match tag {
        0 => ir::Builtin::RustAlloc,
        1 => ir::Builtin::RustAllocZeroed,
        2 => ir::Builtin::RustRealloc,
        _ => ir::Builtin::RustDealloc,
    };
    let av = [a0, a1, a2, a3];
    let (lo, _) = crate::vm::interp::exec_builtin(
        ctx,
        &shared.module.funcs[caller as usize],
        &Cell::new(None),
        &builtin,
        &av,
        None,
        &ir::UnwindAction::Continue,
        ir::BuiltinCallRole::Normal,
    );
    lo
}

/// The trace code domain's `HostSyscall` site. Trace code reads
/// the recorder from the register the boundary pinned and passes it here, so the
/// recording path below never loads thread-local state and never checks whether
/// a session is running. `args` points at the call's operands minus the syscall
/// number, `n` counts them.
///
/// `used` receives the recorder the call actually recorded into. A `fork` child
/// resumes inside the parent's compiled body with the parent's recorder still in
/// the register, so its first recording syscall replaces that recorder and hands
/// the replacement back; the caller writes it into the pinned register, which is
/// what keeps the next syscall in that child from recording into the parent's
/// page.
pub(super) extern "C" fn mirvm_host_syscall_trace(
    producer: u64,
    nr: i64,
    args: *const u64,
    n: u64,
    used: *mut u64,
) -> i64 {
    stat(S_SYSCALL_TRACE);
    let av = unsafe { std::slice::from_raw_parts(args, n as usize) };
    let (result, used_producer) =
        unsafe { crate::telemetry::capture::host_syscall_pinned(producer as *mut _, nr, av) };
    unsafe { *used = used_producer as u64 };
    result
}

// Host unwinder continuation point for the `Resume` terminator (cg_clif's Resume shape).
unsafe extern "C" {
    pub fn _Unwind_Resume(ex: *mut u8) -> !;
}

/// Diagnostics for the `Unreachable` terminator match the interpreter byte for byte.
/// The engine fault returns to the execution boundary and the CLI maps it to 70; it is
/// neither a bare trap's SIGILL nor TerminateAbort's 134.
pub(super) extern "C-unwind" fn mirvm_jit_unreachable(func: u64) -> ! {
    let shared = active_shared();
    let name = shared
        .module
        .funcs
        .get(func as usize)
        .map(|f| &*f.name)
        .unwrap_or("?");
    crate::vm::interp::engine_abort(&format!("reached Unreachable (fn {name})"));
}

/// Trap entry shared by statement-level traps and terminators: the diagnostic and exit
/// code match the interpreter's `engine_abort` byte for byte -- `TRAP: {reason}` for the
/// statement form and `TRAP: {reason} (fn name)` for the terminator form, where
/// `func == u64::MAX` marks the statement form. The engine fault returns to the execution
/// boundary and the CLI maps it to 70; TerminateAbort still aborts as its semantics
/// require.
pub(super) extern "C-unwind" fn mirvm_jit_trap(reason_ptr: u64, reason_len: u64, func: u64) -> ! {
    let reason = unsafe {
        std::str::from_utf8_unchecked(std::slice::from_raw_parts(
            reason_ptr as *const u8,
            reason_len as usize,
        ))
    };
    if func == u64::MAX {
        crate::vm::interp::engine_abort(&format!("TRAP: {reason}"));
    } else {
        let shared = active_shared();
        let name = shared
            .module
            .funcs
            .get(func as usize)
            .map(|f| &*f.name)
            .unwrap_or("?");
        crate::vm::interp::engine_abort(&format!("TRAP: {reason} (fn {name})"));
    }
}

// ===== helpers sharing the interpreter's implementation bodies (no copied logic) =====

/// Division-by-zero diagnostic exit: the same message text and exit code as the
/// interpreter's `engine_abort`.
pub(super) extern "C-unwind" fn mirvm_jit_div_zero(kind: u64) -> ! {
    let what = match kind {
        0 => "guest integer division by zero",
        1 => "guest integer remainder by zero",
        2 => "guest 128-bit integer division by zero",
        _ => "guest 128-bit integer remainder by zero",
    };
    crate::vm::interp::engine_abort(what);
}

/// Volatile read: the interpreter's opaque byte carrier plus chunk decomposition.
pub(super) extern "C-unwind" fn mirvm_volatile_load(addr: u64, dst: u64, size: u64) {
    stat(S_VLOAD);
    crate::vm::interp::mem_read_volatile(addr, dst, size as u32);
}

/// Volatile write (same path).
pub(super) extern "C-unwind" fn mirvm_volatile_store(addr: u64, src: u64, size: u64) {
    stat(S_VSTORE);
    crate::vm::interp::mem_write_volatile(addr, src, size as u32);
}

// ===== SIMD/wide-value helpers over the interpreter's shared `simd_exec` bodies =====

/// SIMD/wide-statement helper: re-match the statement and call the interpreter's shared
/// body, so no semantics are duplicated. Parameter order is (stmt, a, b, c, dst, v0, v1);
/// unused slots are 0; only `SimdExtractDyn` uses the return value.
pub(super) extern "C-unwind" fn mirvm_simd_stmt(
    stmt: u64,
    a: u64,
    b: u64,
    c: u64,
    dst: u64,
    v0: u64,
    v1: u64,
) -> u64 {
    stat(S_SIMD_STMT);
    use crate::vm::interp::simd_exec as x;
    let st = unsafe { &*(stmt as *const ir::Stmt) };
    match st {
        ir::Stmt::SimdBin {
            op,
            lane,
            lanes,
            lane_bytes,
            ..
        } => x::simd_bin_body(
            dst as *mut u8,
            a as *const u8,
            b as *const u8,
            *op,
            *lane,
            *lanes,
            *lane_bytes,
        ),
        ir::Stmt::SimdUn {
            op,
            lane,
            lanes,
            lane_bytes,
            ..
        } => x::simd_un_body(
            dst as *mut u8,
            a as *const u8,
            *op,
            *lane,
            *lanes,
            *lane_bytes,
        ),
        ir::Stmt::SimdFma {
            lanes, lane_bytes, ..
        } => x::simd_fma_body(
            dst as *mut u8,
            a as *const u8,
            b as *const u8,
            c as *const u8,
            *lanes,
            *lane_bytes,
        ),
        ir::Stmt::SimdFunnel {
            left,
            lanes,
            lane_bytes,
            ..
        } => x::simd_funnel_body(
            dst as *mut u8,
            a as *const u8,
            b as *const u8,
            c as *const u8,
            *left,
            *lanes,
            *lane_bytes,
        ),
        ir::Stmt::SimdCast {
            lanes,
            src_lane,
            src_bytes,
            dst_lane,
            dst_bytes,
            ..
        } => x::simd_cast_body(
            dst as *mut u8,
            a as *const u8,
            *lanes,
            *src_lane,
            *src_bytes,
            *dst_lane,
            *dst_bytes,
        ),
        ir::Stmt::SimdSelect {
            mask_bytes,
            lanes,
            lane_bytes,
            ..
        } => x::simd_select_body(
            dst as *mut u8,
            a as *const u8,
            *mask_bytes,
            b as *const u8,
            c as *const u8,
            *lanes,
            *lane_bytes,
        ),
        ir::Stmt::SimdSelectBitmask {
            lanes, lane_bytes, ..
        } => x::simd_select_bitmask_body(
            dst as *mut u8,
            v0,
            a as *const u8,
            b as *const u8,
            *lanes,
            *lane_bytes,
        ),
        ir::Stmt::SimdGather {
            mask_bytes,
            lanes,
            lane_bytes,
            ..
        } => x::simd_gather_body(
            dst as *mut u8,
            a as *const u8,
            b as *const u8,
            c as *const u8,
            *mask_bytes,
            *lanes,
            *lane_bytes,
        ),
        ir::Stmt::SimdScatter {
            mask_bytes,
            lanes,
            lane_bytes,
            ..
        } => x::simd_scatter_body(
            a as *const u8,
            b as *const u8,
            c as *const u8,
            *mask_bytes,
            *lanes,
            *lane_bytes,
        ),
        ir::Stmt::SimdMaskedLoad {
            mask_bytes,
            lanes,
            lane_bytes,
            ..
        } => x::simd_masked_load_body(
            dst as *mut u8,
            a as *const u8,
            *mask_bytes,
            v0,
            b as *const u8,
            *lanes,
            *lane_bytes,
        ),
        ir::Stmt::SimdMaskedStore {
            mask_bytes,
            lanes,
            lane_bytes,
            ..
        } => x::simd_masked_store_body(
            a as *const u8,
            *mask_bytes,
            v0,
            b as *const u8,
            *lanes,
            *lane_bytes,
        ),
        ir::Stmt::SimdExtractDyn {
            lanes, lane_bytes, ..
        } => return x::simd_extract_dyn_body(a as *const u8, v0, *lanes, *lane_bytes),
        ir::Stmt::SimdInsertDyn {
            lanes, lane_bytes, ..
        } => x::simd_insert_dyn_body(dst as *mut u8, a as *const u8, v0, v1, *lanes, *lane_bytes),
        ir::Stmt::SimdArithOffset { stride, lanes, .. } => x::simd_arith_offset_body(
            dst as *mut u8,
            a as *const u8,
            b as *const u8,
            *stride,
            *lanes,
        ),
        ir::Stmt::SimdSplat {
            lanes, lane_bytes, ..
        } => x::simd_splat_body(dst as *mut u8, v0, *lanes, *lane_bytes),
        ir::Stmt::Sat128 { op, signed, .. } => {
            x::sat128_body(a as *const u8, b as *const u8, dst as *mut u8, *op, *signed)
        }
        _ => unreachable!("excluded during admission"),
    }
    0
}

/// SIMD rvalue helper for Bitmask/Reduce/ReduceArith; `pa` is the vector place address.
pub(super) extern "C-unwind" fn mirvm_simd_rv(rv: u64, pa: u64) -> u64 {
    stat(S_SIMD_RV);
    use crate::vm::interp::simd_exec as x;
    let r = unsafe { &*(rv as *const ir::Rvalue) };
    match r {
        ir::Rvalue::SimdBitmask {
            lanes, lane_bytes, ..
        } => x::simd_bitmask_body(pa as *const u8, *lanes, *lane_bytes),
        ir::Rvalue::SimdReduce {
            all,
            lanes,
            lane_bytes,
            ..
        } => x::simd_reduce_body(pa as *const u8, *all, *lanes, *lane_bytes),
        ir::Rvalue::SimdReduceArith {
            op,
            lane,
            lanes,
            lane_bytes,
            ..
        } => x::simd_reduce_arith_body(pa as *const u8, *op, *lane, *lanes, *lane_bytes),
        _ => unreachable!("excluded during admission"),
    }
}
