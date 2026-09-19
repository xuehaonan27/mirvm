fn main() {
    assert!(shared::normal_enabled());
    assert!(shared::build_enabled());
    println!("cargo:rustc-cfg=cless_workspace_lint");
}
