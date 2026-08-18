//! mirvm — Rust runtime with its own execution engine.
//! 架构与决策见 DESIGN.md。engine 是 library（D10），CLI 只是薄壳。

#![feature(rustc_private)]
#![feature(box_patterns)] // lower 匹配 MIR 的 Box 字段
#![feature(cfg_sanitize)] // ctx.rs：TSan 配置下 Ctx dtor 的处置分歧
#![feature(f16)] // D8c：引擎宿主直算 f16（rustc 下降到与 native 同一批转换/libm 符号）
#![feature(f128)] // D8c：同上，f128（compiler-builtins __*tf* + glibc *f128 libm）
#![feature(core_intrinsics)] // 原始 unwind 捕获，按 exception class 区分所有者
#![feature(rustc_attrs)] // raw unwind catch callback 必须保证不展开
#![feature(thread_local)] // async signal mailbox 的无析构原生 ELF TLS 指针
#![allow(internal_features)] // core_intrinsics 仅用于上述引擎边界

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
pub mod baseimage;
pub mod cachectl;
pub mod cargo_shim;
pub(crate) mod cargoless;
pub mod cli;
pub mod depsimage;
pub(crate) mod elfsym;
pub mod ircache;
pub mod lower;
pub(crate) mod native_archive;
pub(crate) mod os;
pub mod pack;
pub mod sysroot;
pub mod telemetry;
pub mod utils;
pub mod vm;
