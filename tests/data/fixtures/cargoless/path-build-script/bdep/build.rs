// D15 P2 cut-③ diff fixture build-dep: writes $OUT_DIR/gen.rs (empirical proof that OUT_DIR propagates into this
// crate's compile), emits links metadata (foo=bar → root build.rs DEP_MYLINKS_FOO) and rustc-cfg (with matching
// check-cfg, zero-warning discipline).
fn main() {
    let out = std::env::var("OUT_DIR").expect("OUT_DIR must be set");
    std::fs::write(format!("{out}/gen.rs"), "pub const N: u32 = 7;\n")
        .expect("gen.rs write failed");
    println!("cargo::metadata=foo=bar");
    println!("cargo::rustc-check-cfg=cfg(bdep_feat)");
    println!("cargo::rustc-cfg=bdep_feat");
}
