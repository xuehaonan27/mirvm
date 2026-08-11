#[cfg(cless_workspace_lint)]
pub fn secondary() -> bool {
    true
}

#[cfg(not(cless_workspace_lint))]
pub fn secondary() -> bool {
    true
}
