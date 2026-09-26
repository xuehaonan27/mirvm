//! Host native objects: the toolchain's half of mirvm.
//!
//! An object file is what a toolchain writes and what a loader reads back, so this layer owns the
//! two things that follow from that: the byte layouts themselves ([`object`]), and the artifacts
//! mirvm builds with the host toolchain ([`artifact`]). What a caller then does with an object —
//! resolving a symbol name through it, naming a synthetic instruction pointer, taking the loader's
//! hands off an image's constructors — is [`symbol`].
//!
//! The converter and the reader belong together because the converter is the only producer of the
//! objects the reader parses: an archive built with `-fvisibility=hidden` is converted with
//! `-shared --whole-archive`, which localizes its symbols out of `.dynsym`, so the reader is what
//! makes them callable again. `vm` reads the same tables at load time; both go through this module
//! rather than through the documents that describe them.
//!
//! The layouts are here for that reason rather than for a platform one: a byte layout does not
//! change with the kernel or the CPU, so it belongs to the layer that produces and parses these
//! objects, not to `os`. What *is* platform-dependent about an image lives on the axis it belongs
//! to — which machine it is for is `arch::ELF_MACHINE`, and which format a host's loader accepts,
//! plus everything the loader does with it (an in-memory file, `/proc/self/fd`, `dlopen`,
//! self-mapping), is `os` and its callers.

pub(crate) mod artifact;
pub(crate) mod object;
pub(crate) mod symbol;

#[cfg(test)]
mod tests;
