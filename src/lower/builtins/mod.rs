//! The built-in symbol table: the guest-visible names the engine answers itself instead of handing
//! over to dlsym.
//!
//! Split by which side owns a name:
//!
//! - this file holds the families every target shares — the allocator shims rustc asks for, the
//!   guest unwinder's entry points, and the host services whose fn-ptr arguments or signal context
//!   cannot go through the generic path;
//! - [`x86_64`] and [`aarch64`] hold one CPU's own guest intrinsic names, one table each. Both
//!   compile on every target, exactly as the semantics lane's bodies do
//!   (`src/vm/semantics/builtin`): an intrinsic name is only ever produced by lowering the guest it
//!   describes, so the other CPU's table is data no guest of this build can reach.

mod aarch64;
mod x86_64;

use super::*;

/// Register a run of names, one per line.
///
/// `builtin_table!(out; "getenv" => HostGetenv, ...)` maps a symbol to one [`ir::Builtin`], and
/// `builtin_table!(out; refused "name", ...)` refuses each name with itself as the reason it
/// carries. Both shapes are what the table is: a name and the one answer it has.
macro_rules! builtin_table {
    ($out:ident; $( $name:literal => $builtin:ident ),* $(,)?) => {
        $(
            $out.insert(Symbol::intern($name), ir::Builtin::$builtin);
        )*
    };
    ($out:ident; refused $( $name:literal ),* $(,)?) => {
        $(
            $out.insert(
                Symbol::intern($name),
                ir::Builtin::Unsupported(ir::StaticStr($name.into())),
            );
        )*
    };
}
pub(super) use builtin_table;

/// Symbols are mangled (`mangle_internal_symbol`). Special = default allocator (engine-managed).
/// Non-special entries are generated and resolved by rustc inside the final crate, not rewritten at
/// the std external-declaration boundary; the four ordinary `__rust_*` entries for a custom
/// #[global_allocator] are routed uniformly at the Module level.
pub(super) fn engine_builtins(tcx: TyCtxt<'_>) -> FxHashMap<Symbol, ir::Builtin> {
    use rustc_ast::expand::allocator::{self, SpecialAllocatorMethod as S};
    use rustc_symbol_mangling::mangle_internal_symbol;

    let mut out = FxHashMap::default();
    if let Some(kind) = tcx.allocator_kind(()) {
        for method in rustc_codegen_ssa::base::allocator_shim_contents(tcx, kind) {
            let Some(special) = method.special else {
                continue;
            };
            let b = match special {
                S::Alloc => ir::Builtin::RustAlloc,
                S::Dealloc => ir::Builtin::RustDealloc,
                S::Realloc => ir::Builtin::RustRealloc,
                S::AllocZeroed => ir::Builtin::RustAllocZeroed,
            };
            let sym = mangle_internal_symbol(tcx, &allocator::global_fn_name(method.name));
            out.insert(Symbol::intern(&sym), b);
        }
    }
    let sentinel =
        mangle_internal_symbol(tcx, rustc_ast::expand::allocator::NO_ALLOC_SHIM_IS_UNSTABLE);
    out.insert(Symbol::intern(&sentinel), ir::Builtin::NoAllocShim);

    // The unwind primitive: panic_unwind is still interpreted, and the engine takes over at the
    // platform unwinder symbol layer.
    //
    // The host unwinder retrieves IPs from the libffi/interpreter native stack and cannot represent
    // guest frozen function entries, so the context/state APIs would see host interpreter frames.
    // Denying the whole group keeps a silently-wrong value from arriving through CFA/LSDA/SetGR.
    // The seven names below are answered honestly instead, from the Ctx shadow frame stack (IP =
    // synthetic fn token).
    builtin_table!(out;
        "_Unwind_RaiseException" => UnwindRaise,
        "_Unwind_Backtrace" => UnwindBacktrace,
        "_Unwind_GetIP" => UnwindGetIp,
        "_Unwind_GetIPInfo" => UnwindGetIpInfo,
        "_Unwind_FindEnclosingFunction" => UnwindFindEnclosing,
        "_Unwind_GetCFA" => UnwindGetCfa,
        "_Unwind_DeleteException" => UnwindDeleteException,
    );
    builtin_table!(out; refused
        "_Unwind_Find_FDE",
        "_Unwind_ForcedUnwind",
        "_Unwind_GetDataRelBase",
        "_Unwind_GetGR",
        "_Unwind_GetLanguageSpecificData",
        "_Unwind_GetRegionStart",
        "_Unwind_GetTextRelBase",
        "_Unwind_Resume",
        "_Unwind_Resume_or_Rethrow",
        "_Unwind_SetGR",
        "_Unwind_SetIP",
    );

    // Host services. getenv/write/strlen/abort are the frequent panic-chain passthroughs; the rest
    // cannot use the generic dlsym+libffi path: `fork` guards the guest thread count first, glibc
    // does not export `atexit` for guest dlsym, and a signal handler is hidden inside an integer or
    // a struct while its trampoline must be async-signal-safe, which an ordinary libffi closure is
    // not.
    builtin_table!(out;
        "getenv" => HostGetenv,
        "write" => HostWrite,
        "strlen" => HostStrlen,
        "abort" => HostAbort,
        "fork" => HostFork,
        "atexit" => HostAtexit,
        "__cxa_atexit" => HostCxaAtexit,
        "on_exit" => HostOnExit,
        "syscall" => HostSyscall,
        "signal" => HostSignal,
        "raise" => HostRaise,
        "sigaction" => HostSigaction,
    );

    x86_64::register(&mut out);
    aarch64::register(&mut out);
    out
}
