//! TSan harness for the execution phase.
//!
//! The product binary links `rustc_private` dynamic libraries and installs its own allocator,
//! so it cannot be instrumented as a whole. `src/vm` is deliberately pure Rust with no
//! `rustc` types, so this crate compiles it **source-for-source** (`#[path]`) under
//! `-Zsanitizer=thread -Zbuild-std` and runs the engine's concurrency cases for real.
//!
//! Two jobs, both structural:
//!
//! 1. **Race verdict.** Exit 0 with zero `WARNING: ThreadSanitizer`. The cases keep guest
//!    memory race-free by construction, so any report is an engine bug, not a test artifact.
//!    This is the only check in the repository that can see a data race at all: every other
//!    gate compares stdout/stderr/exit codes, which is structurally blind to them.
//! 2. **Purity fence.** Building this crate is what keeps `src/vm` free of `rustc_private`:
//!    only `blake3`/`libc`/`libffi`/`libmimalloc-sys`/`serde`/`postcard`/`memmap2` are in
//!    scope here, so one `rustc` type leaking into the execution phase fails this build.
//!    `cargo check --all-features` can never catch that, because the root crate has
//!    `rustc_private` by design.
//!
//! Run it with `bash tests/suites/runtime/tsan.sh` (`runtime.semantics` drives it in the
//! `smoke` and `gate` profiles); see `README.md` for what the cases cover and what they do not.

// Engine modules the cases do not drive are still compiled in through the shared `src/vm`, and
// so are product re-exports only the product binary uses; neither is a defect here.
#![allow(dead_code)]
#![allow(unused_imports)]
// The shared `src/vm` gates its JIT half on `feature = "cranelift"`, which this crate does not
// provide: the JIT is outside the TSan net today (README, "What this does not cover").
#![allow(unexpected_cfgs)]
#![feature(cfg_sanitize)] // engine/ctx: Ctx dtor disposition differs under TSan
#![feature(f16)] // engine: f16/f128 host arithmetic (sharing src/vm needs the same feature set)
#![feature(f128)]
#![feature(core_intrinsics)] // engine raw unwind catch is compiled source-for-source
#![feature(rustc_attrs)] // nounwind contract of the engine's raw catch callback
#![feature(thread_local)] // the engine's deferred signal mailbox uses destructor-free native TLS
#![allow(internal_features)]

// ---- adapters: the product leaves `src/vm` expects at these crate paths ----

#[path = "../../../src/arch/mod.rs"]
mod arch; // arch layer (interp's x86 touch points go through crate::arch::)
#[path = "../../../src/utils/logs.rs"]
mod logs; // source-shared dependency of os::process's mirvm_log!
#[path = "../../../src/os/mod.rs"]
mod os; // os layer (engine touch points go through crate::os:: primitives)
mod product_adapters; // stubs for the rustc-dependent leaves (lower::asm, sysroot)
pub(crate) use product_adapters::{lower, sysroot};
#[path = "../../../src/elfsym.rs"]
mod elfsym; // archive .symtab fallback for ffi.rs (pure Rust, source-shared)
mod telemetry; // capture/format source-shared (name fixed: src/vm says crate::telemetry)
#[path = "../../../src/vm/mod.rs"]
mod vm; // the execution phase under test

// ---- cases ----

mod cases;

fn main() -> std::process::ExitCode {
    // Optional single-case filter: `mirvm-tsan <case-id>`.
    let only = std::env::args().nth(1);
    if cases::run_all(only.as_deref()) {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::from(1)
    }
}
