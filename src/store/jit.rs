//! The JIT machine-code store: one entry per `(fragment id × jit-key)`, in packs.
//!
//! An entry is a compiled symbol's code plus everything the loading session must patch into it — the
//! relocation table, the unwind material and the jit-key it was built under. Its bytes are a pure
//! function of the fragment and the key, so the family is content-keyed like the fragment store, with
//! two differences that are both about *finding* things again:
//!
//! - the index is keyed by `key(fragment, jit_key)`, not by the record's own hash, because a lookup
//!   knows the key and not the bytes; each index entry also carries the fragment, so liveness can be
//!   decided without reading a record;
//! - a record carries the BLAKE3 of its bytes, so integrity is still one hash check per read.
//!
//! Liveness is the fragment store's: an entry is live while some current-generation manifest names its
//! fragment. Nothing else can say whether a *specific* compiled function will ever be asked for again,
//! so `purge --jit` takes the family whole, exactly like the fragment packs it is derived from.
//!
//! Publication is one pack per batch, named by the BLAKE3 of its bytes, written through the store's
//! atomic publish. A key the family already holds is not written again, so republishing a batch is
//! idempotent.

use std::collections::{BTreeMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use super::frags::{Inventory, Liveness};

/// Pack header magic. Distinct from the fragment pack's, so neither reader mistakes the other's file.
const MAGIC: &[u8; 8] = b"MIRVMJIT";
/// Pack footer magic, so a truncated pack is recognisable without trusting its lengths.
const FOOTER_MAGIC: &[u8; 8] = b"MIRVMJND";
/// Pack format version. A reader refuses a version it does not know.
const VERSION: u8 = 1;
/// Header length: magic + version + reserved + count.
const HEADER_LEN: usize = 8 + 1 + 3 + 4;
/// One index entry: key + fragment + record offset + entry length.
const INDEX_ENTRY_LEN: usize = 32 + 32 + 8 + 4;
/// Footer length: index offset + index length + magic.
const FOOTER_LEN: usize = 8 + 4 + 8;
/// A record's own prefix: the BLAKE3 of the entry bytes + their length.
const RECORD_PREFIX_LEN: u64 = 32 + 4;

/// The store key of one entry: the fragment and the jit-key, hashed together with the family's
/// version byte so a key grammar change cannot alias an old key.
pub(crate) fn key(fragment: &[u8; 32], jit_key: &[u8; 32]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&[VERSION]);
    hasher.update(fragment);
    hasher.update(jit_key);
    *hasher.finalize().as_bytes()
}

/// A pack's file name: the BLAKE3 of its bytes, so a republish of the same batch writes the same file.
fn pack_path(dir: &Path, bytes: &[u8]) -> PathBuf {
    let digest = *blake3::hash(bytes).as_bytes();
    dir.join(format!(
        "{}.pack",
        crate::utils::content::digest_hex(&digest)
    ))
}

/// One batch of entries, staged before it is published as one pack.
#[derive(Default)]
pub(crate) struct Session {
    /// key -> (fragment, entry bytes).
    entries: BTreeMap<[u8; 32], ([u8; 32], Vec<u8>)>,
    bytes: u64,
}

/// What one publish wrote.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Published {
    /// Entries the store did not already hold, and whose bytes are therefore in this pack.
    pub stored: usize,
    /// Entries another pack already held.
    pub deduped: usize,
    /// Size of the pack this publish wrote; zero when nothing had to be stored.
    pub bytes: u64,
}

/// Publish a batch once it is worth a file: below this many bytes, staging is cheaper than a pack.
pub(crate) const BATCH_BYTES: u64 = 64 << 10;

impl Session {
    /// Stage one entry under the key its fragment and jit-key make.
    pub(crate) fn add(&mut self, fragment: [u8; 32], jit_key: [u8; 32], bytes: Vec<u8>) {
        let key = key(&fragment, &jit_key);
        self.bytes += bytes.len() as u64;
        self.entries.insert(key, (fragment, bytes));
    }

    /// Whether the staged batch is worth publishing now.
    pub(crate) fn worth_publishing(&self) -> bool {
        self.bytes >= BATCH_BYTES
    }

    /// Publish everything the family does not already hold as one pack. Publishing nothing writes no
    /// file: an empty pack is pure overhead in every later index scan.
    pub(crate) fn publish(self) -> std::io::Result<Published> {
        self.publish_in(&crate::store::JIT.dir())
    }

    pub(crate) fn publish_in(self, dir: &Path) -> std::io::Result<Published> {
        let mut published = Published::default();
        if self.entries.is_empty() {
            return Ok(published);
        }
        let index = Index::load_in(dir);
        let mut new: BTreeMap<[u8; 32], ([u8; 32], Vec<u8>)> = BTreeMap::new();
        for (key, (fragment, bytes)) in self.entries {
            if index.contains(&key) {
                published.deduped += 1;
            } else {
                published.stored += 1;
                new.insert(key, (fragment, bytes));
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

/// Serialize one pack: header, records in key order, the sorted index, the footer.
fn pack_bytes(entries: &BTreeMap<[u8; 32], ([u8; 32], Vec<u8>)>) -> Vec<u8> {
    let count = entries.len() as u32;
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.extend_from_slice(&[0u8; 3]);
    out.extend_from_slice(&count.to_le_bytes());
    let mut index: Vec<u8> = Vec::with_capacity(entries.len() * INDEX_ENTRY_LEN);
    for (key, (fragment, bytes)) in entries {
        let offset = out.len() as u64;
        let hash = *blake3::hash(bytes).as_bytes();
        out.extend_from_slice(&hash);
        out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(bytes);
        index.extend_from_slice(key);
        index.extend_from_slice(fragment);
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

/// What the family holds for one key.
#[derive(Debug)]
pub(crate) enum Lookup {
    /// No pack holds it: the ordinary miss.
    Absent,
    /// A pack holds it and what is there cannot be used. A hash that does not match, a length that does
    /// not agree, or a record that cannot be read — the caller compiles, and says so.
    Bad(String),
    Entry(Vec<u8>),
}

/// One index entry: the key, the fragment it was compiled from, and where the record is.
#[derive(Clone, Copy)]
struct Indexed {
    key: [u8; 32],
    fragment: [u8; 32],
    offset: u64,
    len: u32,
}

/// One pack's index, read from its trailing index.
struct PackIndex {
    path: PathBuf,
    entries: Vec<Indexed>,
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
            let key: [u8; 32] = entry[..32].try_into().ok()?;
            let fragment: [u8; 32] = entry[32..64].try_into().ok()?;
            let offset = u64::from_le_bytes(entry[64..72].try_into().ok()?);
            let len = u32::from_le_bytes(entry[72..76].try_into().ok()?);
            if offset + RECORD_PREFIX_LEN + u64::from(len) > index_offset {
                return None;
            }
            entries.push(Indexed {
                key,
                fragment,
                offset,
                len,
            });
        }
        if !entries.windows(2).all(|w| w[0].key < w[1].key) {
            return None;
        }
        Some(PackIndex {
            path: path.to_path_buf(),
            entries,
        })
    }

    fn find(&self, key: &[u8; 32]) -> Option<Indexed> {
        self.entries
            .binary_search_by(|entry| entry.key.cmp(key))
            .ok()
            .map(|at| self.entries[at])
    }

    /// Read one entry, verifying it against the hash the record carries.
    fn read(&self, entry: &Indexed) -> Option<Vec<u8>> {
        let mut file = std::fs::File::open(&self.path).ok()?;
        file.seek(SeekFrom::Start(entry.offset)).ok()?;
        let mut prefix = [0u8; RECORD_PREFIX_LEN as usize];
        file.read_exact(&mut prefix).ok()?;
        let hash: [u8; 32] = prefix[..32].try_into().ok()?;
        let len = u32::from_le_bytes(prefix[32..36].try_into().ok()?);
        if len != entry.len {
            return None;
        }
        let mut bytes = vec![0u8; len as usize];
        file.read_exact(&mut bytes).ok()?;
        // Content addressing is the integrity story: the bytes must hash back to what the record says.
        (*blake3::hash(&bytes).as_bytes() == hash).then_some(bytes)
    }
}

/// Every entry the packs hold, loaded once per reader batch or writer session.
pub(crate) struct Index {
    packs: Vec<PackIndex>,
}

impl Index {
    pub(crate) fn load() -> Index {
        Self::load_in(&crate::store::JIT.dir())
    }

    pub(crate) fn load_in(dir: &Path) -> Index {
        let mut packs = Vec::new();
        if let Ok(read) = std::fs::read_dir(dir) {
            let mut paths: Vec<PathBuf> = read
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "pack"))
                .collect();
            paths.sort();
            for path in paths {
                if let Some(pack) = PackIndex::load(&path) {
                    packs.push(pack);
                }
            }
        }
        Index { packs }
    }

    pub(crate) fn contains(&self, key: &[u8; 32]) -> bool {
        self.packs.iter().any(|pack| pack.find(key).is_some())
    }

    /// What the family holds for one key.
    pub(crate) fn lookup(&self, key: &[u8; 32]) -> Lookup {
        for pack in &self.packs {
            let Some(entry) = pack.find(key) else {
                continue;
            };
            return match pack.read(&entry) {
                Some(bytes) => Lookup::Entry(bytes),
                // The pack holds the key and not the entry: unreadable, or bytes that do not hash back
                // to what the record says they are. The caller compiles instead, and counts it.
                None => Lookup::Bad(format!("{} holds {key:?} unreadable", pack.path.display())),
            };
        }
        Lookup::Absent
    }

    #[cfg(test)]
    pub(crate) fn read(&self, key: &[u8; 32]) -> Option<Vec<u8>> {
        match self.lookup(key) {
            Lookup::Entry(bytes) => Some(bytes),
            _ => None,
        }
    }
}

/// Size the family by its own indexes, and score distinct entries against `live`: an entry is live
/// while its fragment is, because only the manifests can say whether the code will be asked for again.
pub(crate) fn inventory_marked(live: Option<&HashSet<[u8; 32]>>) -> Inventory {
    inventory_marked_in(&crate::store::JIT.dir(), live)
}

pub(crate) fn inventory_marked_in(dir: &Path, live: Option<&HashSet<[u8; 32]>>) -> Inventory {
    let index = Index::load_in(dir);
    let mut seen: HashSet<[u8; 32]> = HashSet::new();
    let mut inventory = Inventory {
        packs: index.packs.len() as u64,
        liveness: live.map(|_| Liveness::default()),
        ..Inventory::default()
    };
    for pack in &index.packs {
        for entry in &pack.entries {
            inventory.records += 1;
            if !seen.insert(entry.key) {
                inventory.duplicate_bytes += u64::from(entry.len);
                continue;
            }
            inventory.unique_bytes += u64::from(entry.len);
            if let (Some(live), Some(liveness)) = (live, inventory.liveness.as_mut()) {
                if live.contains(&entry.fragment) {
                    liveness.fragments += 1;
                    liveness.bytes += u64::from(entry.len);
                } else {
                    liveness.dead_bytes += u64::from(entry.len);
                }
            }
        }
    }
    inventory
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mirvm-jit-store-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fragment(tag: u8) -> [u8; 32] {
        [tag; 32]
    }

    fn jit_key(tag: u8) -> [u8; 32] {
        [tag.wrapping_add(0x80); 32]
    }

    fn entry(tag: u8) -> Vec<u8> {
        vec![tag; 64 + usize::from(tag)]
    }

    fn staged(dir: &Path, tags: &[u8]) -> Published {
        let mut session = Session::default();
        for tag in tags {
            session.add(fragment(*tag), jit_key(*tag), entry(*tag));
        }
        session.publish_in(dir).unwrap()
    }

    /// One pack round trip: every entry comes back byte for byte, under its own key.
    #[test]
    fn a_pack_round_trip_returns_every_entry() {
        let dir = tmp("round-trip");
        let published = staged(&dir, &[1, 2, 3]);
        assert_eq!(published.stored, 3);
        assert!(published.bytes > 0);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);

        let index = Index::load_in(&dir);
        for tag in [1u8, 2, 3] {
            let key = key(&fragment(tag), &jit_key(tag));
            assert_eq!(index.read(&key).as_deref(), Some(entry(tag).as_slice()));
        }
        assert!(index.read(&key(&fragment(9), &jit_key(9))).is_none());
    }

    /// A key the family already holds is not written again, and the pack name is its content.
    #[test]
    fn publishing_twice_stores_one_copy_and_names_the_pack_by_its_content() {
        let dir = tmp("dedupe");
        staged(&dir, &[1, 2]);
        let first: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().collect();
        assert_eq!(first.len(), 1);

        let again = staged(&dir, &[1, 2]);
        assert_eq!(again.stored, 0);
        assert_eq!(again.deduped, 2);
        assert_eq!(again.bytes, 0);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);

        let third = staged(&dir, &[1, 2, 3]);
        assert_eq!((third.stored, third.deduped), (1, 2));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
    }

    /// Bytes that do not hash to what the record says are a miss, not a wrong value.
    #[test]
    fn a_record_whose_bytes_do_not_hash_back_is_a_miss() {
        let dir = tmp("integrity");
        staged(&dir, &[4]);
        let index = Index::load_in(&dir);
        let key = key(&fragment(4), &jit_key(4));
        assert!(index.read(&key).is_some());

        // Flip one byte of the record body.
        let pack_path = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .next()
            .unwrap()
            .path();
        let mut bytes = std::fs::read(&pack_path).unwrap();
        let at = bytes.len() - FOOTER_LEN - 1;
        bytes[at] ^= 0xff;
        std::fs::write(&pack_path, &bytes).unwrap();
        let index = Index::load_in(&dir);
        assert!(index.read(&key).is_none(), "a corrupted entry was read");
    }

    /// A pack this reader did not write, or one that is truncated, is skipped rather than trusted.
    #[test]
    fn a_truncated_or_foreign_pack_is_skipped() {
        let dir = tmp("foreign");
        staged(&dir, &[5]);
        let pack_path = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .next()
            .unwrap()
            .path();
        std::fs::write(dir.join("foreign.pack"), b"not a pack at all").unwrap();
        let bytes = std::fs::read(&pack_path).unwrap();
        std::fs::write(&pack_path, &bytes[..bytes.len() - 4]).unwrap();
        let index = Index::load_in(&dir);
        assert!(index.read(&key(&fragment(5), &jit_key(5))).is_none());
    }

    /// Liveness follows the fragment: an entry whose fragment no manifest names is dead bytes.
    #[test]
    fn liveness_follows_the_fragment() {
        let dir = tmp("liveness");
        staged(&dir, &[1, 2]);
        let live: HashSet<[u8; 32]> = [fragment(1)].into_iter().collect();
        let inventory = inventory_marked_in(&dir, Some(&live));
        assert_eq!(inventory.packs, 1);
        assert_eq!(inventory.records, 2);
        let liveness = inventory.liveness.expect("the caller marked a set");
        assert_eq!(liveness.fragments, 1);
        assert_eq!(liveness.bytes, entry(1).len() as u64);
        assert_eq!(liveness.dead_bytes, entry(2).len() as u64);
    }
}
