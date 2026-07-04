// bin 必须自带 rustc_private + extern rustc_driver，
// 使 std 统一走 sysroot 的动态库（否则与 lib 的链接形态冲突）。
#![feature(rustc_private)]
extern crate rustc_driver;

fn main() -> std::process::ExitCode {
    mirvm::cli::main()
}
