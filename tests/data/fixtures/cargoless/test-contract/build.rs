fn main() {
    assert_eq!(std::env::var("PROFILE").as_deref(), Ok("debug"));
    assert_eq!(std::env::var("OPT_LEVEL").as_deref(), Ok("1"));
    assert_eq!(std::env::var("DEBUG").as_deref(), Ok("true"));
    let marker =
        std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("contract-build-count");
    let count = std::fs::read_to_string(&marker)
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0)
        + 1;
    std::fs::write(&marker, count.to_string()).unwrap();
    println!("cargo::warning=CLESS_BUILD_SCRIPT_EXECUTED");
    println!("cargo::rustc-check-cfg=cfg(cless_build_cfg)");
    println!("cargo::rustc-cfg=cless_build_cfg");
    println!("cargo::rustc-env=CLESS_BUILD_VALUE=from-build-rs");
}
