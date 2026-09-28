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
//! The file is a **manifest**, not a second copy of the code: each function is one record naming a
//! fragment ([`crate::store::frags`]) and the binding table that gives that fragment's ordinals
//! meaning ([`super::manifest`]). Two programs whose closures lower the same bodies therefore share
//! them, and a version or feature change that leaves a body alone keeps its fragment. The manifest
//! itself is not required to be byte-deterministic: identity is carried by the key and file name, and
//! nothing compares contents.
//!
//! v1 boundary: an empty `--extern` (a pure-std program with no registry dependencies) neither
//! produces nor uses a manifest, since the std base already covers that territory.

use std::path::PathBuf;

use crate::store::entry;
use crate::utils::content::{FileStamp, digest_hex};
use crate::vm::ir;

use super::manifest;

/// The unit view of the layer being stored or loaded: its own id ranges and fixed domains, which is
/// what makes a binding's local half identical on both sides.
pub(crate) fn unit_of(
    below: &[manifest::LayerRanges],
    prefix: crate::vm::verify::Prefix,
    module: &ir::Module,
    home: usize,
    frozen: (u64, u64),
) -> manifest::Unit {
    manifest::Unit {
        funcs: (prefix.funcs as u32, module.funcs.len() as u32),
        tls: (prefix.tls as u32, module.tls.len() as u32),
        asm: (prefix.asm as u32, module.asm_sites.len() as u32),
        frozen,
        // A layer's entry stubs live in its own code spline slot, one 16 GiB step wide.
        code: (
            crate::os_arch::addrspace::image_code_addr(home) as u64,
            crate::os_arch::addrspace::IMAGE_CODE_STEP as u64,
        ),
        layers: Vec::new(),
        self_layer: 0,
    }
    // The layer map decides which ids this unit owns and which it has to name as symbols; the ids it
    // holds itself start where the last layer below ends.
    .above(below)
}

/// Whether the deps-image cache is bypassed. The cache is on by default and the only knob is
/// `MIRVM_NO_DEPS_IMAGE=1` (diagnostics / two-state cross-check).
pub fn bypassed() -> bool {
    crate::options::no_deps_image()
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
    if crate::options::a2_debug() {
        eprintln!("[a2-debug] pre-key={key} externs={paths:?}");
    }
    Some((key, stamps))
}

fn file_path(key: &str) -> PathBuf {
    crate::store::DEPS.dir().join(format!("{key}.img"))
}

/// Load the closure manifest; called before the compiler session. `base` is the already-present base,
/// whose key and fingerprint are validated in layers. On success the returned `BaseImage` is pushed
/// onto the stack, with its key set to the pre-key so the L2 key chain can use it. Any mismatch or
/// failure yields `None`: full lowering self-heals and the main path stays silent, because stderr
/// participates in native diffs.
pub fn try_load(
    rustc_args: &[String],
    base: &crate::image::BaseImage,
) -> Option<crate::image::BaseImage> {
    let (key, stamps) = pre_key(rustc_args, &base.key)?;
    let data = std::fs::read(file_path(&key)).ok()?;
    let mut f: manifest::File = postcard::from_bytes(&data).ok()?;
    // Exact-equality validation: build id, base key, stamp list (collision immune), layered lowering fingerprint
    if !entry::is_current_generation(&f.build_id)
        || f.base_key != base.key
        || f.extern_stamps != stamps
        || f.lowering_fp != base.lowering_fp
    {
        return None;
    }
    // The frozen bytes come back from the chunk store before anything about them can be checked: a
    // chunk the store lost is a miss, never a layer with a hole in its region.
    manifest::restore_frozen(&mut f).ok()?;
    // The frozen area must actually land in the spline k=0 domain the layer below expects: this
    // rejects a swapped file and a stolen domain alike.
    if !entry::frozen_at(&f.module, Some(crate::os_arch::addrspace::image_addr(0))) {
        return None;
    }
    // The bodies come back from the fragment store before anything can be verified: the frozen mapping
    // is what binds their addresses, and the verifier walks the bodies themselves.
    let frozen = f
        .module
        .frozen
        .as_ref()
        .map(|snapshot| (snapshot.home() as u64, snapshot.bytes().len() as u64))?;
    let prefix = crate::vm::verify::Prefix {
        funcs: base.module.funcs.len(),
        tls: base.module.tls.len(),
        asm: base.module.asm_sites.len(),
    };
    // The base is the whole stack below the closure: its ids are named by symbols, and the closure's
    // own ids start after them.
    let base_range = (
        (0, base.module.funcs.len() as u32),
        (0, base.module.tls.len() as u32),
    );
    let unit = unit_of(&[base_range], prefix, &f.module, 0, frozen);
    let symbols = manifest::Symbols::of(std::iter::once(base));
    // The bodies come back from the fragment store, then the id-bearing tables from the canonical
    // form: a missing fragment or an unresolvable symbol is a miss, never a partial layer.
    manifest::rehydrate_module(&mut f.module, &f.funcs, &unit, &symbols).ok()?;
    let (_, tls_syms) = f.tables.restore(&mut f.module, &unit, &symbols).ok()?;
    // The image is verified against the stack it is about to join: its frozen bytes may reference an
    // entry the base owns (`fn_entry_addr` reuses a base entry so one function keeps one address
    // identity), which is legitimate exactly because the base is below it.
    let mut instance = match crate::vm::instance::Instance::materialize(&f.module) {
        Ok(instance) => instance,
        Err(error) => {
            if crate::options::a2_debug() {
                eprintln!("[a2-debug] deps image {key} rejected: {error}");
            }
            return None;
        }
    };
    let base_links = base.entry_links().collect::<Vec<_>>();
    let below = crate::vm::verify::Below {
        prefix,
        entries: &base_links,
    };
    if crate::vm::verify::module_below(&f.module, &instance, below).is_err() {
        if crate::options::a2_debug() {
            eprintln!("[a2-debug] deps image {key} rejected: verification failed against the base");
        }
        return None;
    }
    // asm stubs are materialized from the module's recipes on every load, including this one.
    instance.asm_stub_addrs = crate::lower::asm::materialize(&f.module.asm_sites);
    // A removed required .so is a miss that falls back to the cold-path self-heal (same contract as
    // the L2 cache)
    if !entry::native_libs_present(&f.module) {
        return None;
    }
    let module = f.module;
    Some(crate::image::BaseImage {
        fn_by_sym: module.exports.clone(),
        entry_by_sym: f.fn_entry_syms.into_iter().collect(),
        static_by_sym: f.static_syms.into_iter().collect(),
        tls_by_sym: tls_syms,
        lowering_fp: f.lowering_fp,
        key,
        module,
        instance,
        // A closure image stands for the whole closure, not for one unit of a table.
        unit: None,
    })
}

/// Persist the split product and return the `BaseImage` to push. If it is cacheable the file is written
/// atomically. A write failure, a non-cacheable product, a product the loader would refuse or an
/// unavailable pre-key simply leaves no file for this run, so later runs do full lowering and self-heal;
/// the returned stack-layer key degrades to a process-unique placeholder, which makes the L2 key chain
/// invalid across runs rather than a false hit.
pub fn store_and_wrap(
    rustc_args: &[String],
    base_key: &str,
    fp: (bool, bool, bool),
    stack: &crate::image::ImageStack,
    image: crate::lower::SplitImage,
) -> crate::image::BaseImage {
    let mut bi = image.into_base_image(fp);
    // Cacheability: the frozen area must sit in the fixed spline k=0 domain the layer below expects,
    // a prerequisite for the snapshot's embedded absolute addresses to stay stable across processes.
    // Foreign symbols go through GOT slots: the image-side GOT table travels with the file and is
    // refilled with this process's real values at startup, so it does not block writing the file.
    let publishable = entry::snapshot_is_publishable(
        &bi.module,
        &bi.instance,
        Some(crate::os_arch::addrspace::image_addr(0)),
    );
    let below = stack.below();
    // The writer applies the loader's predicate against the same stack: publishing a file the loader
    // will refuse is worse than publishing none — the image is rebuilt and re-keyed every run while
    // its key chain is already written into the L2 entries above it.
    let verified = crate::vm::verify::module_below(&bi.module, &bi.instance, below).is_ok();
    let cacheable = publishable && verified;
    if !cacheable && crate::options::a2_debug() {
        eprintln!(
            "[a2-debug] split image not published: {}",
            if !publishable {
                "snapshot is not fixed-domain publishable"
            } else {
                "verification against the stack below failed"
            }
        );
    }
    let keyed = pre_key(rustc_args, base_key);
    if let (true, Some((key, stamps))) = (cacheable, keyed) {
        let frozen_home = bi
            .module
            .frozen
            .as_ref()
            .map(|snapshot| (snapshot.home() as u64, snapshot.bytes().len() as u64));
        let Some(unit) =
            frozen_home.map(|frozen| unit_of(&[], below.prefix, &bi.module, 0, frozen))
        else {
            return degraded(bi);
        };
        let symbols = manifest::Symbols::of(stack.layers());
        // The bodies, the id-bearing tables and the frozen region leave the module so the manifest can
        // be serialized without them, and come back right after: this session keeps running the layer
        // it just wrote.
        let mut session = crate::store::frags::Session::default();
        let mut chunks = crate::store::frags::Session::default();
        let projected = manifest::project_module(&mut bi.module, &unit, &symbols, &mut session)
            .and_then(|records| {
                let tables =
                    manifest::Tables::capture(&mut bi.module, &unit, &symbols, &bi.tls_by_sym)?;
                Ok((records, tables))
            });
        if let Err(error) = &projected
            && crate::options::a2_debug()
        {
            eprintln!("[a2-debug] closure manifest not written: {error}");
        }
        let taken = manifest::take_frozen(&mut bi.module, &mut chunks);
        let manifest = projected.ok().map(|(records, tables)| {
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
            let file = manifest::FileRef {
                build_id: crate::options::build::BUILD_ID,
                base_key,
                unit_key: None,
                // A closure manifest is the whole stack above the base, and the base key is checked
                // exactly; there is no prefix of layers to lay out.
                below: &[],
                home: 0,
                lowering_fp: fp,
                extern_stamps: &stamps,
                module: &bi.module,
                funcs: &records,
                tables: &tables,
                fn_entry_syms: &fn_entry_syms,
                static_syms: &static_syms,
                frozen: taken.as_ref().map(manifest::Frozen::reference),
            };
            manifest::encode(&file).map(|bytes| (bytes, tables))
        });
        if let Some(taken) = taken {
            taken.put_back(&mut bi.module);
        }
        let manifest_bytes = match manifest {
            Some(Ok((bytes, tables))) => Some((bytes, tables)),
            _ => None,
        };
        // The publish locks are held across the record packs and the closure manifest that names
        // them: a sweep between the writes would see records no manifest names yet and drop them.
        let _publishing = crate::store::frags::publish_lock();
        let chunks_store = crate::store::frozen::System::open();
        let _chunk_publishing = chunks_store.publish_lock();
        let held = chunks_store.publish(chunks).is_ok()
            && session
                .publish()
                .map(|published| {
                    if crate::options::a2_debug() {
                        eprintln!(
                            "[a2-debug] stored {} fragments ({} already shared), {} B",
                            published.stored, published.deduped, published.bytes
                        );
                    }
                })
                .is_ok();
        if let (true, Some((bytes, tables))) = (held, manifest_bytes) {
            let path = file_path(&key);
            let dir_exists = path
                .parent()
                .is_some_and(|dir| std::fs::create_dir_all(dir).is_ok());
            if dir_exists && crate::store::publish_bytes(&path, &bytes).is_ok() {
                // The manifest is written: put the layer's own ids back, so this session keeps
                // running the layer it just wrote (and the stack it hands to absorb carries them).
                let _ = tables.restore(&mut bi.module, &unit, &symbols);
                bi.key = key;
                return bi;
            }
        }
    }
    degraded(bi)
}

/// A layer the store cannot hold: the key degrades to a process-unique placeholder, which makes the L2
/// key chain invalid across runs rather than a false hit (the in-memory absorb for this run is
/// unaffected).
pub(crate) fn degraded(mut bi: crate::image::BaseImage) -> crate::image::BaseImage {
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
            "mirvm-image-deps-content-test-{}",
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
