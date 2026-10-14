//! Prepare or execute an image through the ordinary compiler driver.

#![feature(rustc_private)]
extern crate rustc_driver;

fn main() -> std::process::ExitCode {
    let mut argv = std::env::args().skip(1);
    let mode = argv.next().expect("prepare or run");
    let path = argv.next().expect("compiler argument JSON path");
    let args = serde_json::from_slice(&std::fs::read(path).expect("read compiler arguments"))
        .expect("decode compiler arguments");
    match mode.as_str() {
        "prepare" => mirvm::cli::prepare_runtime_image(args),
        "run" => mirvm::cli::run_runtime_image(args, argv.collect()),
        _ => panic!("expected prepare or run"),
    }
}
