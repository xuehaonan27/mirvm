//! The object-file formats the host loader and linker speak.
//!
//! This is format knowledge rather than syscall knowledge: the byte layout of an ELF64 image is
//! the same wherever it is read, which is why nothing here sits behind the `linux/` ladder. What
//! does follow the platform is split off: which machine an image is for and which byte a
//! never-executed slot is filled with are the CPU's (`arch::ELF_MACHINE`, `asmstub::RET`), and the
//! loader-bound half of using an image at all (`memfd_create`, `/proc/self/fd`, `dlopen`,
//! self-mapping) is the kernel's and stays in `os/linux/` and in the engine that calls it.

pub mod ar;
pub mod elf;
