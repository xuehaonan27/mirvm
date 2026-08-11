#[test]
fn bench_target_uses_dev_dependencies() {
    assert_eq!(test_helper::value(), "dev-contract");
    println!("CLESS_BENCH_RAN");
}
