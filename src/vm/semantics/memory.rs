//! The memory model both backends execute against: one real address, one width, one access.
//!
//! A guest pointer is a host address, so an access needs no translation and no range check --
//! the guest's validity assumption is taken as given. What is left to define is the width
//! discipline: the value is read and written at exactly the declared width, unaligned, and
//! zero-extended into the 64-bit slot the engine carries.

use crate::vm::ir::Width;

/// Raw read through a real address: the guest's validity assumption is taken as given, with
/// no range check (the real-address model).
#[inline]
pub(crate) fn mem_read(addr: u64, w: Width) -> u64 {
    let p = addr as *const u8;
    unsafe {
        match w {
            Width::W8 => p.read_unaligned() as u64,
            Width::W16 => (p as *const u16).read_unaligned() as u64,
            Width::W32 => (p as *const u32).read_unaligned() as u64,
            Width::W64 => (p as *const u64).read_unaligned(),
        }
    }
}

/// Raw write through a real address, with the same no-range-check fast model as `mem_read`.
#[inline]
pub(crate) fn mem_write(addr: u64, w: Width, v: u64) {
    let p = addr as *mut u8;
    unsafe {
        match w {
            Width::W8 => p.write_unaligned(v as u8),
            Width::W16 => (p as *mut u16).write_unaligned(v as u16),
            Width::W32 => (p as *mut u32).write_unaligned(v as u32),
            Width::W64 => (p as *mut u64).write_unaligned(v),
        }
    }
}
