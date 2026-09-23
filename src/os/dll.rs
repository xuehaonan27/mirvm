//! The dynamic-loading vocabulary.
//!
//! A caller asks for a library and says how much resolving it wants done before the call returns.
//! That request is the same one on every platform that has `dlopen`, so the mode is declared here.
//!
//! Everything else in this subsystem is the platform's, because the loader is the platform: the
//! handle, the symbol lookup, the loader's own error text, and the structure that yields a handle's
//! load bias. The platform half must provide, under the same names a caller already uses:
//! `open`, `open_with_flags`, `close`, `sym`, `error_string`, `load_bias`, and the `RTLD_*` flag
//! constants `open_with_flags` accepts.
//!
//! `load_bias` takes both the handle and the path it was opened under even though a platform that
//! can read the base out of the handle needs only the first: the one that keys its loader's image
//! list by name needs only the second, and a call site names one function on every target.
//!
//! Two more items are the platform's because they are what a platform's loader *accepts*, and a
//! caller that has to publish an object for the loader cannot know either of them: the format the
//! object has to be written in (`SYMBOL_IMAGE_FORMAT`) and the sequence that gets bytes to the
//! loader at all (`load_private_image`). The bytes themselves come from `crate::native`, which
//! writes a layout without knowing which platform is asking.

/// The dlopen mode.
/// All call points always carry RTLD_GLOBAL (fixed as an internal constant).
#[derive(Clone, Copy)]
pub enum Mode {
    Now,
    Lazy,
}

/// The object format a platform's loader accepts.
///
/// A byte layout does not vary with the platform, which is why the writers for both of these live
/// in `crate::native`; what varies is which of them this platform's loader will load at all, and
/// that is one `SYMBOL_IMAGE_FORMAT` per platform.
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

/// An object this process published and then loaded: the handle that releases it again, and the
/// base its contents were mapped at.
///
/// Reaching the loader is the platform's sequence, and it leaves something behind that has to live
/// exactly as long as the image does: a descriptor the bytes are read back through, or a file name
/// that this platform's signer saw and a symbolizer will read the symbols out of. Both are owned
/// here rather than by the caller, which knows about neither.
#[derive(Debug)]
pub struct PrivateImage {
    handle: usize,
    bias: usize,
    /// A descriptor with no directory entry, dropped with the image.
    _descriptor: Option<std::fs::File>,
    /// A named file, removed with the image.
    _named: Option<std::path::PathBuf>,
}

impl PrivateImage {
    /// A loaded image: the handle its loader returned, the base it mapped the contents at, and
    /// whichever of the two things above has to outlive the mapping.
    pub(crate) fn new(
        handle: usize,
        bias: usize,
        descriptor: Option<std::fs::File>,
        named: Option<std::path::PathBuf>,
    ) -> Self {
        PrivateImage {
            handle,
            bias,
            _descriptor: descriptor,
            _named: named,
        }
    }

    /// The load base. An address the object defines is this plus the offset its writer returned.
    pub fn bias(&self) -> usize {
        self.bias
    }

    /// The handle the loader returned, which a test asks the loader about directly.
    #[cfg(test)]
    pub(crate) fn handle(&self) -> usize {
        self.handle
    }
}

impl Drop for PrivateImage {
    fn drop(&mut self) {
        // The name goes first: while the image is loaded, a symbolizer may still be reading the
        // symbols back out of it, and the handle is what ends that.
        if let Some(path) = self._named.take() {
            let _ = std::fs::remove_file(path);
        }
        unsafe { close(self.handle) };
    }
}

#[cfg(target_os = "linux")]
pub(crate) use super::linux::dll::*;
#[cfg(target_os = "macos")]
pub(crate) use super::macos::dll::*;
