//! `cargoless/` —— D15「砍 cargo」：自有依赖解析与编译调度
//! （[designs/d15-cargoless-design.md](../../docs/designs/d15-cargoless-design.md)）。
//!
//! 分期（每期闭合契约见设计档 §5）：
//! - P1：解析地基——`manifest`（Cargo.toml 模型）、`lockfile`
//!   （Cargo.lock 读写）、`registry`（自有 store + 读穿 + sparse index +
//!   .crate 解包）、`resolve`（版本求解 + feature 统一 → 编译单元图）
//!   + 审计工具（`audit`）。
//! - P2（已闭合）：`schedule`（拓扑 + 指纹 + 每 crate rustc 参数）与
//!   `driver`（`mirvm run` 新路径，MIRVM_DEPS=self 启用）切①②③
//!   （proc-macro 真 rustc host 编译；`buildrs` = build.rs 全生命周期——
//!   host 编译 → cargo 兼容 env 执行 → 指令解析 → cfg/env/OUT_DIR/link/
//!   DEP_* 传播）。
//! - P3 切⑤a：`rustflags`（RUSTFLAGS/CARGO_ENCODED_RUSTFLAGS/config
//!   rustflags 子集——实证只落 target 单元，host 侧不吃）+ 根包 lib
//!   target 编译（[lib]+[[bin]] 双 target，hexyl 实锤）。
//!   剩余拒绝面 = P1 的 P5 边界（git 源/alt registry/workspace 多包图等）
//!   与 P3 余量（rerun-if 精细增量/双轨 gate/并行调度）；机制全 → 迁移 →
//!   退场见设计档。

pub mod audit;
pub mod buildrs;
pub mod driver;
pub mod lockfile;
pub mod manifest;
pub mod registry;
pub mod resolve;
pub mod rustflags;
pub mod schedule;

#[cfg(test)]
mod real_probe;
