//! Spike 4 的 TSan harness：同源复用 `src/vm`（纯 Rust，零 rustc_private），
//! 在全量插桩（-Zsanitizer=thread + -Zbuild-std）下跑并发用例。
//! 判定：退出码 0 且无 "WARNING: ThreadSanitizer"（见 tests/spike4_tsan.sh）。
#![allow(dead_code)] // spike1-3 一并编入但只跑 spike4
#![feature(cfg_sanitize)] // engine/ctx.rs：TSan 配置下 Ctx dtor 的处置分歧
#![feature(f16)] // engine D8c：f16/f128 宿主直算（同源复用 src/vm 必须同 feature 集）
#![feature(f128)]

#[path = "../../src/utils/logs.rs"]
mod logs; // os::process 的 mirvm_log! 同源依赖
#[path = "../../src/os/mod.rs"]
mod os; // P7 os 层（engine 触点经 crate::os:: 原语；同源复用门禁随之扩展）
#[path = "../../src/arch/mod.rs"]
mod arch; // arch 层（interp 的 x86 触点经 crate::arch::；同上纪律）
#[path = "../../src/vm/mod.rs"]
mod vm;
#[path = "../../src/elfsym.rs"]
mod elfsym; // ffi.rs 的归档 .symtab 兜底（纯 Rust，同源复用）

fn main() -> std::process::ExitCode {
    // spike4（冻结工件）+ M4 引擎多线程真身（M4.4：共享 Shared/每线程 Ctx/thunk 工厂）
    if vm::spikes::spike4::run_cases() && vm::engine::tsan_mt::run() {
        println!("tsan-harness: 用例全 PASS（竞争判定看 TSan 输出）");
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::from(1)
    }
}
