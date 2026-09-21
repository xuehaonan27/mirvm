//! The object-file formats mirvm reads and writes.
//!
//! This is a data format, not a platform axis: the byte layout of an ELF64 image does not change
//! with the kernel or the CPU, and nothing here is conditional. Keeping it out of `src/os` is what
//! keeps that axis honest — `src/os` holds what changes when the platform does, and a byte layout
//! does not.
//!
//! What *is* platform-dependent about an image lives on the axis it belongs to:
//!
//! - Which machine an image is for is the CPU's: `arch::ELF_MACHINE`, and the byte a never-executed
//!   `.text` slot is filled with (`asmstub::RET`).
//! - Which format a host's loader accepts, and everything the loader does with it (an in-memory
//!   file, `/proc/self/fd`, `dlopen`, self-mapping), is the platform's: `os::linux` and the engine
//!   that calls it. A host whose loader reads a different format puts that format beside `elf` here
//!   and the choice in `os`, so a consumer keeps naming one path.
//!
//! The archive container is here for the same reason: `ar` is a byte layout too, and the linker that
//! consumes it is not this module's business.

pub mod ar;
pub mod elf;
