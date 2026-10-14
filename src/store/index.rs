//! Rebuildable catalogues of immutable pack indexes. Directory entries decide which packs exist;
//! a catalogue only saves opening each one to rediscover its index. Consumers validate a selected
//! pack's file identity and content before using its records.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::{Serialize, de::DeserializeOwned};

const CATALOGUE: &str = "index-v1";

#[derive(Serialize, serde::Deserialize)]
pub(super) struct PackFile {
    #[serde(skip)]
    pub path: PathBuf,
    identity: crate::os::fs::FileIdentity,
    #[serde(skip)]
    current: OnceLock<bool>,
}

impl PackFile {
    pub fn new(path: &Path, metadata: &std::fs::Metadata) -> Self {
        Self {
            path: path.to_path_buf(),
            identity: crate::os::fs::FileIdentity::of(metadata),
            current: OnceLock::new(),
        }
    }

    fn matches(&self, metadata: &std::fs::Metadata) -> bool {
        let current = self.identity == crate::os::fs::FileIdentity::of(metadata);
        if !current {
            invalidate(self.path.parent().expect("pack directory"));
        }
        current
    }

    /// Writers validate a selected pack once before deduplicating against its index.
    pub fn is_current(&self) -> bool {
        *self
            .current
            .get_or_init(|| match std::fs::metadata(&self.path) {
                Ok(metadata) => self.matches(&metadata),
                Err(_) => {
                    invalidate(self.path.parent().expect("pack directory"));
                    false
                }
            })
    }

    /// Recheck the opened descriptor, so replacing a selected pack cannot make a cached offset
    /// address different bytes. A change invalidates discovery; the ordinary miss rebuilds it.
    pub fn open(&self) -> Option<std::fs::File> {
        let file = match std::fs::File::open(&self.path) {
            Ok(file) => file,
            Err(_) => {
                invalidate(self.path.parent().expect("pack directory"));
                return None;
            }
        };
        self.matches(&file.metadata().ok()?).then_some(file)
    }
}

fn paths(dir: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "pack"))
        .collect();
    paths.sort();
    paths
}

/// Inventory and collection read the authoritative indexes, including packs no lookup touches.
#[cfg(any(feature = "compiler", test))]
pub(super) fn scan<T>(dir: &Path, mut read: impl FnMut(&Path) -> Option<T>) -> Vec<(PathBuf, T)> {
    paths(dir)
        .into_iter()
        .filter_map(|path| read(&path).map(|index| (path, index)))
        .collect()
}

pub(super) fn invalidate(dir: &Path) {
    let _ = std::fs::remove_file(dir.join(CATALOGUE));
}

/// Reconcile a cached catalogue with current filenames. Added packs are read, removed packs are
/// dropped, and concurrent publishers may replace the catalogue in either order: the next reader
/// reconciles again. Relative filenames keep a copied store independent of its original location.
pub(super) fn load<T: Serialize + DeserializeOwned>(
    dir: &Path,
    magic: &[u8; 8],
    mut read: impl FnMut(&Path) -> Option<T>,
) -> Vec<(PathBuf, T)> {
    let cached = std::fs::read(dir.join(CATALOGUE)).ok().and_then(|bytes| {
        let payload = bytes.get(40..)?;
        if bytes.get(..8)? != magic || bytes[8..40] != *blake3::hash(payload).as_bytes() {
            return None;
        }
        postcard::from_bytes::<BTreeMap<PathBuf, T>>(payload).ok()
    });
    let mut changed = cached.is_none();
    let mut cached = cached.unwrap_or_default();
    let mut entries = Vec::new();
    for path in paths(dir) {
        let name = path.file_name().expect("directory entry");
        let index = cached.remove(Path::new(name)).or_else(|| {
            changed = true;
            read(&path)
        });
        if let Some(index) = index {
            entries.push((path, index));
        }
    }
    changed |= !cached.is_empty();
    if changed && !entries.is_empty() {
        let relative: BTreeMap<_, _> = entries
            .iter()
            .map(|(path, index)| (Path::new(path.file_name().expect("directory entry")), index))
            .collect();
        if let Ok(payload) = postcard::to_stdvec(&relative) {
            let mut bytes = Vec::with_capacity(40 + payload.len());
            bytes.extend_from_slice(magic);
            bytes.extend_from_slice(blake3::hash(&payload).as_bytes());
            bytes.extend_from_slice(&payload);
            // Failure only loses an optimization. Packs remain authoritative and independently
            // readable, and atomic replacement keeps concurrent readers off partial catalogues.
            let _ = super::publish_bytes(&dir.join(CATALOGUE), &bytes);
        }
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn catalogue_reconciles_additions_removals_corruption_and_relocation() {
        let dir = std::env::temp_dir().join(format!("mirvm-index-{}", std::process::id()));
        let moved = dir.with_extension("moved");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&moved);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.pack"), b"first").unwrap();
        let reads = Cell::new(0);
        let read = |path: &Path| {
            reads.set(reads.get() + 1);
            std::fs::read(path).ok()
        };
        assert_eq!(load(&dir, b"TESTIDX1", read)[0].1, b"first");
        assert_eq!(reads.get(), 1);
        assert_eq!(load(&dir, b"TESTIDX1", read)[0].1, b"first");
        assert_eq!(reads.get(), 1, "unchanged packs must not be reopened");

        std::fs::write(dir.join("b.pack"), b"second").unwrap();
        assert_eq!(load(&dir, b"TESTIDX1", read).len(), 2);
        assert_eq!(reads.get(), 2, "only the new pack is read");
        std::fs::remove_file(dir.join("a.pack")).unwrap();
        assert_eq!(load(&dir, b"TESTIDX1", read)[0].1, b"second");
        assert_eq!(reads.get(), 2);

        std::fs::rename(&dir, &moved).unwrap();
        assert_eq!(load(&moved, b"TESTIDX1", read)[0].0, moved.join("b.pack"));
        assert_eq!(reads.get(), 2);
        let path = moved.join(CATALOGUE);
        let mut bytes = std::fs::read(&path).unwrap();
        *bytes.last_mut().unwrap() ^= 1;
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(load(&moved, b"TESTIDX1", read)[0].1, b"second");
        assert_eq!(reads.get(), 3, "a damaged catalogue is rebuilt");
        assert_eq!(load(&moved, b"TESTIDX2", read)[0].1, b"second");
        assert_eq!(reads.get(), 4, "index grammars cannot alias");
        std::fs::remove_dir_all(moved).unwrap();
    }

    #[test]
    fn a_rewritten_pack_invalidates_cached_offsets_even_with_restored_mtime() {
        let dir = std::env::temp_dir().join(format!("mirvm-index-edit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data.pack");
        std::fs::write(&path, b"old").unwrap();
        let metadata = std::fs::metadata(&path).unwrap();
        let pack = PackFile::new(&path, &metadata);
        assert!(pack.is_current());
        std::fs::write(dir.join(CATALOGUE), b"cached offsets").unwrap();
        // Replacement also changes the inode, so this does not depend on timestamp resolution.
        let replacement = dir.join("replacement");
        std::fs::write(&replacement, b"new").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&replacement)
            .unwrap()
            .set_modified(metadata.modified().unwrap())
            .unwrap();
        std::fs::rename(replacement, &path).unwrap();
        assert!(
            pack.open().is_none(),
            "opening rechecks even a previously validated pack"
        );
        assert!(!dir.join(CATALOGUE).exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
