//! Immutable sysroot generations and discardable local integrity receipts. Selection belongs to
//! the sysroot producer; a matching path or recipe alone never grants a managed input identity.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::utils::content::{FileStamp, digest_hex};

#[derive(Serialize, Deserialize)]
struct Member {
    path: PathBuf,
    size: u64,
    digest: [u8; 32],
}

#[derive(Serialize, Deserialize)]
struct Manifest {
    version: u32,
    target: String,
    recipe: String,
    members: Vec<Member>,
}

#[derive(Serialize, Deserialize)]
#[cfg(any(feature = "cli", test))]
struct Selection {
    freshness: String,
    generation: String,
}

#[derive(PartialEq, Eq, Serialize, Deserialize)]
struct Observation {
    path: PathBuf,
    identity: crate::os::fs::FileIdentity,
    directory: bool,
}

/// One selected generation. The lease outlives every path supplied to the compiler.
pub(super) struct Generation {
    pub path: PathBuf,
    pub id: [u8; 32],
    manifest: Manifest,
    receipt: PathBuf,
    _lease: crate::store::InputLease,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Input {
    pub generation: [u8; 32],
    pub member: PathBuf,
}

fn invalid(detail: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, detail)
}

fn encode<T: Serialize>(value: &T) -> io::Result<Vec<u8>> {
    postcard::to_stdvec(value).map_err(io::Error::other)
}

/// Include directories to detect namespace changes as well as edits to existing members.
fn observe(root: &Path) -> io::Result<Vec<Observation>> {
    fn walk(root: &Path, relative: &Path, out: &mut Vec<Observation>) -> io::Result<()> {
        let metadata = std::fs::symlink_metadata(root.join(relative))?;
        if !metadata.is_file() && !metadata.is_dir() {
            return Err(invalid(
                "managed inputs must be regular files or directories",
            ));
        }
        out.push(Observation {
            path: relative.to_path_buf(),
            identity: crate::os::fs::FileIdentity::of(&metadata),
            directory: metadata.is_dir(),
        });
        if metadata.is_dir() {
            for entry in std::fs::read_dir(root.join(relative))? {
                walk(root, &relative.join(entry?.file_name()), out)?;
            }
        }
        Ok(())
    }
    let mut observations = Vec::new();
    walk(root, Path::new(""), &mut observations)?;
    observations.sort_unstable_by(|a, b| a.path.cmp(&b.path));
    Ok(observations)
}

fn receipt_bytes(observations: &[Observation]) -> io::Result<Vec<u8>> {
    let payload = encode(&observations)?;
    let mut bytes = blake3::hash(&payload).as_bytes().to_vec();
    bytes.extend(payload);
    Ok(bytes)
}

/// Each scan enumerates the full namespace and file revisions. Directory revision timestamps
/// can be updated lazily by a filesystem after creating children; their inode identity and the
/// enumerated names detect replacement or namespace changes without treating that update as a
/// changed compiler input. Receipt hits still compare complete observations; this comparison
/// is used only around a full copy or content-hash validation.
fn snapshot_unchanged(before: &[Observation], after: &[Observation]) -> bool {
    before.len() == after.len()
        && before.iter().zip(after).all(|(left, right)| {
            left.path == right.path
                && left.directory == right.directory
                && if left.directory {
                    left.identity.same_object(&right.identity)
                } else {
                    left.identity == right.identity
                }
        })
}

impl Generation {
    #[cfg(any(feature = "compiler", test))]
    pub fn input(&self, path: &Path) -> Option<Input> {
        let relative = path.strip_prefix(&self.path).ok()?;
        self.manifest
            .members
            .binary_search_by(|member| member.path.as_path().cmp(relative))
            .ok()?;
        Some(Input {
            generation: self.id,
            member: relative.to_path_buf(),
        })
    }

    pub fn contains(&self, input: &Input) -> bool {
        input.generation == self.id
            && self
                .manifest
                .members
                .binary_search_by(|member| member.path.cmp(&input.member))
                .is_ok()
    }

    #[cfg(feature = "compiler")]
    pub fn provenance(&self, input: &Input) -> Option<FileStamp> {
        self.contains(input)
            .then(|| self.path.join(&input.member))
            .and_then(|path| FileStamp::of(&path).ok())
    }

    pub fn validate(&self) -> io::Result<()> {
        let before = observe(&self.path)?;
        let receipt = std::fs::read(&self.receipt).ok().and_then(|bytes| {
            let payload = bytes.get(32..)?;
            (bytes[..32] == *blake3::hash(payload).as_bytes())
                .then(|| postcard::from_bytes::<Vec<Observation>>(payload).ok())
                .flatten()
        });
        if receipt.as_ref() == Some(&before) {
            return Ok(());
        }
        let files: Vec<_> = before.iter().filter(|entry| !entry.directory).collect();
        if files.len() != self.manifest.members.len() {
            return Err(invalid("managed input member set changed"));
        }
        for (entry, member) in files.iter().zip(&self.manifest.members) {
            if entry.path != member.path {
                return Err(invalid("managed input member set changed"));
            }
            let stamp = FileStamp::of(&self.path.join(&member.path))?;
            if stamp.size != member.size || stamp.digest != member.digest {
                return Err(invalid("managed input content changed"));
            }
        }
        let after = observe(&self.path)?;
        if !snapshot_unchanged(&before, &after) {
            return Err(invalid("managed input changed during validation"));
        }
        let _ = crate::store::publish_bytes(&self.receipt, &receipt_bytes(&after)?);
        Ok(())
    }
}

fn load(root: &Path, store_root: &Path, id: &str, recipe: Option<&str>) -> io::Result<Generation> {
    if id.len() != 64 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid("invalid managed generation identity"));
    }
    let lease = crate::store::input_lease(store_root, false)?;
    let directory = root.join("generations").join(id);
    if !std::fs::symlink_metadata(&directory)?.is_dir()
        || !std::fs::symlink_metadata(directory.join("payload"))?.is_dir()
    {
        return Err(invalid("managed generation directories cannot be symlinks"));
    }
    let bytes = std::fs::read(directory.join("manifest"))?;
    let digest = *blake3::hash(&bytes).as_bytes();
    if digest_hex(&digest) != id {
        return Err(invalid("managed generation manifest hash mismatch"));
    }
    let manifest: Manifest = postcard::from_bytes(&bytes).map_err(io::Error::other)?;
    if manifest.version != 1
        || manifest.target != crate::options::build::HOST
        || recipe.is_some_and(|recipe| recipe != manifest.recipe)
        || !manifest
            .members
            .windows(2)
            .all(|members| members[0].path < members[1].path)
        || manifest.members.iter().any(|member| {
            member.path.as_os_str().is_empty()
                || member
                    .path
                    .components()
                    .any(|part| !matches!(part, std::path::Component::Normal(_)))
        })
    {
        return Err(invalid("invalid managed generation manifest"));
    }
    let generation = Generation {
        path: std::fs::canonicalize(directory.join("payload"))?,
        id: digest,
        manifest,
        receipt: directory.join("receipt"),
        _lease: lease,
    };
    generation.validate()?;
    Ok(generation)
}

#[cfg(any(feature = "cli", test))]
pub(super) fn selected(
    root: &Path,
    store_root: &Path,
    freshness: &str,
    recipe: &str,
) -> io::Result<Generation> {
    let bytes = std::fs::read(root.join("current"))?;
    let selection: Selection = postcard::from_bytes(&bytes).map_err(io::Error::other)?;
    if selection.freshness != freshness {
        return Err(invalid("managed producer inputs changed"));
    }
    load(root, store_root, &selection.generation, Some(recipe))
}

/// A child reopens only a concrete generation in its owner's registered family.
pub(super) fn reopen(
    root: &Path,
    store_root: &Path,
    path: &Path,
) -> io::Result<Option<Generation>> {
    let Ok(path) = std::fs::canonicalize(path) else {
        return Ok(None);
    };
    let Ok(generations) = std::fs::canonicalize(root.join("generations")) else {
        return Ok(None);
    };
    let Ok(relative) = path.strip_prefix(generations) else {
        return Ok(None);
    };
    let components: Vec<_> = relative.components().collect();
    if components.len() != 2 || components[1].as_os_str() != "payload" {
        return Ok(None);
    }
    let id = components[0]
        .as_os_str()
        .to_str()
        .ok_or_else(|| invalid("invalid generation name"))?;
    load(root, store_root, id, None).map(Some)
}

/// Copy the producer's complete lib tree; no hard links retain a mutable source inode.
#[cfg(any(feature = "cli", test))]
pub(super) fn publish(
    root: &Path,
    store_root: &Path,
    source: &Path,
    freshness: &str,
    recipe: &str,
) -> io::Result<Generation> {
    let lease = crate::store::input_lease(store_root, false)?;
    let before = observe(&source.join("lib"))?;
    let staged = crate::store::staging_path(&root.join("generation"));
    let result = (|| {
        let payload = staged.join("payload");
        std::fs::create_dir_all(payload.join("lib"))?;
        let mut members = Vec::new();
        for entry in &before {
            let relative = Path::new("lib").join(&entry.path);
            // Producer freshness is machine-local metadata, not a compiler input.
            if source.join(&relative) == super::stamp_file(source) {
                continue;
            }
            let destination = payload.join(&relative);
            if entry.directory {
                std::fs::create_dir_all(&destination)?;
            } else {
                std::fs::copy(source.join(&relative), &destination)?;
                let stamp = FileStamp::of(&destination)?;
                let mut permissions = std::fs::metadata(&destination)?.permissions();
                permissions.set_readonly(true);
                std::fs::set_permissions(&destination, permissions)?;
                members.push(Member {
                    path: relative,
                    size: stamp.size,
                    digest: stamp.digest,
                });
            }
        }
        if !snapshot_unchanged(&before, &observe(&source.join("lib"))?) {
            return Err(invalid("sysroot changed during generation publication"));
        }
        members.sort_unstable_by(|a, b| a.path.cmp(&b.path));
        let manifest = Manifest {
            version: 1,
            target: crate::options::build::HOST.into(),
            recipe: recipe.into(),
            members,
        };
        let bytes = encode(&manifest)?;
        let id = digest_hex(blake3::hash(&bytes).as_bytes());
        let selection = encode(&Selection {
            freshness: freshness.into(),
            generation: id.clone(),
        })?;
        std::fs::write(staged.join("manifest"), &bytes)?;
        std::fs::write(staged.join("receipt"), receipt_bytes(&observe(&payload)?)?)?;
        let generations = root.join("generations");
        std::fs::create_dir_all(&generations)?;
        let destination = generations.join(&id);
        if destination.exists() {
            if load(root, store_root, &id, Some(recipe)).is_err() {
                // Repair only after existing readers release their paths. The writer lock held by
                // the producer prevents another publisher from racing this replacement.
                drop(lease);
                let exclusive = crate::store::input_lease(store_root, true)?;
                if destination.exists() {
                    std::fs::remove_dir_all(&destination)?;
                }
                std::fs::create_dir_all(&generations)?;
                std::fs::rename(&staged, &destination)?;
                crate::store::publish_bytes(&root.join("current"), &selection)?;
                drop(exclusive);
                return load(root, store_root, &id, Some(recipe));
            }
        } else {
            std::fs::rename(&staged, &destination)?;
        }
        crate::store::publish_bytes(&root.join("current"), &selection)?;
        load(root, store_root, &id, Some(recipe))
    })();
    let _ = std::fs::remove_dir_all(staged);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new(tag: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("mirvm-managed-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join("source/lib/target")).unwrap();
            std::fs::write(root.join("source/lib/target/core.rmeta"), b"first").unwrap();
            Self(root)
        }

        fn root(&self) -> PathBuf {
            self.0.join("sysroot")
        }

        fn publish(&self) -> Generation {
            let _writer = crate::store::input_writer(&self.0).unwrap();
            publish(
                &self.root(),
                &self.0,
                &self.0.join("source"),
                "freshness",
                "recipe",
            )
            .unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn copy_observations_check_namespace_and_file_revisions() {
        let f = Fixture::new("copy-observations");
        let root = f.0.join("source/lib");
        let before = observe(&root).unwrap();
        let extra = root.join("extra");
        std::fs::write(&extra, b"new").unwrap();
        assert!(!snapshot_unchanged(&before, &observe(&root).unwrap()));
        std::fs::remove_file(&extra).unwrap();
        assert!(snapshot_unchanged(&before, &observe(&root).unwrap()));

        let old = root.join("target");
        let moved = f.0.join("moved");
        std::fs::rename(&old, &moved).unwrap();
        std::fs::create_dir(&old).unwrap();
        std::fs::rename(moved.join("core.rmeta"), old.join("core.rmeta")).unwrap();
        assert!(!snapshot_unchanged(&before, &observe(&root).unwrap()));

        let before = observe(&root).unwrap();
        let file = old.join("core.rmeta");
        let modified = std::fs::metadata(&file).unwrap().modified().unwrap();
        std::fs::write(&file, b"other").unwrap();
        std::fs::File::options()
            .write(true)
            .open(file)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        assert!(!snapshot_unchanged(&before, &observe(&root).unwrap()));
    }

    #[test]
    fn identity_tracks_outputs_and_selection_not_recipe_or_old_presence() {
        let f = Fixture::new("identity");
        let first = f.publish();
        let input = first
            .input(&first.path.join("lib/target/core.rmeta"))
            .unwrap();
        assert!(
            first
                .input(&f.0.join("source/lib/target/core.rmeta"))
                .is_none()
        );
        std::fs::write(f.0.join("source/lib/target/core.rmeta"), b"other").unwrap();
        let second = f.publish();
        assert_ne!(first.id, second.id);
        assert!(first.path.exists());
        assert!(!second.contains(&input));
        assert_eq!(
            selected(&f.root(), &f.0, "freshness", "recipe").unwrap().id,
            second.id
        );
        assert!(selected(&f.root(), &f.0, "changed freshness", "recipe").is_err());
    }

    #[test]
    fn producer_metadata_does_not_change_output_identity() {
        let f = Fixture::new("freshness");
        let source = f.0.join("source");
        let stamp = super::super::stamp_file(&source);
        std::fs::create_dir_all(stamp.parent().unwrap()).unwrap();
        std::fs::write(&stamp, "local metadata one").unwrap();
        let first = f.publish();
        std::fs::write(&stamp, "local metadata two").unwrap();
        let _writer = crate::store::input_writer(&f.0).unwrap();
        let second = publish(&f.root(), &f.0, &source, "changed freshness", "recipe").unwrap();
        assert_eq!(first.id, second.id);
        assert!(!super::super::stamp_file(&second.path).exists());
        assert!(selected(&f.root(), &f.0, "freshness", "recipe").is_err());
        assert_eq!(
            selected(&f.root(), &f.0, "changed freshness", "recipe")
                .unwrap()
                .id,
            first.id
        );
        assert!(selected(&f.root(), &f.0, "changed freshness", "different recipe").is_err());
    }

    #[test]
    fn receipts_recover_but_changed_bytes_and_names_are_rejected() {
        let f = Fixture::new("receipt");
        let generation = f.publish();
        std::fs::remove_file(&generation.receipt).unwrap();
        generation.validate().unwrap();
        assert!(generation.receipt.exists());
        let member = generation.path.join("lib/target/core.rmeta");
        let before = std::fs::metadata(&member).unwrap();
        let replacement = f.0.join("replacement");
        std::fs::write(&replacement, b"other").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&replacement)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(before.modified().unwrap()))
            .unwrap();
        std::fs::rename(replacement, &member).unwrap();
        assert!(generation.validate().is_err());
        drop(generation);
        let repaired = f.publish();
        assert_eq!(std::fs::read(&member).unwrap(), b"first");
        std::fs::write(repaired.path.join("lib/extra"), b"extra").unwrap();
        assert!(repaired.validate().is_err());
        std::fs::remove_file(repaired.path.join("lib/extra")).unwrap();
        repaired.validate().unwrap();
        std::fs::remove_file(member).unwrap();
        assert!(repaired.validate().is_err());
    }

    #[test]
    fn relocated_generation_revalidates_without_changing_identity() {
        let f = Fixture::new("relocation");
        let generation = f.publish();
        let id = generation.id;
        drop(generation);
        let moved = f.0.join("moved");
        std::fs::rename(f.root(), &moved).unwrap();
        let selected = selected(&moved, &f.0, "freshness", "recipe").unwrap();
        assert_eq!(selected.id, id);
        assert!(reopen(&moved, &f.0, &selected.path).unwrap().is_some());
        assert!(reopen(&f.root(), &f.0, &selected.path).unwrap().is_none());
    }

    #[test]
    fn staging_is_not_selectable_and_live_readers_exclude_collection() {
        let f = Fixture::new("lease");
        std::fs::create_dir_all(f.root().join(".generation.tmp/payload")).unwrap();
        assert!(selected(&f.root(), &f.0, "freshness", "recipe").is_err());
        let generation = f.publish();
        let (started, start) = std::sync::mpsc::channel();
        let (locked, lock) = std::sync::mpsc::channel();
        let store_root = f.0.clone();
        let collector = std::thread::spawn(move || {
            started.send(()).unwrap();
            let _lease = crate::store::input_lease(&store_root, true).unwrap();
            locked.send(()).unwrap();
        });
        start.recv().unwrap();
        assert!(
            lock.recv_timeout(std::time::Duration::from_millis(30))
                .is_err()
        );
        drop(generation);
        lock.recv_timeout(std::time::Duration::from_secs(3))
            .unwrap();
        collector.join().unwrap();
    }

    #[test]
    fn producer_and_generation_symlinks_are_rejected() {
        let f = Fixture::new("symlinks");
        std::os::unix::fs::symlink("core.rmeta", f.0.join("source/lib/target/alias")).unwrap();
        assert!(publish(&f.root(), &f.0, &f.0.join("source"), "freshness", "recipe").is_err());
        std::fs::remove_file(f.0.join("source/lib/target/alias")).unwrap();
        let generation = f.publish();
        let id = digest_hex(&generation.id);
        let payload = generation.path.clone();
        drop(generation);
        let moved = f.0.join("moved-payload");
        std::fs::rename(&payload, &moved).unwrap();
        std::os::unix::fs::symlink(&moved, payload).unwrap();
        assert!(load(&f.root(), &f.0, &id, Some("recipe")).is_err());
    }
}
