//! mirvm — Rust runtime with its own execution engine.
//! 架构与决策见 DESIGN.md。engine 是 library（D10），CLI 只是薄壳。

#![feature(rustc_private)]
#![feature(yeet_expr)] // rustc_middle 的 throw_* 宏需要
#![feature(map_try_insert)]
#![feature(box_patterns)] // lower 匹配 MIR 的 Box 字段
#![feature(cfg_sanitize)] // ctx.rs：TSan 配置下 Ctx dtor 的处置分歧

extern crate rustc_abi;
extern crate rustc_apfloat;
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
pub mod lower;
pub mod sysroot;
pub mod vm;
