//! A2 deps-image (s3b-a2-design §3): cross-run cache for binary-independent instances — extension of the S4 std base
//! to registry dependency closures. The lowering product of a dependency closure ("purified aggregate": only purity=Pure
//! instances) is produced by split lower and written to disk during a binary's **first cold run**; editing the binary and rerunning loads
//! it into the image stack `[std base, deps-image]`, and the runner lowers only delta (binary attachments).
//!
//! Key (pre-key, **computable pre-compiler** — L2 hot path runs before the compiler session, cannot depend on tcx):
//! `fnv(MIRVM_BUILD_ID, base key, sorted --extern artifact content stamps)`.
//! --extern contains only direct dependencies (4 in the eco); the transitive closure is covered by **cargo rebuild propagation** (any changed transitive
//! crate change ⇒ its direct dependencies on the reverse dependency chain are recompiled by cargo ⇒ direct rlib stamp changes);
//! content digest catches same-size rewrites with restored mtime. **Does not include binary source or project
//! identity** ⇒ binary edits always hit; same lockfile + same toolchain projects share (S3′c).
//! **Enabled by default from A2-3**; `MIRVM_NO_DEPS_IMAGE=1` bypasses entirely (for two-state cross-checking).
//! v1 boundary: empty --extern (pure-std program with no registry dependencies) does not produce/use image —
//! that territory is already covered by the S4 base.
//!
//! Correctness red line (same as the chain era): image's absolute FuncId/addresses are valid only when "load stack below ==
//! build stack below" (below always = [base], key includes base key); any validation mismatch = do not load
//! (full lowering self-heals, never wrong-value). Layered lowering-fingerprint validation: image.fp == base.fp (at load time) +
//! base.fp == session.fp (after_analysis fp_matches) ⇒ image.fp == session.fp.
//! Byte determinism: **not guaranteed** (same rule as L2 entries — identity is carried by key/file name, no cmp consumer;
//! base-file determinism contract is specific to "sharing bases across programs", see decision-history §7.3).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::vm::engine::ir;

/// --extern artifact stamp list (path, size, mtime_ns, BLAKE3; sorted and deduped)
type ExternStamps = Vec<(String, u64, u128, [u8; 32])>;

/// deps-image file (v1 = postcard whole package; module's exports/fn_addrs stay inside the module
/// — no byte-determinism contract, avoiding BaseFile's sorted Vec extraction/reconstruction dance).
#[derive(Serialize, Deserialize)]
struct DepsFile {
    build_id: String,
    /// Exact identity of below = [base] (mismatched load = total wrong-value, must be exactly equal)
    base_key: String,
    lowering_fp: (bool, bool, bool),
    /// Pre-key material for comparison (hash-collision immune): sorted --extern artifact stamps
    extern_stamps: ExternStamps,
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
    extern_stamps: &'a [(String, u64, u128, [u8; 32])],
    module: &'a ir::Module,
    fn_entry_syms: &'a [(Box<str>, u64)],
    static_syms: &'a [(Box<str>, u64)],
    tls_syms: &'a [(Box<str>, ir::TlsId)],
}

/// Enablement check (default on from A2-3): only knob = bypass `MIRVM_NO_DEPS_IMAGE=1` (diagnostics / two-state
/// cross-check). The A2-2 `MIRVM_DEPS_IMAGE=1` enable knob is retired (harmless if left over).
pub fn bypassed() -> bool {
    std::env::var_os("MIRVM_NO_DEPS_IMAGE").is_some_and(|v| !v.is_empty())
}

fn deps_dir() -> PathBuf {
    crate::sysroot::cache_dir().join("deps")
}

/// Material for pre-key: in rustc_args, `--extern name=path` (two-arg form) and
/// `--extern=name=path` (single-arg form) path lists (deduped and sorted).
/// --extern without path (bare name) ⇒ None: v1 does not produce/use image (self-heal, prevent silent wrong-value).
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

/// Content stamping; any file not stably readable ⇒ None (do not produce/use image).
fn stamp_externs(paths: &[String]) -> Option<ExternStamps> {
    paths
        .iter()
        .map(|p| {
            let stamped =
                crate::utils::content::file_content_stamp(std::path::Path::new(p)).ok()?;
            Some((p.clone(), stamped.size, stamped.mtime_ns, stamped.digest))
        })
        .collect()
}

/// pre-key = fnv(build_id, base key, sorted stamps). Returns (key, stamp list);
/// any material unavailable ⇒ None (caller treats as "no image", full lowering self-heals).
/// **Empty --extern ⇒ None** (v1 boundary: dependency-free pure-std programs go through S4 base domain,
/// no "std residual shared image" is built for them — that territory is already covered by S4).
pub fn pre_key(rustc_args: &[String], base_key: &str) -> Option<(String, ExternStamps)> {
    let paths = extern_paths(rustc_args)?;
    if paths.is_empty() {
        return None;
    }
    let stamps = stamp_externs(&paths)?;
    let mut key = String::from(env!("MIRVM_BUILD_ID"));
    key.push('\u{1f}');
    key.push_str(base_key);
    for (p, size, mt, digest) in &stamps {
        key.push('\u{1f}');
        key.push_str(p);
        key.push('\u{1e}');
        key.push_str(&size.to_string());
        key.push('\u{1e}');
        key.push_str(&mt.to_string());
        key.push('\u{1e}');
        key.push_str(&crate::utils::content::digest_hex(digest));
    }
    let h = crate::lower::asm::fnv1a(key.as_bytes());
    if std::env::var_os("MIRVM_A2_DEBUG").is_some() {
        eprintln!("[a2-debug] pre-key={h:016x} externs={paths:?}");
    }
    Some((format!("{h:016x}"), stamps))
}

fn file_path(key: &str) -> PathBuf {
    deps_dir().join(format!("{key}.img"))
}

/// Load deps-image (pre-compiler call). `base` = already-present base (key and fp validated in layers);
/// success = BaseImage to push onto stack (its key = pre-key, used for L2 key chain).
/// any mismatch/failure = None (full lowering self-heals, main path stays silent — stderr participates in native diff).
pub fn try_load(
    rustc_args: &[String],
    base: &crate::baseimage::BaseImage,
) -> Option<crate::baseimage::BaseImage> {
    let (key, stamps) = pre_key(rustc_args, &base.key)?;
    let data = std::fs::read(file_path(&key)).ok()?;
    let mut f: DepsFile = postcard::from_bytes(&data).ok()?;
    f.module.rebuild_load_map();
    f.module.rebuild_fn_addrs();
    // Exact-equality validation: build id, base key, stamp list (collision immune), layered lowering fingerprint
    if f.build_id != env!("MIRVM_BUILD_ID")
        || f.base_key != base.key
        || f.extern_stamps != stamps
        || f.lowering_fp != base.lowering_fp
    {
        return None;
    }
    // Frozen area must actually land in spline k=0 domain (defense: reject if file swapped or domain stolen)
    let frozen_ok = f.module.frozen.as_ref().is_some_and(|fr| {
        fr.at_fixed_base() && fr.home() == crate::vm::engine::addrlayout::image_addr(0)
    });
    if !frozen_ok {
        return None;
    }
    crate::vm::engine::verify::module_with_prefix(
        &f.module,
        crate::vm::engine::verify::Prefix {
            funcs: base.module.funcs.len(),
            tls: base.module.tls.len(),
            asm: base.module.asm_sites.len(),
        },
    )
    .ok()?;
    // required .so removed ⇒ miss falls back to cold-path self-heal (same contract as ircache)
    if !f
        .module
        .required_native_libs
        .iter()
        .all(|p| std::path::Path::new(&**p).is_file())
    {
        return None;
    }
    let module = f.module;
    Some(crate::baseimage::BaseImage {
        fn_by_sym: module.exports.clone(),
        entry_by_sym: f.fn_entry_syms.into_iter().collect(),
        static_by_sym: f.static_syms.into_iter().collect(),
        tls_by_sym: f.tls_syms.into_iter().collect(),
        lowering_fp: f.lowering_fp,
        key,
        module,
    })
}

/// Persist and push split product (A2-2 pipeline): write to disk if cacheable (atomic publish), returning the
/// BaseImage to push. Write failure / not cacheable / pre-key unavailable = no file for this run only (subsequent runs do full lowering
/// self-heal), returned stack-layer key degrades to process-unique placeholder — L2 key chain naturally invalid across runs, never false-hit.
pub fn store_and_wrap(
    rustc_args: &[String],
    base_key: &str,
    fp: (bool, bool, bool),
    image: crate::lower::SplitImage,
) -> crate::baseimage::BaseImage {
    let mut bi = image.into_base_image(fp);
    // Cacheability criterion: frozen area must be in fixed spline k=0 domain (prerequisite for embedded absolute addresses in snapshot to remain stable across processes)
    // Entry stub code area follows same rule (P1: fn-ptr value domain = stub code address). Foreign
    // symbols from P2 onward go through GOT slots indirectly (decision-history §7.5c): image-side GOT table travels with
    // the file and is refilled with this process's real values after loading during startup — no longer a write-to-disk obstacle.
    let cacheable = bi.module.frozen.as_ref().is_some_and(|fr| {
        fr.at_fixed_base() && fr.home() == crate::vm::engine::addrlayout::image_addr(0)
    }) && (bi.module.entry_stub_sites.is_empty()
        || bi.module.entry_stubs.at_fixed_base());
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
            build_id: env!("MIRVM_BUILD_ID"),
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
            let written = path.parent().is_some_and(|dir| {
                if std::fs::create_dir_all(dir).is_err() {
                    return false;
                }
                let tmp = dir.join(format!(
                    ".{}.tmp-{}",
                    path.file_name().unwrap_or_default().to_string_lossy(),
                    std::process::id()
                ));
                if std::fs::write(&tmp, &bytes).is_err() || std::fs::rename(&tmp, &path).is_err() {
                    let _ = std::fs::remove_file(&tmp);
                    return false;
                }
                true
            });
            if written {
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
