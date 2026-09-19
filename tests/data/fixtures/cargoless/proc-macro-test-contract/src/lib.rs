use proc_macro::TokenStream;

#[proc_macro]
pub fn identity(input: TokenStream) -> TokenStream {
    assert!(pm_helper::enabled());
    input
}

#[cfg(test)]
mod tests {
    #[test]
    fn unit_test_has_host_and_dev_dependencies() {
        assert!(pm_helper::enabled());
        assert_eq!(test_helper::value(), "proc-macro-dev");
        println!("CLESS_PROC_MACRO_UNIT_RAN");
    }
}
