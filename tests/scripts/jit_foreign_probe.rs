//! T1-b CallForeign probe (m5.4-design §3.2): libc qsort + guest comparator
//! (thunk_args materialization: real thunk code is materialized before the guest fn entry address escapes to native,
//! qsort callback bounces back into interpreter/JIT frames through the thunk) + C-unwind scalar return (live Drop
//! forces MIR to emit cleanup edges, locking the try_call normal-return path). Byte-identical across three dimensions
//! + release evidence.
use std::ffi::c_void;

unsafe extern "C" {
    fn qsort(base: *mut c_void, nmemb: usize, size: usize, compar: *const c_void);
    fn write(fd: i32, buf: *const c_void, count: usize) -> isize;
}

unsafe extern "C-unwind" {
    #[link_name = "getpid"]
    fn c_getpid() -> i32;
}

struct ForeignGuard;

impl Drop for ForeignGuard {
    fn drop(&mut self) {}
}

#[inline(never)]
fn cmp_i64(a: &i64, b: &i64) -> i32 {
    a.cmp(b) as i32
}

unsafe extern "C" fn compar(p: *const c_void, q: *const c_void) -> i32 {
    let (a, b) = unsafe { (&*(p as *const i64), &*(q as *const i64)) };
    cmp_i64(a, b)
}

#[inline(never)]
fn sort4(mut v: [i64; 4]) -> [i64; 4] {
    unsafe {
        qsort(v.as_mut_ptr() as *mut c_void, 4, 8, compar as *const c_void);
    }
    v
}

#[inline(never)]
fn foreign_with_cleanup() -> i32 {
    let _guard = ForeignGuard;
    unsafe { c_getpid() }
}

fn main() {
    let mut acc = [0i64; 4];
    for _ in 0..30000 {
        acc = sort4([9, -4, 7, 0]);
    }
    println!("sorted={acc:?}");
    let mut pid = 0;
    for _ in 0..30000 {
        pid = foreign_with_cleanup();
    }
    println!("getpid positive={}", pid > 0);
    let r = unsafe { write(1, "libc-write ok\n".as_ptr() as *const _, 14) };
    println!("write ret={r}");
}
