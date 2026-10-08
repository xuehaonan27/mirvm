//! The frozen-chunk store: one layer's frozen region, cut into fixed-size chunks and addressed by
//! their own content.
//!
//! A frozen region is the statics, const pools and fn-ptr entries of one layer, snapshotted as its
//! used prefix ([`crate::vm::frozen`]). It is rewritten whenever the layer is: a rebuilt unit's
//! region changes, and a program's own region changes on almost every edit. Measured on the
//! `frag-sharing` fixture, an edit that touches one body keeps all of a 6 120 B region's chunks and
//! one that adds a function keeps 88.9% of a 71 720 B one, so the store keeps the chunks and the
//! manifest keeps only their addresses.
//!
//! A chunk is a byte string addressed by the BLAKE3 of its bytes, which is exactly what a fragment
//! record is ([`crate::vm::ir::frag::id_of`] is that hash), so this family is packed in the fragment
//! store's own format and served by its [`System`]. What differs is only what a record means: a
//! fixed [`CHUNK`]-sized block rather than a canonical body, and a region whose length is *not*
//! implied by its chunks — the last one is short — so the manifest carries the length and the reader
//! refuses a region that does not reassemble to it.
//!
//! **Liveness** is the manifests' as everywhere else: a chunk is live exactly while a
//! current-generation manifest names it, and `image::collect` marks and sweeps it in one locked pass.

use std::collections::HashSet;

use crate::store::frags::{self, Index, Session, Sweep};

/// Bytes per chunk. One page: the unit a region is mapped in, and the coarsest block that still
/// shares something after an edit moves the bytes after it.
pub(crate) const CHUNK: usize = 4096;

/// A system: the family's directory, so one [`System`] value is all a writer or reader needs.
pub(crate) struct System {
    dir: std::path::PathBuf,
}

/// Stage `region` as chunks in `session` and return their ids in order. Cutting a region up needs no
/// store — the chunks are staged, not written — so this is free-standing; only [`System`]'s reads and
/// writes need a family directory.
pub(crate) fn split(region: &[u8], session: &mut Session) -> Vec<[u8; 32]> {
    region
        .chunks(CHUNK)
        .map(|chunk| {
            let id = crate::vm::ir::frag::id_of(chunk);
            session.add(chunk.to_vec());
            id
        })
        .collect()
}

impl System {
    /// The process's own store.
    pub(crate) fn open() -> System {
        Self::in_dir(crate::store::FROZEN.dir())
    }

    /// A store rooted elsewhere, which is what the collection tests use.
    pub(crate) fn in_dir(dir: std::path::PathBuf) -> System {
        System { dir }
    }

    /// Reassemble a region from the chunks a manifest names, truncating to the length it records.
    ///
    /// A chunk the store no longer holds, or a reassembled length that is not the recorded one, is an
    /// error: a region missing a byte is wrong at every address in it, so the caller turns this into a
    /// miss rather than a partially restored layer. Two chunks of one region may be equal — a zeroed
    /// page appears twice — which is why a repeated id is looked up again, not consumed.
    pub(crate) fn read(
        &self,
        chunks: &[[u8; 32]],
        len: u64,
    ) -> Result<Vec<u8>, crate::error::Error> {
        // The length sizes the buffer, so it is checked against what an arena can hold before it is
        // believed: a manifest is a file another build may have written.
        if len > crate::vm::frozen::FROZEN_CAP as u64 {
            return Err(crate::fail!(
                Store,
                format!("frozen region of {len} B exceeds the arena capacity")
            ));
        }
        let index = Index::load_in(&self.dir);
        let held = index.read_many(chunks);
        let mut region = Vec::with_capacity(len as usize);
        for id in chunks {
            let Some(bytes) = held.get(id) else {
                return Err(crate::fail!(
                    Store,
                    format!(
                        "frozen chunk {} is not in the store",
                        crate::utils::content::digest_hex(id)
                    )
                ));
            };
            region.extend_from_slice(bytes);
        }
        if region.len() as u64 != len {
            return Err(crate::fail!(
                Store,
                format!(
                    "frozen chunks reassemble to {} B, not the recorded {len} B",
                    region.len()
                )
            ));
        }
        Ok(region)
    }

    /// Publish everything this session staged. Publishing nothing writes no file.
    pub(crate) fn publish(&self, session: Session) -> std::io::Result<frags::Published> {
        session.publish_in(&self.dir)
    }

    /// Drop every chunk no current-generation manifest names. The caller holds the family's
    /// exclusive lock across its mark and its sweep.
    pub(crate) fn sweep(&self, live: &HashSet<[u8; 32]>) -> std::io::Result<Sweep> {
        frags::sweep_in(&self.dir, live)
    }

    /// The lock a publisher holds across the chunk pack and the manifest that names it. A filesystem
    /// that cannot lock publishes unlocked, as in the fragment store: the lock protects a share, not
    /// correctness.
    pub(crate) fn publish_lock(&self) -> Option<frags::Lock> {
        match frags::Lock::shared(&self.dir) {
            Ok(lock) => Some(lock),
            Err(error) => {
                if crate::options::a2_debug() {
                    crate::diag::instrument(format_args!(
                        "[a2-debug] frozen lock not taken ({error}); publishing unlocked"
                    ));
                }
                None
            }
        }
    }

    /// The exclusive lock of a mark-and-sweep pass.
    pub(crate) fn sweep_lock(&self) -> std::io::Result<frags::Lock> {
        frags::Lock::exclusive(&self.dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> System {
        let dir = std::env::temp_dir().join(format!("mirvm-frozen-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        System::in_dir(dir)
    }

    /// The region's own bytes, so an assertion can tell a restored region from a fresh one. The
    /// chunk index enters the pattern, because the tests turn on which chunk a change landed in.
    fn region(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|at| {
                (at as u8)
                    .wrapping_mul(31)
                    .wrapping_add(((at / CHUNK) as u8).wrapping_mul(7))
                    .wrapping_add(seed)
            })
            .collect()
    }

    #[test]
    fn a_region_round_trips_through_its_chunks() {
        let store = scratch("roundtrip");
        let bytes = region(CHUNK * 2 + 7, 1);
        let mut session = Session::default();
        let chunks = split(&bytes, &mut session);
        assert_eq!(chunks.len(), 3);
        store.publish(session).unwrap();
        assert_eq!(store.read(&chunks, bytes.len() as u64).unwrap(), bytes);
        std::fs::remove_dir_all(&store.dir).unwrap();
    }

    /// The point of the family: a region that grew or changed at its end still reuses the chunks
    /// before the change, and a region with a repeated chunk stores that chunk once.
    #[test]
    fn an_edited_region_reuses_the_chunks_it_did_not_change() {
        let store = scratch("reuse");
        let before = region(CHUNK * 3, 1);
        let mut first = Session::default();
        let old = split(&before, &mut first);
        let published = store.publish(first).unwrap();
        assert_eq!((published.stored, published.deduped), (3, 0));
        let mut after = before.clone();
        after[CHUNK * 2..].fill(0);
        let mut second = Session::default();
        let new = split(&after, &mut second);
        let published = store.publish(second).unwrap();
        assert_eq!((published.stored, published.deduped), (1, 2));
        assert_eq!(new[..2], old[..2]);
        assert_eq!(store.read(&new, after.len() as u64).unwrap(), after);
        std::fs::remove_dir_all(&store.dir).unwrap();
    }

    #[test]
    fn a_missing_chunk_is_an_error_and_not_a_short_region() {
        let store = scratch("missing");
        let bytes = region(CHUNK + 3, 2);
        let mut session = Session::default();
        let chunks = split(&bytes, &mut session);
        store.publish(session).unwrap();
        let mut unknown = chunks.clone();
        unknown[1] = [0xab; 32];
        assert!(store.read(&unknown, bytes.len() as u64).is_err());
        // A length the chunks cannot produce is refused even though every chunk is present.
        assert!(store.read(&chunks, bytes.len() as u64 + 1).is_err());
        std::fs::remove_dir_all(&store.dir).unwrap();
    }

    /// A sweep keeps what a manifest names and takes back the rest: one live chunk of three is a
    /// pack worth rewriting, so the dead chunks go and the live one stays readable.
    #[test]
    fn a_sweep_keeps_the_chunks_a_manifest_names() {
        let store = scratch("sweep");
        let bytes = region(CHUNK * 3, 3);
        let mut session = Session::default();
        let chunks = split(&bytes, &mut session);
        store.publish(session).unwrap();
        let live: HashSet<[u8; 32]> = [chunks[0]].into_iter().collect();
        let sweep = store.sweep(&live).unwrap();
        assert_eq!(sweep.removed, 0);
        assert_eq!(sweep.compacted, 1);
        assert!(store.read(&[chunks[0]], CHUNK as u64).is_ok());
        assert!(store.read(&[chunks[1]], CHUNK as u64).is_err());
        std::fs::remove_dir_all(&store.dir).unwrap();
    }
}
