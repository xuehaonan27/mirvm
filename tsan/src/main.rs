//! TSan harness for the `vm::spikes::spike4` cases: reuses `src/vm`
//! source-for-source (pure Rust, no rustc_private) and runs the concurrency cases
//! under full instrumentation (`-Zsanitizer=thread -Zbuild-std`).
//! Passes when the exit code is 0 and TSan prints no "WARNING: ThreadSanitizer"
//! (see the `runtime.tsan` suite).
#![allow(dead_code)] // the earlier spikes are compiled in but only spike4 runs
#![feature(cfg_sanitize)] // engine/ctx.rs: Ctx dtor disposition differs under TSan
#![feature(f16)] // engine: f16/f128 host arithmetic (sharing src/vm needs the same feature set)
#![feature(f128)]
#![feature(core_intrinsics)] // engine raw unwind catch is compiled source-for-source
#![feature(rustc_attrs)] // nounwind contract of the engine's raw catch callback
#![feature(thread_local)] // the engine's deferred signal mailbox uses destructor-free native TLS
#![allow(internal_features)]

#[path = "../../src/arch/mod.rs"]
mod arch; // arch layer (interp's x86 touch points go through crate::arch::)
#[path = "../../src/utils/logs.rs"]
mod logs; // source-shared dependency of os::process's mirvm_log!
#[path = "../../src/os/mod.rs"]
mod os; // os layer (engine touch points go through crate::os:: primitives)
mod product_adapters;
pub(crate) use product_adapters::{lower, sysroot};
#[path = "../../src/elfsym.rs"]
mod elfsym; // archive .symtab fallback for ffi.rs (pure Rust, source-shared)
mod telemetry; // capture/format source-shared; also runs one real arm session lifecycle
#[path = "../../src/vm/mod.rs"]
mod vm;

fn main() -> std::process::ExitCode {
    // spike4 cases plus the real multithreaded engine (shared Shared, per-thread Ctx/thunk factory)
    if vm::spikes::spike4::run_cases()
        && vm::engine::tsan_mt::run()
        && telemetry::run_capture_lifecycle_case()
    {
        println!("tsan-harness: all cases PASS (read TSan output for race verdicts)");
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::from(1)
    }
}
