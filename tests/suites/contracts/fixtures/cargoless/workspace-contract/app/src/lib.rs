pub fn profile_contract() -> bool {
    cfg!(feature = "default-on") && !cfg!(debug_assertions)
}

#[cfg(test)]
mod tests {
    #[test]
    fn app_contract() {
        println!(
            "WORKSPACE_APP default={} base={} app={} extra={} implicit={} helper={} authors={}",
            cfg!(feature = "default-on"),
            shared::base(),
            shared::app_side(),
            shared::extra(),
            cfg!(feature = "implicit-helper"),
            test_helper::marker(),
            env!("CARGO_PKG_AUTHORS")
        );
        assert_eq!(env!("CARGO_PKG_AUTHORS"), "Workspace Author");
        assert!(shared::base());
        assert!(shared::app_side());
        assert_eq!(cfg!(debug_assertions), false);
        #[cfg(feature = "implicit-helper")]
        assert_eq!(implicit_helper::marker(), "workspace-implicit-helper");
    }
}
