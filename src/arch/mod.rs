//! Architecture specific items.
//! The only channel through which MIRVM touches architecture specific operations.
//!
//! # Boundary Contract
//! - **Leaf**: Function signatures only contain raw pointers/integers/own
//!   small types—no engine types, no OS dependencies (the OS resides in the OS/
//!   module, which this module does not touch).
//! - **Primitives, No Adjudication**: CPUID dispatch (guest feature selection),
//!   call timing, and failure semantics are left to the engine; this module only
//!   executes true host instructions/emits true machine code bytes.
//! - Acceptance Surface: Hardware intrinsic execution body (the true form
//!   behind the llvm.x86.* boundary), machine code byte emission (stub),
//!   single-issue instruction primitives (int3/xgetbv).
//! - Not accepted: Register allocation for `lower/asm.rs` is coupled with the
//!   llvm.x86 name table (rustc type coupling) – reserve the lower field and
//!   document it (decision-history §7.16).
//!
//! # Platform Selection
//! Currently the only implementation is `x86_64/` (with the same prerequisites
//! as os/linux, global_asm/asm-stub hardgate). Non-x86_64 targets fail to
//! compile at compile time. New architecture implementation adds directory and
//! cfg dispatch here (OpenJDK cpu/family model).

#[cfg(target_arch = "x86_64")]
pub(crate) mod x86_64;

#[cfg(not(target_arch = "x86_64"))]
compile_error!(
    "The `arch` module currently only implements x86_64 (with the same prerequisites as the global_asm/asm-stub hard gate)."
);
