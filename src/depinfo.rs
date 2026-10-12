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

use std::path::PathBuf;

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
    pub managed: Vec<crate::sysroot::managed::Input>,
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
        let paths: Vec<_> = paths
            .iter()
            .map(|path| std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path)))
            .collect();
        let mut external = Vec::new();
        let mut managed = Vec::new();
        for path in paths {
            if let Some(input) = crate::sysroot::managed_input(&path) {
                managed.push(input);
            } else {
                external.push(path);
            }
        }
        let files = stamp_files(&external)?;
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
        Some(InputManifest {
            files,
            managed,
            envs,
        })
    }

    /// A distributed package records file provenance without depending on the producer's store.
    pub(crate) fn provenance_files(&self) -> Option<Vec<FileStamp>> {
        let mut files = self.files.clone();
        for input in &self.managed {
            files.push(crate::sysroot::managed_provenance(input)?);
        }
        files.sort_unstable_by(|a, b| a.path.cmp(&b.path));
        Some(files)
    }

    /// Whether every recorded input still holds its recorded value.
    pub(crate) fn is_current(&self) -> bool {
        if !crate::sysroot::managed_inputs_current(&self.managed) {
            return false;
        }
        if !self
            .envs
            .iter()
            .all(|(name, recorded)| env_matches(name, recorded))
        {
            return false;
        }
        if self.files.len() < 4 {
            return self.files.iter().all(FileStamp::is_current);
        }
        let paths: Vec<_> = self
            .files
            .iter()
            .map(|file| PathBuf::from(&file.path))
            .collect();
        stamp_files(&paths).is_some_and(|files| files == self.files)
    }
}

/// Hash independent compiler inputs concurrently, keeping manifest order deterministic. A small
/// bound limits memory bandwidth and file I/O contention; all workers finish before guest startup.
fn stamp_files(paths: &[PathBuf]) -> Option<Vec<FileStamp>> {
    if paths.len() < 4 {
        return paths.iter().map(|path| FileStamp::of(path).ok()).collect();
    }
    std::thread::scope(|scope| {
        let batch = move |offset| {
            paths
                .iter()
                .enumerate()
                .skip(offset)
                .step_by(4)
                .map(|(index, path)| FileStamp::of(path).map(|stamp| (index, stamp)))
                .collect::<Result<Vec<_>, _>>()
        };
        let mut workers = Vec::new();
        let mut files = Vec::with_capacity(paths.len());
        for offset in 1..4 {
            match std::thread::Builder::new()
                .name("mirvm-inputs".into())
                .spawn_scoped(scope, move || {
                    let _service = crate::os::thread::ServiceThreadGuard::register();
                    batch(offset)
                }) {
                Ok(worker) => workers.push(worker),
                Err(_) => files.extend(batch(offset).ok()?),
            }
        }
        files.extend(batch(0).ok()?);
        for worker in workers {
            files.extend(
                worker
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
                    .ok()?,
            );
        }
        files.sort_unstable_by_key(|(index, _)| *index);
        Some(files.into_iter().map(|(_, stamp)| stamp).collect())
    })
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
    fn parallel_stamps_preserve_order_and_reject_changed_or_missing_inputs() {
        let dir = std::env::temp_dir().join(format!("mirvm-input-stamps-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let paths: Vec<_> = (0..7)
            .map(|index| dir.join(format!("{index}.bin")))
            .collect();
        for (index, path) in paths.iter().enumerate() {
            std::fs::write(path, vec![index as u8; (1 << 20) + index]).unwrap();
        }
        let files = stamp_files(&paths).unwrap();
        let serial: Vec<_> = paths
            .iter()
            .map(|path| FileStamp::of(path).unwrap())
            .collect();
        assert_eq!(files, serial);
        let manifest = InputManifest {
            files,
            managed: Vec::new(),
            envs: Vec::new(),
        };
        assert!(manifest.is_current());
        let modified = std::fs::metadata(&paths[4]).unwrap().modified().unwrap();
        std::fs::write(&paths[4], vec![99; (1 << 20) + 4]).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&paths[4])
            .unwrap()
            .set_modified(modified)
            .unwrap();
        assert!(
            !manifest.is_current(),
            "content changes must survive restored timestamps"
        );
        std::fs::remove_file(&paths[4]).unwrap();
        assert!(stamp_files(&paths).is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

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
