//! 托管 Rust Heap（D3 v1）：mimalloc 后端 + 薄真地址包装。
//!
//! `__rust_alloc` 系引擎原语的实现（lower 在 std 声明的 extern 边界改写为
//! CallBuiltin，libc::malloc 直通不动——内存三分，DESIGN §4）。真地址直出：
//! guest 指针 = mimalloc 返回的宿主地址，FFI 零编组。
//! hand-rolled TLAB 后置（mimalloc 本身已 per-thread heap，M4.4 真线程直接受益）。

use libmimalloc_sys as mi;

#[inline]
pub fn alloc(size: u64, align: u64) -> u64 {
    unsafe { mi::mi_malloc_aligned(size as usize, align as usize) as u64 }
}

#[inline]
pub fn alloc_zeroed(size: u64, align: u64) -> u64 {
    unsafe { mi::mi_zalloc_aligned(size as usize, align as usize) as u64 }
}

#[inline]
pub fn dealloc(ptr: u64, _size: u64, _align: u64) {
    unsafe { mi::mi_free(ptr as *mut _) }
}

#[inline]
pub fn realloc(ptr: u64, _old_size: u64, align: u64, new_size: u64) -> u64 {
    unsafe { mi::mi_realloc_aligned(ptr as *mut _, new_size as usize, align as usize) as u64 }
}
