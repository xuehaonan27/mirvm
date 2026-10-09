//! mirvm — Rust runtime with its own execution engine.
//! Architecture and design decisions are in docs/designs/ (README.md is the entry point). The engine is the library (D10); the CLI is just a thin shell.

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

/// The bound a test gives a child process before it calls the child a hang.
///
/// A child builds one Engine per scenario, and building one publishes a private code image through
/// this platform's loader: measured on a macos aarch64 host, each new image costs 0.2-0.5 s to load,
/// because the kernel validates it through the security daemon, against well under a millisecond
/// for the same load on Linux. A child that runs four Engines therefore takes seconds there, and in
/// a parallel suite it competes with the tests around it. This bounds a hang, not a latency: a wait
/// whose subject is another thread or process has a cost that moves with the host's load, so it is
/// sized past the slow platform's cost for work that finished, and every such wait in the suite
/// uses this one bound instead of a constant of its own.
#[cfg(test)]
pub(crate) const CHILD_HANG_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

pub(crate) mod arch;
pub mod cargo_shim;
pub(crate) mod cargoless;
pub mod cli;
pub mod depinfo;
pub(crate) mod diag;
pub mod error;
pub mod image;
pub mod lower;
pub(crate) mod native;
pub mod options;
pub(crate) mod os;
pub(crate) mod os_arch;
pub(crate) mod out;
pub mod pack;
pub(crate) mod store;
pub mod sysroot;
pub mod telemetry;
pub mod utils;
pub mod vm;
