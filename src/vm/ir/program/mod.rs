//! Instruction families and module container.
//!
//! Owns the typed instructions (`Rvalue`, `Stmt`, `Terminator`), the functions and blocks they
//! form, and the `Module` tables that carry a linked artifact: frozen-area fixups, entry stubs,
//! TLS slots, and the eager/lazy function table. The operand, width, and operation vocabulary
//! these are written in stays in the parent module.
//!
//! The four are separate concerns and separate files: [`instr`] is what a guest body is made of,
//! [`ffi`] is the signature a foreign call is made under, [`funcs`] is the function table and its
//! lazy decode, and [`module`] is the artifact those are assembled into.

use super::{
    AsmIoDst, AsmIoVal, AsmSite, AsmStubId, Bb, FuncId, LinkAddr, LoadMap, Operand, PlaceExpr,
    ScalarPlace, Slot, Stmt, UnwindAction,
};

pub use ffi::*;
pub use funcs::*;
pub use instr::*;
pub use module::*;

mod ffi;
mod funcs;
mod instr;
mod module;

/// Owned name of an unsupported builtin. Deserialization cannot produce a `&'static str`, so the name
/// is owned and released together with the Module.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct StaticStr(pub Box<str>);

/// Guest TLS slot id: a dense index over `#[thread_local]` statics.
pub type TlsId = u32;
