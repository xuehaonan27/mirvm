//! The dynamic-loading vocabulary.
//!
//! A caller asks for a library and says how much resolving it wants done before the call returns.
//! That request is the same one on every platform that has `dlopen`, so the mode is declared here.
//!
//! Everything else in this subsystem is the platform's, because the loader is the platform: the
//! handle, the symbol lookup, the loader's own error text, and the link-map structure that yields a
//! handle's load bias. The platform half must provide, under the same names a caller already uses:
//! `open`, `open_with_flags`, `close`, `sym`, `error_string`, `load_bias`, and the `RTLD_*` flag
//! constants `open_with_flags` accepts.

/// The dlopen mode.
/// All call points always carry RTLD_GLOBAL (fixed as an internal constant).
#[derive(Clone, Copy)]
pub enum Mode {
    Now,
    Lazy,
}

#[cfg(target_os = "linux")]
pub(crate) use super::linux::dll::*;
#[cfg(target_os = "macos")]
pub(crate) use super::macos::dll::*;
