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
        // 作为 workspace 测试根时 Cargo 启用本包 default；作为 app/tool 的
        // 依赖单元时才启用 workspace.dependencies 继承的 base。
        assert!(super::base() || cfg!(feature = "default-on"));
    }
}
