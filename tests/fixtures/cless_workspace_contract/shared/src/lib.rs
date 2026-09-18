pub const fn base() -> bool {
    cfg!(feature = "base")
}

pub const fn app_side() -> bool {
    cfg!(feature = "app-side")
}

pub const fn tool_side() -> bool {
    cfg!(feature = "tool-side")
}

pub const fn extra() -> bool {
    cfg!(feature = "extra")
}

#[cfg(test)]
mod tests {
    #[test]
    fn shared_contract() {
        if std::env::var_os("WORKSPACE_FORCE_FAIL").is_some() {
            panic!("WORKSPACE_FORCED_FAILURE");
        }
        println!(
            "WORKSPACE_SHARED base={} default={} app={} tool={} extra={}",
            super::base(),
            cfg!(feature = "default-on"),
            super::app_side(),
            super::tool_side(),
            super::extra()
        );
        // When running as the workspace test root, Cargo enables this crate's default feature;
        // when used as a dependency of app/tool, only the base inherited from workspace.dependencies is enabled.
        assert!(super::base() || cfg!(feature = "default-on"));
    }
}
