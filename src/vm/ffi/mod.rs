//! The C ABI in both directions.
//!
//! Outbound ([`outbound`]) is a guest call into native code: resolve the symbol, then call it
//! through libffi under the signature that was frozen at the call site. Inbound ([`inbound`]) is
//! a native call into a guest function: convert the C shape of the arguments and of the result
//! into calling convention v2 and back.
//!
//! The two directions share the signature vocabulary ([`crate::vm::ir::ForeignSig`]) and one
//! simplification -- a guest pointer *is* a host pointer -- which is what keeps both of them free
//! of marshalling beyond the shape translation itself. The per-thread resolution cache
//! ([`FfiState`]) belongs to the outbound half.

pub(crate) mod inbound;
mod outbound;

/// The libffi type of one argument or result kind, which the thunk factory also builds CIFs from.
pub(crate) use outbound::ffi_type;
pub(crate) use outbound::resolve_got_fixups;
pub use outbound::{FfiState, amplify_pthread_stack, call_addr};
