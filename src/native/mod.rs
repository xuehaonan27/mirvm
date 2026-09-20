//! Host native objects: turning a constrained static archive into a dlopen-able `.so`
//! ([`archive`]), and reading the symbol tables of the objects mirvm produces ([`symtab`]).
//!
//! The two belong together because the converter is the only producer of the objects the reader
//! parses: an archive built with `-fvisibility=hidden` is converted with `-shared --whole-archive`,
//! which localizes its symbols out of `.dynsym`, so the reader is what makes them callable again.
//! `vm` reads the same tables at load time; both go through this module rather than through the
//! documents that describe them.

pub(crate) mod archive;
pub(crate) mod symtab;

#[cfg(test)]
mod tests;
