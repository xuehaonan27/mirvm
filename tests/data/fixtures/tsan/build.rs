fn main() {
    let rustc = std::env::var_os("RUSTC").expect("Cargo provides RUSTC");
    let output = std::process::Command::new(rustc)
        .args(["--print", "sysroot"])
        .output()
        .expect("read the harness toolchain's sysroot");
    assert!(output.status.success());
    let sysroot = String::from_utf8(output.stdout).expect("UTF-8 sysroot path");
    println!("cargo:rustc-env=MIRVM_DEFAULT_SYSROOT={}", sysroot.trim());
    println!(
        "cargo:rustc-env=MIRVM_HOST={}",
        std::env::var("TARGET").unwrap()
    );
    println!("cargo:rustc-env=MIRVM_BUILD_ID=0000000000000000");
    println!("cargo:rerun-if-changed=build.rs");
}
