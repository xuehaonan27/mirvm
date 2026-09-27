//! The fragment store: canonical function bodies, addressed by the BLAKE3 of their bytes and shared
//! by every layer that stores lowered code.
//!
//! A fragment is one body's canonical form ([`crate::vm::ir::frag`]), so its id is the same in every
//! program, project and stack that lowers that body the same way. What the store holds is therefore
//! **not** keyed by a manifest or a generation: fragments are reached through the manifests that
//! reference them, and a fragment no manifest references is collectable. That is also why a reader
//! may serve an id an older build wrote — content addressing is self-consistent — while the manifests
//! that name them stay generational.
//!
//! Fragments are stored in **packs**: append-only files holding one session's new fragments, each
//! followed by a sorted index. One file per fragment is the obvious shape and the wrong one —
//! measured on a small dependency closure a fragment averages ~131 bytes, so a filesystem block per
//! fragment would spend roughly thirty times the data. A pack costs one file per publishing session
//! and gives readers a binary search instead of a directory scan.
//!
//! ```text
//! header   magic "MIRVMFRG" (8) | version u8 | reserved [u8; 3] | count u32
//! records  count x ( id [u8; 32] | len u32 | fragment bytes )      in id order
//! index    count x ( id [u8; 32] | offset u64 | len u32 )          sorted by id
//! footer   index_offset u64 | index_len u32 | magic "MIRVMEND" (8)
//! ```
//!
//! All integers are little-endian and offsets are from the start of the file. Records are written in
//! id order and the pack is named by the BLAKE3 of its bytes, so two sessions that publish the same
//! fragments produce the same pack name and the second publish is a no-op instead of a duplicate.
//! Every read re-hashes the fragment's bytes against the id it was asked for: a corrupted pack is a
//! miss, never a wrong body.
//!
//! **Collection**: a pack holds no liveness of its own. A fragment is live exactly when a
//! current-generation manifest names it (`image::collect` marks them), which is what
//! [`inventory_marked`] scores a pack against; until the sweep runs, the family's unit of removal is
//! the whole family, the same all-or-nothing contract the other keyed families have.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Pack header magic.
const MAGIC: &[u8; 8] = b"MIRVMFRG";
/// Pack footer magic, so a truncated pack is recognisable without trusting its lengths.
const FOOTER_MAGIC: &[u8; 8] = b"MIRVMEND";
/// Pack format version. A reader refuses a version it does not know.
const VERSION: u8 = 1;
/// Header length: magic + version + reserved + count.
const HEADER_LEN: usize = 8 + 1 + 3 + 4;
/// One index entry: id + record offset + fragment length.
const INDEX_ENTRY_LEN: usize = 32 + 8 + 4;
/// Footer length: index offset + index length + magic.
const FOOTER_LEN: usize = 8 + 4 + 8;
/// A record's own prefix: id + length.
const RECORD_PREFIX_LEN: u64 = 32 + 4;

/// A pack's file name for a completed pack: the BLAKE3 of its bytes.
fn pack_path(dir: &Path, bytes: &[u8]) -> PathBuf {
    let digest = crate::vm::ir::frag::id_of(bytes);
    dir.join(format!(
        "{}.pack",
        crate::utils::content::digest_hex(&digest)
    ))
}

/// The family's publish/collect lock.
///
/// A publisher holds it shared across the fragment publish *and* the manifest that names them: a sweep
/// between those two writes would see a fragment no manifest names yet and drop it, leaving a live
/// manifest pointing at nothing. A sweep holds it exclusive across its mark and its sweep for the same
/// reason in the other direction. `flock` releases it when the process dies, so a crash cannot wedge
/// the family.
pub(crate) struct Lock {
    file: std::fs::File,
}

impl Lock {
    pub(crate) fn shared(dir: &Path) -> std::io::Result<Lock> {
        Lock::take(dir, false)
    }

    pub(crate) fn exclusive(dir: &Path) -> std::io::Result<Lock> {
        Lock::take(dir, true)
    }

    fn take(dir: &Path, exclusive: bool) -> std::io::Result<Lock> {
        std::fs::create_dir_all(dir)?;
        // The lock's identity is the file, not its contents: it is never written, and a reader that
        // only opens packs ignores it (its extension is not `.pack`).
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(dir.join("lock"))?;
        crate::os::fs::flock(std::os::fd::AsRawFd::as_raw_fd(&file), exclusive)?;
        Ok(Lock { file })
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = crate::os::fs::unlock(std::os::fd::AsRawFd::as_raw_fd(&self.file));
    }
}

/// One session's fragments, staged before they are published as one pack.
#[derive(Default)]
pub(crate) struct Session {
    fragments: BTreeMap<[u8; 32], Vec<u8>>,
}

/// What one publish wrote.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Published {
    /// Fragments the store did not already hold, and whose bytes are therefore in this pack.
    pub stored: usize,
    /// Fragments another pack already held.
    pub deduped: usize,
    /// Size of the pack this publish wrote; zero when nothing had to be stored.
    pub bytes: u64,
}

impl Session {
    /// Stage one fragment's canonical bytes under its content address ([`crate::vm::ir::frag::id_of`],
    /// the one definition of a fragment id).
    pub(crate) fn add(&mut self, bytes: Vec<u8>) {
        self.fragments
            .insert(crate::vm::ir::frag::id_of(&bytes), bytes);
    }

    /// Publish everything the family does not already hold as one pack. Publishing nothing writes no
    /// file: an empty pack is pure overhead in every later index scan.
    pub(crate) fn publish(self) -> std::io::Result<Published> {
        self.publish_in(&crate::store::FRAGS.dir())
    }

    pub(crate) fn publish_in(self, dir: &Path) -> std::io::Result<Published> {
        let mut published = Published::default();
        if self.fragments.is_empty() {
            return Ok(published);
        }
        let index = Index::load_in(dir);
        let mut new: BTreeMap<[u8; 32], Vec<u8>> = BTreeMap::new();
        for (id, bytes) in self.fragments {
            if index.contains(&id) {
                published.deduped += 1;
            } else {
                published.stored += 1;
                new.insert(id, bytes);
            }
        }
        if new.is_empty() {
            return Ok(published);
        }
        let bytes = pack_bytes(&new);
        std::fs::create_dir_all(dir)?;
        published.bytes = bytes.len() as u64;
        crate::store::publish_bytes(&pack_path(dir, &bytes), &bytes)?;
        Ok(published)
    }
}

/// Serialize one pack: header, records in id order, the sorted index, the footer.
fn pack_bytes(fragments: &BTreeMap<[u8; 32], Vec<u8>>) -> Vec<u8> {
    let count = fragments.len() as u32;
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.extend_from_slice(&[0u8; 3]);
    out.extend_from_slice(&count.to_le_bytes());
    let mut index: Vec<u8> = Vec::with_capacity(fragments.len() * INDEX_ENTRY_LEN);
    for (id, bytes) in fragments {
        let offset = out.len() as u64;
        out.extend_from_slice(id);
        out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(bytes);
        index.extend_from_slice(id);
        index.extend_from_slice(&offset.to_le_bytes());
        index.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    }
    let index_offset = out.len() as u64;
    out.extend_from_slice(&index);
    out.extend_from_slice(&index_offset.to_le_bytes());
    out.extend_from_slice(&(index.len() as u32).to_le_bytes());
    out.extend_from_slice(FOOTER_MAGIC);
    out
}

/// One pack's index, read from its trailing index.
struct PackIndex {
    path: PathBuf,
    /// (id, record offset, fragment length), sorted by id.
    entries: Vec<([u8; 32], u64, u32)>,
}

impl PackIndex {
    /// Read a pack's header, footer and index. Anything inconsistent is `None`: a pack this reader
    /// cannot trust is skipped, so a half-written or foreign file cannot fail a run.
    fn load(path: &Path) -> Option<PackIndex> {
        let mut file = std::fs::File::open(path).ok()?;
        let len = file.metadata().ok()?.len();
        if len < (HEADER_LEN + FOOTER_LEN) as u64 {
            return None;
        }
        let mut header = [0u8; HEADER_LEN];
        file.read_exact(&mut header).ok()?;
        if &header[..8] != MAGIC || header[8] != VERSION {
            return None;
        }
        let count = u32::from_le_bytes(header[12..16].try_into().ok()?) as u64;
        let mut footer = [0u8; FOOTER_LEN];
        file.seek(SeekFrom::Start(len - FOOTER_LEN as u64)).ok()?;
        file.read_exact(&mut footer).ok()?;
        if &footer[12..] != FOOTER_MAGIC {
            return None;
        }
        let index_offset = u64::from_le_bytes(footer[..8].try_into().ok()?);
        let index_len = u32::from_le_bytes(footer[8..12].try_into().ok()?) as u64;
        if index_len != count * INDEX_ENTRY_LEN as u64
            || index_offset + index_len != len - FOOTER_LEN as u64
        {
            return None;
        }
        let mut index = vec![0u8; index_len as usize];
        file.seek(SeekFrom::Start(index_offset)).ok()?;
        file.read_exact(&mut index).ok()?;
        let mut entries = Vec::with_capacity(count as usize);
        for entry in index.as_chunks::<INDEX_ENTRY_LEN>().0 {
            let id: [u8; 32] = entry[..32].try_into().ok()?;
            let offset = u64::from_le_bytes(entry[32..40].try_into().ok()?);
            let fragment_len = u32::from_le_bytes(entry[40..44].try_into().ok()?);
            if offset + RECORD_PREFIX_LEN + u64::from(fragment_len) > index_offset {
                return None;
            }
            entries.push((id, offset, fragment_len));
        }
        if !entries.windows(2).all(|w| w[0].0 < w[1].0) {
            return None;
        }
        Some(PackIndex {
            path: path.to_path_buf(),
            entries,
        })
    }

    /// Where this pack holds `id`, if it does.
    fn find(&self, id: &[u8; 32]) -> Option<(u64, u32)> {
        self.entries
            .binary_search_by(|entry| entry.0.cmp(id))
            .ok()
            .map(|at| (self.entries[at].1, self.entries[at].2))
    }

    /// Read up to `ids` from this pack in one open; an id already in `out` is left alone.
    fn read_into(&self, ids: &[[u8; 32]], out: &mut HashMap<[u8; 32], Vec<u8>>) {
        let mut file = match std::fs::File::open(&self.path) {
            Ok(file) => file,
            Err(_) => return,
        };
        for id in ids {
            if out.contains_key(id) {
                continue;
            }
            let Some((offset, len)) = self.find(id) else {
                continue;
            };
            if file
                .seek(SeekFrom::Start(offset + RECORD_PREFIX_LEN))
                .is_err()
            {
                continue;
            }
            let mut bytes = vec![0u8; len as usize];
            if file.read_exact(&mut bytes).is_err() {
                continue;
            }
            // Content addressing is the whole integrity story: the bytes must hash back to the id.
            if crate::vm::ir::frag::id_of(&bytes) == *id {
                out.insert(*id, bytes);
            }
        }
    }
}

/// Every fragment the packs hold, loaded once per writer session or reader batch.
pub(crate) struct Index {
    packs: Vec<PackIndex>,
}

impl Index {
    /// Load the index of every readable pack in the family.
    pub(crate) fn load() -> Index {
        Self::load_in(&crate::store::FRAGS.dir())
    }

    pub(crate) fn load_in(dir: &Path) -> Index {
        let mut packs = Vec::new();
        if let Ok(read) = std::fs::read_dir(dir) {
            let mut paths: Vec<PathBuf> = read
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "pack"))
                .collect();
            // Stable order: two readers of one store agree on which pack answers first.
            paths.sort();
            for path in paths {
                if let Some(pack) = PackIndex::load(&path) {
                    packs.push(pack);
                }
            }
        }
        Index { packs }
    }

    pub(crate) fn contains(&self, id: &[u8; 32]) -> bool {
        self.packs.iter().any(|pack| pack.find(id).is_some())
    }

    /// Read a batch, skipping ids the family does not hold or cannot verify.
    pub(crate) fn read_many(&self, ids: &[[u8; 32]]) -> HashMap<[u8; 32], Vec<u8>> {
        let mut out = HashMap::with_capacity(ids.len());
        for pack in &self.packs {
            pack.read_into(ids, &mut out);
        }
        out
    }
}

/// What one sweep took back.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Sweep {
    /// Packs with no live fragment at all.
    pub removed: u64,
    /// Packs rewritten with only their live fragments.
    pub compacted: u64,
    /// Bytes the family no longer holds.
    pub reclaimed: u64,
}

impl Sweep {
    pub(crate) fn is_empty(&self) -> bool {
        self.removed == 0 && self.compacted == 0
    }
}

/// Drop every fragment no current-generation manifest names.
///
/// A pack is the unit of removal but not of liveness: a pack with no live fragment is deleted whole,
/// and a pack whose live share is at most half its bytes is rewritten with only its live records under
/// a new content-addressed name — published before the old file is removed, so a reader that has not
/// seen the removal still finds every live fragment. A mostly live pack is left alone: a copy of it
/// would buy back few bytes. The caller holds the family's exclusive lock.
pub(crate) fn sweep_in(dir: &Path, live: &HashSet<[u8; 32]>) -> std::io::Result<Sweep> {
    let mut sweep = Sweep::default();
    for pack in Index::load_in(dir).packs {
        let size = std::fs::metadata(&pack.path).map(|m| m.len()).unwrap_or(0);
        let live_bytes: u64 = pack
            .entries
            .iter()
            .filter(|entry| live.contains(&entry.0))
            .map(|entry| u64::from(entry.2))
            .sum();
        if live_bytes == 0 {
            std::fs::remove_file(&pack.path)?;
            sweep.removed += 1;
            sweep.reclaimed += size;
            continue;
        }
        let total: u64 = pack
            .entries
            .iter()
            .map(|entry| u64::from(entry.2))
            .sum();
        if live_bytes * 2 >= total {
            continue;
        }
        let ids: Vec<[u8; 32]> = pack
            .entries
            .iter()
            .map(|entry| entry.0)
            .filter(|id| live.contains(id))
            .collect();
        let mut kept = HashMap::with_capacity(ids.len());
        pack.read_into(&ids, &mut kept);
        // A fragment the pack cannot hand back is kept where it is: compaction must never be the
        // reason a live fragment disappears.
        if kept.len() != ids.len() {
            continue;
        }
        let bytes = pack_bytes(&kept.into_iter().collect());
        crate::store::publish_bytes(&pack_path(dir, &bytes), &bytes)?;
        std::fs::remove_file(&pack.path)?;
        sweep.compacted += 1;
        sweep.reclaimed += size;
    }
    Ok(sweep)
}

/// What the family holds, beyond the bytes on disk.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Inventory {
    pub packs: u64,
    /// Fragment records, counting repeats across packs.
    pub records: u64,
    /// Bytes of distinct fragments: what the store would take if every pack were repacked.
    pub unique_bytes: u64,
    /// Bytes of fragments a later pack stored again.
    pub duplicate_bytes: u64,
    /// What the manifests keep alive, when the caller marked them.
    pub liveness: Option<Liveness>,
}

/// Distinct fragments against the mark: a fragment no current-generation manifest names is dead.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Liveness {
    /// Distinct fragments the manifests name.
    pub fragments: u64,
    /// Their bytes: what a sweep must keep.
    pub bytes: u64,
    /// Bytes of distinct fragments no manifest names: what a sweep could take back.
    pub dead_bytes: u64,
}

/// Size the family by its own indexes: distinct fragments, and what repeated publishing cost. Each
/// distinct fragment is additionally scored against `live` when the caller marked the manifests.
pub(crate) fn inventory_marked(live: Option<&HashSet<[u8; 32]>>) -> Inventory {
    let index = Index::load();
    let mut seen: HashMap<[u8; 32], u32> = HashMap::new();
    let mut inv = Inventory {
        packs: index.packs.len() as u64,
        liveness: live.map(|_| Liveness::default()),
        ..Default::default()
    };
    for pack in &index.packs {
        for (id, _, len) in &pack.entries {
            inv.records += 1;
            let copies = seen.entry(*id).or_insert(0);
            *copies += 1;
            if *copies == 1 {
                inv.unique_bytes += u64::from(*len);
                if let (Some(live), Some(liveness)) = (live, inv.liveness.as_mut()) {
                    if live.contains(id) {
                        liveness.fragments += 1;
                        liveness.bytes += u64::from(*len);
                    } else {
                        liveness.dead_bytes += u64::from(*len);
                    }
                }
            } else {
                inv.duplicate_bytes += u64::from(*len);
            }
        }
    }
    inv
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(tag: u8) -> Vec<u8> {
        vec![1u8, tag, tag.wrapping_add(7)]
    }

    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mirvm-frags-test-{}-{}-{tag}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Write one pack of `tags` directly, as another session's publish would.
    fn write_pack(dir: &Path, tags: &[u8]) {
        let fragments: BTreeMap<[u8; 32], Vec<u8>> = tags
            .iter()
            .map(|tag| {
                let bytes = body(*tag);
                (crate::vm::ir::frag::id_of(&bytes), bytes)
            })
            .collect();
        let bytes = pack_bytes(&fragments);
        std::fs::write(pack_path(dir, &bytes), &bytes).unwrap();
    }

    #[test]
    fn pack_round_trip_keeps_every_fragment_and_its_index() {
        let dir = tmp("round-trip");
        write_pack(&dir, &[3, 9, 1]);
        let index = Index::load_in(&dir);
        assert_eq!(index.packs.len(), 1);
        assert_eq!(index.packs[0].entries.len(), 3);
        for tag in [3u8, 9, 1] {
            let bytes = body(tag);
            let id = crate::vm::ir::frag::id_of(&bytes);
            assert!(index.contains(&id), "index missed a fragment");
            assert_eq!(index.read_many(&[id]).get(&id), Some(&bytes));
        }
        assert!(!index.contains(&crate::vm::ir::frag::id_of(&body(200))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_sweep_deletes_dead_packs_and_compacts_mostly_dead_ones() {
        let dir = tmp("sweep");
        // Three publishes: one entirely dead, one a quarter live, one three quarters live.
        write_pack(&dir, &[1, 2, 3, 4]);
        write_pack(&dir, &[5, 6, 7, 8]);
        write_pack(&dir, &[9, 10, 11, 12]);
        let live: HashSet<[u8; 32]> = [5u8, 9, 10, 11]
            .iter()
            .map(|tag| crate::vm::ir::frag::id_of(&body(*tag)))
            .collect();

        let swept = sweep_in(&dir, &live).unwrap();
        assert_eq!(swept.removed, 1, "the pack with nothing live goes whole");
        assert_eq!(swept.compacted, 1, "the mostly dead pack is rewritten");
        assert!(swept.reclaimed > 0);
        assert_eq!(Index::load_in(&dir).packs.len(), 2);
        // Every live fragment is still readable, which is the property the rewrite must not break: the
        // compacted pack is published under its new name before the old file is removed.
        let ids: Vec<[u8; 32]> = live.iter().copied().collect();
        assert_eq!(Index::load_in(&dir).read_many(&ids).len(), live.len());
        // The mostly live pack keeps its dead fragment; a sweep of the same mark has nothing left to
        // do beyond that.
        let again = sweep_in(&dir, &live).unwrap();
        assert!(again.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_pack_whose_bytes_do_not_hash_to_their_id_is_a_miss() {
        let dir = tmp("corrupt");
        let bytes = body(4);
        let id = crate::vm::ir::frag::id_of(&bytes);
        let mut corrupted = bytes.clone();
        corrupted[1] ^= 0xff;
        // Write the record with a stale id, the way a corrupted inode would present it.
        let mut pack = Vec::new();
        pack.extend_from_slice(MAGIC);
        pack.push(VERSION);
        pack.extend_from_slice(&[0u8; 3]);
        pack.extend_from_slice(&1u32.to_le_bytes());
        pack.extend_from_slice(&id);
        pack.extend_from_slice(&(corrupted.len() as u32).to_le_bytes());
        pack.extend_from_slice(&corrupted);
        let index_offset = pack.len() as u64;
        pack.extend_from_slice(&id);
        pack.extend_from_slice(&16u64.to_le_bytes());
        pack.extend_from_slice(&(corrupted.len() as u32).to_le_bytes());
        pack.extend_from_slice(&index_offset.to_le_bytes());
        pack.extend_from_slice(&(INDEX_ENTRY_LEN as u32).to_le_bytes());
        pack.extend_from_slice(FOOTER_MAGIC);
        std::fs::write(dir.join("stale.pack"), &pack).unwrap();

        let index = Index::load_in(&dir);
        assert_eq!(index.packs.len(), 1);
        assert!(index.contains(&id), "the index does name the id");
        assert!(
            index.read_many(&[id]).is_empty(),
            "a wrong body must never be served"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_truncated_or_foreign_pack_is_skipped() {
        let dir = tmp("broken");
        std::fs::write(dir.join("short.pack"), b"MIRVMFRG").unwrap();
        std::fs::write(dir.join("garbage.pack"), vec![0u8; 200]).unwrap();
        std::fs::write(dir.join("not-a-pack.bin"), vec![0u8; 200]).unwrap();
        let index = Index::load_in(&dir);
        assert!(index.packs.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn publishing_twice_stores_one_copy_and_names_the_pack_by_its_content() {
        let dir = tmp("dedupe");
        let mut first = Session::default();
        for tag in [1u8, 2, 3] {
            first.add(body(tag));
        }
        let published = first.publish_in(&dir).unwrap();
        assert_eq!(
            published,
            Published {
                stored: 3,
                deduped: 0,
                bytes: published.bytes
            }
        );

        // The same fragments again: nothing new, and no second file.
        let mut again = Session::default();
        for tag in [1u8, 2, 3] {
            again.add(body(tag));
        }
        let published = again.publish_in(&dir).unwrap();
        assert_eq!(published.stored, 0);
        assert_eq!(published.deduped, 3);
        assert_eq!(published.bytes, 0);

        // One new fragment: one pack holding only what was missing.
        let mut third = Session::default();
        for tag in [3u8, 4] {
            third.add(body(tag));
        }
        let published = third.publish_in(&dir).unwrap();
        assert_eq!(published.stored, 1);
        assert_eq!(published.deduped, 1);

        let index = Index::load_in(&dir);
        assert_eq!(index.packs.len(), 2);
        for tag in [1u8, 2, 3, 4] {
            let id = crate::vm::ir::frag::id_of(&body(tag));
            assert!(index.contains(&id));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_duplicate_across_packs_is_reported_as_dead_bytes() {
        let dir = tmp("inventory");
        write_pack(&dir, &[1, 2]);
        write_pack(&dir, &[2, 3]);
        let index = Index::load_in(&dir);
        let mut seen: HashMap<[u8; 32], u32> = HashMap::new();
        let mut records = 0u64;
        let mut duplicate_bytes = 0u64;
        for pack in &index.packs {
            for (id, _, len) in &pack.entries {
                records += 1;
                let copies = seen.entry(*id).or_insert(0);
                *copies += 1;
                if *copies > 1 {
                    duplicate_bytes += u64::from(*len);
                }
            }
        }
        assert_eq!(records, 4);
        assert_eq!(duplicate_bytes, body(2).len() as u64);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_pack_name_is_the_content_and_the_index_is_sorted() {
        let fragments: BTreeMap<[u8; 32], Vec<u8>> = [2u8, 8]
            .iter()
            .map(|tag| {
                let bytes = body(*tag);
                (crate::vm::ir::frag::id_of(&bytes), bytes)
            })
            .collect();
        let first = pack_bytes(&fragments);
        assert_eq!(first, pack_bytes(&fragments), "one fragment set, one pack");
        let index_offset = u64::from_le_bytes(
            first[first.len() - FOOTER_LEN..first.len() - FOOTER_LEN + 8]
                .try_into()
                .unwrap(),
        ) as usize;
        let entries = &first[index_offset..first.len() - FOOTER_LEN];
        let ids: Vec<&[u8]> = entries
            .as_chunks::<INDEX_ENTRY_LEN>()
            .0
            .iter()
            .map(|entry| &entry[..32])
            .collect();
        assert!(
            ids[0] < ids[1],
            "the index must be sorted for binary search"
        );
    }
}
