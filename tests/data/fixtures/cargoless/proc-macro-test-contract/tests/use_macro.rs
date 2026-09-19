use cless_proc_macro_test_contract::identity;

identity! {
    fn generated() -> &'static str {
        "expanded"
    }
}

#[test]
fn integration_uses_root_proc_macro() {
    assert_eq!(generated(), "expanded");
    assert_eq!(test_helper::value(), "proc-macro-dev");
    println!("CLESS_PROC_MACRO_INTEGRATION_RAN");
}
