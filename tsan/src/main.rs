//! Spike 4 的 TSan harness：同源复用 `src/vm`（纯 Rust，零 rustc_private），
//! 在全量插桩（-Zsanitizer=thread + -Zbuild-std）下跑并发用例。
//! 判定：退出码 0 且无 "WARNING: ThreadSanitizer"（见 `runtime.tsan`）。
#![allow(dead_code)] // spike1-3 一并编入但只跑 spike4
#![feature(cfg_sanitize)] // engine/ctx.rs：TSan 配置下 Ctx dtor 的处置分歧
#![feature(f16)] // engine D8c：f16/f128 宿主直算（同源复用 src/vm 必须同 feature 集）
#![feature(f128)]
#![feature(core_intrinsics)] // engine raw unwind catch 同源编译
#![feature(rustc_attrs)] // engine raw catch callback 的 nounwind 契约
#![feature(thread_local)] // engine deferred signal mailbox 使用无析构原生 TLS
#![allow(internal_features)]

#[path = "../../src/arch/mod.rs"]
mod arch; // arch 层（interp 的 x86 触点经 crate::arch::；同上纪律）
#[path = "../../src/utils/logs.rs"]
mod logs; // os::process 的 mirvm_log! 同源依赖
#[path = "../../src/os/mod.rs"]
mod os; // P7 os 层（engine 触点经 crate::os:: 原语；同源复用门禁随之扩展）
mod product_adapters;
pub(crate) use product_adapters::{lower, sysroot};
#[path = "../../src/elfsym.rs"]
mod elfsym; // ffi.rs 的归档 .symtab 兜底（纯 Rust，同源复用）
mod telemetry; // capture/format 同源编译；另跑一条真实 arm session 生命周期
#[path = "../../src/vm/mod.rs"]
mod vm;

fn main() -> std::process::ExitCode {
    // spike4（冻结工件）+ M4 引擎多线程真身（M4.4：共享 Shared/每线程 Ctx/thunk 工厂）
    if vm::spikes::spike4::run_cases()
        && vm::engine::tsan_mt::run()
        && telemetry::run_capture_lifecycle_case()
    {
        println!("tsan-harness: 用例全 PASS（竞争判定看 TSan 输出）");
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::from(1)
    }
}
