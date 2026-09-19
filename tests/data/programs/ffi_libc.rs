use std::ffi::c_void;
unsafe extern "C" {
    fn abs(input: i32) -> i32;
    fn sqrt(x: f64) -> f64;
    fn strlen(s: *const i8) -> usize;
    fn memcmp(a: *const c_void, b: *const c_void, n: usize) -> i32;
}
fn main() {
    unsafe {
        println!("abs(-42) = {}", abs(-42));
        println!("sqrt(2.0) = {:.5}", sqrt(2.0));
        let s = c"hello ffi";
        println!("strlen = {}", strlen(s.as_ptr()));
        let (x, y) = (b"abcd", b"abce");
        println!("memcmp<0 = {}", memcmp(x.as_ptr().cast(), y.as_ptr().cast(), 4) < 0);
    }
}
