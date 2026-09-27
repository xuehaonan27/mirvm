//! The units one program's dependency closure is made of, and the home rule that decides which of
//! them a lowered instance belongs to.
//!
//! A **unit** is one build-graph compilation unit whose source is a registry or git crate — the
//! granularity at which a lowered dependency is shared. The table is built before the compiler
//! session by the track that owns the build graph (cargoless: the resolver's plan), so a unit's
//! identity is its rlib file: `unit_key = digest(build_id, base key, FileStamp(rlib))`. The rlib
//! content pins version, resolved features, cfgs and — through rustc's SVH chain — the unit's whole
//! dependency sub-closure, and no `tcx` is needed to compute it.
//!
//! **The home rule** (dep-sharing-design §3.2) places every instance the base does not already
//! provide: `home(inst)` is the first unit, in topological order, whose closure contains both the
//! crate that defines `inst` and every unit its generic arguments mention. A unit's closure is itself
//! plus its transitive dependencies, so a home's contents can only reference the home or a layer
//! below it — the property the split relies on and that makes a unit valid above any stack that
//! satisfies its symbols. An instance mentioning the local crate (the program's own, above every
//! unit) or defined by a crate no unit covers is **residue**: it stays in the delta, duplicated per
//! program, which is what the purity ledger already measures.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::SystemTime;

use crate::store::entry;
use crate::utils::content::FileStamp;
use crate::vm::instance::Instance;

use super::manifest;

/// One unit of the table, in topological order (dependencies before dependents).
pub(crate) struct Unit {
    /// The crate name the session sees (the `--extern` key / lib target name).
    pub name: Box<str>,
    /// The rlib this unit was compiled to: how the session matches a crate to its unit.
    pub rlib: PathBuf,
    /// Direct dependencies, by index.
    pub deps: Vec<u32>,
    /// This unit's closure: itself plus its transitive dependencies.
    pub closure: BTreeSet<u32>,
}

/// Where one instance belongs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Home {
    pub unit: u32,
    /// The instance names no unit: it is below all of them (see [`UnitTable::home_of`]).
    pub unattached: bool,
}

/// Every unit of one program's dependency closure.
pub(crate) struct UnitTable {
    units: Vec<Unit>,
}

impl UnitTable {
    /// Build the table and compute each unit's closure. `units` must be in topological order, so a
    /// dependency's index is always below its dependent's.
    pub(crate) fn new(units: Vec<(Box<str>, PathBuf, Vec<u32>)>) -> UnitTable {
        let mut table = UnitTable {
            units: units
                .into_iter()
                .map(|(name, rlib, deps)| Unit {
                    name,
                    // The session hands back the canonical path it resolved the crate from
                    // (`used_crate_source`), so the table canonicalizes too; a path that cannot be
                    // canonicalized is kept as given and simply never matches.
                    rlib: std::fs::canonicalize(&rlib).unwrap_or(rlib),
                    deps,
                    closure: BTreeSet::new(),
                })
                .collect(),
        };
        for index in 0..table.units.len() {
            let mut closure = BTreeSet::from([index as u32]);
            for &dep in &table.units[index].deps {
                closure.extend(table.units[dep as usize].closure.iter().copied());
            }
            table.units[index].closure = closure;
        }
        table
    }

    pub(crate) fn get(&self, index: u32) -> Option<&Unit> {
        self.units.get(index as usize)
    }

    pub(crate) fn len(&self) -> usize {
        self.units.len()
    }

    /// The unit a crate was compiled to, matched by the rlib path the session resolved it from.
    /// Two semver-forked versions of one crate are two units with two paths, so the path is the
    /// identity and the name is not.
    pub(crate) fn by_rlib(&self, rlib: &Path) -> Option<u32> {
        self.units
            .iter()
            .position(|unit| unit.rlib == rlib)
            .map(|index| index as u32)
    }

    /// The home of an instance defined by `def` and mentioning the units in `mentioned`: the first
    /// unit in topological order whose closure covers all of them. `def` is `None` when the defining
    /// crate is not a unit — a sysroot crate is below every unit and so constrains nothing. `None` is
    /// residue (see the module header): no unit can hold the instance without referencing a layer
    /// above itself.
    pub(crate) fn home_of(&self, def: Option<u32>, mentioned: &BTreeSet<u32>) -> Option<Home> {
        self.units
            .iter()
            .position(|unit| {
                def.is_none_or(|def| unit.closure.contains(&def))
                    && mentioned.iter().all(|m| unit.closure.contains(m))
            })
            .map(|unit| Home {
                unit: unit as u32,
                // An instance that names no unit at all is below every unit: the base lacks it, any
                // unit's closure covers it, and the first unit therefore hosts it. The probe counts
                // these separately because a base-owned home would take them out of every manifest.
                unattached: def.is_none() && mentioned.is_empty(),
            })
    }

    /// This unit's key: the build id, the stack below it and the rlib it was compiled to.
    ///
    /// The stack below is part of the key because a unit's bindings name symbols of the layers under
    /// it; the same rlib above a different base is a different unit.
    pub(crate) fn key_of(&self, index: u32, base_key: &str) -> Option<String> {
        let unit = self.get(index)?;
        let stamp = FileStamp::of(&unit.rlib).ok()?;
        let mut key = crate::store::entry::Key::new();
        key.part(base_key);
        key.part(&format!(
            "{}\u{1e}{}\u{1e}{}\u{1e}{}",
            stamp.path,
            stamp.size,
            stamp.mtime_ns,
            crate::utils::content::digest_hex(&stamp.digest)
        ));
        Some(key.digest())
    }
}

static TABLE: OnceLock<UnitTable> = OnceLock::new();

/// Hand the session the table the track that owns the build graph computed. Called before the
/// compiler session starts; the process-global shape is deliberate (threading it through the driver's
/// signatures is the perturbation `open-issues.md` E35 is about).
pub(crate) fn install(table: UnitTable) {
    let _ = TABLE.set(table);
}

/// The table, when the running track has one (the cargoless path; the Cargo track has no build graph).
pub(crate) fn current() -> Option<&'static UnitTable> {
    TABLE.get()
}

/// How many manifests per unit stay on disk. A running program pins the digest it loaded through the
/// L2 key chain, so the newest few are what a reader can still need; older ones are pruned on publish
/// (their readers simply rebuild).
const KEEP_PER_UNIT: usize = 3;

/// The extension every unit manifest carries. The name is the one place that spells it, so a reader
/// that walks the family (`image::collect`) cannot disagree with the writer.
pub(crate) const EXT: &str = "unit";

/// One unit manifest's file name: the unit's key and the digest of the manifest's own bytes.
fn file_name(unit_key: &str, digest: &str) -> String {
    format!("{unit_key}-{digest}.{EXT}")
}

/// Every manifest file for one unit, newest first (mtime, then name for a stable tie break).
fn manifests_of(dir: &Path, unit_key: &str) -> Vec<(PathBuf, String)> {
    let prefix = format!("{unit_key}-");
    let mut found: Vec<(PathBuf, String, SystemTime)> = Vec::new();
    if let Ok(read) = std::fs::read_dir(dir) {
        for entry in read.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != EXT) {
                continue;
            }
            let suffix = format!(".{EXT}");
            let Some(digest) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.strip_prefix(&prefix))
                .and_then(|rest| rest.strip_suffix(&suffix))
                .map(str::to_string)
            else {
                continue;
            };
            let mtime = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            found.push((path, digest, mtime));
        }
    }
    found.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.1.cmp(&b.1)));
    found
        .into_iter()
        .map(|(path, digest, _)| (path, digest))
        .collect()
}

/// Load one unit's newest usable manifest, keyed by its rlib. `stack` is what is already below it
/// (the base and every unit loaded before it in topological order), which is both what its symbols
/// resolve against and what it is verified against.
///
/// Every failure — no manifest, a header that no longer matches, a fragment the store lost, a symbol
/// the stack does not provide — is a miss that leaves the unit to this session's lowering.
pub(crate) fn try_load(
    index: u32,
    base: &super::BaseImage,
    stack: &super::ImageStack,
    base_key: &str,
) -> Option<super::BaseImage> {
    let table = current()?;
    let unit = table.get(index)?;
    let unit_key = table.key_of(index, base_key)?;
    let dir = crate::store::UNITS.dir();
    for (path, digest) in manifests_of(&dir, &unit_key) {
        if let Some(layer) = load_one(index, &path, &digest, &unit_key, unit, base, stack) {
            return Some(layer);
        }
    }
    None
}

fn load_one(
    index: u32,
    path: &Path,
    digest: &str,
    unit_key: &str,
    unit: &Unit,
    base: &super::BaseImage,
    stack: &super::ImageStack,
) -> Option<super::BaseImage> {
    let reason = |why: &str| {
        if crate::options::a2_debug() {
            eprintln!("[a2-debug] unit {unit_key} manifest {digest} unusable: {why}");
        }
    };
    let Some(data) = std::fs::read(path).ok() else {
        reason("unreadable");
        return None;
    };
    let Ok(mut f) = postcard::from_bytes::<manifest::File>(&data) else {
        reason("undecodable");
        return None;
    };
    let Ok(stamp) = FileStamp::of(&unit.rlib) else {
        reason("the unit's rlib cannot be stamped");
        return None;
    };
    if !entry::is_current_generation(&f.build_id) {
        reason("another build wrote it");
        return None;
    }
    if f.base_key != base.key {
        reason("built above a different base");
        return None;
    }
    if f.unit_key.as_deref() != Some(unit_key) {
        reason("key mismatch");
        return None;
    }
    if f.lowering_fp != base.lowering_fp {
        reason("built with a different lowering fingerprint");
        return None;
    }
    if f.extern_stamps != vec![stamp] {
        reason("the unit's rlib is not the one it was built from");
        return None;
    }
    // The frozen bytes must land where the manifest's baked link addresses point.
    if !entry::frozen_at(
        &f.module,
        Some(crate::os_arch::addrspace::image_addr(f.home)),
    ) {
        reason("its frozen region is not in the slot it records");
        return None;
    }
    let Some(frozen) = f
        .module
        .frozen
        .as_ref()
        .map(|snapshot| (snapshot.home() as u64, snapshot.bytes().len() as u64))
    else {
        reason("no frozen region");
        return None;
    };
    let unit_view = super::deps::unit_of(&stack.layer_ranges(), stack.below().prefix, &f.module, f.home, frozen);
    // Bindings and the id-bearing tables resolve against the layers below this unit, and the whole
    // module is verified against the stack it is about to join.
    let symbols = manifest::Symbols::of(stack.layers());
    if let Err(error) = manifest::rehydrate_module(&mut f.module, &f.funcs, &unit_view, &symbols) {
        reason(&error);
        return None;
    }
    let tls_by_sym = match f.tables.restore(&mut f.module, &unit_view, &symbols) {
        Ok((_, tls)) => tls,
        Err(error) => {
            reason(&error);
            return None;
        }
    };
    if crate::options::a2_debug() {
        eprintln!(
            "[a2-debug] unit {unit_key}: {} records, {} module funcs, {} names, home {}",
            f.funcs.len(),
            f.module.funcs.len(),
            f.module.function_names.len(),
            f.home
        );
    }
    let mut instance = match Instance::materialize(&f.module) {
        Ok(instance) => instance,
        Err(error) => {
            if crate::options::a2_debug() {
                eprintln!("[a2-debug] unit {unit_key} rejected: {error}");
            }
            return None;
        }
    };
    if let Err(error) = crate::vm::verify::module_below(&f.module, &instance, stack.below()) {
        if crate::options::a2_debug() {
            eprintln!("[a2-debug] unit {unit_key} rejected: {error}");
        }
        return None;
    }
    instance.asm_stub_addrs = crate::lower::asm::materialize(&f.module.asm_sites);
    if !entry::native_libs_present(&f.module) {
        return None;
    }
    let module = f.module;
    Some(super::BaseImage {
        fn_by_sym: module.exports.clone(),
        entry_by_sym: f.fn_entry_syms.into_iter().collect(),
        static_by_sym: f.static_syms.into_iter().collect(),
        tls_by_sym,
        lowering_fp: f.lowering_fp,
        key: format!("{unit_key}-{digest}"),
        module,
        instance,
        unit: Some(index),
    })
}

/// Persist one home as its unit's manifest: the fragments it references, then the manifest itself,
/// then prune the unit's older digests. A layer the store cannot hold stays in memory for this run
/// and is simply not cached.
pub(crate) fn store(
    index: u32,
    stack: &super::ImageStack,
    fp: (bool, bool, bool),
    image: crate::lower::SplitImage,
) -> super::BaseImage {
    let mut bi = image.into_base_image(fp);
    let Some(table) = current() else {
        return super::deps::degraded(bi);
    };
    let Some(base) = stack.base_image() else {
        return super::deps::degraded(bi);
    };
    let Some(unit) = table.get(index) else {
        return super::deps::degraded(bi);
    };
    let (Some(unit_key), Ok(stamp)) = (table.key_of(index, &base.key), FileStamp::of(&unit.rlib))
    else {
        return super::deps::degraded(bi);
    };
    let below = stack.below();
    // The writer applies the loader's predicate against the same stack. The frozen area must sit at a
    // fixed base — any slot will do, because the manifest records which one (its `home`) and the
    // loader restores it there.
    let publishable = entry::snapshot_is_publishable(&bi.module, &bi.instance, None);
    let cacheable =
        publishable && crate::vm::verify::module_below(&bi.module, &bi.instance, below).is_ok();
    if !cacheable {
        if crate::options::a2_debug() {
            eprintln!(
                "[a2-debug] unit {unit_key} not stored: {}",
                if publishable {
                    "verification against the stack below failed"
                } else {
                    "snapshot is not in its spline slot"
                }
            );
        }
        return super::deps::degraded(bi);
    }
    let Some(snapshot) = bi.module.frozen.as_ref() else {
        return super::deps::degraded(bi);
    };
    // The slot the arenas actually landed in: the manifest records it, and the loader restores there.
    // A unit's arenas always occupy one, so a module whose frozen bytes are not in the spline was not
    // publishable in the first place.
    let Some(home) = crate::os_arch::addrspace::image_slot(snapshot.home()) else {
        return super::deps::degraded(bi);
    };
    let frozen = (snapshot.home() as u64, snapshot.bytes().len() as u64);
    let unit_view = super::deps::unit_of(&stack.layer_ranges(), below.prefix, &bi.module, home, frozen);
    let symbols = manifest::Symbols::of(stack.layers());
    let mut session = crate::store::frags::Session::default();
    let records = match manifest::project_module(&mut bi.module, &unit_view, &symbols, &mut session)
    {
        Ok(records) => records,
        Err(error) => {
            if crate::options::a2_debug() {
                eprintln!("[a2-debug] unit {unit_key} not stored: {error}");
            }
            return super::deps::degraded(bi);
        }
    };
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
    // The module's id-bearing tables leave it in canonical form before serializing, and come back
    // right after so this session keeps running the layer it wrote.
    let tables =
        match manifest::Tables::capture(&mut bi.module, &unit_view, &symbols, &bi.tls_by_sym) {
            Ok(tables) => tables,
            Err(error) => {
                if crate::options::a2_debug() {
                    eprintln!("[a2-debug] unit {unit_key} not stored: {error}");
                }
                return super::deps::degraded(bi);
            }
        };
    let file = manifest::FileRef {
        build_id: crate::options::build::BUILD_ID,
        base_key: &base.key,
        unit_key: Some(&unit_key),
        home,
        lowering_fp: fp,
        extern_stamps: std::slice::from_ref(&stamp),
        module: &bi.module,
        funcs: &records,
        fn_entry_syms: &fn_entry_syms,
        static_syms: &static_syms,
        tables: &tables,
    };
    let Ok(bytes) = manifest::encode(&file) else {
        return super::deps::degraded(bi);
    };
    // Fragments first: a manifest that names a fragment the store does not hold is a manifest the
    // loader will refuse.
    let published = session.publish();
    if published.is_err() {
        return super::deps::degraded(bi);
    }
    let dir = crate::store::UNITS.dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return super::deps::degraded(bi);
    }
    let digest = crate::utils::content::digest_hex(&crate::vm::ir::frag::id_of(&bytes));
    if crate::store::publish_bytes(&dir.join(file_name(&unit_key, &digest)), &bytes).is_err() {
        return super::deps::degraded(bi);
    }
    for (path, _) in manifests_of(&dir, &unit_key)
        .into_iter()
        .skip(KEEP_PER_UNIT)
    {
        let _ = std::fs::remove_file(path);
    }
    bi.key = format!("{unit_key}-{digest}");
    bi
}

#[cfg(test)]
mod tests {
    use super::*;

    /// diamond: base <- mid <- top, and base <- side
    fn table() -> UnitTable {
        UnitTable::new(vec![
            ("base".into(), PathBuf::from("/d/libbase.rlib"), vec![]),
            ("mid".into(), PathBuf::from("/d/libmid.rlib"), vec![0]),
            ("side".into(), PathBuf::from("/d/libside.rlib"), vec![0]),
            ("top".into(), PathBuf::from("/d/libtop.rlib"), vec![1, 2]),
        ])
    }

    #[test]
    fn closures_are_transitive_and_include_self() {
        let table = table();
        assert_eq!(table.get(0).unwrap().closure, BTreeSet::from([0]));
        assert_eq!(table.get(1).unwrap().closure, BTreeSet::from([0, 1]));
        assert_eq!(table.get(3).unwrap().closure, BTreeSet::from([0, 1, 2, 3]));
    }

    #[test]
    fn home_is_the_first_unit_whose_closure_covers_everything() {
        let table = table();
        // A base instance stays in base, not in the first unit that happens to contain base.
        let home = |unit: u32, unattached: bool| Home { unit, unattached };
        assert_eq!(
            table.home_of(Some(0), &BTreeSet::new()),
            Some(home(0, false))
        );
        // mid::f::<base::T> is mid's, even though base is below it.
        assert_eq!(
            table.home_of(Some(1), &BTreeSet::from([0])),
            Some(home(1, false))
        );
        // mid::f::<side::T>: mid cannot hold it (side is not in mid's closure), so it is top's.
        assert_eq!(
            table.home_of(Some(1), &BTreeSet::from([2])),
            Some(home(3, false))
        );
        // side::f::<mid::T> is top's for the same reason.
        assert_eq!(
            table.home_of(Some(2), &BTreeSet::from([1])),
            Some(home(3, false))
        );
        // An instance no unit defines (a sysroot crate's) constrains nothing, so the smallest
        // closure that covers its arguments holds it — and it is marked as naming no unit.
        assert_eq!(table.home_of(None, &BTreeSet::new()), Some(home(0, true)));
        assert_eq!(
            table.home_of(None, &BTreeSet::from([2])),
            Some(home(2, false))
        );
        // Nothing covers an instance that spans two siblings under a unit that is not in the table.
        let partial = UnitTable::new(vec![
            ("base".into(), PathBuf::from("/d/libbase.rlib"), vec![]),
            ("mid".into(), PathBuf::from("/d/libmid.rlib"), vec![0]),
            ("side".into(), PathBuf::from("/d/libside.rlib"), vec![0]),
        ]);
        assert_eq!(partial.home_of(Some(1), &BTreeSet::from([2])), None);
    }

    #[test]
    fn a_crate_matches_its_unit_by_artifact_path() {
        let table = table();
        assert_eq!(table.by_rlib(Path::new("/d/libmid.rlib")), Some(1));
        assert_eq!(table.by_rlib(Path::new("/d/libother.rlib")), None);
    }

    #[test]
    fn a_unit_key_carries_the_file_that_was_compiled() {
        let dir = std::env::temp_dir().join(format!(
            "mirvm-units-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let rlib = dir.join("libx.rlib");
        std::fs::write(&rlib, b"first").unwrap();
        let table = UnitTable::new(vec![("x".into(), rlib.clone(), vec![])]);
        let first = table.key_of(0, "base").unwrap();
        std::fs::write(&rlib, b"other").unwrap();
        let second = table.key_of(0, "base").unwrap();
        assert_ne!(first, second, "the rlib content is the unit's identity");
        assert_ne!(first, table.key_of(0, "autre").unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
