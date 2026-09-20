//! Linux anonymous mapping primitives.
//! (mmap/mprotect/munmap + preference for fixed base address)
//! The engine's sole channel for all anonymous mappings, merging three mmap
//! forms: frozen, codearena, and frame. Only `usize`/raw pointer/Prot appears.
//! No guest concept is allowed. Capacity, base address value (addrlayout),
//! and exhaustion message are all the caller's responsibility.
//!
//! Failure semantics (aligned verbatim with the three memory map forms, each
//! caller decides its wording):
//! - Dynamic mapping failure → null pointer (caller assert/panic).
//! - Preference for fixed base address is occupied or fails → Ok(None)
//!   (caller decides dynamic rollback/loud error; MAP_FIXED_NOREPLACE never
//!   overwrites existing mappings).

/// Narrow enumeration of mmap protection flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Prot(std::os::raw::c_int);

impl Prot {
    /// No access. Used for guard pages around guest-owned mappings.
    pub const NONE: Prot = Prot(libc::PROT_NONE);
    /// PROT_READ | PROT_WRITE
    pub const RW: Prot = Prot(libc::PROT_READ | libc::PROT_WRITE);
    /// PROT_READ | PROT_EXEC
    pub const RX: Prot = Prot(libc::PROT_READ | libc::PROT_EXEC);
}

pub fn page_size() -> usize {
    let n = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    assert!(n > 0, "sysconf(_SC_PAGESIZE) failed");
    n as usize
}

/// Anonymous private dynamic mapping.
/// `noreserve`: virtual reservation that does not occupy a commit.
/// Returns a null pointer on failure (consistent with the `!= MAP_FAILED`
/// condition before merging, wording attributed to the caller).
pub fn map_anon(size: usize, prot: Prot, noreserve: bool) -> *mut u8 {
    let mut flags = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS;
    if noreserve {
        flags |= libc::MAP_NORESERVE;
    }
    let p = unsafe { libc::mmap(std::ptr::null_mut(), size, prot.0, flags, -1, 0) };
    if p == libc::MAP_FAILED {
        std::ptr::null_mut()
    } else {
        p as *mut u8
    }
}

/// Preferred fixed base address mapping (MAP_FIXED_NOREPLACE).
/// Some on success, None on failure.
/// Engine cacheability base (same approach as JVM CDS). Faulty base address
/// means silent fault value, therefore never overwrite existing mappings.
pub fn map_fixed_preferred(addr: usize, size: usize, prot: Prot) -> Option<*mut u8> {
    let p = unsafe {
        libc::mmap(
            addr as *mut libc::c_void,
            size,
            prot.0,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED_NOREPLACE,
            -1,
            0,
        )
    };
    if p == libc::MAP_FAILED {
        return None;
    }
    Some(p as *mut u8)
}

/// Thin wrapper of `mprotect`.
/// Codearena fills the W^X shape of the sealed RX.
pub fn protect(addr: *mut u8, size: usize, prot: Prot) -> Result<(), crate::os::Error> {
    let rc = unsafe { libc::mprotect(addr as *mut libc::c_void, size, prot.0) };
    if rc != 0 {
        return Err(crate::os::Error::Mprotect {
            addr: addr as usize,
            size,
            rc,
        });
    }
    Ok(())
}

/// Unmap the memory region. Caller holds capacity and lifecycle.
///
/// # Safety
/// addr/size must come from the same range that was successfully mapped in
/// this module. the caller guarantees that it will not be touched again.
pub unsafe fn unmap(addr: *mut u8, size: usize) {
    unsafe { libc::munmap(addr as *mut libc::c_void, size) };
}
