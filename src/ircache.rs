//! L2 post-mono engine-IR cache (M6 slice 2; distribution-design.md D9b/D9c, JVM AppCDS
//! counterpart).
//!
//! Cold path (miss): rustc frontend → lower → **store** (clean snapshot before guest runs) → run.
//! Hot path (hit): **lookup** → asm-stub rematerialization → argv finalization → run — the entire
//! rustc session (frontend + metadata + mono + lower) is skipped.
//!
//! Key = fnv(MIRVM_BUILD_ID, rustc_args); entry header = full args replay (hash-collision proof) +
//! input manifest validation. Manifest scope is isomorphic to rustc's own dep-info
//! (rustc_interface::passes): local source files (source_map non-imported) + `include!` tracked
//! files (sess.file_depinfo) + all upstream crate artifacts (used_crate_source: includes sysroot
//! std rlib, so sysroot changes naturally mismatch) + `env!` dependencies (sess.env_depinfo). Files
//! are validated by content digest; size/mtime are also stored for diagnostics but are no longer
//! treated as content identity.
//!
//! Guard against silent wrong values: any validation mismatch is a miss (cold path rebuilds and
//! overwrites); frozen area not at fixed base is rejected for serialization/restore (see
//! frozen.rs); missing required .so is a miss (self-heal rather than runtime error).
//! `MIRVM_NO_IR_CACHE=1` bypasses the cache entirely.

use std::path::{Path, PathBuf};

use rustc_middle::ty::TyCtxt;
use serde::{Deserialize, Serialize};

use crate::vm::ir;

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
pub(crate) struct FileStamp {
    pub path: String,
    pub size: u64,
    pub mtime_ns: u128,
    pub digest: [u8; 32],
}

#[derive(Serialize, Deserialize)]
struct Header {
    build_id: String,
    args: Vec<String>,
    files: Vec<FileStamp>,
    /// `env!`/`option_env!` dependencies: (name, value at compile time; None = not set at compile time)
    envs: Vec<(String, Option<String>)>,
    /// S4 layering: base key referenced by the delta module (None = full module with no base).
    /// Delta bytecode/frozen area embeds base absolutes (FuncId offset, base addresses) — a
    /// mismatched base load is globally wrong, so keys must match exactly.
    base_key: Option<String>,
}

fn disabled() -> bool {
    crate::options::get().no_ir_cache
}

fn entry_path(rustc_args: &[String]) -> PathBuf {
    let mut key = String::from(crate::options::build::BUILD_ID);
    for a in rustc_args {
        key.push('\u{1f}');
        key.push_str(a);
    }
    let h = crate::utils::content::fnv1a(key.as_bytes());
    crate::options::get()
        .home
        .join("ir")
        .join(format!("{h:016x}.bin"))
}

fn stamp(path: &str) -> Option<FileStamp> {
    let stamped = crate::utils::content::file_content_stamp(Path::new(path)).ok()?;
    Some(FileStamp {
        path: path.to_string(),
        size: stamped.size,
        mtime_ns: stamped.mtime_ns,
        digest: stamped.digest,
    })
}

/// Header triple equality: build id (stale across builds) + full args replay (hash-collision proof) +
/// base key exact equality (S4: delta embeds base absolutes — FuncId offsets / base addresses — a
/// base replacement or presence change makes a mismatched load globally wrong; None side must also
/// match exactly, a no-base session must not consume a base delta).
fn header_matches(header: &Header, rustc_args: &[String], base_key: Option<&str>) -> bool {
    header.build_id == crate::options::build::BUILD_ID
        && header.args == rustc_args
        && header.base_key.as_deref() == base_key
}

fn env_matches(name: &str, recorded: &Option<String>) -> bool {
    match (std::env::var(name), recorded) {
        (Ok(cur), Some(rec)) => cur == *rec,
        (Err(std::env::VarError::NotPresent), None) => true,
        _ => false,
    }
}

/// Stamp collection result: (file stamp manifest, `env!` dependency manifest).
pub(crate) type InputStamps = (Vec<FileStamp>, Vec<(String, Option<String>)>);

/// Input manifest collection (isomorphic to rustc dep-info scope; shared by mode B packaging and
/// L2): local source files (source_map non-imported) + `include!` tracked files + all upstream
/// crate artifacts (used_crate_source: includes sysroot std rlib) + `env!` dependencies. If any
/// file cannot be stamped (missing/unusual) = None (prefer not to cache/package).
pub(crate) fn collect_input_stamps(tcx: TyCtxt<'_>) -> Option<InputStamps> {
    let sess = tcx.sess;
    let mut files: Vec<String> = sess
        .source_map()
        .files()
        .iter()
        .filter(|f| !f.is_imported())
        .filter_map(|f| match &f.name {
            rustc_span::FileName::Real(real) => real.local_path().map(|p| p.display().to_string()),
            _ => None,
        })
        .collect();
    files.extend(
        sess.file_depinfo
            .borrow()
            .iter()
            .map(|sym| sym.as_str().to_string()),
    );
    for &cnum in tcx.crates(()) {
        files.extend(
            tcx.used_crate_source(cnum)
                .paths()
                .map(|p| p.display().to_string()),
        );
    }
    files.sort();
    files.dedup();
    // Make absolute (mode B evidence: cargo gives local crate relative paths (src/main.rs), but the
    // package may be loaded from any cwd; keep original path if canonicalize fails, stamp check
    // covers it)
    let files: Vec<String> = files
        .iter()
        .map(|p| {
            std::fs::canonicalize(p)
                .map(|c| c.display().to_string())
                .unwrap_or_else(|_| p.clone())
        })
        .collect();
    let stamps = files
        .iter()
        .map(|p| stamp(p))
        .collect::<Option<Vec<FileStamp>>>()?;
    let envs: Vec<(String, Option<String>)> = sess
        .env_depinfo
        .borrow()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.map(|s| s.as_str().to_string())))
        .collect();
    Some((stamps, envs))
}

/// Replay each stamp against the current local file state (shared by mode B package load validation
/// and L2 lookup).
pub(crate) fn stamps_current(files: &[FileStamp]) -> bool {
    files.iter().all(|f| stamp(&f.path).as_ref() == Some(f))
}

/// Replay each env dependency against the current environment (shared as above).
pub(crate) fn envs_current(envs: &[(String, Option<String>)]) -> bool {
    envs.iter().all(|(k, v)| env_matches(k, v))
}

/// Hot-path lookup. The returned Module already has its frozen area restored to a fixed base;
/// asm_stub_addrs are stale addresses from serialization, the caller **must** rematerialize and
/// overwrite via asm_sites before executing.
pub fn lookup(
    rustc_args: &[String],
    base_key: Option<&str>,
    prefix: crate::vm::verify::Prefix,
) -> Option<ir::Module> {
    if disabled() {
        return None;
    }
    let data = std::fs::read(entry_path(rustc_args)).ok()?;
    let (header, module_bytes) = postcard::take_from_bytes::<Header>(&data).ok()?;
    if !header_matches(&header, rustc_args, base_key) {
        return None;
    }
    if !stamps_current(&header.files) {
        return None;
    }
    if !envs_current(&header.envs) {
        return None;
    }
    // Module deserialization includes frozen-area fixed-base restoration; failure (base occupied
    // etc.) → miss
    let mut module: ir::Module = postcard::from_bytes(module_bytes).ok()?;
    module.rebuild_load_map();
    module.rebuild_fn_addrs();
    // Correct shape does not guarantee index and frame range safety. Bad cache is treated as miss
    // and self-healed by the cold path.
    crate::vm::verify::module_with_prefix(&module, prefix).ok()?;
    // Materialized .so files (native archive / global_asm) removed → miss, self-healed by cold path
    if !module
        .required_native_libs
        .iter()
        .all(|p| Path::new(&**p).is_file())
    {
        return None;
    }
    Some(module)
}

/// Cold-path store (clean state right after lower finishes and before guest runs). Returns whether
/// the write actually happened.
pub fn store(
    tcx: TyCtxt<'_>,
    rustc_args: &[String],
    module: &ir::Module,
    base_key: Option<&str>,
    prefix: crate::vm::verify::Prefix,
) -> bool {
    if disabled() {
        return false;
    }
    if crate::vm::verify::module_with_prefix(module, prefix).is_err() {
        return false;
    }
    // Frozen area not at fixed base (concurrent preempt / ASLR conflict) ⇒ embedded addresses in
    // snapshot are cross-process invalid, do not cache
    if !module.frozen.as_ref().is_some_and(|f| f.at_fixed_base()) {
        return false;
    }
    // P1 entry stub domain not at fixed base ⇒ fn-ptr value domain is cross-process unstable, do
    // not cache (same rule)
    if !module.entry_stub_sites.is_empty() && !module.entry_stubs.at_fixed_base() {
        return false;
    }
    // Foreign symbols (environ-like extern static / extern fn address-taking) are indirected
    // through GOT slots since P2 (decision-history §7.5c): GOT table travels with snapshot and is
    // refilled by this process's real values at startup — no longer a cache blocker, the old
    // "embedded host address rejects cache" criterion (M6 slice 2) is retired.

    // Input manifest (isomorphic to rustc dep-info scope, shared collector with mode B packaging)
    let Some((stamps, envs)) = collect_input_stamps(tcx) else {
        return false; // some input file could not be stamped (missing/unusual) — prefer not to cache
    };

    let header = Header {
        build_id: crate::options::build::BUILD_ID.to_string(),
        args: rustc_args.to_vec(),
        files: stamps,
        envs,
        base_key: base_key.map(str::to_owned),
    };
    let Ok(mut buf) = postcard::to_stdvec(&header) else {
        return false;
    };
    match postcard::to_stdvec(module) {
        Ok(m) => buf.extend(m),
        Err(_) => return false,
    }

    // Atomic publish (same as asm-stub factory: write full temp then rename, readers never see a
    // half-finished file)
    let path = entry_path(rustc_args);
    let Some(dir) = path.parent() else {
        return false;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let tmp = dir.join(format!(
        ".{}.tmp-{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    if std::fs::write(&tmp, &buf).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return false;
    }
    std::fs::rename(&tmp, &path).is_ok()
}

#[cfg(test)]
mod tests {
    use super::{FileStamp, env_matches, stamp};

    #[test]
    fn stamp_detects_content_length_and_mtime_change() {
        let dir = std::env::temp_dir().join(format!("mirvm-ircache-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("input.rs");
        std::fs::write(&f, b"fn main() {}").unwrap();
        let p = f.display().to_string();
        let s0 = stamp(&p).expect("stampable");

        // size change must mismatch
        std::fs::write(&f, b"fn main() { let _ = 1; }").unwrap();
        assert_ne!(stamp(&p).as_ref(), Some(&s0));

        // same size but later mtime must also mismatch (rewrites of equal length are caught by mtime)
        std::fs::write(&f, b"fn main() {}").unwrap();
        let s1 = stamp(&p).unwrap();
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(7);
        std::fs::File::options()
            .write(true)
            .open(&f)
            .unwrap()
            .set_modified(later)
            .unwrap();
        assert_ne!(stamp(&p).as_ref(), Some(&s1));

        // missing = no stamp
        std::fs::remove_file(&f).unwrap();
        assert_eq!(stamp(&p), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stamp_detects_same_length_content_change_with_restored_mtime() {
        let dir =
            std::env::temp_dir().join(format!("mirvm-ircache-content-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("input.rs");
        std::fs::write(&f, b"fn value() -> u8 { 1 }").unwrap();
        let original_mtime = std::fs::metadata(&f).unwrap().modified().unwrap();
        let p = f.display().to_string();
        let before = stamp(&p).expect("stampable");

        std::fs::write(&f, b"fn value() -> u8 { 2 }").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&f)
            .unwrap()
            .set_modified(original_mtime)
            .unwrap();
        assert_ne!(stamp(&p).as_ref(), Some(&before));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn env_dep_matching_covers_set_unset_and_drift() {
        let name = "MIRVM_IRCACHE_TEST_ENV";
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

    #[test]
    fn stamps_compare_structurally() {
        let a = FileStamp {
            path: "a".into(),
            size: 1,
            mtime_ns: 2,
            digest: [3; 32],
        };
        assert_eq!(
            a,
            FileStamp {
                path: "a".into(),
                size: 1,
                mtime_ns: 2,
                digest: [3; 32],
            }
        );
    }

    #[test]
    fn base_key_must_match_exactly_including_absence() {
        let args = vec!["mirvm".to_string(), "x.rs".to_string()];
        let mk = |base_key: Option<&str>| super::Header {
            build_id: crate::options::build::BUILD_ID.to_string(),
            args: args.clone(),
            files: Vec::new(),
            envs: Vec::new(),
            base_key: base_key.map(str::to_owned),
        };
        // same key ✓; replacement ✗; presence change (some→none / none→some) both ways ✗
        assert!(super::header_matches(&mk(Some("k1")), &args, Some("k1")));
        assert!(!super::header_matches(&mk(Some("k1")), &args, Some("k2")));
        assert!(!super::header_matches(&mk(Some("k1")), &args, None));
        assert!(!super::header_matches(&mk(None), &args, Some("k1")));
        assert!(super::header_matches(&mk(None), &args, None));
        // existing axis regression: args drift still rejected
        let other = vec!["mirvm".to_string(), "y.rs".to_string()];
        assert!(!super::header_matches(&mk(None), &other, None));
    }
}
