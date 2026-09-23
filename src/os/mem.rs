//! The protection vocabulary every mapping call speaks.
//!
//! `Prot` is the caller's side of a mapping: the three combinations the engine ever asks for. The
//! values are the C library's and the same on every platform, so the type is declared here.
//!
//! What *is* the platform's stays in the platform directory: which flags reserve address space
//! without committing it, which mapping form refuses to replace what is already there, how an
//! anonymous file is created, and the failure shape of each. The platform half must provide, under
//! the same names a caller already uses: `page_size`, `map_anon`, `map_fixed_preferred`,
//! `anonymous_file`, `protect`, and `unmap`.

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

#[cfg(target_os = "linux")]
pub(crate) use super::linux::mem::*;
