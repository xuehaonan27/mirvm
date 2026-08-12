unsafe extern "Rust" {
    #[link_name = "cpp_no_throw"]
    fn foreign_with_rust_abi() -> i32;
}

fn main() {
    println!("unexpected {}", unsafe { foreign_with_rust_abi() });
}
