pub fn normal_enabled() -> bool {
    cfg!(feature = "normal")
}

pub fn build_enabled() -> bool {
    cfg!(feature = "build")
}
