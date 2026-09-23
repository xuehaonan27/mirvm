//! Host native objects: turning a constrained static archive into a dlopen-able `.so`
//! ([`archive`]), building the symbol-only object a backtrace is named by ([`symimage`]),
//! reading the symbol tables of the objects mirvm produces ([`symtab`]), and the two byte layouts
//! both of them work on ([`elf`], [`ar`]).
//!
//! The converter and the reader belong together because the converter is the only producer of the
//! objects the reader parses: an archive built with `-fvisibility=hidden` is converted with
//! `-shared --whole-archive`, which localizes its symbols out of `.dynsym`, so the reader is what
//! makes them callable again. `vm` reads the same tables at load time; both go through this module
//! rather than through the documents that describe them.
//!
//! The two layouts are here for that reason rather than for a platform one: a byte layout does not
//! change with the kernel or the CPU, so it belongs to the layer that produces and parses these
//! objects, not to `os`. What *is* platform-dependent about an image lives on the axis it belongs
//! to — which machine it is for is `arch::ELF_MACHINE`, and which format a host's loader accepts,
//! plus everything the loader does with it (an in-memory file, `/proc/self/fd`, `dlopen`,
//! self-mapping), is `os` and its callers.

pub(crate) mod ar;
pub(crate) mod archive;
pub(crate) mod elf;
pub(crate) mod symimage;
pub(crate) mod symtab;

#[cfg(test)]
mod tests;
