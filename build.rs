use std::env;
use std::process::Command;

fn main() {
    // 用构建时的 toolchain 解析 sysroot：
    // 1) 烘焙进二进制，运行期作为默认 --sysroot（rustc 前端需要它找到 core/std）
    // 2) rpath 指向 sysroot/lib，让二进制免 LD_LIBRARY_PATH 找到 librustc_driver.so
    let rustc = env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let out = Command::new(rustc)
        .args(["--print", "sysroot"])
        .output()
        .expect("failed to run `rustc --print sysroot`");
    let sysroot = String::from_utf8(out.stdout).unwrap().trim().to_string();
    assert!(!sysroot.is_empty(), "empty sysroot");

    println!("cargo:rustc-env=MIRVM_DEFAULT_SYSROOT={sysroot}");
    println!("cargo:rustc-link-arg=-Wl,-rpath,{sysroot}/lib");
    println!("cargo:rerun-if-changed=build.rs");
}
