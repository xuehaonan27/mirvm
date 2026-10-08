//! The process's own stdout: the bytes a command was asked to produce.
//!
//! This is not diagnostics. A survey, a report, `--dump-mir`, `--vm-call`, the version block and the
//! help block are the command's result, so they go to inherited fd 1 in the mode their own renderer
//! chose and never carry the `mirvm[component]:` grammar. `src/diag` owns fd 2; keeping the product's
//! stdout writer here — and writing it with `std::io` rather than the `print!` macros — is what makes
//! "no raw print under `src/`" a checkable rule instead of a habit.

use std::io::Write as _;

/// Write an already-rendered block exactly as given: the renderer owns its newlines.
pub fn text(text: impl std::fmt::Display) {
    write!(std::io::stdout().lock(), "{text}").expect("failed printing to stdout");
}

/// Write one line to stdout.
pub fn line(text: impl std::fmt::Display) {
    writeln!(std::io::stdout().lock(), "{text}").expect("failed printing to stdout");
}
