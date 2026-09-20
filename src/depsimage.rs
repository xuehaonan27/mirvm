//! Cross-run cache for the lowering product of registry dependency closures: the binary-independent
//! instances that sit below a program delta. Split lowering writes it to disk during a binary's first
//! cold run. A later run, even of an edited binary, loads it into the image stack as
//! `[std base, deps-image]` and lowers only the delta, i.e. the binary's own attachments. The cached
//! instances are the "purified aggregate": only purity=Pure instances.
//!
//! The pre-key is computable before the compiler session (the hot path runs before it and has no tcx):
//! `fnv(MIRVM_BUILD_ID, base key, sorted --extern artifact content stamps)`. `--extern` lists only direct
//! dependencies, so the transitive closure is covered by cargo rebuild propagation: a changed transitive
//! crate makes cargo recompile its direct dependents, which changes those rlib stamps. The content digest
//! catches same-size rewrites with a restored mtime. The key includes neither binary source nor project
//! identity, so binary edits always hit, and projects with the same lockfile and toolchain share an image.
//! The cache is on by default; `MIRVM_NO_DEPS_IMAGE=1` bypasses it entirely for two-state cross-checking.
//!
//! A cached image is used only when it cannot be wrong: its absolute FuncIds and addresses are valid only
//! if the load-time stack below it equals the build-time stack below it. Validation is layered: the image
//! fingerprint must equal the base's at load time, and the base's must equal the session's fingerprint
//! after analysis. Any mismatch means do not load, and full lowering self-heals rather than producing
//! wrong values.
//!
//! The file is not required to be byte-deterministic: identity is carried by the key and file name, and
//! nothing compares contents. v1 boundary: an empty `--extern` (a pure-std program with no registry
//! dependencies) neither produces nor uses an image, since the std base already covers that territory.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::store::entry;
use crate::utils::content::{FileStamp, digest_hex};
use crate::vm::ir;

/// deps-image file (v1 = the whole package as postcard). The module's exports and fn_addrs stay inside the
/// module: this file has no byte-determinism contract, so it does not need BaseFile's sorted
/// extraction/reconstruction.
#[derive(Serialize, Deserialize)]
struct DepsFile {
    build_id: String,
    /// Exact identity of the stack below (= [base]); a mismatch is wrong at every value, so it must be equal
    base_key: String,
    lowering_fp: (bool, bool, bool),
    /// Pre-key material for comparison (hash-collision immune): sorted --extern artifact stamps
    extern_stamps: Vec<FileStamp>,
    module: ir::Module,
    fn_entry_syms: Vec<(Box<str>, u64)>,
    static_syms: Vec<(Box<str>, u64)>,
    tls_syms: Vec<(Box<str>, ir::TlsId)>,
}

/// Borrowed shape for writing (ir::Module is not Clone — FrozenArena owns the mmap).
#[derive(Serialize)]
struct DepsFileRef<'a> {
    build_id: &'a str,
    base_key: &'a str,
    lowering_fp: (bool, bool, bool),
    extern_stamps: &'a [FileStamp],
    module: &'a ir::Module,
    fn_entry_syms: &'a [(Box<str>, u64)],
    static_syms: &'a [(Box<str>, u64)],
    tls_syms: &'a [(Box<str>, ir::TlsId)],
}

/// Whether the deps-image cache is bypassed. The cache is on by default and the only knob is
/// `MIRVM_NO_DEPS_IMAGE=1` (diagnostics / two-state cross-check).
pub fn bypassed() -> bool {
    crate::options::get().no_deps_image
}

fn deps_dir() -> PathBuf {
    crate::store::DEPS.dir()
}

/// Pre-key material: the paths in `--extern name=path` (two-arg form) and `--extern=name=path` (single-arg
/// form), deduped and sorted. A bare-name `--extern` without a path returns `None`, so v1 neither produces
/// nor uses an image and full lowering self-heals instead of risking silently wrong values.
fn extern_paths(rustc_args: &[String]) -> Option<Vec<String>> {
    let mut paths = Vec::new();
    let mut it = rustc_args.iter();
    while let Some(a) = it.next() {
        if a == "--extern" {
            let v = it.next()?;
            let (_, p) = v.split_once('=')?;
            paths.push(p.to_string());
        } else if let Some(v) = a.strip_prefix("--extern=") {
            let (_, p) = v.split_once('=')?;
            paths.push(p.to_string());
        }
    }
    paths.sort();
    paths.dedup();
    Some(paths)
}

/// Content stamping; a file that is not stably readable yields `None`, so no image is produced or used.
fn stamp_externs(paths: &[String]) -> Option<Vec<FileStamp>> {
    paths
        .iter()
        .map(|p| FileStamp::of(std::path::Path::new(p)).ok())
        .collect()
}

/// pre-key = digest(build_id, base key, sorted stamps), returned together with the stamp list. Any
/// material that is unavailable yields `None`, which the caller treats as "no image"; full lowering
/// then self-heals. An empty `--extern` also yields `None`: dependency-free pure-std programs are
/// covered by the std base and get no shared image of their own.
pub(crate) fn pre_key(rustc_args: &[String], base_key: &str) -> Option<(String, Vec<FileStamp>)> {
    let paths = extern_paths(rustc_args)?;
    if paths.is_empty() {
        return None;
    }
    let stamps = stamp_externs(&paths)?;
    let mut key = entry::Key::new();
    key.part(base_key);
    for stamp in &stamps {
        // One part per artifact: its four fields are separated inside the part, so a path cannot
        // impersonate a field boundary.
        key.part(&format!(
            "{}\u{1e}{}\u{1e}{}\u{1e}{}",
            stamp.path,
            stamp.size,
            stamp.mtime_ns,
            digest_hex(&stamp.digest)
        ));
    }
    let key = key.digest();
    if crate::options::get().a2_debug {
        eprintln!("[a2-debug] pre-key={key} externs={paths:?}");
    }
    Some((key, stamps))
}

fn file_path(key: &str) -> PathBuf {
    deps_dir().join(format!("{key}.img"))
}

/// Load the deps-image; called before the compiler session. `base` is the already-present base, whose key
/// and fingerprint are validated in layers. On success the returned `BaseImage` is pushed onto the stack,
/// with its key set to the pre-key so the L2 key chain can use it. Any mismatch or failure yields `None`:
/// full lowering self-heals and the main path stays silent, because stderr participates in native diffs.
pub fn try_load(
    rustc_args: &[String],
    base: &crate::lower::image::BaseImage,
) -> Option<crate::lower::image::BaseImage> {
    let (key, stamps) = pre_key(rustc_args, &base.key)?;
    let data = std::fs::read(file_path(&key)).ok()?;
    let mut f: DepsFile = postcard::from_bytes(&data).ok()?;
    // Exact-equality validation: build id, base key, stamp list (collision immune), layered lowering fingerprint
    if !entry::is_current_generation(&f.build_id)
        || f.base_key != base.key
        || f.extern_stamps != stamps
        || f.lowering_fp != base.lowering_fp
    {
        return None;
    }
    // The frozen area must actually land in the spline k=0 domain the layer below expects: this
    // rejects a swapped file and a stolen domain alike.
    if !entry::frozen_at(&f.module, Some(crate::vm::addrlayout::image_addr(0))) {
        return None;
    }
    let prefix = crate::vm::verify::Prefix {
        funcs: base.module.funcs.len(),
        tls: base.module.tls.len(),
        asm: base.module.asm_sites.len(),
    };
    if !entry::revive(&mut f.module, prefix) {
        return None;
    }
    // A removed required .so is a miss that falls back to the cold-path self-heal (same contract as
    // the L2 cache)
    if !entry::native_libs_present(&f.module) {
        return None;
    }
    let module = f.module;
    Some(crate::lower::image::BaseImage {
        fn_by_sym: module.exports.clone(),
        entry_by_sym: f.fn_entry_syms.into_iter().collect(),
        static_by_sym: f.static_syms.into_iter().collect(),
        tls_by_sym: f.tls_syms.into_iter().collect(),
        lowering_fp: f.lowering_fp,
        key,
        module,
    })
}

/// Persist the split product and return the `BaseImage` to push. If it is cacheable the file is written
/// atomically. A write failure, a non-cacheable product or an unavailable pre-key simply leaves no file for
/// this run, so later runs do full lowering and self-heal; the returned stack-layer key degrades to a
/// process-unique placeholder, which makes the L2 key chain invalid across runs rather than a false hit.
pub fn store_and_wrap(
    rustc_args: &[String],
    base_key: &str,
    fp: (bool, bool, bool),
    image: crate::lower::SplitImage,
) -> crate::lower::image::BaseImage {
    let mut bi = image.into_base_image(fp);
    // Cacheability: the frozen area must sit in the fixed spline k=0 domain the layer below expects,
    // a prerequisite for the snapshot's embedded absolute addresses to stay stable across processes.
    // Foreign symbols go through GOT slots: the image-side GOT table travels with the file and is
    // refilled with this process's real values at startup, so it does not block writing the file.
    let cacheable =
        entry::snapshot_is_publishable(&bi.module, Some(crate::vm::addrlayout::image_addr(0)));
    let keyed = pre_key(rustc_args, base_key);
    if let (true, Some((key, stamps))) = (cacheable, keyed) {
        let mut fn_entry_syms = bi
            .entry_by_sym
            .iter()
            .map(|(s, a)| (s.clone(), *a))
            .collect::<Vec<_>>();
        fn_entry_syms.sort_unstable();
        let mut static_syms = bi
            .static_by_sym
            .iter()
            .map(|(s, a)| (s.clone(), *a))
            .collect::<Vec<_>>();
        static_syms.sort_unstable();
        let mut tls_syms = bi
            .tls_by_sym
            .iter()
            .map(|(s, id)| (s.clone(), *id))
            .collect::<Vec<_>>();
        tls_syms.sort_unstable();
        let file = DepsFileRef {
            build_id: crate::options::build::BUILD_ID,
            base_key,
            lowering_fp: fp,
            extern_stamps: &stamps,
            module: &bi.module,
            fn_entry_syms: &fn_entry_syms,
            static_syms: &static_syms,
            tls_syms: &tls_syms,
        };
        if let Ok(bytes) = postcard::to_stdvec(&file) {
            let path = file_path(&key);
            let dir_exists = path
                .parent()
                .is_some_and(|dir| std::fs::create_dir_all(dir).is_ok());
            if dir_exists && crate::store::publish_bytes(&path, &bytes).is_ok() {
                bi.key = key;
                return bi;
            }
        }
    }
    // Degraded key: process-unique ⇒ L2 key chain never false-hits across runs (in-memory absorb for this run is unaffected)
    bi.key = format!("a2-unstable-{}", std::process::id());
    bi
}

#[cfg(test)]
mod tests {
    /// Both two-arg and single-arg --extern forms are parsed to path; bare-name --extern ⇒ None (self-heal)
    #[test]
    fn extern_paths_parse_both_forms_and_reject_bare_name() {
        let args = vec![
            "mirvm".to_string(),
            "src/main.rs".to_string(),
            "--extern".to_string(),
            "regex=/t/deps/libregex-abc.rlib".to_string(),
            "--extern=serde=/t/deps/libserde-def.rlib".to_string(),
            "--extern".to_string(),
            "rand=/t/deps/librand-123.rlib".to_string(),
            "-C".to_string(),
            "metadata=xyz".to_string(),
        ];
        let paths = super::extern_paths(&args).expect("parse succeeded");
        assert_eq!(
            paths,
            vec![
                "/t/deps/librand-123.rlib",
                "/t/deps/libregex-abc.rlib",
                "/t/deps/libserde-def.rlib"
            ]
        );
        let bare = vec!["--extern".to_string(), "regex".to_string()];
        assert_eq!(super::extern_paths(&bare), None);
        // Duplicate --extern deduped
        let dup = vec![
            "--extern".to_string(),
            "regex=/t/a.rlib".to_string(),
            "--extern".to_string(),
            "regex=/t/a.rlib".to_string(),
        ];
        assert_eq!(super::extern_paths(&dup).unwrap().len(), 1);
    }

    #[test]
    fn pre_key_detects_same_length_content_change_with_restored_mtime() {
        let dir = std::env::temp_dir().join(format!(
            "mirvm-depsimage-content-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let artifact = dir.join("libdep.rlib");
        std::fs::write(&artifact, b"first").unwrap();
        let original_mtime = std::fs::metadata(&artifact).unwrap().modified().unwrap();
        let args = vec![format!("--extern=dep={}", artifact.display())];
        let before = super::pre_key(&args, "base").unwrap().0;

        std::fs::write(&artifact, b"other").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&artifact)
            .unwrap()
            .set_modified(original_mtime)
            .unwrap();
        let after = super::pre_key(&args, "base").unwrap().0;
        assert_ne!(before, after);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
