//! T1-b CallForeign 探针（m5.4-design §3.2）：libc qsort + guest 比较子
//! （thunk_args 物化：guest fn 条目地址逃逸给 native 前物化 thunk 真码，
//! qsort 回调经 thunk 蹦回解释器/JIT 帧）。三维逐字节一致 + 发布实证。
use std::ffi::c_void;

unsafe extern "C" {
    fn qsort(base: *mut c_void, nmemb: usize, size: usize, compar: *const c_void);
    fn write(fd: i32, buf: *const c_void, count: usize) -> isize;
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
        qsort(
            v.as_mut_ptr() as *mut c_void,
            4,
            8,
            compar as *const c_void,
        );
    }
    v
}

fn main() {
    let mut acc = [0i64; 4];
    for _ in 0..30000 {
        acc = sort4([9, -4, 7, 0]);
    }
    println!("sorted={acc:?}");
    let r = unsafe { write(1, "libc-write ok\n".as_ptr() as *const _, 14) };
    println!("write ret={r}");
}
