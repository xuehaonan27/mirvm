use std::env;
use std::path::Path;
use std::process::Command;

/// FNV-1a（与 src/lower/asm.rs 同参；build.rs 独立编译单元，无法复用）。
fn fnv1a(h: &mut u64, bytes: &[u8]) {
    for b in bytes {
        *h ^= *b as u64;
        *h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
}

/// 按路径序稳定遍历 dir 下全部文件，哈希（路径+内容）。
fn hash_tree(h: &mut u64, dir: &Path) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read_dir {} 失败: {e}", dir.display()))
        .map(|e| e.expect("read_dir entry").path())
        .collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            hash_tree(h, &p);
        } else {
            fnv1a(h, p.display().to_string().as_bytes());
            fnv1a(
                h,
                &std::fs::read(&p).unwrap_or_else(|e| panic!("读 {} 失败: {e}", p.display())),
            );
        }
    }
}

fn main() {
    // MIRVM_BUILD_ID（M6 片2，D9c）：src 树 + Cargo.lock + build.rs 内容哈希。
    // 任何引擎/lower 源变更 ⇒ id 变 ⇒ L2 IR 缓存整体失配重建（脆性无害，正确性优先）。
    let mut id: u64 = 0xcbf2_9ce4_8422_2325;
    hash_tree(&mut id, Path::new("src"));
    for extra in ["Cargo.lock", "build.rs"] {
        fnv1a(&mut id, &std::fs::read(extra).unwrap_or_default());
    }
    println!("cargo:rustc-env=MIRVM_BUILD_ID={id:016x}");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=Cargo.lock");
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
    println!("cargo:rustc-env=MIRVM_HOST={}", env::var("TARGET").unwrap());
    println!("cargo:rustc-link-arg=-Wl,-rpath,{sysroot}/lib");
    println!("cargo:rerun-if-changed=build.rs");
}
