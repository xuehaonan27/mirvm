//! Compiler worker with the same command and diagnostic contracts as the full CLI.

#![feature(rustc_private)]
extern crate rustc_driver;

fn main() -> std::process::ExitCode {
    mirvm::cli::main()
}
