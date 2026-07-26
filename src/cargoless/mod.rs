//! `cargoless/` —— D15「砍 cargo」：自有依赖解析与编译调度
//! （[designs/d15-cargoless-design.md](../../docs/designs/d15-cargoless-design.md)）。
//!
//! 分期（每期闭合契约见设计档 §5）：
//! - P1（本期）：解析地基——`manifest`（Cargo.toml 模型）、`lockfile`
//!   （Cargo.lock 读写）、`registry`（自有 store + 读穿 + sparse index +
//!   .crate 解包）、`resolve`（版本求解 + feature 统一 → 编译单元图）
//!   + 审计工具；不接 run 路径。
//! - P2+：schedule/buildrs/proc_macro/driver（机制全 → 迁移 → 退场）。

pub mod audit;
pub mod lockfile;
pub mod manifest;
pub mod registry;
pub mod resolve;
