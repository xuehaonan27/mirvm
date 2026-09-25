//! Guest build diagnostics, held back until their build's outcome is known.
//!
//! `mirvm run` is the command that executes a guest, not the one that builds it. The front-end
//! diagnostics a guest crate produces are preparation detail: a run that succeeds says what the
//! guest said and nothing about the work that got it there. Preparation has its own command
//! (`mirvm prepare`), which asks for that detail explicitly.
//!
//! While `MIRVM_BUILD_LOG=hold` — set by `run` and `prepare` for their whole process tree, so every
//! mirvm process that compiles a guest crate behaves the same way — the compiler's emitter hands its
//! bytes here instead of to stderr. [`finish`] then releases them when the build failed (the user
//! has to see why) or when the run asked for detail (`-v`, `MIRVM_LOG=info|debug`), and drops them
//! otherwise.
//!
//! Scope: the compiler sessions mirvm drives itself — the guest root/bin session, and the
//! `__cless-dep` sessions of both dependency tracks. A host crate (a build script, a proc-macro) is
//! compiled by invoking real rustc, whose stderr is inherited and therefore still live, as are the
//! Cargo track's own probe compilations.
//!
//! Releasing goes through [`crate::diag::write`], the one primitive that writes inherited fd 2 and
//! hands the same bytes to an active capture stream. That is what keeps the capture invariant true:
//! `diagnostics.log` holds exactly the compiler and control bytes that reached stderr, so a held
//! diagnostic that is dropped is absent from both and one that is released is present in both.
//!
//! Consequence for the L2 cache: a session that diagnosed something is still never written to it
//! (see `driver.rs`'s `SESSION_WARNINGS`), so a program whose build warns keeps recompiling on every
//! run. `-v` is how a user sees that it does.

use std::sync::Mutex;

/// Bytes held since the last [`finish`]. One compiler session per process is the norm (a guest root
/// session, or one dependency unit); should a process run two, the second session's diagnostics
/// join the first's and are released together.
static HELD: Mutex<Vec<u8>> = Mutex::new(Vec::new());

/// Keep one diagnostic's bytes back until the build's outcome is known.
pub(crate) fn hold(bytes: &[u8]) {
    let mut held = HELD.lock().unwrap_or_else(|e| e.into_inner());
    held.extend_from_slice(bytes);
}

/// Release everything held back, or drop it, once the compiler session that produced it ended.
///
/// `build_failed` is the caller's verdict on that session: a rejected compilation is a failure the
/// user asked to see, a clean one is preparation the user did not.
pub(crate) fn finish(build_failed: bool) {
    let bytes = {
        let mut held = HELD.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut *held)
    };
    if bytes.is_empty() {
        return;
    }
    if build_failed || crate::diag::verbose() {
        crate::diag::write(&bytes);
    }
}
