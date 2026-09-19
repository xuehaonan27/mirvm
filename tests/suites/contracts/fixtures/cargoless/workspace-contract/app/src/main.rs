fn main() {
    println!("WORKSPACE_APP_BIN {}", workspace_app::profile_contract());
}

#[cfg(test)]
mod tests {
    #[test]
    fn bin_contract() {
        println!("WORKSPACE_APP_BIN_TEST");
        assert!(shared::app_side());
    }
}
