fn main() {
    if std::env::args().nth(1).as_deref() == Some("--contract-probe") {
        println!("CLESS_BIN_EXECUTED");
    } else {
        println!("{}", cless_test_contract::library_value());
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn bin_uses_root_lib_and_dev_dependency() {
        assert!(cless_test_contract::build_cfg_present());
        assert_eq!(test_helper::value(), "dev-contract");
    }
}
