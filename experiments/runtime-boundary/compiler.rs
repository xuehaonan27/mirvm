//! Prepare an image through the ordinary driver, finishing diagnostics before the runner enters it.

#![feature(rustc_private)]
extern crate rustc_driver;

fn main() -> std::process::ExitCode {
    let args = std::env::args()
        .nth(1)
        .expect("compiler argument JSON path");
    let args = serde_json::from_slice(&std::fs::read(args).expect("read compiler arguments"))
        .expect("decode compiler arguments");
    mirvm::cli::prepare_runtime_image(args)
}
