//! `cargoless/` -- dependency resolution and compilation scheduling without cargo: the
//! `manifest`/`lockfile` models, the registry store (read-through cache, sparse index, `.crate`
//! unpacking), `resolve` (dual-mode resolution and feature unification into the compilation-unit
//! graph), `audit`, `schedule` (topology, content fingerprints, per-crate rustc arguments) and
//! `driver` (`mirvm run` with no cargo in the process). `buildrs`, `rustflags`, `vendor`,
//! `workspace` and the Cargo-config dependency sources cover the remaining Cargo contracts.

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
