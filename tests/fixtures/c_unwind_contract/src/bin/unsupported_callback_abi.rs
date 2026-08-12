#![allow(improper_ctypes)]

unsafe extern "C" {
    #[link_name = "cpp_call_plain_c"]
    fn call_with_rust_callback(callback: unsafe fn()) -> i32;
}

unsafe fn rust_callback() {}

fn main() {
    println!("unexpected {}", unsafe {
        call_with_rust_callback(rust_callback)
    });
}
