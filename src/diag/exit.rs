//! Named process exit codes.
//!
//! A number is chosen in exactly one place. `diag::Kind` maps a failure class onto these, and every
//! other exit path names the constant instead of repeating a digit, which is the only way a shell
//! consumer can rely on the vocabulary.
//!
//! Codes that are not mirvm's own are deliberately absent: a guest exit code, a rustc exit code and
//! the `SIGABRT` re-raise travel through unchanged and must not be renumbered here.

/// An operation failed: I/O, resolution, compilation, packaging.
pub const FAILURE: u8 = 1;

/// A command line, environment value or input spelling was rejected.
pub const USAGE: u8 = 2;

/// An internal invariant broke, or a subprocess or thread could not be started.
pub const SOFTWARE: u8 = 70;

/// A test run failed. This is the cargo/rustc convention, not a mirvm invention.
pub const TEST_FAILED: u8 = 101;
