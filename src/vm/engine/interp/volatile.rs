//! Volatile access: an opaque byte carrier plus the chunked decomposition used by
//! `mem_read_volatile`/`mem_write_volatile`, which respect `MaybeUninit<[u8; N]>`
//! alignment discipline. The JIT helpers reuse the same implementation through the
//! `pub(crate)` re-export in `mod.rs`.

use super::*;

/// Reads one whole guest value as an opaque bit pattern. `MaybeUninit<[u8; N]>` has
/// alignment 1, so a low-alignment guest type such as `[u8; N]` is not wrongly
/// strengthened to host integer alignment; `MaybeUninit` also permits an aggregate
/// value with uninitialized padding.
#[inline]
pub(super) unsafe fn volatile_load_n<const N: usize>(src: *const u8, dst: *mut u8) {
    let value = unsafe { (src as *const MaybeUninit<[u8; N]>).read_volatile() };
    unsafe {
        std::ptr::copy_nonoverlapping((&value as *const MaybeUninit<[u8; N]>).cast::<u8>(), dst, N)
    };
}

/// Copies the raw bytes (including possibly uninitialized padding) into the opaque
/// carrier first, then issues one equally wide volatile store.
#[inline]
pub(super) unsafe fn volatile_store_n<const N: usize>(dst: *mut u8, src: *const u8) {
    let mut value = MaybeUninit::<[u8; N]>::uninit();
    unsafe { std::ptr::copy_nonoverlapping(src, value.as_mut_ptr().cast::<u8>(), N) };
    unsafe { (dst as *mut MaybeUninit<[u8; N]>).write_volatile(value) };
}

/// Backend decomposition of a wide memory-repr volatile value. Both ends only touch
/// `MaybeUninit<[u8; N]>`, so padding stays opaque. The 16/8/4/2/1 chunking mirrors
/// the machine accesses the target must ultimately perform and promises no atomicity.
#[inline]
pub(super) unsafe fn volatile_load_chunks(mut src: *const u8, mut dst: *mut u8, mut size: usize) {
    while size >= 16 {
        unsafe { volatile_load_n::<16>(src, dst) };
        src = src.wrapping_add(16);
        dst = dst.wrapping_add(16);
        size -= 16;
    }
    if size >= 8 {
        unsafe { volatile_load_n::<8>(src, dst) };
        src = src.wrapping_add(8);
        dst = dst.wrapping_add(8);
        size -= 8;
    }
    if size >= 4 {
        unsafe { volatile_load_n::<4>(src, dst) };
        src = src.wrapping_add(4);
        dst = dst.wrapping_add(4);
        size -= 4;
    }
    if size >= 2 {
        unsafe { volatile_load_n::<2>(src, dst) };
        src = src.wrapping_add(2);
        dst = dst.wrapping_add(2);
        size -= 2;
    }
    if size == 1 {
        unsafe { volatile_load_n::<1>(src, dst) };
    }
}

#[inline]
pub(super) unsafe fn volatile_store_chunks(mut dst: *mut u8, mut src: *const u8, mut size: usize) {
    while size >= 16 {
        unsafe { volatile_store_n::<16>(dst, src) };
        dst = dst.wrapping_add(16);
        src = src.wrapping_add(16);
        size -= 16;
    }
    if size >= 8 {
        unsafe { volatile_store_n::<8>(dst, src) };
        dst = dst.wrapping_add(8);
        src = src.wrapping_add(8);
        size -= 8;
    }
    if size >= 4 {
        unsafe { volatile_store_n::<4>(dst, src) };
        dst = dst.wrapping_add(4);
        src = src.wrapping_add(4);
        size -= 4;
    }
    if size >= 2 {
        unsafe { volatile_store_n::<2>(dst, src) };
        dst = dst.wrapping_add(2);
        src = src.wrapping_add(2);
        size -= 2;
    }
    if size == 1 {
        unsafe { volatile_store_n::<1>(dst, src) };
    }
}

#[inline]
pub(crate) fn mem_read_volatile(addr: u64, dst: u64, size: u32) {
    unsafe {
        match size {
            1 => volatile_load_n::<1>(addr as *const u8, dst as *mut u8),
            2 => volatile_load_n::<2>(addr as *const u8, dst as *mut u8),
            4 => volatile_load_n::<4>(addr as *const u8, dst as *mut u8),
            8 => volatile_load_n::<8>(addr as *const u8, dst as *mut u8),
            16 => volatile_load_n::<16>(addr as *const u8, dst as *mut u8),
            _ => {
                let size = size as usize;
                let mut snapshot = vec![MaybeUninit::<u8>::uninit(); size];
                volatile_load_chunks(addr as *const u8, snapshot.as_mut_ptr().cast::<u8>(), size);
                std::ptr::copy_nonoverlapping(snapshot.as_ptr().cast::<u8>(), dst as *mut u8, size);
            }
        }
    }
}

#[inline]
pub(crate) fn mem_write_volatile(addr: u64, src: u64, size: u32) {
    unsafe {
        match size {
            1 => volatile_store_n::<1>(addr as *mut u8, src as *const u8),
            2 => volatile_store_n::<2>(addr as *mut u8, src as *const u8),
            4 => volatile_store_n::<4>(addr as *mut u8, src as *const u8),
            8 => volatile_store_n::<8>(addr as *mut u8, src as *const u8),
            16 => volatile_store_n::<16>(addr as *mut u8, src as *const u8),
            _ => {
                let size = size as usize;
                let mut snapshot = vec![MaybeUninit::<u8>::uninit(); size];
                std::ptr::copy_nonoverlapping(
                    src as *const u8,
                    snapshot.as_mut_ptr().cast::<u8>(),
                    size,
                );
                volatile_store_chunks(addr as *mut u8, snapshot.as_ptr().cast::<u8>(), size);
            }
        }
    }
}
