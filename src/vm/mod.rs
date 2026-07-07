//! `vm` — greenfield 模型 A 执行引擎（M4 的地基，与 tier-0 `interp`（rustc InterpCx）并列）。
//!
//! **纯 Rust，不依赖 rustc_private**——这正是"脱离 InterpCx、自研 VM"的体现。
//! 当前只含 Spike 1（最小模型 A 骨架：手写字节码 + slaved 操作数区 + tree-walking
//! interp_frame + 真地址内存）。后续 spike（i2c/c2i、混合栈 unwind、并发）在此扩展。
//! 设计见 docs/frame-abi-bytecode.md、docs/spike1-model-a-skeleton.md。

pub mod bytecode;
pub mod frame;
pub mod interp;
pub mod memory;
pub mod spike1;
