use std::path::PathBuf;
use std::process::Command;

fn run(command: &mut Command, label: &str) {
    let status = command
        .status()
        .unwrap_or_else(|e| panic!("cannot start {label}: {e}"));
    assert!(status.success(), "{label} failed with {status}");
}

fn main() {
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let source = manifest.join("native/probe.cpp");
    let object = out.join("probe.o");
    let archive = out.join("libc_unwind_probe.a");
    let cxx = std::env::var_os("CXX").unwrap_or_else(|| "c++".into());
    let ar = std::env::var_os("AR").unwrap_or_else(|| "ar".into());

    run(
        Command::new(cxx)
            .arg("-std=c++17")
            .arg("-fPIC")
            .arg("-fexceptions")
            .arg("-c")
            .arg(&source)
            .arg("-o")
            .arg(&object),
        "C++ fixture compilation",
    );
    run(
        Command::new(ar)
            .arg("crs")
            .arg(&archive)
            .arg(&object),
        "C++ fixture archive",
    );

    println!("cargo::rerun-if-changed={}", source.display());
    println!("cargo::rerun-if-env-changed=CXX");
    println!("cargo::rerun-if-env-changed=AR");
    println!("cargo::rustc-link-search=native={}", out.display());
    println!("cargo::rustc-link-lib=static=c_unwind_probe");
    println!("cargo::rustc-link-lib=dylib=stdc++");
}

