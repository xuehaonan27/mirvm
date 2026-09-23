//! Managed Rust heap: mimalloc backend behind a thin real-address wrapper.
//!
//! Implements the `__rust_alloc` family of engine primitives; lowering rewrites the
//! std-declared extern boundary into a CallBuiltin, while the C library's own `malloc`
//! passes straight through. Real addresses are handed out directly: a guest pointer *is* the
//! host address mimalloc returned, so FFI needs zero marshalling.
//! A hand-rolled TLAB is deferred -- mimalloc already has a per-thread heap.

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
