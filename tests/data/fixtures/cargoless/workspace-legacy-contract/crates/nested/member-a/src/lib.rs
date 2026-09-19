#[cfg(cless_workspace_lint)]
pub fn contract() -> bool {
    shared::normal_enabled() && shared::build_enabled()
}

#[cfg(test)]
mod tests {
    #[test]
    fn resolver_one_unifies_normal_and_build_features() {
        assert!(super::contract());
        println!("CLESS_RESOLVER_ONE_RAN");
    }
}
