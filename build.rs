use std::env;
use std::path::Path;
use std::process::Command;

/// FNV-1a (same parameters as src/lower/asm.rs; build.rs is a separate
/// compilation unit and cannot reuse it).
fn fnv1a(h: &mut u64, bytes: &[u8]) {
    for b in bytes {
        *h ^= *b as u64;
        *h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
}

/// Walks every file under dir in stable path order, hashing (path + content).
fn hash_tree(h: &mut u64, dir: &Path) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read_dir {} failed: {e}", dir.display()))
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
                &std::fs::read(&p).unwrap_or_else(|e| panic!("read {} failed: {e}", p.display())),
            );
        }
    }
}

fn main() {
    // MIRVM_BUILD_ID: a hash of the src tree + Cargo.lock + build.rs contents.
    // Any engine/lower source change changes the id, so the whole L2 IR cache
    // misses and is rebuilt (brittle but harmless; correctness first).
    let mut id: u64 = 0xcbf2_9ce4_8422_2325;
    hash_tree(&mut id, Path::new("src"));
    for extra in ["Cargo.lock", "build.rs"] {
        fnv1a(&mut id, &std::fs::read(extra).unwrap_or_default());
    }
    println!("cargo:rustc-env=MIRVM_BUILD_ID={id:016x}");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=Cargo.lock");
    // Resolve the sysroot with the build-time toolchain:
    // 1) bake it into the binary as the runtime default --sysroot (the rustc
    //    frontend needs it to find core/std)
    // 2) set rpath to sysroot/lib so the binary finds librustc_driver.so without
    //    LD_LIBRARY_PATH
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
