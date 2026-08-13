//! Byte area frame storage: mmap fixed-size true address area.
//! (M4.1 F6 - frame address is stable for life).
//!
//! Rust structures like `Ref`/`RawPtr` takes the true address of the local a
//! rea within the frame (&arr, buf.as_mut_ptr()), so the runtime area cannot
//! be moved and fixed-size mmap is needed (same as GuestMemory).
//! Virtual pages reservation is large, so anonymous mapping and lazy commit
//! are adopted for not taking too much actual memory. (RSS is calculated
//! based on touched pages.)
//!
//! Frame is cut into segments according to (frame_size, frame_align).
//! Interface is kept narrow: reserve/restore/read/write.
//! Base is true address, so intra-frame/heap/statics are unified as raw address
//! read and write, forming the foundation of place evaluation.

use super::ir::{Slot, Width};
use crate::os::mem;

/// Size per region (M5.2 D8a: Virtually reserved 1 GiB + MAP_NORESERVE,
/// RSS still based on touch pages - on the same scale as the host execution
/// stack, operand region no longer becomes the implicit upper limit of deep
/// recursion before stack guards; one region per guest thread from M4.4
/// onwards).
const REGION_CAP: usize = 1 << 30;

pub struct ByteRegion {
    /// Whole mapping, including one inaccessible page on each side.
    mapping: *mut u8,
    mapping_len: usize,
    base: *mut u8,
    /// Bump water level in the area (number of bytes relative to base)
    sp: usize,
}

impl Default for ByteRegion {
    fn default() -> Self {
        Self::new()
    }
}

impl ByteRegion {
    pub fn new() -> Self {
        let guard = mem::page_size();
        let mapping_len = REGION_CAP
            .checked_add(guard * 2)
            .expect("ByteRegion: mapping length overflow");
        let mapping = mem::map_anon(mapping_len, mem::Prot::NONE, true);
        assert!(!mapping.is_null(), "ByteRegion: mmap failed");
        let base = unsafe { mapping.add(guard) };
        if let Err(e) = mem::protect(base, REGION_CAP, mem::Prot::RW) {
            unsafe { mem::unmap(mapping, mapping_len) };
            panic!("ByteRegion: {e}");
        }
        ByteRegion {
            mapping,
            mapping_len,
            base,
            sp: 0,
        }
    }

    /// Cuts `size` bytes for the new frame (aligned by `align`, zeroed out).
    /// Returns the frame base address (true address). Strictly paired with
    /// `restore` (slaved in interp_frame recursion).
    pub fn reserve(&mut self, size: u32, align: u32) -> usize {
        let align = align.max(1) as usize;
        // mmap base page alignment.
        let aligned = (self.base as usize + self.sp + align - 1) & !(align - 1);
        let start = aligned - self.base as usize;
        let end = start + size as usize;
        if end > REGION_CAP {
            crate::vm::engine::interp::engine_abort(&format!(
                "guest stack overflow (operand area {} MiB exhausted)",
                REGION_CAP >> 20
            ));
        }
        unsafe { std::ptr::write_bytes(self.base.add(start), 0, size as usize) };
        self.sp = end;
        aligned
    }

    pub fn restore(&mut self, base: usize) {
        debug_assert!(base >= self.base as usize && base <= self.base as usize + self.sp);
        self.sp = base - self.base as usize;
    }

    #[cfg(test)]
    pub(crate) fn used(&self) -> usize {
        self.sp
    }

    /// Read a scalar with zero-width extension (base = frame true address).
    #[inline]
    pub fn read(&self, base: usize, slot: Slot) -> u64 {
        let p = (base + slot.off as usize) as *const u8;
        // Frame offsets are constructed based on layout alignment.
        // `read_unaligned` is the starting point (correctness takes precedence, optimization follows).
        unsafe {
            match slot.width {
                Width::W8 => p.read_unaligned() as u64,
                Width::W16 => (p as *const u16).read_unaligned() as u64,
                Width::W32 => (p as *const u32).read_unaligned() as u64,
                Width::W64 => (p as *const u64).read_unaligned(),
            }
        }
    }

    /// Write a scalar truncated to the width (base = frame true address) .
    #[inline]
    pub fn write(&mut self, base: usize, slot: Slot, v: u64) {
        let p = (base + slot.off as usize) as *mut u8;
        unsafe {
            match slot.width {
                Width::W8 => p.write_unaligned(v as u8),
                Width::W16 => (p as *mut u16).write_unaligned(v as u16),
                Width::W32 => (p as *mut u32).write_unaligned(v as u32),
                Width::W64 => (p as *mut u64).write_unaligned(v),
            }
        }
    }
}

impl Drop for ByteRegion {
    fn drop(&mut self) {
        unsafe { mem::unmap(self.mapping, self.mapping_len) };
    }
}
