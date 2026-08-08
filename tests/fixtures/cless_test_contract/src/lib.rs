pub fn library_value() -> &'static str {
    env!("CLESS_BUILD_VALUE")
}

#[cfg(cless_build_cfg)]
pub fn build_cfg_present() -> bool {
    true
}

#[cfg(not(cless_build_cfg))]
pub fn build_cfg_present() -> bool {
    false
}

#[cfg(test)]
mod tests {
    #[test]
    fn lib_uses_cfg_test_and_dev_dependency() {
        assert!(!cfg!(debug_assertions));
        assert!(super::build_cfg_present());
        assert_eq!(super::library_value(), "from-build-rs");
        assert_eq!(test_helper::value(), "dev-contract");
    }

    #[test]
    #[ignore = "contract checks --ignored forwarding"]
    fn lib_ignored() {
        assert_eq!(test_helper::value(), "dev-contract");
    }

    #[test]
    #[should_panic(expected = "contract panic")]
    fn lib_expected_panic() {
        panic!("contract panic");
    }

    #[test]
    fn lib_nocapture_marker() {
        println!("CLESS_LIB_NOCAPTURE");
    }

    #[test]
    fn optional_failure_for_exit_contract() {
        assert_ne!(std::env::var("CLESS_FORCE_FAIL").as_deref(), Ok("1"));
    }
}
