//! The mapping interface, and the protection vocabulary every mapping call speaks.
//!
//! `Prot` is the caller's side of a mapping: the three combinations the engine ever asks for. The
//! values are the C library's and the same on every platform, so the type is declared here.
//!
//! `page_size`, `map_anon`, `protect` and `unmap` are the same C library calls with the same flags
//! on every platform this build supports, so they are here too. What is the platform's is the
//! place where the kernels genuinely differ: asking for a *preferred fixed* base without being
//! allowed to replace what is already there.
//!
//! A file with no name belongs to one platform alone. Its kernel can make one directly, which is
//! how bytes reach something that reads them back through a descriptor; the other has no such call,
//! and where it needs the same thing — an object for its own loader, which its signer has to see by
//! name first — it creates a named file instead, as a step of `dll`'s publication.

/// Asking for a *preferred fixed* base: the address if the kernel grants it, and nothing if it does
/// not, rather than replacing whatever is mapped there.
#[cfg(target_os = "linux")]
pub(crate) use super::linux::mem::map_fixed_preferred;
#[cfg(target_os = "macos")]
pub(crate) use super::macos::mem::map_fixed_preferred;

/// A file with no name, for bytes that are only ever read back through the descriptor.
#[cfg(target_os = "linux")]
pub(crate) use super::linux::mem::anonymous_file;

/// Narrow enumeration of mmap protection flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Prot(pub(crate) std::os::raw::c_int);

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
