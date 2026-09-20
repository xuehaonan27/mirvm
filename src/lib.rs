//! mirvm — Rust runtime with its own execution engine.
//! Architecture and design decisions are in DESIGN.md. The engine is the library (D10); the CLI is just a thin shell.

#![feature(rustc_private)]
#![feature(box_patterns)] // lower matches MIR Box fields
#![feature(cfg_sanitize)] // ctx.rs: divergence in Ctx dtor handling under TSan config
#![feature(f16)]
// D8c: engine host computes f16 directly (rustc lowers to the same conversion/libm symbols as native)
#![feature(f128)] // D8c: same as above, f128 (compiler-builtins __*tf* + glibc *f128 libm)
#![feature(core_intrinsics)] // raw unwind capture; distinguish owner by exception class
#![feature(rustc_attrs)] // raw unwind catch callback must guarantee no unwinding
#![feature(thread_local)] // destructor-less native ELF TLS pointer for async signal mailbox
#![allow(internal_features)] // core_intrinsics only used for the above engine boundary

extern crate rustc_abi;
extern crate rustc_apfloat;
extern crate rustc_ast;
extern crate rustc_attr_parsing;
extern crate rustc_codegen_ssa;
extern crate rustc_const_eval;
extern crate rustc_data_structures;
extern crate rustc_driver;
extern crate rustc_errors;
extern crate rustc_hir;
extern crate rustc_interface;
extern crate rustc_middle;
extern crate rustc_session;
extern crate rustc_span;
extern crate rustc_symbol_mangling;
extern crate rustc_target;

pub(crate) mod arch;
pub mod cargo_shim;
pub(crate) mod cargoless;
pub mod cli;
pub mod image;
pub mod inputs;
pub mod lower;
pub(crate) mod native;
pub mod options;
pub(crate) mod os;
pub mod pack;
pub(crate) mod store;
pub mod sysroot;
pub mod telemetry;
pub mod utils;
pub mod vm;
