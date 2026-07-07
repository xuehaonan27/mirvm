//! Spike 4 的 TSan harness：同源复用 `src/vm`（纯 Rust，零 rustc_private），
//! 在全量插桩（-Zsanitizer=thread + -Zbuild-std）下跑并发用例。
//! 判定：退出码 0 且无 "WARNING: ThreadSanitizer"（见 tests/spike4_tsan.sh）。
#![allow(dead_code)] // spike1-3 一并编入但只跑 spike4

#[path = "../../src/vm/mod.rs"]
mod vm;

fn main() -> std::process::ExitCode {
    if vm::spike4::run_cases() {
        println!("tsan-harness: 用例全 PASS（竞争判定看 TSan 输出）");
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::from(1)
    }
}
