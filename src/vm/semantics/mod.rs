//! What a guest operation computes, defined once for the two execution backends.
//!
//! The interpreter and the JIT have to agree bit for bit, and they can only do so if the
//! definition of an operation has one home. Where the operation is a loop or a piece of
//! intricate bit handling, both backends call the body here ([`simd`], [`volatile`], [`wide`], the
//! [`builtin`] families); where the host instruction is the answer, the translator emits it and
//! the identity in [`arith`] is the specification it mirrors.
//!
//! Nothing here chooses a backend or dispatches on one: these bodies are reached with raw
//! slots, real addresses and already-selected widths, and they never ask who called.
//!
//! `Ctx` appears in a signature only where the operation has per-thread state to reach
//! (guest TLS, the unwinder); the pure operations take values and addresses.

pub(crate) mod arith;
pub(crate) mod builtin;
pub(crate) mod memory;
pub(crate) mod simd;
pub(crate) mod tls;
pub(crate) mod volatile;
pub(crate) mod wide;

#[cfg(test)]
mod tests;
