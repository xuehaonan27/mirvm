//! Build and cache the sysroot that carries full MIR.
//!
//! Release std rlibs only encode MIR for generic/#[inline] functions, so interpreting
//! non-generic std functions requires rebuilding std from rust-src with
//! `-Zalways-encode-mir`. The build runs entirely through cargoless's own scheduler: zero
//! cargo processes and zero crates.io / `~/.cargo` dependencies (std's backtrace closure
//! resolves through `library/vendor/`).
//!
//! - Pseudo-root package: a synthetic manifest materialized at `$MIRVM_HOME/build/sysroot-build/root/`
//!   (path edges point to `library/{std,test,proc_macro}`, std with panic-unwind+backtrace
//!   features) plus an augmented library/Cargo.lock (original text + pseudo-root row);
//!   resolve uses lock mode with all versions pinned;
//! - Supply side = `VendorDir` (`library/vendor/` + four `[patch.crates-io]` overrides --
//!   the rustc-std-workspace trio and windows-sys point back into library/);
//! - Compile = the same driver::compile_plan pipeline (Layout::at points to the sysroot lib
//!   flat dir plus separate staging for host artifacts; --sysroot passes the **toolchain**,
//!   since the outputs cannot be their own compile input). Flags:
//!   - debug-assertions off, overflow-checks on;
//!   - `-Zalways-encode-mir` carried by dep_rustc_args;
//!   - `-Zforce-unstable-if-unmarked` on the rustflags channel (target units only).
//!
//! Dependency artifacts use `-Zno-codegen` (metadata-only): mirvm consumes only MIR, so
//! object code is pure waste.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::cargoless::driver::compile_plan;
use crate::cargoless::manifest::{OptLevel, PackageManifest, ProfileFlags};
use crate::cargoless::schedule::Layout;
use crate::cargoless::vendor::VendorDir;
use crate::cargoless::{buildrs, resolve};

/// Why the MIR-rich sysroot could not be produced.
///
/// The stages that still return `String` (the manifest reader, the resolver, the scheduler) are
/// carried as `detail`: each becomes a `#[source]` when its own module is typed, and the rendered
/// message stays what it is today in the meantime.
#[derive(Debug, thiserror::Error, serde::Serialize)]
pub enum Error {
    /// A filesystem operation on the staging tree failed.
    #[error("{source}")]
    Io {
        #[from]
        #[serde(skip)]
        source: std::io::Error,
    },

    /// The toolchain's own `library/Cargo.lock` could not be read.
    #[error("failed to read {}: {source}", path.display())]
    ReadLock {
        path: PathBuf,
        #[serde(skip)]
        #[source]
        source: std::io::Error,
    },

    /// The toolchain ships no rust-src, so no sysroot can be built from it.
    #[error(
        "rust-src is not present ({} lacks std/Cargo.toml) -- the MIR sysroot is built from \
         rust-src, so the toolchain needs the rust-src component",
        path.display()
    )]
    RustSrcMissing { path: PathBuf },

    /// The synthetic root manifest the sysroot build resolves from did not parse.
    #[error("failed to parse the pseudo-root manifest: {detail}")]
    ParseManifest { detail: String },

    /// The sysroot dependency graph could not be resolved.
    #[error("sysroot dependency resolution failed: {detail}")]
    Resolve { detail: String },

    /// Two packages in the sysroot graph claim the same `links` key.
    #[error("{detail}")]
    Links { detail: String },

    /// Compiling the sysroot crates failed.
    #[error("sysroot compilation failed: {detail}")]
    Compile { detail: String },

    /// The content key could not be recomputed after a successful build, which leaves the published
    /// sysroot unstamped and therefore rebuilt on every run.
    #[error("stamp computation failed after the sysroot build (abnormal rust-src tree read)")]
    StampUnavailable,

    /// The cargo-track dependency cache could not be dropped after the sysroot was replaced. Its
    /// stale rmeta would be treated as fresh and mixed into the new sysroot (E0463), so the failure
    /// is named now rather than left to a later, more confusing compile error.
    #[error(
        "failed to purge the cargo-track dep cache {} after the sysroot was replaced \
         (deleting it by hand is enough): {source}",
        path.display()
    )]
    PurgeCargoDeps {
        path: PathBuf,
        #[serde(skip)]
        #[source]
        source: std::io::Error,
    },
}

crate::diag_codes! {
    Error: Sysroot => {
        Io => "sysroot.io",
        ReadLock => "sysroot.read_lock",
        RustSrcMissing => "sysroot.rust_src_missing",
        ParseManifest => "sysroot.parse_manifest",
        Resolve => "sysroot.resolve",
        Links => "sysroot.links_conflict",
        Compile => "sysroot.compile",
        StampUnavailable => "sysroot.stamp_unavailable",
        PurgeCargoDeps => "sysroot.purge_cargo_deps",
    }
}

/// Toolchain sysroot baked at compile time (see build.rs); rustc takes it from here.
fn toolchain_root() -> &'static Path {
    Path::new(crate::options::build::DEFAULT_SYSROOT)
}

/// The rust-src library/ tree (home of the std workspace crates and vendor/).
fn library_dir() -> PathBuf {
    toolchain_root().join("lib/rustlib/src/rust/library")
}

/// Location of the stamp file (the content key) inside the sysroot.
fn stamp_file(sysroot_dir: &Path) -> PathBuf {
    sysroot_dir
        .join("lib/rustlib")
        .join(crate::options::build::HOST)
        .join(".mirvm-sysroot-hash")
}

/// Ensure the MIR-rich sysroot exists and return its path.
///
/// The fast path recomputes the content key and compares it with the stamp file; any
/// mismatch (or a missing stamp) triggers a rebuild. A rebuild happens entirely in
/// staging + tmp directories and is published with an atomic rename, so the old sysroot
/// stays usable until the moment it is swapped out. A crash mid-build leaves tmp/old
/// directories behind, and the missing stamp self-heals on the next run.
pub fn ensure_sysroot() -> Result<PathBuf, Error> {
    let sysroot_dir = crate::store::SYSROOT.dir();
    if let Some(want) = stamp_value()
        && std::fs::read_to_string(stamp_file(&sysroot_dir)).is_ok_and(|have| have == want)
    {
        return Ok(sysroot_dir);
    }
    build_sysroot(&sysroot_dir)?;
    Ok(sysroot_dir)
}

/// Stamp value of the currently built sysroot (reused in the base-image key); `None` when
/// the sysroot is not built.
pub(crate) fn current_stamp_value() -> Option<String> {
    std::fs::read_to_string(stamp_file(&crate::store::SYSROOT.dir())).ok()
}

/// Content key (the regeneration criterion): the rustc binary stat, the library/ top-level
/// sentinel, and the serialized build recipe. It catches a toolchain swap (rustc stat plus
/// sentinel -- a rustup install touches the top-level directory mtime) and a recipe change
/// (the serialized section).
///
/// **MIRVM_BUILD_ID is excluded**: a new mirvm version does not rebuild the sysroot. The
/// cargo-track dep cache fingerprint cannot see the sysroot contents, so a BUILD_ID-level
/// rebuild would mix old rmeta into the new sysroot (E0463). mirvm-side invalidation is
/// covered instead by the BUILD_ID component in each key (cargoless fp and base-image keys
/// both carry it).
///
/// **No whole-tree stat**: recursively stamping 2348 files measured ~20ms per run, which
/// pushes the fib(32) JIT gate (<80ms) over the line (103ms), and the per-run fast path
/// cannot afford it. Hand-editing a rust-src leaf file inside the toolchain is therefore
/// outside the protected surface; the escape hatch is to delete the stamp or the sysroot
/// directory, and the next run rebuilds.
///
/// A missing rust-src (sentinel failure) yields `None` and forces a rebuild, which then
/// fails loudly on the rust-src check.
fn stamp_value() -> Option<String> {
    let mut key = rustc_stat()?;
    key.push('\n');
    key.push_str(&library_sentinel()?);
    key.push('\n');
    // Recipe serialization, from the same profile/rustflags build_sysroot uses
    let p = sysroot_profile();
    key.push_str(&format!(
        "da{} oc{} opt{}\n",
        p.debug_assertions as u8, p.overflow_checks as u8, p.opt_level
    ));
    for f in sysroot_rustflags() {
        key.push_str(&f);
        key.push('\n');
    }
    Some(key)
}

/// rustc binary stat string (len + mtime_ns; `None` when absent).
fn rustc_stat() -> Option<String> {
    let md = std::fs::metadata(toolchain_root().join("bin/rustc")).ok()?;
    if !md.is_file() {
        return None;
    }
    let mtime_ns = md
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some(format!("{}\n{}", md.len(), mtime_ns))
}

/// The sysroot build recipe (matches the distribution std: debug-assertions off,
/// overflow-checks on). Full MIR comes from `-Zalways-encode-mir` in dep_rustc_args, not
/// here.
fn sysroot_profile() -> ProfileFlags {
    ProfileFlags {
        debug_assertions: false,
        overflow_checks: true,
        opt_level: OptLevel::O0,
    }
}

/// Rustflags for the sysroot build (the rustflags channel reaches target units only --
/// build.rs compilation does not see them, matching cargo). Unstable features in the std
/// crates are admitted without `#[unstable]` markers, as rustbuild / `cargo -Zbuild-std` do.
fn sysroot_rustflags() -> Vec<String> {
    vec!["-Zforce-unstable-if-unmarked".to_string()]
}

/// Top-level library/ sentinel: a sorted fold of (name, len, mtime_ns) over library/ itself
/// and each of its direct entries (~40 stats, sub-millisecond). Installing or swapping
/// rust-src replaces top-level entries, so their mtimes move; reinstalling the same version
/// is the same content and needs no rebuild, which leaving the sentinel unchanged expresses.
/// Hand-edited leaf files are not caught (see the protected-surface note and escape hatch on
/// `stamp_value`).
///
/// `None` when the sentinel cannot be read: the sysroot is then rebuilt, which fails loudly on the
/// rust-src check. There is no separate report for an unreadable sentinel because nothing acts on
/// one — the only two answers are "this content key" and "cannot say".
fn library_sentinel() -> Option<String> {
    let root = library_dir();
    let mut rows: Vec<String> = Vec::new();
    let mut put = |p: &Path, name: String| -> Option<()> {
        let md = std::fs::metadata(p).ok()?;
        let mtime_ns = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        rows.push(format!("{name}:{}:{}", md.len(), mtime_ns));
        Some(())
    };
    put(&root, ".".to_string())?;
    for ent in std::fs::read_dir(&root).ok()? {
        let ent = ent.ok()?;
        put(&ent.path(), ent.file_name().to_string_lossy().into_owned())?;
    }
    rows.sort();
    Some(rows.join("\u{1e}"))
}

/// The sysroot component of the fingerprint (the third input to schedule::fingerprints):
/// a stamp of the compiling toolchain (BUILD_ID + rustc binary len+mtime_ns). A toolchain
/// swap changes the fingerprint and correctly invalidates the host-side staging cache that
/// is otherwise reused across rebuilds.
fn toolchain_stamp() -> String {
    match rustc_stat() {
        Some(stat) => format!("{}\n{}", crate::options::build::BUILD_ID, stat),
        // Missing is not fatal: the fingerprint is coarser (BUILD_ID remains) and no new
        // error path appears
        None => format!("{}\nrustc-stat-missing", crate::options::build::BUILD_ID),
    }
}

/// The four `[patch.crates-io]` entries (as in library/Cargo.toml): registry name -> local path.
fn workspace_overrides(library: &Path) -> BTreeMap<String, PathBuf> {
    [
        "rustc-std-workspace-core",
        "rustc-std-workspace-alloc",
        "rustc-std-workspace-std",
        "windows-sys",
    ]
    .iter()
    .map(|n| (n.to_string(), library.join(n)))
    .collect()
}

/// Materialize the pseudo-root (write-if-changed: stable mtimes are a precondition for
/// fingerprints and incremental builds, the same discipline as driver.rs):
/// - Cargo.toml: path edges point at library/{std,test,proc_macro}, std with
///   panic-unwind+backtrace features. proc_macro must be present -- it bridges proc-macro
///   crates and every standard sysroot has it -- but it is outside the std+test dependency
///   closure, so it is listed separately.
/// - Cargo.lock: the original library/Cargo.lock plus a pseudo-root package row, so lock-mode
///   resolve walks the graph from the root row with every version pinned. (Using library/ as
///   the root would make the pseudo-root missing from the lock a loud failure, and the
///   toolchain directory is not writable, so the pseudo-root is materialized in our own
///   staging.)
fn materialize_pseudo_root(library: &Path, root_dir: &Path) -> Result<(), Error> {
    std::fs::create_dir_all(root_dir)?;
    let toml = format!(
        "[package]\nname = \"mirvm-mir-sysroot\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\
         \n[dependencies]\n\
         std = {{ path = '{}', features = [\"panic-unwind\", \"backtrace\"] }}\n\
         test = {{ path = '{}' }}\n\
         proc_macro = {{ path = '{}' }}\n",
        library.join("std").display(),
        library.join("test").display(),
        library.join("proc_macro").display(),
    );
    write_if_changed(&root_dir.join("Cargo.toml"), toml.as_bytes())?;
    let lock_src =
        std::fs::read_to_string(library.join("Cargo.lock")).map_err(|source| Error::ReadLock {
            path: library.join("Cargo.lock"),
            source,
        })?;
    let lock = format!(
        "{lock_src}\n[[package]]\nname = \"mirvm-mir-sysroot\"\nversion = \"0.0.0\"\n\
         dependencies = [\n \"proc_macro\",\n \"std\",\n \"test\",\n]\n"
    );
    write_if_changed(&root_dir.join("Cargo.lock"), lock.as_bytes())?;
    Ok(())
}

/// Do not rewrite identical content (stable mtimes are a precondition for fingerprints and
/// incremental builds).
fn write_if_changed(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    if std::fs::read(path).ok().is_none_or(|old| old != bytes) {
        std::fs::write(path, bytes)?;
    }
    Ok(())
}

/// Full rebuild: pseudo-root -> resolve (supplied by VendorDir) -> compile_plan (using the
/// toolchain as the compile base) -> write the stamp into tmp -> publish with an atomic
/// rename.
fn build_sysroot(sysroot_dir: &Path) -> Result<(), Error> {
    let target = crate::options::build::HOST;
    let library = library_dir();
    if !library.join("std/Cargo.toml").is_file() {
        return Err(Error::RustSrcMissing { path: library });
    }
    crate::diag_info!(
        Sysroot,
        "building the MIR-rich sysroot (one-time, takes a few minutes)..."
    );

    // staging (persistent; reuses host artifacts / build-script cache across rebuilds) and
    // the pseudo-root
    let staging = crate::store::SYSROOT_BUILD.dir();
    let root_dir = staging.join("root");
    materialize_pseudo_root(&library, &root_dir)?;
    let manifest =
        PackageManifest::read_dir(&root_dir).map_err(|detail| Error::ParseManifest { detail })?;
    let mut src = VendorDir::new(vec![library.join("vendor")], workspace_overrides(&library));
    let plan = resolve::resolve(&manifest, &mut src).map_err(|detail| Error::Resolve { detail })?;
    buildrs::check_links_unique(Some((&manifest.name, manifest.links.as_deref())), &plan)
        .map_err(|detail| Error::Links { detail })?;

    // Artifacts are built in a staging directory next to the sysroot (same filesystem, so the
    // publish rename is atomic) and the old sysroot stays usable until then.
    let tmp = crate::store::staging_path(sysroot_dir);
    let layout = Layout::at(
        tmp.join("lib/rustlib").join(target).join("lib"),
        staging.join("host-deps"),
        staging.join("build"),
    );
    compile_plan(
        &plan,
        &layout,
        &sysroot_profile(),
        &sysroot_rustflags(),
        toolchain_root(),
        &toolchain_stamp(),
        manifest.has_build_script,
        false,
        false,
    )
    .map_err(|detail| Error::Compile { detail })?;

    // Write the stamp inside tmp so it is published by the same rename; the content key is
    // recomputed here from the same source as the fast path
    let want = stamp_value().ok_or(Error::StampUnavailable)?;
    let stamp_in_tmp = stamp_file(&tmp);
    std::fs::write(&stamp_in_tmp, &want)?;

    // Publication is the store's directory publish: the old sysroot is renamed aside first, because
    // rename(2) cannot replace a non-empty directory. The old directory is absent only between the
    // two renames; a crash there leaves the stamp missing and the next run rebuilds.
    crate::store::publish(sysroot_dir, &tmp)?;
    // Purge the cargo-track dep cache along with it: the sysroot contents changed, but
    // cargo's fingerprint cannot see that (the --sysroot path string is unchanged), so stale
    // rmeta would be treated as fresh and mixed into the new sysroot, producing E0463/E0460.
    // The cargoless track and each image key carry the stamp/BUILD_ID component themselves
    // and need no action. A failed purge is treated as an FS fault and fails loudly: keeping
    // the cache is certain to break later compilations, so name it now.
    let cargo_deps = crate::store::TARGET.dir().join("mirvm");
    if cargo_deps.exists() {
        std::fs::remove_dir_all(&cargo_deps).map_err(|source| Error::PurgeCargoDeps {
            path: cargo_deps.clone(),
            source,
        })?;
    }
    crate::diag_info!(Sysroot, "sysroot build complete: {}", sysroot_dir.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("mirvm-sysroot-test-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn stamp_file_lives_inside_sysroot_lib() {
        // The triple the stamp is placed under is the host's rather than the one the sysroot
        // directory happens to be named after, so the expectation names the host too.
        let f = stamp_file(Path::new("/x/sysroot-x86_64-unknown-linux-gnu"));
        assert_eq!(
            f,
            Path::new(&format!(
                "/x/sysroot-x86_64-unknown-linux-gnu/lib/rustlib/{}/.mirvm-sysroot-hash",
                crate::options::build::HOST
            ))
        );
    }

    #[test]
    fn pseudo_root_reparses_and_lock_gains_root_row() {
        let tmp = tmpdir("pseudo-root");
        let library = tmp.join("library");
        std::fs::create_dir_all(&library).unwrap();
        std::fs::write(
            library.join("Cargo.lock"),
            "# This file is automatically @generated by Cargo.\n\
             # It is not intended for manual editing.\nversion = 4\n\n\
             [[package]]\nname = \"std\"\nversion = \"0.0.0\"\n\n\
             [[package]]\nname = \"test\"\nversion = \"0.0.0\"\n\
             dependencies = [\n \"std\",\n]\n",
        )
        .unwrap();
        let root = tmp.join("root");
        materialize_pseudo_root(&library, &root).unwrap();

        // The pseudo manifest parses: three path edges, std with two features
        let m = PackageManifest::read_dir(&root).unwrap();
        assert_eq!(m.name, "mirvm-mir-sysroot");
        assert_eq!(m.deps.len(), 3);
        let std_dep = m.deps.iter().find(|d| d.package == "std").unwrap();
        assert_eq!(std_dep.features, vec!["panic-unwind", "backtrace"]);
        assert!(m.deps.iter().any(|d| d.package == "proc_macro"));

        // The augmented lock parses: original rows preserved plus the pseudo-root row
        // (required for lock-mode graph walking from the root row)
        let lf = crate::cargoless::lockfile::Lockfile::read(&root.join("Cargo.lock")).unwrap();
        let root_pkg = lf
            .packages
            .iter()
            .find(|p| p.name == "mirvm-mir-sysroot")
            .expect("the pseudo-root package row must be present");
        assert_eq!(root_pkg.version.to_string(), "0.0.0");
        assert_eq!(root_pkg.dependencies.len(), 3);
        assert_eq!(lf.find("std").len(), 1);
        assert_eq!(lf.find("test").len(), 1);

        // write-if-changed: identical content on the second run leaves mtime unchanged
        let first = std::fs::metadata(root.join("Cargo.toml"))
            .unwrap()
            .modified()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        materialize_pseudo_root(&library, &root).unwrap();
        let second = std::fs::metadata(root.join("Cargo.toml"))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(first, second);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn stamp_value_tracks_recipe_and_tree() {
        // This repo's toolchain ships rust-src, so the stamp must exist and contain the
        // recipe lines
        let v = stamp_value().expect("with rust-src present the stamp must exist");
        // The content key excludes MIRVM_BUILD_ID (a new mirvm version does not rebuild the
        // sysroot; the axes are rustc/rust-src/recipe -- see the note on stamp_value)
        assert!(!v.starts_with(crate::options::build::BUILD_ID));
        assert!(v.contains("da0 oc1 opt0"));
        assert!(v.contains("-Zforce-unstable-if-unmarked"));
        // Same source and tree, so the value is deterministic
        assert_eq!(stamp_value().unwrap(), v);
    }
}
