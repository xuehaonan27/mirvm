fn main() {
    assert!(cless_test_contract::build_cfg_present());
    assert_eq!(test_helper::value(), "dev-contract");
}
