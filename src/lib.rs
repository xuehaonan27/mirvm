//! mirvm — Rust runtime with its own execution engine.
//! 架构与决策见 DESIGN.md。engine 是 library（D10），CLI 只是薄壳。

#![feature(rustc_private)]
#![feature(yeet_expr)] // rustc_middle 的 throw_* 宏需要
#![feature(map_try_insert)]

extern crate rustc_abi;
extern crate rustc_ast;
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

pub mod cargo_shim;
pub mod cli;
pub mod interp;
pub mod sysroot;
