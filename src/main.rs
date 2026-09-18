// The bin must carry rustc_private + extern rustc_driver itself,
// so std uniformly goes through the sysroot dynamic libraries (otherwise the link shape conflicts with lib).
#![feature(rustc_private)]
extern crate rustc_driver;

fn main() -> std::process::ExitCode {
    mirvm::cli::main()
}
