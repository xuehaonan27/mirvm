//! Runtime-boundary probe using the product's real loaders, native materialization and JIT.

#![cfg_attr(feature = "compiler", feature(rustc_private))]
#[cfg(feature = "compiler")]
extern crate rustc_driver;

use std::os::unix::process::CommandExt;
use std::path::Path;

fn run() -> Result<i32, Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("identity") => {
            println!(
                "{} {}",
                mirvm::options::build::BUILD_ID,
                mirvm::options::build::HOST
            );
            Ok(0)
        }
        Some("package") => {
            let path = args.next().ok_or("missing package path")?;
            let guest = std::iter::once(path.clone())
                .chain(args)
                .collect::<Vec<_>>();
            Ok(unsafe { mirvm::runtime::run_package(Path::new(&path), &guest)? })
        }
        Some(mode @ ("image" | "image-exec")) => {
            let worker = args.next().ok_or("missing compiler worker path")?;
            let recipe = args.next().ok_or("missing compiler arguments JSON path")?;
            let compiler_args: Vec<String> = serde_json::from_slice(&std::fs::read(&recipe)?)?;
            let guest = args.collect::<Vec<_>>();
            if let Some(code) = unsafe { mirvm::runtime::run_image(&compiler_args, &guest)? } {
                return Ok(code);
            }
            if mode == "image-exec" {
                // Replacement retains the caller's PID, cwd, environment and signal exit status.
                // This miss executes in the compiler process after its session has finished.
                return Err(std::process::Command::new(worker)
                    .arg("run")
                    .arg(&recipe)
                    .args(&guest)
                    .exec()
                    .into());
            }
            let status = std::process::Command::new(worker).arg("prepare").arg(&recipe).status()?;
            if !status.success() {
                return Ok(status.code().unwrap_or(70));
            }
            unsafe { mirvm::runtime::run_image(&compiler_args, &guest)? }
                .ok_or_else(|| "compiler worker did not publish a current execution image".into())
        }
        _ => Err(
            "usage: runtime-runner package PATH [ARGS...] | image[-exec] WORKER RECIPE GUEST_ARGV..."
                .into(),
        ),
    }
}

fn main() {
    let code = match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("runtime-runner: {error}");
            70
        }
    };
    std::process::exit(code);
}
