#[test]
fn integration_uses_normal_lib_and_dev_dependency() {
    assert!(!cfg!(debug_assertions));
    assert!(cless_test_contract::build_cfg_present());
    assert_eq!(cless_test_contract::library_value(), "from-build-rs");
    assert_eq!(test_helper::value(), "dev-contract");
    println!("CLESS_INTEGRATION_RAN");
    assert!(std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).is_dir());
}

#[test]
fn integration_can_execute_package_binary() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_cless_test_contract"))
        .arg("--contract-probe")
        .output()
        .expect("package binary launcher failed");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "CLESS_BIN_EXECUTED\n"
    );
}
