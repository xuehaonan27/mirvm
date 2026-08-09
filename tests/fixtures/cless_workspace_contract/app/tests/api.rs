#[test]
fn integration_contract() {
    println!("WORKSPACE_APP_INTEGRATION {}", test_helper::marker());
    assert!(workspace_app::profile_contract());
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_workspace-app"))
        .output()
        .expect("workspace app bin");
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), "WORKSPACE_APP_BIN true");
}
