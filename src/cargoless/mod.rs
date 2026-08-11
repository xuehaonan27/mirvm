//! `cargoless/` —— D15「砍 cargo」：自有依赖解析与编译调度
//! （[designs/d15-cargoless-design.md](../../docs/designs/d15-cargoless-design.md)）。
//!
//! 分期（每期闭合契约见设计档 §5；P1–P3 已收口，验收与语义实证清单见
//! decision-history §7.28/§7.29/§7.30）：
//! - P1（已收）：解析地基——`manifest`（Cargo.toml 模型 + cfg 平台求值 +
//!   frontmatter 伪包）、`lockfile`（v1–v4 读写 + canonical v3/v4）、
//!   `registry`（自有 store + 读穿 + sparse index + .crate 解包）、
//!   `resolve`（双模式求解 + feature 统一 → 编译单元图）、`audit`。
//! - P2（已收）：`schedule`（拓扑 + 内容指纹 + 每 crate rustc 参数 +
//!   proc-macro/host 双侧编译形态）、`driver`（`mirvm run` 零 cargo 新
//!   路径，MIRVM_DEPS=self 启用）、`buildrs`（build.rs 全生命周期：
//!   host 编译 → cargo 兼容 env 执行 → 指令解析 → 传播）。
//! - P3（已收）：`rustflags`（env/config 子集，实证只落 target 单元）、
//!   buildrs rerun-if 精细增量（registry 源不可变一次跑）、
//!   `schedule::run_scheduler`（Kahn 就绪队列并行调度）；full 层迁移与
//!   双轨 gate 验收闭合（corpus_deps_pair full 138 pass 1 p5 0 fail）。
//! - P4（已收）：`vendor`（vendored-dir PkgSource，sysroot 自管与未来的
//!   source replacement 共用）+ sysroot 构建换 cargoless 调度（零 cargo
//!   零 crates.io）+ `--bin` 多目标选择 + **MIRVM_DEPS 默认翻 self**
//!   （=cargo 显式 compat，双轨各自完整）。`workspace` + `mirvm test` 已接
//!   resolver=1/2/3 常见多包形态与 rust-version-aware 选择。后续来源批次已
//!   接入 Cargo config 依赖子集、替代 registry、registry/local/directory
//!   source replacement、patch/replace；`pack` 缺省复用同一自有调度并保留
//!   `MIRVM_DEPS=cargo` 回退。resolver 1/2/3 与 `mirvm test` 的剩余 Cargo
//!   合同也复用这一实现。

pub mod audit;
pub mod buildrs;
pub mod config;
pub mod driver;
pub mod git;
pub mod lockfile;
pub mod manifest;
pub mod registry;
pub mod resolve;
pub mod resolver_config;
pub mod rustflags;
pub mod schedule;
pub mod vendor;
pub mod workspace;

#[cfg(test)]
mod real_probe;
