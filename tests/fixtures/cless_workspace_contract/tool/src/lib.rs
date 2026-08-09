#[cfg(test)]
mod tests {
    #[test]
    fn tool_contract() {
        println!(
            "WORKSPACE_TOOL base={} tool={} app={}",
            workspace_bridge::base(),
            workspace_bridge::tool_side(),
            workspace_bridge::app_side()
        );
        assert!(workspace_bridge::base());
        assert!(workspace_bridge::tool_side());
    }
}
