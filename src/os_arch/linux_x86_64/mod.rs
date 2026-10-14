//! Linux on x86_64: the kernel ABI as this CPU encodes it.
//!
//! See the module header of `crate::os_arch` for the boundary contract, for why this pair has a
//! directory instead of living on one axis, and for which part of each subsystem is the pair's.
//! `addrspace` is numbers only: the structure they describe is declared one level up.

/// Whether plain compiled code pins the current thread's `Ctx` in the register Cranelift reserves
/// ([`crate::arch::PINNED_REG`]).
///
/// The pin is a codegen convention with an unwind obligation: a frame that unwinds has to leave the
/// register as its Rust caller had it, so the CFI this pair's JIT output carries must describe that
/// register the way Cranelift's pinned-register exclusion leaves it. This pair's JIT output does,
/// and the engine's own suites cover panics and signals crossing compiled frames here.
pub(crate) const PLAIN_CODE_PINS_CTX: bool = true;

pub mod addrspace;
#[cfg(any(feature = "compiler", test))]
pub mod bridge;
pub mod reloc;
pub mod signal;
pub mod syscall_asm;
pub mod thread;
