//! volatile 族（自 interp.rs I3 整搬）：opaque 字节载体 + 分块分解的
//! mem_read_volatile/mem_write_volatile（MaybeUninit<[u8;N]> 对齐纪律）。
//! jit helpers 复用同一实现（pub(crate) 再出口在 mod.rs）。

use super::*;

/// 把 guest 中的一个完整值作为 opaque 位型读入。`MaybeUninit<[u8; N]>`
/// 的对齐是 1，因此不会把 `[u8; N]` 之类低对齐 guest 类型错误地
/// 强化为宿主整数对齐；`MaybeUninit` 同时允许聚合值含未初始化 padding。
#[inline]
pub(super) unsafe fn volatile_load_n<const N: usize>(src: *const u8, dst: *mut u8) {
    let value = unsafe { (src as *const MaybeUninit<[u8; N]>).read_volatile() };
    unsafe {
        std::ptr::copy_nonoverlapping((&value as *const MaybeUninit<[u8; N]>).cast::<u8>(), dst, N)
    };
}

/// 先按原始字节（包括可能未初始化的 padding）搬入 opaque 载体，
/// 再发出一个等宽 volatile store。
#[inline]
pub(super) unsafe fn volatile_store_n<const N: usize>(dst: *mut u8, src: *const u8) {
    let mut value = MaybeUninit::<[u8; N]>::uninit();
    unsafe { std::ptr::copy_nonoverlapping(src, value.as_mut_ptr().cast::<u8>(), N) };
    unsafe { (dst as *mut MaybeUninit<[u8; N]>).write_volatile(value) };
}

/// 宽 memory-repr volatile 值的后端分解。先/后端都只接触
/// `MaybeUninit<[u8; N]>`，所以 padding 保持 opaque；16/8/4/2/1 的分块
/// 对应目标最终必须完成的若干机器访问，不承诺原子性。
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
