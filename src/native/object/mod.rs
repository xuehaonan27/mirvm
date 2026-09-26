//! The container formats mirvm reads and writes.
//!
//! An object format is a byte layout, and a byte layout does not vary with the kernel or the CPU:
//! the same layout is written by the toolchains of several platforms. What varies per platform is
//! *which* one this one's toolchain writes and its loader reads back, which is why the answer is one
//! `OBJECT_FORMAT` in `crate::os`: the loader is what accepts the bytes.
//!
//! One file per format, holding what the bytes say and nothing about what to do with them. Writing
//! an object or resolving a name through it is `crate::native::symbol`'s, which reads these layouts
//! and never re-spells them.

pub(crate) mod ar;
pub(crate) mod elf;
pub(crate) mod macho;

/// An object format, as a value. Which one a build meets is the platform's answer either way round:
/// it is what its toolchain writes and what its loader reads back.
///
/// Both variants therefore exist in every build. The one this platform does not name is still what
/// the shared dispatch matches on, so it is unreachable by construction rather than unused.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectFormat {
    /// An ELF shared object.
    Elf,
    /// A Mach-O dylib.
    MachO,
}
