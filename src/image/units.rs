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

use crate::utils::content::FileStamp;

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
