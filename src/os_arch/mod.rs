//! The code that is specific to one operating system **and** one CPU at the same time.
//!
//! Most platform knowledge belongs to exactly one axis: an instruction encoding is the CPU's
//! (`src/arch/`), a page size or a `/proc` path is the kernel's (`src/os/`). Some knowledge
//! belongs to neither alone, because the kernel defines the interface and the CPU supplies its
//! encoding. A signal frame is the clearest case: Linux decides that a handler receives a
//! `ucontext_t` and that `rt_sigreturn` restores it, while x86_64 decides that the restorer is
//! `mov rax,15; syscall` and that the fault address sits in `mcontext.gregs[22]`. Neither home
//! can hold that without lying about itself, so the pair gets its own.
//!
//! # Boundary Contract
//!
//! - **Leaf**: no engine types and no `rustc_private`; the types that cross into `os/`, `arch/`
//!   or the engine's platform calls are integers, raw pointers, and the platform's own ABI
//!   structures (`libc::sigaction`, `libc::ucontext_t`). Those structures are the reason this
//!   module exists, so naming them here is the point rather than a leak.
//! - **Primitives, No Adjudication**: what a disposition means, which guest handler may be
//!   installed, and where a stub is mapped all stay with the engine. This layer encodes and
//!   decodes what the kernel and the CPU agreed on.
//!
//! # Pair Selection
//!
//! A pair directory is named `<os>_<arch>` (OpenJDK spells the same idea `os_cpu`, and names it
//! OS-first for the same reason: the kernel is the subject and the CPU the qualifier). The
//! ladder in this file selects the one pair a build is for; `os/mod.rs` and `arch/mod.rs`
//! dispatch on their own axis and reach a pair only through this name, so a call site spells
//! `crate::os_arch::signal::...` on every target.
//!
//! Every pair provides the same subsystem names. A pair that has not implemented a subsystem
//! yet carries its own `compile_error!` in place of the body, so a port's error list is its
//! to-do list.

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod linux_x86_64;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub(crate) use linux_x86_64::*;

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
compile_error!(
    "No os_arch pair is implemented for this target. The implemented pairs are linux_x86_64; \
     adding one means a directory under src/os_arch/ named <os>_<arch> plus an arm in the \
     ladder in this file."
);
