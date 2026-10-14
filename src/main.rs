// Compiler-enabled binaries use the same dynamic standard library as rustc_driver.
#![cfg_attr(feature = "compiler", feature(rustc_private))]
#[cfg(feature = "compiler")]
extern crate rustc_driver;

fn main() -> std::process::ExitCode {
    mirvm::cli::main()
}
