#[test]
fn required_feature_contract() {
    println!(
        "WORKSPACE_REQUIRED extra={} helper={}",
        shared::extra(),
        feature_helper::marker()
    );
    assert!(shared::extra());
    assert_eq!(feature_helper::marker(), "workspace-feature-helper");
}
