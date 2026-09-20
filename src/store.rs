//! The local store (`$HOME/.mirvm`, relocatable through `MIRVM_HOME`): what it holds, and how an
//! artifact is published into it.
//!
//! Three concepts, in the order they decide things:
//!
//! - [`Class`] — the lifetime of a sub-root. This is what decides whether a tree may be deleted.
//! - [`Family`] — one directory, written by exactly one module, with a [`Shape`].
//! - [`Shape`] — how the entries inside a family are named and keyed. This is what decides whether
//!   cleanup may remove part of a family or only the whole thing.
//!
//! [`families`] is the register: one line per family, and the only place the layout is written down.
//! `mirvm cache status` and `mirvm cache purge` report and clean exactly what it declares, so a
//! family added there is visible to both without a second edit, and a directory no family claims is
//! reported instead of silently ignored.
//!
//! Two rules are shared by every writer, so they are implemented here rather than at each call site:
//!
//! - **Publish atomically**: fill [`staging_path`], then call [`publish`]. A reader never sees a
//!   half-written artifact, and a crash leaves one dot-prefixed orphan, which [`classify`] reports
//!   as garbage.
//! - **Record the generation**: a [`Shape::Generation`] entry's first serialized field is the
//!   `build_id` of the mirvm that wrote it. A stale generation is therefore recognisable with a
//!   zero-decode peek — a full decode would restore the entry's frozen region and map memory.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// The lifetime of a sub-root: what decides whether it may be deleted.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Class {
    /// Derived from inputs mirvm already has. Deleting it costs recomputation and nothing else, so
    /// it is the one tree a user may clear at any time.
    Cache,
    /// Fetched or built once at real cost: the crate store and the MIR-rich sysroot.
    Data,
    /// Project and session space: materialized scripts and every target directory.
    Build,
    /// Per-process scratch for mappings that must not outlive the process.
    Run,
}

impl Class {
    /// Every class, in the order `status` prints them.
    pub(crate) const ALL: [Class; 4] = [Class::Cache, Class::Data, Class::Build, Class::Run];

    /// Directory name under the store root, and the heading `status` groups by.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Class::Cache => "cache",
            Class::Data => "data",
            Class::Build => "build",
            Class::Run => "run",
        }
    }

    /// The lifetime contract, printed by `status` so the classification is not folklore.
    pub(crate) fn contract(self) -> &'static str {
        match self {
            Class::Cache => "derived; safe to delete at any time",
            Class::Data => "fetched or built once; expensive to lose",
            Class::Build => "project and session space",
            Class::Run => "process scratch",
        }
    }
}

/// How the entries inside a family are named and keyed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Shape {
    /// One artifact: the directory and everything in it is published and removed as a unit.
    Unit,
    /// One file per key, content-addressed: no entry can go stale on its own, so the family is
    /// purged all or nothing.
    Keyed,
    /// One file per key, with the `build_id` that wrote it as the entry's first serialized field:
    /// a stale generation is individually recognisable and removable.
    Generation { ext: &'static str },
}

/// The family flag of `mirvm cache purge` that takes a whole family, as declared in the register.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum FamilyFlag {
    Base,
    Deps,
    Ir,
    Scripts,
    Target,
}

/// One directory in the store.
pub(crate) struct Family {
    pub(crate) class: Class,
    /// Directory name inside the class, with `{host}` standing for the target triple a sysroot was
    /// built for.
    dir_name: &'static str,
    pub(crate) shape: Shape,
    /// The `--flag` that takes this whole family; `None` for a family reached only through its
    /// class (`--all` / `--data`).
    pub(crate) flag: Option<FamilyFlag>,
}

impl Family {
    /// Store-relative path, with `{host}` resolved. Doubles as the display name.
    pub(crate) fn path(&self) -> String {
        format!(
            "{}/{}",
            self.class.name(),
            self.dir_name.replace("{host}", crate::options::build::HOST)
        )
    }

    /// The family directory under the store root.
    pub(crate) fn dir(&self, root: &Path) -> PathBuf {
        root.join(self.path())
    }
}

/// Applies one register shape. An unknown spelling is a compile error, so the register cannot grow a
/// shape the rest of the module does not understand.
macro_rules! shape_of {
    (unit) => {
        Shape::Unit
    };
    (keyed) => {
        Shape::Keyed
    };
    (generation($ext:literal)) => {
        Shape::Generation { ext: $ext }
    };
}

/// The optional `flag(..)` of a register line.
macro_rules! flag_of {
    () => {
        None
    };
    ($flag:ident) => {
        Some(FamilyFlag::$flag)
    };
}

/// Declares the family register, one family per line:
///
/// `Class "dir" shape[(ext)] [flag(Flag)];`
///
/// The comment above a line names the module that writes the family and the key or contract that
/// makes deleting it safe. `{host}` in a path is resolved by [`Family::name`].
macro_rules! families {
    ( $( $class:ident $path:literal $shape:ident $(($ext:literal))? $( flag($flag:ident) )? ; )* ) => {
        /// The register: every family in the store, in the order `status` prints them.
        pub(crate) fn families() -> &'static [Family] {
            static REGISTER: OnceLock<Vec<Family>> = OnceLock::new();
            REGISTER.get_or_init(|| {
                vec![
                    $(
                        Family {
                            class: Class::$class,
                            dir_name: $path,
                            shape: shape_of!($shape $(($ext))?),
                            flag: flag_of!($($flag)?),
                        },
                    )*
                ]
            })
        }
    };
}

families! {
    // `baseimage` — the MIR-rich std base; keyed by build id + sysroot stamp, `build_id` first.
    Cache "base"            generation("img") flag(Base);
    // `depsimage` — the lowered registry dependency closure; keyed by base key + `--extern` stamps.
    Cache "deps"            generation("img") flag(Deps);
    // `ircache` — the post-mono engine IR; keyed by rustc args + input manifest, `build_id` first.
    Cache "ir"              generation("bin") flag(Ir);
    // `lower::asm` — materialized per-site asm stubs; keyed by the generated assembly's content.
    Cache "asm-stubs"       keyed;
    // `lower::global_asm` — materialized `global_asm!`/naked-fn objects; keyed by the final text.
    Cache "global-asm"      keyed;
    // `native_archive` — a PIC `.a` converted into a dlopen-able `.so`; keyed by archive + cc identity.
    Cache "native-archives" keyed;
    // `pack` — native libraries carried inside a `.mirvm`; keyed by their content hash.
    Cache "package-native"  keyed;
    // `vm::ir` — the function heat order learned from a package's first runs; keyed by its code.
    Cache "package-heat"    keyed;
    // `cargoless::registry` — crate and git sources, read through to `~/.cargo`; fetched, not derived.
    Data  "registry"        unit;
    // `sysroot::build_sysroot` — the MIR-rich std; keyed by the toolchain stat + build recipe.
    Data  "sysroot-{host}"  unit;
    // `cli::frontmatter::script_cache_dir` — materialized frontmatter projects; keyed by script path.
    Build "scripts"         unit flag(Scripts);
    // `sysroot::build_sysroot` staging; reused across rebuilds, so `--data` keeps it.
    Build "sysroot-build"   unit;
    // Cargo track, cargoless scheduler and native differential builds; rebuilt from the sources.
    Build "target"          unit flag(Target);
    // `vm::native_instance` — per-Engine copies of required native libraries (glibc keys by path).
    Run   "runtime-native"  unit;
    // `vm::native_instance` — private copies of self-produced objects while lowering opens them.
    Run   "lower-native"    unit;
}
/// The staging name for `target`.
///
/// In the target's own directory, so `rename` publishes on one filesystem; dot-prefixed, so a crash
/// orphan is recognisable as [`Staleness::Garbage`]; and unique per process *and* per call, so two
/// threads producing the same key cannot truncate each other's staging file.
pub(crate) fn staging_path(target: &Path) -> PathBuf {
    orphan_path(target, "tmp")
}

/// The name `publish` moves a replaced directory aside to before removing it.
fn vacated_path(target: &Path) -> PathBuf {
    orphan_path(target, "old")
}

fn orphan_path(target: &Path, tag: &str) -> PathBuf {
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    let serial = SERIAL.fetch_add(1, Ordering::Relaxed);
    let name = target.file_name().unwrap_or_default().to_string_lossy();
    target.with_file_name(format!(".{name}.{tag}-{}-{serial}", std::process::id()))
}

/// Publish the artifact staged at `staged` as `target`.
///
/// A file target is replaced by one `rename`, which is atomic. A non-empty directory cannot be
/// replaced that way, so an existing directory target is first moved aside and removed afterwards;
/// the target is absent only between the two renames, and a crash there leaves the artifact missing
/// rather than mixed, which the producer's own key or stamp turns into a rebuild.
pub(crate) fn publish(target: &Path, staged: &Path) -> std::io::Result<()> {
    let displaced = if target.is_dir() {
        let old = vacated_path(target);
        std::fs::rename(target, &old)?;
        Some(old)
    } else {
        None
    };
    std::fs::rename(staged, target)?;
    if let Some(old) = displaced {
        let _ = std::fs::remove_dir_all(old);
    }
    Ok(())
}

/// Write `bytes` to the staging file next to `target` and publish it.
pub(crate) fn publish_bytes(target: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let staged = staging_path(target);
    if let Err(error) = std::fs::write(&staged, bytes) {
        let _ = std::fs::remove_file(&staged);
        return Err(error);
    }
    match publish(target, &staged) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = std::fs::remove_file(&staged);
            Err(error)
        }
    }
}

/// Whether a generational entry belongs to this build.
#[derive(Debug, PartialEq, Eq)]
enum Staleness {
    Current,
    Stale,
    /// A dot-prefixed orphan or an unreadable header: removing it cannot lose anything.
    Garbage,
}

/// Classify one entry of a generational family.
fn classify(path: &Path) -> Staleness {
    if is_orphan(path) {
        return Staleness::Garbage;
    }
    match entry_build_id(path) {
        Some(id) if id == crate::options::build::BUILD_ID => Staleness::Current,
        Some(_) => Staleness::Stale,
        None => Staleness::Garbage,
    }
}

/// Split a generational family into (current, stale, garbage) entries.
///
/// Only `ext` entries and dot-prefixed orphans are classified; anything else (a build log and other
/// byproducts, the `src/` directory the base-image build writes) is left alone and unreported.
pub(crate) fn split_generational(
    dir: &Path,
    ext: &str,
) -> (Vec<PathBuf>, Vec<PathBuf>, Vec<PathBuf>) {
    let (mut current, mut stale, mut garbage) = (Vec::new(), Vec::new(), Vec::new());
    let Ok(read) = std::fs::read_dir(dir) else {
        return (current, stale, garbage);
    };
    for entry in read.flatten() {
        let path = entry.path();
        if !path.is_file() || (!is_orphan(&path) && path.extension().is_none_or(|x| x != ext)) {
            continue;
        }
        match classify(&path) {
            Staleness::Current => current.push(path),
            Staleness::Stale => stale.push(path),
            Staleness::Garbage => garbage.push(path),
        }
    }
    (current, stale, garbage)
}

/// Whether a path is one of the dot-prefixed orphans a failed publish leaves behind.
fn is_orphan(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| name.to_string_lossy().starts_with('.'))
}

/// The `build_id` recorded as the first field of a generational entry.
///
/// Zero-decode peek: postcard writes a `String` as a varint length followed by UTF-8, and a full
/// decode would restore the entry's frozen region and map memory. 32 bytes are enough — a build id
/// is always 16 lowercase hex digits (`{:016x}`).
fn entry_build_id(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut buf = [0u8; 32];
    let mut file = std::fs::File::open(path).ok()?;
    let read = file.read(&mut buf).ok()?;
    let buf = &buf[..read];
    let (len, used) = varint(buf)?;
    let text = buf.get(used..used + len as usize)?;
    String::from_utf8(text.to_vec()).ok()
}

/// postcard varint (1.x stable scheme: ≤250 single byte; 0xFB=u16 / 0xFC=u32 / 0xFD=u64 /
/// 0xFE=u128 little-endian follows). Returns (value, bytes consumed).
fn varint(bytes: &[u8]) -> Option<(u128, usize)> {
    let (&tag, rest) = bytes.split_first()?;
    Some(match tag {
        0..=250 => (tag as u128, 1),
        251 => (
            u16::from_le_bytes(rest.get(..2)?.try_into().ok()?) as u128,
            3,
        ),
        252 => (
            u32::from_le_bytes(rest.get(..4)?.try_into().ok()?) as u128,
            5,
        ),
        253 => (
            u64::from_le_bytes(rest.get(..8)?.try_into().ok()?) as u128,
            9,
        ),
        254 => (u128::from_le_bytes(rest.get(..16)?.try_into().ok()?), 17),
        255 => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mirvm-store-test-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Same shape as a generational entry: first field String (postcard varint + UTF-8).
    fn fake_entry(dir: &Path, name: &str, build_id: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        let mut bytes = postcard::to_stdvec(&build_id.to_string()).unwrap();
        bytes.extend_from_slice(b"payload-bytes-after-header");
        std::fs::write(&path, &bytes).unwrap();
        path
    }

    #[test]
    fn register_is_well_formed() {
        let mut names = Vec::new();
        let mut flags = Vec::new();
        for family in families() {
            let name = family.path();
            let prefix = format!("{}/", family.class.name());
            assert!(
                name.starts_with(&prefix),
                "`{name}` is not under `{prefix}`"
            );
            assert!(
                names.iter().all(|n| n != &name),
                "`{name}` is declared twice"
            );
            names.push(name);
            if let Some(flag) = family.flag {
                assert!(
                    flags.iter().all(|f| f != &flag),
                    "`{flag:?}` names two families"
                );
                flags.push(flag);
            }
        }
        // Both sysroot families are spelled for every target, and neither is left unclaimed.
        assert!(names.contains(&format!("data/sysroot-{}", crate::options::build::HOST)));
    }

    #[test]
    fn staging_name_is_dot_prefixed_unique_and_a_sibling() {
        let dir = temp_dir("staging");
        let target = dir.join("x.bin");
        let (a, b) = (staging_path(&target), staging_path(&target));
        assert_ne!(a, b, "two callers must never share a staging file");
        for path in [&a, &b] {
            assert_eq!(path.parent(), Some(dir.as_path()), "rename needs a sibling");
            assert!(
                path.to_string_lossy()
                    .contains(&format!(".x.bin.tmp-{}", std::process::id()))
            );
            assert!(is_orphan(path));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn publish_replaces_a_file_and_a_directory() {
        let dir = temp_dir("publish");
        // File: one rename replaces the old artifact.
        let file = dir.join("a.bin");
        std::fs::write(&file, b"old").unwrap();
        publish_bytes(&file, b"new").unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"new");
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            1,
            "no orphan left"
        );
        // Directory: the old tree is moved aside, the new one takes its place, nothing is mixed.
        let tree = dir.join("tree");
        std::fs::create_dir_all(tree.join("sub")).unwrap();
        std::fs::write(tree.join("sub/old"), b"old").unwrap();
        let staged = staging_path(&tree);
        std::fs::create_dir_all(&staged).unwrap();
        std::fs::write(staged.join("new"), b"new").unwrap();
        publish(&tree, &staged).unwrap();
        assert!(!tree.join("sub").exists() && tree.join("new").exists());
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            2,
            "no orphan left"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn peek_reads_the_first_postcard_string() {
        let dir = temp_dir("peek");
        let entry = fake_entry(&dir, "a.bin", "0123456789abcdef");
        assert_eq!(entry_build_id(&entry).as_deref(), Some("0123456789abcdef"));
        assert_eq!(entry_build_id(&dir.join("missing.bin")), None);
        let junk = dir.join("junk.bin");
        std::fs::write(&junk, b"\xff\xff\xff\xff").unwrap();
        assert_eq!(entry_build_id(&junk), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn classify_covers_current_stale_and_garbage() {
        let dir = temp_dir("classify");
        let current = fake_entry(&dir, "cur.bin", crate::options::build::BUILD_ID);
        let stale = fake_entry(&dir, "old.bin", "0000000000000000");
        let orphan = dir.join(".cur.bin.tmp-123");
        std::fs::write(&orphan, b"orphan").unwrap();
        assert_eq!(classify(&current), Staleness::Current);
        assert_eq!(classify(&stale), Staleness::Stale);
        assert_eq!(classify(&orphan), Staleness::Garbage);
        let (cur, old, garb) = split_generational(&dir, "bin");
        assert_eq!((cur.len(), old.len(), garb.len()), (1, 1, 1));
        // A build byproduct is neither an entry nor an orphan: left alone and unreported.
        std::fs::write(dir.join("build.log"), b"output").unwrap();
        let (_, _, _) = split_generational(&dir, "bin");
        assert_eq!(split_generational(&dir, "bin").2.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
