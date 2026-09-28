//! Collection: which fragments the manifests keep alive.
//!
//! A fragment carries no generation of its own — it is reachable only through a manifest, and
//! manifests are generational — so liveness is *marked* from the manifests rather than counted.
//! Refcounts are rejected on purpose: a crash between two counter updates would strand bytes with no
//! way back or drop bytes a live manifest still names.
//!
//! Both manifest families use one format ([`manifest::File`]), so one walk serves them: the closure
//! manifest in `cache/deps` and the per-unit manifests in `cache/units`. A file from another build is
//! no more reachable than a missing one, so only current-generation files mark anything.

use std::collections::HashSet;
use std::path::Path;

use crate::store::entry;

use super::manifest;

/// The fragment ids every current-generation manifest names. A fragment the store lost stays marked:
/// the manifest that names it is a miss the cold path rebuilds, never a reason to drop the file.
pub(crate) fn live_fragments(root: &Path) -> HashSet<[u8; 32]> {
    let mut live = HashSet::new();
    mark(&crate::store::DEPS.dir_in(root), "img", &mut live);
    mark(
        &crate::store::UNITS.dir_in(root),
        super::units::EXT,
        &mut live,
    );
    live
}

/// Mark and sweep in one locked pass: a fragment no current-generation manifest names is dropped,
/// and so is a pack that held only those.
///
/// The lock is exclusive across the mark *and* the sweep: a publisher that added a manifest in
/// between would have its fragments look dead here, and its manifest would then name fragments that
/// are gone. A missing fragment is a miss the cold path rebuilds, never a wrong value, but it is also
/// a share that was thrown away.
pub(crate) fn collect(root: &Path) -> std::io::Result<crate::store::frags::Sweep> {
    let frags = crate::store::FRAGS.dir_in(root);
    let _sweeping = crate::store::frags::Lock::exclusive(&frags)?;
    let live = live_fragments(root);
    crate::store::frags::sweep_in(&frags, &live)
}

/// Mark one family's manifests. An unreadable or undecodable file is skipped rather than fatal:
/// collection reads a store other processes write.
fn mark(dir: &Path, ext: &str, live: &mut HashSet<[u8; 32]>) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|found| found != ext) {
            continue;
        }
        let Ok(data) = std::fs::read(&path) else {
            continue;
        };
        let Ok(file) = postcard::from_bytes::<manifest::File>(&data) else {
            continue;
        };
        if !entry::is_current_generation(&file.build_id) {
            continue;
        }
        live.extend(file.funcs.iter().map(|record| record.fragment));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A manifest file's bytes with one record per fragment. Only the header material the mark reads
    /// and the records matter here, so the module is empty.
    fn manifest(build_id: &str, fragments: &[[u8; 32]]) -> Vec<u8> {
        let file = manifest::File {
            build_id: build_id.to_string(),
            base_key: "base".into(),
            unit_key: None,
            below: Vec::new(),
            home: 0,
            lowering_fp: (false, false, false),
            extern_stamps: Vec::new(),
            module: crate::vm::ir::Module::default(),
            funcs: fragments
                .iter()
                .map(|fragment| manifest::Record {
                    fragment: *fragment,
                    bindings: Vec::new(),
                })
                .collect(),
            tables: manifest::Tables::default(),
            fn_entry_syms: Vec::new(),
            static_syms: Vec::new(),
        };
        postcard::to_stdvec(&file).unwrap()
    }

    fn id(tag: u8) -> [u8; 32] {
        [tag; 32]
    }

    fn root(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("mirvm-collect-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn only_current_generation_manifests_mark_fragments() {
        let root = root("mark");
        let current = crate::options::build::BUILD_ID;
        std::fs::create_dir_all(crate::store::UNITS.dir_in(&root)).unwrap();
        std::fs::create_dir_all(crate::store::DEPS.dir_in(&root)).unwrap();
        // A unit manifest and a closure manifest of this build both mark, a file from another build
        // does not, and neither does a file whose name lies about its extension.
        std::fs::write(
            crate::store::UNITS
                .dir_in(&root)
                .join(format!("key-{:064x}.unit", 1)),
            manifest(current, &[id(1), id(2)]),
        )
        .unwrap();
        std::fs::write(
            crate::store::DEPS.dir_in(&root).join("closure.img"),
            manifest(current, &[id(2), id(3)]),
        )
        .unwrap();
        std::fs::write(
            crate::store::UNITS
                .dir_in(&root)
                .join(format!("key-{:064x}.unit", 4)),
            manifest("some-other-build", &[id(4)]),
        )
        .unwrap();
        std::fs::write(crate::store::UNITS.dir_in(&root).join("notes.txt"), b"junk").unwrap();

        let live = live_fragments(&root);
        assert_eq!(live, HashSet::from([id(1), id(2), id(3)]));
        let _ = std::fs::remove_dir_all(&root);
    }
}
