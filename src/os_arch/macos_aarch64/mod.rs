//! macOS on aarch64: the kernel ABI as this CPU encodes it.
//!
//! See the module header of `crate::os_arch` for the boundary contract, for why this pair has a
//! directory instead of living on one axis, and for which part of each subsystem is the pair's.
//! `addrspace` is numbers only: the structure they describe is declared one level up.

/// Whether plain compiled code pins the current thread's `Ctx` in the register Cranelift reserves.
///
/// Not on this pair: the Mach-O loader half of the JIT is not written (see `reloc` beside this
/// file), so a compiled frame's unwind on this pair is an open item of the JIT design rather than
/// something the engine's suites cover. Plain code reads its stack floor through the helper's TLS
/// lookup instead, which is the same check without the register.
pub(crate) const PLAIN_CODE_PINS_CTX: bool = false;

pub mod addrspace;
#[cfg(any(feature = "compiler", test))]
pub mod bridge;
pub mod reloc;
pub mod signal;
pub mod syscall_asm;
pub mod thread;
