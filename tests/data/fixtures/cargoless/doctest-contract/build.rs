fn main() {
    println!("cargo::rustc-env=CLESS_DOCTEST_BUILD=from-build-rs");
    println!("cargo::rustc-check-cfg=cfg(cless_doctest_cfg)");
    println!("cargo::rustc-cfg=cless_doctest_cfg");
}
