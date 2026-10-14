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
    // Source and pinned compiler facts identify products shared by the compiler and runtime.
    let mut id: u64 = 0xcbf2_9ce4_8422_2325;
    hash_tree(&mut id, Path::new("src"));
    for extra in ["Cargo.lock", "build.rs"] {
        fnv1a(&mut id, &std::fs::read(extra).unwrap_or_default());
    }
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
    assert!(out.status.success(), "rustc --print sysroot failed");
    assert!(!sysroot.is_empty(), "empty sysroot");

    // Query the same pinned executable used for host dependencies. Cargo's own cfg environment
    // includes its build flags; dependency platform matching needs the compiler's baseline set.
    let target = env::var("TARGET").unwrap();
    let pinned_rustc = Path::new(&sysroot).join("bin/rustc");
    let version = Command::new(&pinned_rustc)
        .arg("-vV")
        .output()
        .expect("failed to run pinned rustc -vV");
    assert!(version.status.success(), "pinned rustc -vV failed");
    let version_text = String::from_utf8(version.stdout).expect("rustc version is not UTF-8");
    let version = rustc_version::version_meta_for(&version_text)
        .expect("invalid pinned rustc version")
        .semver;
    println!(
        "cargo:rustc-env=MIRVM_RUSTC_VERSION={}.{}.{}",
        version.major, version.minor, version.patch
    );
    let cfg = Command::new(&pinned_rustc)
        .args(["--print", "cfg", "--target", &target])
        .output()
        .expect("failed to run pinned rustc --print cfg");
    assert!(cfg.status.success(), "pinned rustc --print cfg failed");
    let cfg = String::from_utf8(cfg.stdout).expect("rustc cfg is not UTF-8");
    assert!(!cfg.trim().is_empty(), "empty rustc cfg");
    std::fs::write(
        Path::new(&env::var_os("OUT_DIR").unwrap()).join("host-cfg.txt"),
        &cfg,
    )
    .expect("failed to write host cfg");
    for fact in [&target, &version_text, &cfg] {
        fnv1a(&mut id, fact.as_bytes());
        fnv1a(&mut id, &[0]);
    }
    println!("cargo:rerun-if-changed={}", pinned_rustc.display());
    println!("cargo:rustc-env=MIRVM_BUILD_ID={id:016x}");
    println!("cargo:rustc-env=MIRVM_DEFAULT_SYSROOT={sysroot}");
    println!("cargo:rustc-env=MIRVM_HOST={target}");
    if env::var_os("CARGO_FEATURE_COMPILER").is_some() {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{sysroot}/lib");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
