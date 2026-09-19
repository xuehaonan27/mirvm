fn main() {
    assert!(cfg!(test));
    assert!(!cfg!(debug_assertions));
    assert_eq!(test_helper::value(), "dev-contract");
    println!(
        "CLESS_CUSTOM_HARNESS {:?}",
        std::env::args().skip(1).collect::<Vec<_>>()
    );
}
