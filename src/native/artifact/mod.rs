//! The artifacts mirvm builds for, or with, the host toolchain.
//!
//! This is the half of the native layer that runs `cc`: [`archive`] turns a constrained static
//! archive into something the loader can map, [`bridge`] turns the runtime-interposition bridge into
//! the file a link line names, and [`asmtext`] is the assembler source vocabulary both of them and
//! the materializer in `src/lower/` emit for a format. The flags a platform's linker needs are
//! `crate::os::linker`'s, and the formats themselves are [`crate::native::object`]'s.

pub(crate) mod archive;
pub(crate) mod asmtext;
pub(crate) mod bridge;
