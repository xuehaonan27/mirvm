//! `cargoless/` —— D15「砍 cargo」：自有依赖解析与编译调度
//! （[designs/d15-cargoless-design.md](../../docs/designs/d15-cargoless-design.md)）。
//!
//! 分期（每期闭合契约见设计档 §5）：
//! - P1：解析地基——`manifest`（Cargo.toml 模型）、`lockfile`
//!   （Cargo.lock 读写）、`registry`（自有 store + 读穿 + sparse index +
//!   .crate 解包）、`resolve`（版本求解 + feature 统一 → 编译单元图）
//!   + 审计工具（`audit`）。
//! - P2（施工中）：`schedule`（拓扑 + 指纹 + 每 crate rustc 参数）与
//!   `driver`（`mirvm run` 新路径，MIRVM_DEPS=self 启用）**切① 骨架已接**
//!   （子集 = 无 build.rs / 无 proc-macro，子集外响亮拒绝）；buildrs /
//!   proc_macro 归切②③，机制全 → 迁移 → 退场见设计档。

pub mod audit;
pub mod driver;
pub mod lockfile;
pub mod manifest;
pub mod registry;
pub mod resolve;
pub mod schedule;

#[cfg(test)]
mod real_probe;
