//! `mirvm_*` runtime helpers called by compiled code through import symbols: the c2i
//! universal wrapper, the unreachable/trap/div_zero/volatile entries, host direct-eval of
//! 128-bit/f128/f16 arithmetic, and the libm symbol table. `compiler.rs` registers them.

use super::*;

fn active() -> (*mut crate::vm::engine::ctx::Ctx, &'static Shared) {
    let ctx = crate::vm::engine::ctx::current();
    (ctx, unsafe { &*(*ctx).shared })
}

fn active_shared() -> &'static Shared {
    active().1
}

/// JIT landing pads classify the exception they actually received. A separate
/// EngineFault may be suspended by a native catch on the same thread, so TLS
/// state cannot answer whether this particular unwind should skip cleanup.
pub(super) extern "C" fn mirvm_exception_is_engine_fault(exception: u64) -> u64 {
    u64::from(unsafe { crate::vm::engine::unwind::raw_is_engine_fault(exception as *mut u8) })
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
        crate::vm::engine::interp::engine_abort(&format!(
            "guest stack overflow (JIT compiled frame hit safety margin before entry; fn {name})"
        ));
    }
}

/// Compiled-code safe point. This runs in ordinary VM state, never in the
/// kernel signal frame, so pthread TLS lookup and guest execution are allowed.
pub(super) extern "C-unwind" fn mirvm_poll_signals() {
    let (ctx, _shared) = active();
    crate::vm::engine::ctx::drain_pending_signals(ctx);
}
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU64};

// ===== helper frequency stats (MIRVM_JIT_STATS=1) =====
// Each helper entry does one fetch_add(Relaxed); at process exit a single line is dumped
// through libc atexit. With the knob off the cost is one relaxed load per entry.
pub(super) static STAT_ON: AtomicBool = AtomicBool::new(false);
static STAT: [AtomicU64; 13] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];
const STAT_NAMES: [&str; 13] = [
    "alloc",
    "tls_ref",
    "c2i",
    "call_indirect",
    "call_foreign",
    "call_builtin",
    "simd_stmt",
    "simd_rv",
    "volatile_load",
    "volatile_store",
    "call_terminate",
    "bin128_ovf",
    "syscall_trace",
];
const S_ALLOC: usize = 0;
const S_TLS: usize = 1;
const S_C2I: usize = 2;
const S_INDIR: usize = 3;
const S_FOREIGN: usize = 4;
const S_BUILTIN: usize = 5;
const S_SIMD_STMT: usize = 6;
const S_SIMD_RV: usize = 7;
const S_VLOAD: usize = 8;
const S_VSTORE: usize = 9;
const S_CTERM: usize = 10;
const S_BIN128: usize = 11;
/// The one bucket reachable only from trace-domain compiled code, so a non-zero
/// count is direct evidence that the pinned syscall site executed rather than the
/// interpreter's thread-local one.
const S_SYSCALL_TRACE: usize = 12;

#[inline(always)]
fn stat(i: usize) {
    if STAT_ON.load(Ordering::Relaxed) {
        STAT[i].fetch_add(1, Ordering::Relaxed);
    }
}

/// Frequency of one helper bucket. A test that must prove a path ran -- rather
/// than only that its effects match another path's -- reads it here.
#[cfg(test)]
pub(crate) fn stat_value(name: &str) -> u64 {
    let index = STAT_NAMES
        .iter()
        .position(|n| *n == name)
        .expect("unknown helper stat bucket");
    STAT[index].load(Ordering::Relaxed)
}

extern "C" fn stat_dump() {
    let mut line = String::from("mirvm-jit-stats:");
    for (i, n) in STAT_NAMES.iter().enumerate() {
        let v = STAT[i].load(Ordering::Relaxed);
        if v != 0 {
            line.push_str(&format!(" {n}={v}"));
        }
    }
    eprintln!("{line}");
}

/// Called once from `Compiler::new`: enables the stats from the environment and registers
/// the exit dump.
pub(super) fn stat_init() {
    if std::env::var_os("MIRVM_JIT_STATS").is_some() {
        STAT_ON.store(true, Ordering::Relaxed);
        crate::os::process::atexit_native(stat_dump);
    }
}

pub(super) extern "C-unwind" fn mirvm_c2i(func: u64, args: *const u64, n: u64, ret: *mut u64) {
    stat(S_C2I);
    let (ctx, _shared) = active();
    let a = unsafe { std::slice::from_raw_parts(args, n as usize) };
    let (lo, hi) = crate::vm::engine::interp::call_guest(ctx, func as u32, a);
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
    crate::vm::engine::ctx::call_main_panic_boundary(ctx, || {
        let a = unsafe { std::slice::from_raw_parts(args, n as usize) };
        let (lo, hi) = crate::vm::engine::interp::call_guest(ctx, func as u32, a);
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
        crate::vm::engine::interp::call_guest(ctx, callee as u32, a)
    };
    let (lo, hi) = crate::vm::engine::unwind::guard_terminate(f);
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
        crate::vm::engine::unwind::guard_terminate(|| {
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
        crate::vm::engine::interp::engine_abort(&format!(
            "indirect call through a null fn pointer (caller {caller_name})"
        ));
    }
    let av = unsafe { std::slice::from_raw_parts(args, n as usize) };
    let (lo, hi) = if let Some(&fid) = module.fn_addrs.get(&addr) {
        crate::vm::engine::interp::call_guest(ctx, fid, av)
    } else if native_sig != 0 {
        // The guest holds a native code pointer obtained from dlsym at run time: call it
        // directly through the frozen signature. For an Agg return the first slot is the
        // destination, since libffi's sret slot is not an argument slot; same as the
        // interpreter.
        let nsig = unsafe { &*(native_sig as *const crate::vm::engine::ir::ForeignSig) };
        let (ret_dst, arg_slice) = if matches!(nsig.ret, crate::vm::engine::ir::FfiKind::Agg(_)) {
            (av.first().copied(), &av[1..])
        } else {
            (None, av)
        };
        (
            crate::vm::engine::ffi::call_addr(addr as usize, nsig, arg_slice, ret_dst),
            0,
        )
    } else {
        crate::vm::engine::interp::engine_abort(&format!(
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
    crate::vm::engine::interp::tls_addr(ctx, id as u32)
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
        return crate::vm::engine::unwind::guard_terminate(|| {
            mirvm_call_foreign(sym_ptr, sym_len, sig, args, n, ret_dst, caller, 0)
        });
    }
    let (ctx, shared) = active();
    let module = &shared.module;
    let sym = unsafe {
        std::str::from_utf8_unchecked(std::slice::from_raw_parts(sym_ptr, sym_len as usize))
    };
    let sig = unsafe { &*(sig as *const crate::vm::engine::ir::ForeignSig) };
    let mut av: Vec<u64> = unsafe { std::slice::from_raw_parts(args, n as usize) }.to_vec();
    let callbacks = crate::vm::engine::thunks::prepare_foreign_callbacks(shared, sym, sig, &mut av);
    let ret_dst = (ret_dst != 0).then_some(ret_dst);
    // Temporarily raise the pthread stack size for the call and restore it afterwards; a
    // self-supplied stack is left alone.
    let stack_restore = crate::vm::engine::ffi::amplify_pthread_stack(sym, &av);
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
    let r = resolved.map(|resolved| {
        resolved.map(|fnptr| crate::vm::engine::ffi::call_addr(fnptr, sig, &av, ret_dst))
    });
    if let Some((attr, orig)) = stack_restore {
        crate::os::thread::attr_set_stack_size(attr, orig);
    }
    let caller_name = &module.funcs[caller as usize].name;
    let r = r.unwrap_or_else(|reason| {
        crate::vm::engine::interp::engine_abort(&format!(
            "failed to load a required native library for foreign `{sym}` (fn {caller_name}): {reason}"
        ))
    });
    let Some(r) = r else {
        crate::vm::engine::interp::engine_abort(&format!(
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
        crate::vm::engine::unwind::guard_terminate(|| {
            mirvm_call_builtin(builtin, args, n, ret_dst, caller, ret, 0, role)
        });
        return;
    }
    let (ctx, shared) = active();
    let av = unsafe { std::slice::from_raw_parts(args, n as usize) };
    let (lo, hi) = crate::vm::engine::interp::exec_builtin(
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
            _ => crate::vm::engine::interp::engine_abort("invalid JIT builtin call role encoding"),
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
    let (lo, _) = crate::vm::engine::interp::exec_builtin(
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
    crate::vm::engine::interp::engine_abort(&format!("reached Unreachable (fn {name})"));
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
        crate::vm::engine::interp::engine_abort(&format!("TRAP: {reason}"));
    } else {
        let shared = active_shared();
        let name = shared
            .module
            .funcs
            .get(func as usize)
            .map(|f| &*f.name)
            .unwrap_or("?");
        crate::vm::engine::interp::engine_abort(&format!("TRAP: {reason} (fn {name})"));
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
    crate::vm::engine::interp::engine_abort(what);
}

/// Volatile read: the interpreter's opaque byte carrier plus chunk decomposition.
pub(super) extern "C-unwind" fn mirvm_volatile_load(addr: u64, dst: u64, size: u64) {
    stat(S_VLOAD);
    crate::vm::engine::interp::mem_read_volatile(addr, dst, size as u32);
}

/// Volatile write (same path).
pub(super) extern "C-unwind" fn mirvm_volatile_store(addr: u64, src: u64, size: u64) {
    stat(S_VSTORE);
    crate::vm::engine::interp::mem_write_volatile(addr, src, size as u32);
}

// ===== f16/f128/128-bit helpers: the interpreter's host direct-eval channel. Rust
// f16/f128/i128/u128 arithmetic lowers to the same compiler-builtins `__*tf*` and glibc
// `*f128` libm symbols that interp/native use, so both paths are bit-identical. =====

pub(super) fn lo_hi(lo: u64, hi: u64) -> u128 {
    (lo as u128) | ((hi as u128) << 64)
}
pub(super) fn hi_lo(v: u128) -> (u64, u64) {
    (v as u64, (v >> 64) as u64)
}
pub(super) fn f128_of(lo: u64, hi: u64) -> f128 {
    f128::from_bits(lo_hi(lo, hi))
}
pub(super) fn pair_of(v: f128) -> (u64, u64) {
    hi_lo(v.to_bits())
}

/// i128/u128 `overflowing_add/sub/mul` for `Bin128`: returns the overflow flag and
/// writes the result pair to `out`.
pub(super) extern "C-unwind" fn mirvm_bin128_ovf(
    op: u64,
    signed: bool,
    alo: u64,
    ahi: u64,
    blo: u64,
    bhi: u64,
    out: *mut u64,
) -> u64 {
    stat(S_BIN128);
    let (r, ovf) = if signed {
        let (x, y) = (lo_hi(alo, ahi) as i128, lo_hi(blo, bhi) as i128);
        match op {
            0 => x.overflowing_add(y),
            1 => x.overflowing_sub(y),
            _ => x.overflowing_mul(y),
        }
    } else {
        let (x, y) = (lo_hi(alo, ahi), lo_hi(blo, bhi));
        let (v, ovf) = match op {
            0 => x.overflowing_add(y),
            1 => x.overflowing_sub(y),
            _ => x.overflowing_mul(y),
        };
        (v as i128, ovf)
    };
    let (lo, hi) = hi_lo(r as u128);
    unsafe {
        *out = lo;
        *out.add(1) = hi;
    }
    ovf as u64
}

/// `Bin128` Div/Rem. Cranelift's ISLE has no I128 division (it reports
/// `udiv.i128 "should be implemented in ISLE"`), so this cannot compile to CLIF. The host
/// wrapping arithmetic matches the interpreter; a zero divisor takes the 128-bit div_zero
/// diagnostic (kind 2/3).
pub(super) extern "C-unwind" fn mirvm_bin128_divrem(
    is_rem: u64,
    signed: bool,
    alo: u64,
    ahi: u64,
    blo: u64,
    bhi: u64,
    out: *mut u64,
) {
    let (a, b) = (lo_hi(alo, ahi), lo_hi(blo, bhi));
    if b == 0 {
        mirvm_jit_div_zero(2 + is_rem);
    }
    let r: u128 = if signed {
        let (x, y) = (a as i128, b as i128);
        (if is_rem != 0 {
            x.wrapping_rem(y)
        } else {
            x.wrapping_div(y)
        }) as u128
    } else if is_rem != 0 {
        a.wrapping_rem(b)
    } else {
        a.wrapping_div(b)
    };
    let (lo, hi) = hi_lo(r);
    unsafe {
        *out = lo;
        *out.add(1) = hi;
    }
}

/// f128 arithmetic (op: 0=add, 1=sub, 2=mul, 3=rem (`fmodf128`), 4=div).
pub(super) extern "C-unwind" fn mirvm_f128_bin(
    op: u64,
    alo: u64,
    ahi: u64,
    blo: u64,
    bhi: u64,
    out: *mut u64,
) {
    let (a, b) = (f128_of(alo, ahi), f128_of(blo, bhi));
    let r = match op {
        0 => a + b,
        1 => a - b,
        2 => a * b,
        4 => a / b,
        _ => a % b,
    };
    let (lo, hi) = pair_of(r);
    unsafe {
        *out = lo;
        *out.add(1) = hi;
    }
}

/// f128 comparison (cc: 0=Eq, 1=Ne, 2=Lt, 3=Le, 4=Gt, 5=Ge). IEEE semantics: NaN makes
/// every predicate false except Ne.
pub(super) extern "C-unwind" fn mirvm_f128_cmp(
    cc: u64,
    alo: u64,
    ahi: u64,
    blo: u64,
    bhi: u64,
) -> u64 {
    let (a, b) = (f128_of(alo, ahi), f128_of(blo, bhi));
    match cc {
        0 => (a == b) as u64,
        1 => (a != b) as u64,
        2 => (a < b) as u64,
        3 => (a <= b) as u64,
        4 => (a > b) as u64,
        _ => (a >= b) as u64,
    }
}

/// f128 unary (op: 0=neg; the rest are the unary math family, glibc `*f128` libm
/// symbols).
pub(super) extern "C-unwind" fn mirvm_f128_un(op: u64, alo: u64, ahi: u64, out: *mut u64) {
    let a = f128_of(alo, ahi);
    let r = match op {
        0 => -a,
        1 => a.sqrt(),
        2 => a.sin(),
        3 => a.cos(),
        4 => a.exp(),
        5 => a.exp2(),
        6 => a.ln(),
        7 => a.log2(),
        8 => a.log10(),
        9 => a.abs(),
        10 => a.floor(),
        11 => a.ceil(),
        12 => a.trunc(),
        13 => a.round(),
        _ => a.round_ties_even(),
    };
    let (lo, hi) = pair_of(r);
    unsafe {
        *out = lo;
        *out.add(1) = hi;
    }
}

/// f128 binary math (op: 0=pow, 1=powi, 2=copysign, 3=minnum, 4=maxnum, 5=fma with the
/// addend in `clo`/`chi`).
pub(super) extern "C-unwind" fn mirvm_f128_math(
    op: u64,
    alo: u64,
    ahi: u64,
    blo: u64,
    bhi: u64,
    clo: u64,
    chi: u64,
    out: *mut u64,
) {
    let (a, b, c) = (f128_of(alo, ahi), f128_of(blo, bhi), f128_of(clo, chi));
    let r = match op {
        0 => a.powf(b),
        // powi's rhs is an i32 scalar (the F128Rhs::Scalar contract; the interpreter's
        // stmt.rs reads it as a raw scalar too). `blo` holds the raw integer bits, so it
        // must never go through `f128_of`.
        1 => a.powi(blo as i32),
        2 => a.copysign(b),
        3 => a.min(b),
        4 => a.max(b),
        _ => a.mul_add(b, c),
    };
    let (lo, hi) = pair_of(r);
    unsafe {
        *out = lo;
        *out.add(1) = hi;
    }
}

/// Scalar -> f128 (kind: 0=f16, 1=f32, 2=f64, then 3=i8, 4=u8, 5=i16, 6=u16, 7=i32,
/// 8=u32, 9=i64, 10=u64; anything else is taken as u64).
pub(super) extern "C-unwind" fn mirvm_f128_from_scalar(kind: u64, v: u64, out: *mut u64) {
    let r = match kind {
        0 => f128::from(f16::from_bits(v as u16)),
        1 => f128::from(f32::from_bits(v as u32)),
        2 => f128::from(f64::from_bits(v)),
        3 => f128::from(v as i8),
        4 => f128::from(v as u8),
        5 => f128::from(v as i16),
        6 => f128::from(v as u16),
        7 => f128::from(v as i32),
        8 => f128::from(v as u32),
        9 => f128::from(v as i64),
        _ => f128::from(v),
    };
    let (lo, hi) = pair_of(r);
    unsafe {
        *out = lo;
        *out.add(1) = hi;
    }
}

/// f128 -> scalar (the same kind codes; float conversions preserve the bit pattern,
/// integer conversions use `as` saturation semantics).
pub(super) extern "C-unwind" fn mirvm_f128_to_scalar(kind: u64, alo: u64, ahi: u64) -> u64 {
    let a = f128_of(alo, ahi);
    match kind {
        0 => (a as f16).to_bits() as u64,
        1 => (a as f32).to_bits() as u64,
        2 => (a as f64).to_bits(),
        3 => (a as i8) as u8 as u64,
        4 => (a as u8) as u64,
        5 => (a as i16) as u16 as u64,
        6 => (a as u16) as u64,
        7 => (a as i32) as u32 as u64,
        8 => (a as u32) as u64,
        9 => (a as i64) as u64,
        _ => a as u64,
    }
}

/// i128/u128 -> f128 (signed: 0=unsigned, 1=signed). The reverse direction is
/// `mirvm_f128_to_wide`, which saturates.
pub(super) extern "C-unwind" fn mirvm_f128_from_wide(
    signed: bool,
    lo: u64,
    hi: u64,
    out: *mut u64,
) {
    let r = if signed {
        (lo_hi(lo, hi) as i128) as f128
    } else {
        lo_hi(lo, hi) as f128
    };
    let (l, h) = pair_of(r);
    unsafe {
        *out = l;
        *out.add(1) = h;
    }
}
pub(super) extern "C-unwind" fn mirvm_f128_to_wide(
    signed: bool,
    alo: u64,
    ahi: u64,
    out: *mut u64,
) {
    let a = f128_of(alo, ahi);
    let v: u128 = if signed {
        (a as i128) as u128
    } else {
        a as u128
    };
    let (l, h) = hi_lo(v);
    unsafe {
        *out = l;
        *out.add(1) = h;
    }
}

/// float -> i128/u128 with saturation, the reverse of `Wide128ToFloat`
/// (kind: 0=f16, 1=f32, 2=f64).
pub(super) extern "C-unwind" fn mirvm_float_to_wide(
    kind: u64,
    v: u64,
    signed: bool,
    out: *mut u64,
) {
    let r: u128 = match (kind, signed) {
        (0, true) => (f16::from_bits(v as u16) as i128) as u128,
        (0, false) => f16::from_bits(v as u16) as u128,
        (1, true) => (f32::from_bits(v as u32) as i128) as u128,
        (1, false) => f32::from_bits(v as u32) as u128,
        (2, true) => (f64::from_bits(v) as i128) as u128,
        _ => f64::from_bits(v) as u128,
    };
    let (l, h) = hi_lo(r);
    unsafe {
        *out = l;
        *out.add(1) = h;
    }
}

/// i128/u128 -> f16, the f16 target of `Wide128ToFloat`.
pub(super) extern "C-unwind" fn mirvm_wide_to_f16(lo: u64, hi: u64, signed: bool) -> u64 {
    let v = if signed {
        (lo_hi(lo, hi) as i128) as f16
    } else {
        lo_hi(lo, hi) as f16
    };
    v.to_bits() as u64
}

/// i128/u128 -> f32/f64. Computed with a host `as` cast (round to nearest, the same
/// semantics as compiler-builtins `__float*ti*`) rather than by calling those symbols:
/// they return in XMM0 while the helper call ABI reads RAX, so the call would read
/// garbage. Returning the bit pattern as u64 keeps the return lane unambiguous.
pub(super) extern "C-unwind" fn mirvm_wide_to_f32(lo: u64, hi: u64, signed: bool) -> u64 {
    let v = if signed {
        (lo_hi(lo, hi) as i128) as f32
    } else {
        lo_hi(lo, hi) as f32
    };
    v.to_bits() as u64
}

pub(super) extern "C-unwind" fn mirvm_wide_to_f64(lo: u64, hi: u64, signed: bool) -> u64 {
    let v = if signed {
        (lo_hi(lo, hi) as i128) as f64
    } else {
        lo_hi(lo, hi) as f64
    };
    v.to_bits()
}

// ===== f16 helpers on the interpreter's host direct-eval channel =====

/// f16 arithmetic (op as in `mirvm_f128_bin`: 0=add, 1=sub, 2=mul, 3=rem, 4=div;
/// arguments and result are f16 bit patterns in u64). Note that div is the fallback arm,
/// so an op-4 Div is never answered with `%`.
pub(super) extern "C-unwind" fn mirvm_f16_bin(op: u64, a: u64, b: u64) -> u64 {
    let (x, y) = (f16::from_bits(a as u16), f16::from_bits(b as u16));
    let r = match op {
        0 => x + y,
        1 => x - y,
        2 => x * y,
        3 => x % y,
        _ => x / y,
    };
    r.to_bits() as u64
}
pub(super) extern "C-unwind" fn mirvm_f16_cmp(cc: u64, a: u64, b: u64) -> u64 {
    let (x, y) = (f16::from_bits(a as u16), f16::from_bits(b as u16));
    match cc {
        0 => (x == y) as u64,
        1 => (x != y) as u64,
        2 => (x < y) as u64,
        3 => (x <= y) as u64,
        4 => (x > y) as u64,
        _ => (x >= y) as u64,
    }
}
pub(super) extern "C-unwind" fn mirvm_f16_neg(a: u64) -> u64 {
    (-f16::from_bits(a as u16)).to_bits() as u64
}

/// f16 unary math (op order matches the interpreter's `MathUn` macro; the same host f16
/// methods, so there is no drift). These helpers exist so that MathUn/MathBin/MathFma
/// never reach `as_float(F16)`: that panics on the compiler thread, which would silently
/// leave a compressible function interpreted.
pub(super) extern "C-unwind" fn mirvm_f16_math_un(op: u64, a: u64) -> u64 {
    let x = f16::from_bits(a as u16);
    let r = match op {
        0 => x.sqrt(),
        1 => x.sin(),
        2 => x.cos(),
        3 => x.exp(),
        4 => x.exp2(),
        5 => x.ln(),
        6 => x.log2(),
        7 => x.log10(),
        8 => x.abs(),
        9 => x.floor(),
        10 => x.ceil(),
        11 => x.trunc(),
        12 => x.round(),
        _ => x.round_ties_even(),
    };
    r.to_bits() as u64
}

/// f16 binary math (op: 0=pow, 1=powi, 2=copysign, 3=minnum, 4=maxnum; the interpreter's
/// MathBin f16 arm has the same shape). For powi, `b` is the raw i32 bits and must not go
/// through `from_bits`.
pub(super) extern "C-unwind" fn mirvm_f16_math_bin(op: u64, a: u64, b: u64) -> u64 {
    let (x, y) = (f16::from_bits(a as u16), f16::from_bits(b as u16));
    let r = match op {
        0 => x.powf(y),
        1 => x.powi(b as i32),
        2 => x.copysign(y),
        3 => x.min(y),
        _ => x.max(y),
    };
    r.to_bits() as u64
}

/// f16 fused multiply-add (host `mul_add`, a single rounding; the interpreter's MathFma
/// f16 arm has the same shape).
pub(super) extern "C-unwind" fn mirvm_f16_fma(a: u64, b: u64, c: u64) -> u64 {
    f16::from_bits(a as u16)
        .mul_add(f16::from_bits(b as u16), f16::from_bits(c as u16))
        .to_bits() as u64
}
/// f16 conversions (kind: 1=f16->f32, 2=f16->f64, 3=f32->f16, 4=f64->f16).
pub(super) extern "C-unwind" fn mirvm_f16_cast(kind: u64, v: u64) -> u64 {
    match kind {
        1 => (f16::from_bits(v as u16) as f32).to_bits() as u64,
        2 => (f16::from_bits(v as u16) as f64).to_bits(),
        3 => (f32::from_bits(v as u32) as f16).to_bits() as u64,
        _ => (f64::from_bits(v) as f16).to_bits() as u64,
    }
}
/// f16 <-> int (`to`: 0=i8, 1=u8, 2=i16, 3=u16, 4=i32, 5=u32, 6=i64, 7=u64; `from` uses
/// the same codes).
pub(super) extern "C-unwind" fn mirvm_f16_to_int(kind: u64, a: u64) -> u64 {
    let x = f16::from_bits(a as u16);
    match kind {
        0 => (x as i8) as u8 as u64,
        1 => (x as u8) as u64,
        2 => (x as i16) as u16 as u64,
        3 => (x as u16) as u64,
        4 => (x as i32) as u32 as u64,
        5 => (x as u32) as u64,
        6 => (x as i64) as u64,
        _ => x as u64,
    }
}
pub(super) extern "C-unwind" fn mirvm_f16_from_int(kind: u64, v: u64) -> u64 {
    let r = match kind {
        0 => (v as i8) as f16,
        1 => (v as u8) as f16,
        2 => (v as i16) as f16,
        3 => (v as u16) as f16,
        4 => (v as i32) as f16,
        5 => (v as u32) as f16,
        6 => (v as i64) as f16,
        _ => v as f16,
    };
    r.to_bits() as u64
}

// powi goes through compiler-builtins: Rust's `powi` lowers to the same symbols.
unsafe extern "C" {
    fn __powidf2(x: f64, n: i32) -> f64;
    fn __powisf2(x: f32, n: i32) -> f32;
}

/// libm symbol table, registered into the JITBuilder and holding the same symbols the
/// interpreter's host libm calls use. The libc crate no longer binds the math functions,
/// so they are declared extern here to take their addresses (the process already links
/// libm).
mod libm_decls {
    unsafe extern "C" {
        pub fn sqrtf();
        pub fn sqrt();
        pub fn sinf();
        pub fn sin();
        pub fn cosf();
        pub fn cos();
        pub fn expf();
        pub fn exp();
        pub fn exp2f();
        pub fn exp2();
        pub fn logf();
        pub fn log();
        pub fn log2f();
        pub fn log2();
        pub fn log10f();
        pub fn log10();
        pub fn fabsf();
        pub fn fabs();
        pub fn floorf();
        pub fn floor();
        pub fn ceilf();
        pub fn ceil();
        pub fn truncf();
        pub fn trunc();
        pub fn roundf();
        pub fn round();
        pub fn rintf();
        pub fn rint();
        pub fn powf();
        pub fn pow();
        pub fn copysignf();
        pub fn copysign();
        pub fn fminf();
        pub fn fmin();
        pub fn fmaxf();
        pub fn fmax();
        pub fn fmodf();
        pub fn fmod();
    }
}
pub(super) fn libm_syms() -> Vec<(&'static str, usize)> {
    vec![
        ("sqrtf", libm_decls::sqrtf as *const () as usize),
        ("sqrt", libm_decls::sqrt as *const () as usize),
        ("sinf", libm_decls::sinf as *const () as usize),
        ("sin", libm_decls::sin as *const () as usize),
        ("cosf", libm_decls::cosf as *const () as usize),
        ("cos", libm_decls::cos as *const () as usize),
        ("expf", libm_decls::expf as *const () as usize),
        ("exp", libm_decls::exp as *const () as usize),
        ("exp2f", libm_decls::exp2f as *const () as usize),
        ("exp2", libm_decls::exp2 as *const () as usize),
        ("logf", libm_decls::logf as *const () as usize),
        ("log", libm_decls::log as *const () as usize),
        ("log2f", libm_decls::log2f as *const () as usize),
        ("log2", libm_decls::log2 as *const () as usize),
        ("log10f", libm_decls::log10f as *const () as usize),
        ("log10", libm_decls::log10 as *const () as usize),
        ("fabsf", libm_decls::fabsf as *const () as usize),
        ("fabs", libm_decls::fabs as *const () as usize),
        ("floorf", libm_decls::floorf as *const () as usize),
        ("floor", libm_decls::floor as *const () as usize),
        ("ceilf", libm_decls::ceilf as *const () as usize),
        ("ceil", libm_decls::ceil as *const () as usize),
        ("truncf", libm_decls::truncf as *const () as usize),
        ("trunc", libm_decls::trunc as *const () as usize),
        ("roundf", libm_decls::roundf as *const () as usize),
        ("round", libm_decls::round as *const () as usize),
        ("rintf", libm_decls::rintf as *const () as usize),
        ("rint", libm_decls::rint as *const () as usize),
        ("powf", libm_decls::powf as *const () as usize),
        ("pow", libm_decls::pow as *const () as usize),
        ("copysignf", libm_decls::copysignf as *const () as usize),
        ("copysign", libm_decls::copysign as *const () as usize),
        ("fminf", libm_decls::fminf as *const () as usize),
        ("fmin", libm_decls::fmin as *const () as usize),
        ("fmaxf", libm_decls::fmaxf as *const () as usize),
        ("fmax", libm_decls::fmax as *const () as usize),
        ("fmodf", libm_decls::fmodf as *const () as usize),
        ("fmod", libm_decls::fmod as *const () as usize),
    ]
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
    use crate::vm::engine::interp::simd_exec as x;
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
    use crate::vm::engine::interp::simd_exec as x;
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
