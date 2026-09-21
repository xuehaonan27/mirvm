//! The dep-info of one compilation: the files whose content its product depends on, and the `env!`
//! values it was compiled against.
//!
//! The scope is isomorphic to rustc's own dep-info (`rustc_interface::passes`): local source files
//! (`source_map` non-imported) + `include!` tracked files (`sess.file_depinfo`) + every upstream
//! crate artifact (`used_crate_source`, which includes the sysroot std rlib, so a sysroot change
//! naturally mismatches) + `env!`/`option_env!` dependencies (`sess.env_depinfo`).
//!
//! Two consumers, one meaning: the L2 cache validates a stored entry against the current state
//! before reusing it, and `mirvm pack` records the manifest in the package as provenance — a
//! package carries its own module and native libraries, so it never replays it (see
//! modeb-mirvmar-design.md).

use std::path::Path;

use rustc_middle::ty::TyCtxt;
use serde::{Deserialize, Serialize};

use crate::utils::content::FileStamp;

/// The input manifest of one compilation.
///
/// Files are identified by content digest; size and mtime are recorded for diagnostics only. A
/// manifest that could not be collected completely must not be cached or packaged, so [`collect`]
/// fails closed instead of recording a partial one.
///
/// [`collect`]: InputManifest::collect
#[derive(Serialize, Deserialize, Default, PartialEq, Eq, Debug)]
pub(crate) struct InputManifest {
    pub files: Vec<FileStamp>,
    /// `env!`/`option_env!` dependencies: (name, value at compile time; `None` = unset then).
    pub envs: Vec<(String, Option<String>)>,
}

impl InputManifest {
    /// Collect from a live compiler session; `None` when any input cannot be stamped (missing or
    /// unusual), because a product with an incomplete manifest is worse than none.
    pub(crate) fn collect(tcx: TyCtxt<'_>) -> Option<Self> {
        let sess = tcx.sess;
        let mut paths: Vec<String> = sess
            .source_map()
            .files()
            .iter()
            .filter(|file| !file.is_imported())
            .filter_map(|file| match &file.name {
                rustc_span::FileName::Real(real) => {
                    real.local_path().map(|path| path.display().to_string())
                }
                _ => None,
            })
            .collect();
        paths.extend(
            sess.file_depinfo
                .borrow()
                .iter()
                .map(|sym| sym.as_str().to_string()),
        );
        for &cnum in tcx.crates(()) {
            paths.extend(
                tcx.used_crate_source(cnum)
                    .paths()
                    .map(|path| path.display().to_string()),
            );
        }
        paths.sort();
        paths.dedup();
        // Make absolute: cargo gives local crate relative paths (`src/main.rs`), but the product can
        // be loaded from any cwd. A path that cannot be canonicalized is kept as it came — the
        // content stamp still decides.
        let files = paths
            .iter()
            .map(|path| {
                std::fs::canonicalize(path)
                    .map(|canonical| canonical.display().to_string())
                    .unwrap_or_else(|_| path.clone())
            })
            .map(|path| FileStamp::of(Path::new(&path)))
            .collect::<Result<Vec<FileStamp>, _>>()
            .ok()?;
        let envs = sess
            .env_depinfo
            .borrow()
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_string(),
                    value.map(|value| value.as_str().to_string()),
                )
            })
            .collect();
        Some(InputManifest { files, envs })
    }

    /// Whether every recorded input still holds its recorded value.
    pub(crate) fn is_current(&self) -> bool {
        self.files.iter().all(FileStamp::is_current)
            && self
                .envs
                .iter()
                .all(|(name, recorded)| env_matches(name, recorded))
    }
}

/// One `env!` dependency: present with the same value, or absent in both.
fn env_matches(name: &str, recorded: &Option<String>) -> bool {
    match (std::env::var(name), recorded) {
        (Ok(current), Some(recorded)) => current == *recorded,
        (Err(std::env::VarError::NotPresent), None) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_dep_matching_covers_set_unset_and_drift() {
        let name = "MIRVM_INPUTS_TEST_ENV";
        // SAFETY: test-only variable within this single test process
        unsafe { std::env::remove_var(name) };
        assert!(env_matches(name, &None));
        assert!(!env_matches(name, &Some("x".into())));
        unsafe { std::env::set_var(name, "x") };
        assert!(env_matches(name, &Some("x".into())));
        assert!(!env_matches(name, &Some("y".into())));
        assert!(!env_matches(name, &None));
        unsafe { std::env::remove_var(name) };
    }
}
