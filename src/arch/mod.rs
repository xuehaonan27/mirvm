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
//!   single-issue instruction primitives (int3).
//! - Not accepted: Register allocation for `lower/asm.rs` is coupled with the
//!   llvm.x86 name table (rustc type coupling) – reserve the lower field and
//!   document it (decision-history §7.16).
//!
//! # Architecture Selection
//! `x86_64/` and `aarch64/` are the implementations, each with the same prerequisites as the
//! platform it is paired with in `src/os_arch/`. A target that is neither fails to compile at
//! compile time. Adding one means a directory and an arm in each ladder here that names it
//! (OpenJDK cpu/family model).
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
//! - `PINNED_REG` — the register Cranelift reserves when a module enables the pinned register,
//!   which the trace domain does.
//! - `asm_text` — the assembly vocabulary the materializer in `src/lower/` asks for: the
//!   architecture's name and its syntax directives. The forms mirvm rewrites a `syscall` into are
//!   the pair's, because they name the object format as well as the instruction.
//! - `asmstub` — machine-code byte emission (entry stubs), the single-issue instruction primitive
//!   a breakpoint is (`int3`), the trampoline a rewritten `syscall` lands on, and the registers the
//!   runtime-interposition bridge's entries load their engine into. The bridge's own text is the
//!   pair's, because it names the object format as well as the instructions: the same is true of
//!   the jump a P1 entry trampoline takes and of the form a `syscall` is rewritten into.
//! - `reloc` — the meanings a relocation can carry, which every psABI and object format shares.
//!   The classification of a format's own numbering onto them is the pair's
//!   (`crate::os_arch::reloc`), because the numbering belongs to the format.
//! - `x86_64` — the architecture's own instruction implementations, named directly only by guest
//!   semantics that target this CPU: such a caller states that one guest operation *is* one host
//!   instruction, which is a fact about a specific guest and host together.
//! - `intrinsics` — the instruction bodies behind the guest's own intrinsic boundary. A CPU whose
//!   guest has no such intrinsic answers with one unreachable body per name, so the builtin lane
//!   that dispatches them names this path on every target rather than an architecture's directory.
//! - `f16` — the 16-bit float lane conversions the guest's portable SIMD cast needs. A CPU with
//!   the instruction in hardware uses it; one without keeps the software model beside its own
//!   instructions.

#[cfg(target_arch = "aarch64")]
pub(crate) mod aarch64;
#[cfg(target_arch = "x86_64")]
pub(crate) mod x86_64;
#[cfg(target_arch = "aarch64")]
pub(crate) use aarch64::ELF_MACHINE;
/// The register the trace domain's pinned register is, named once for every caller:
/// `arch::PINNED_REG`.
#[cfg(target_arch = "aarch64")]
pub(crate) use aarch64::PINNED_REG;
#[cfg(target_arch = "aarch64")]
pub(crate) use aarch64::{asm_text, asmstub};
/// The architecture's ELF machine identity, named once for every caller: `arch::ELF_MACHINE`.
#[cfg(target_arch = "x86_64")]
pub(crate) use x86_64::ELF_MACHINE;
#[cfg(target_arch = "x86_64")]
pub(crate) use x86_64::PINNED_REG;
/// The architecture's assembly vocabulary and its machine-code emission, named once for every
/// caller: `arch::asm_text::…` and `arch::asmstub::…`. The materializer in `src/lower/` asks for
/// the syntax directive and the instruction forms through the first and supplies no
/// architecture-specific text itself.
#[cfg(target_arch = "x86_64")]
pub(crate) use x86_64::{asm_text, asmstub};

/// The instruction bodies behind the guest's own intrinsic boundary, which the builtin lane in
/// `src/vm/semantics/builtin` names through one path on every target: `arch::intrinsics::…`.
#[cfg(target_arch = "aarch64")]
pub(crate) use aarch64::intrinsics;
#[cfg(target_arch = "x86_64")]
pub(crate) use x86_64::intrinsics;

/// The 16-bit float lane conversions the guest's portable SIMD cast needs, one path per call site:
/// `arch::f16::{to_f16, to_f32}`.
#[cfg(target_arch = "aarch64")]
pub(crate) use aarch64::f16;
#[cfg(target_arch = "x86_64")]
pub(crate) use x86_64::f16;

/// Relocation meanings, which are no architecture's, beside the one classification that is:
/// `arch::reloc::{Kind, field_width, classify}`.
pub(crate) mod reloc;

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
compile_error!("Not implemented for this architecture.");
