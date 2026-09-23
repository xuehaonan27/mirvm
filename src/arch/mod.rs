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
//! # Architecture Selection
//! Currently the only implementation is `x86_64/` (with the same prerequisites
//! as os/linux, global_asm/asm-stub hardgate). Non-x86_64 targets fail to
//! compile at compile time. New architecture implementation adds directory and
//! cfg dispatch here (OpenJDK cpu/family model).
//!
//! Nothing here may depend on an OS: the kernel's own view of the CPU — signal frames, syscall
//! sequences, address-space layout — belongs to the matching `crate::os_arch` pair, which is
//! where a new CPU's Linux half is written.
//!
//! # The Surface an Architecture Provides
//!
//! The names below are the interface. Each is declared once here and dispatches through one
//! `#[cfg]` ladder, so a caller names `arch::<name>` on every target and no call site outside this
//! directory spells an architecture. What a name holds that does not vary with the CPU is declared
//! here rather than repeated in every architecture directory; what does vary is the architecture's.
//!
//! - `ELF_MACHINE` — the architecture's `e_machine`, which every ELF platform it runs on shares.
//! - `asm_text` — the assembly vocabulary the materializer in `src/lower/` asks for: the
//!   architecture's name, its syntax directives, and the instruction forms mirvm rewrites.
//! - `asmstub` — machine-code byte emission (entry stubs), the single-issue instruction primitives
//!   (`int3`, `xgetbv`), and the trampoline a rewritten `syscall` lands on.
//! - `reloc` — the meanings a relocation can carry, which every psABI shares, and this
//!   architecture's classification of its own type numbers onto them.
//! - `x86_64` — the architecture's own instruction implementations, named directly only by guest
//!   semantics that target this CPU: such a caller states that one guest operation *is* one host
//!   instruction, which is a fact about a specific guest and host together.

#[cfg(target_arch = "aarch64")]
pub(crate) mod aarch64;
#[cfg(target_arch = "x86_64")]
pub(crate) mod x86_64;
#[cfg(target_arch = "aarch64")]
pub(crate) use aarch64::ELF_MACHINE;
#[cfg(target_arch = "aarch64")]
pub(crate) use aarch64::asmstub;
/// The architecture's ELF machine identity, named once for every caller: `arch::ELF_MACHINE`.
#[cfg(target_arch = "x86_64")]
pub(crate) use x86_64::ELF_MACHINE;
/// The architecture's assembly vocabulary and its machine-code emission, named once for every
/// caller: `arch::asm_text::…` and `arch::asmstub::…`. The materializer in `src/lower/` asks for
/// the syntax directive and the instruction forms through the first and supplies no
/// architecture-specific text itself.
#[cfg(target_arch = "x86_64")]
pub(crate) use x86_64::{asm_text, asmstub};

/// Relocation meanings, which are no architecture's, beside the one classification that is:
/// `arch::reloc::{Kind, field_width, classify}`.
pub(crate) mod reloc;

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
compile_error!("Not implemented for this architecture.");
