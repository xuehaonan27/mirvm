//! mirvm — Rust runtime with its own execution engine.
//! 架构与决策见 DESIGN.md。engine 是 library（D10），CLI 只是薄壳。

#![feature(rustc_private)]
#![feature(box_patterns)] // lower 匹配 MIR 的 Box 字段
#![feature(cfg_sanitize)] // ctx.rs：TSan 配置下 Ctx dtor 的处置分歧
#![feature(f16)] // D8c：引擎宿主直算 f16（rustc 下降到与 native 同一批转换/libm 符号）
#![feature(f128)] // D8c：同上，f128（compiler-builtins __*tf* + glibc *f128 libm）

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

pub mod baseimage;
pub mod cachectl;
pub mod cargo_shim;
pub mod cli;
pub mod depsimage;
pub(crate) mod elfsym;
pub mod ircache;
pub mod lower;
pub(crate) mod native_archive;
pub mod sysroot;
pub mod vm;
