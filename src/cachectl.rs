//! Inventory and cleanup of the mirvm local store (`$HOME/.mirvm`, relocatable through
//! `MIRVM_HOME`): `mirvm cache status` / `mirvm cache purge`.
//!
//! The store is split by **lifetime**, and this module is where that split is visible:
//!
//! - `cache/` — derived from inputs mirvm already has. Deleting it costs recomputation and nothing
//!   else, so it is the one tree a user may clear at any time. Three of its families (`base/deps/ir`)
//!   are generational: the first field of every entry is `build_id`, so a stale generation can be
//!   recognised and removed on its own. Reading it is a zero-decode peek (postcard varint string,
//!   no side effects — a full decode would trigger the frozen-region base mmap).
//! - `data/` — fetched or built once at real cost: the crate store and the MIR-rich sysroot.
//! - `build/` — project and session space: materialized scripts and every target directory.
//! - `run/` — per-process scratch for mappings that must not outlive the process.
//!
//! Every family self-heals: purging one only affects the next run's speed, never correctness.

use std::path::{Path, PathBuf};

/// Cleanup plan (product of CLI flag parsing).
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Purge {
    /// Remove stale generations and garbage (tmp orphans, junk) from the generational families. This
    /// is the default when no family flag is given, and the only thing that is ever partial.
    pub stale: bool,
    pub deps: bool,
    pub base: bool,
    pub ir: bool,
    pub scripts: bool,
    /// The target directories (shared dependency storage and the native differential builds).
    pub target: bool,
    /// Every `cache/`, `build/` and `run/` family: everything that costs no network to rebuild.
    pub all: bool,
    /// Also `data/`: the crate store must be fetched again and the sysroot rebuilt. With `--all`
    /// this is a full cold start.
    pub data: bool,
    pub dry_run: bool,
}

/// What a family holds. The class is what decides how `purge` may treat it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Class {
    Cache,
    Data,
    Build,
    Run,
}

impl Class {
    /// Directory under the store root, and the heading `status` groups by.
    fn path(self) -> &'static str {
        match self {
            Class::Cache => "cache",
            Class::Data => "data",
            Class::Build => "build",
            Class::Run => "run",
        }
    }

    /// The lifetime contract, printed by `status` so the classification is not folklore.
    fn contract(self) -> &'static str {
        match self {
            Class::Cache => "derived; safe to delete at any time",
            Class::Data => "fetched or built once; expensive to lose",
            Class::Build => "project and session space",
            Class::Run => "process scratch",
        }
    }

    fn all() -> [Class; 4] {
        [Class::Cache, Class::Data, Class::Build, Class::Run]
    }
}

struct Family {
    class: Class,
    /// Path under the store root; doubles as the display name.
    path: String,
    /// Generational entry extension (`build_id` first field); empty = one indivisible directory.
    entry_ext: &'static str,
}

fn families() -> Vec<Family> {
    let host = crate::options::build::HOST;
    let fam = |class, path: String, entry_ext| Family {
        class,
        path,
        entry_ext,
    };
    vec![
        fam(Class::Data, "data/registry".into(), ""),
        fam(Class::Data, format!("data/sysroot-{host}"), ""),
        fam(Class::Build, "build/scripts".into(), ""),
        fam(Class::Build, "build/target".into(), ""),
        fam(Class::Cache, "cache/base".into(), "img"),
        fam(Class::Cache, "cache/deps".into(), "img"),
        fam(Class::Cache, "cache/ir".into(), "bin"),
        fam(Class::Cache, "cache/native-archives".into(), ""),
        fam(Class::Cache, "cache/global-asm".into(), ""),
        fam(Class::Cache, "cache/asm-stubs".into(), ""),
        fam(Class::Cache, "cache/package-native".into(), ""),
        fam(Class::Cache, "cache/package-heat".into(), ""),
        fam(Class::Run, "run".into(), ""),
    ]
}

/// The `--deps/--base/--ir` flag that names a generational family, if any.
fn named_cache_flag(path: &str, plan: &Purge) -> bool {
    match path {
        "cache/deps" => plan.deps,
        "cache/base" => plan.base,
        "cache/ir" => plan.ir,
        _ => false,
    }
}

/// The `--scripts/--target` flag that names a build family, if any.
fn named_build_flag(path: &str, plan: &Purge) -> bool {
    match path {
        "build/scripts" => plan.scripts,
        "build/target" => plan.target,
        _ => false,
    }
}

/// Recursively accumulate directory size and file count; nonexistent ⇒ (0, 0).
fn du(path: &Path) -> (u64, u64) {
    let mut bytes = 0u64;
    let mut files = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if let Ok(md) = p.metadata() {
                bytes += md.len();
                files += 1;
            }
        }
    }
    (bytes, files)
}

fn human(bytes: u64) -> String {
    const U: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes}B")
    } else {
        format!("{v:.1}{}", U[i])
    }
}

/// postcard varint (1.x stable scheme: ≤250 single byte; 0xFB=u16 / 0xFC=u32 / 0xFD=u64 /
/// 0xFE=u128 little-endian follows). Returns (value, bytes consumed).
fn varint(b: &[u8]) -> Option<(u128, usize)> {
    let (&tag, rest) = b.split_first()?;
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

/// Zero-decode peek of the first-field build_id in the three generational families (String = varint length + UTF-8).
/// Read only first 32 bytes: build_id is always 16-digit lowercase hex (`{:016x}`), more than enough.
fn file_build_id(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut buf = [0u8; 32];
    let mut f = std::fs::File::open(path).ok()?;
    let n = f.read(&mut buf).ok()?;
    let buf = &buf[..n];
    let (len, used) = varint(buf)?;
    let s = buf.get(used..used + len as usize)?;
    String::from_utf8(s.to_vec()).ok()
}

/// Single-file generational classification: Staleness::Current / Stale / Garbage (tmp orphans and parse failures).
#[derive(Debug, PartialEq, Eq)]
enum Staleness {
    Current,
    Stale,
    Garbage,
}

fn classify(path: &Path) -> Staleness {
    // tmp orphans (`.{name}.tmp-{pid}` dot files) and any parse failures = garbage (self-heal risk-free)
    if path
        .file_name()
        .is_some_and(|n| n.to_string_lossy().starts_with('.'))
    {
        return Staleness::Garbage;
    }
    match file_build_id(path) {
        Some(id) if id == crate::options::build::BUILD_ID => Staleness::Current,
        Some(_) => Staleness::Stale,
        None => Staleness::Garbage,
    }
}

/// Classify within family by generation (only for generational families). Returns (current, stale, garbage) three lists.
/// Only entry extensions (ir=bin / deps,base=img) and tmp orphans (dot files) enter classification;
/// other files (build.log and other build byproducts, src/ directories) are left untouched and unreported.
fn split_generational(dir: &Path, entry_ext: &str) -> (Vec<PathBuf>, Vec<PathBuf>, Vec<PathBuf>) {
    let (mut cur, mut stale, mut garb) = (Vec::new(), Vec::new(), Vec::new());
    let Ok(rd) = std::fs::read_dir(dir) else {
        return (cur, stale, garb);
    };
    for e in rd.flatten() {
        let p = e.path();
        if !p.is_file() {
            continue;
        }
        let is_tmp_orphan = p
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with('.'));
        let is_entry = p.extension().is_some_and(|x| x == entry_ext);
        if !is_tmp_orphan && !is_entry {
            continue;
        }
        match classify(&p) {
            Staleness::Current => cur.push(p),
            Staleness::Stale => stale.push(p),
            Staleness::Garbage => garb.push(p),
        }
    }
    (cur, stale, garb)
}

fn size_of(paths: &[PathBuf]) -> u64 {
    paths
        .iter()
        .map(|p| p.metadata().map(|m| m.len()).unwrap_or(0))
        .sum()
}

/// Full text of `mirvm cache status`.
pub fn status(root: &Path) -> String {
    // The first line is parsed by the test harness for the build id; keep its shape.
    let mut out = format!(
        "mirvm local cache {} (build {})\n",
        root.display(),
        crate::options::build::BUILD_ID
    );
    let (mut total, mut total_stale) = (0u64, 0u64);
    let mut claimed: Vec<String> = Vec::new();
    for class in Class::all() {
        out += &format!("  {}/  ({})\n", class.path(), class.contract());
        for fam in families().iter().filter(|f| f.class == class) {
            claimed.push(fam.path.clone());
            let dir = root.join(&fam.path);
            if !fam.entry_ext.is_empty() {
                let (cur, stale, garb) = split_generational(&dir, fam.entry_ext);
                let (cs, ss, gs) = (size_of(&cur), size_of(&stale), size_of(&garb));
                total += cs + ss + gs;
                total_stale += ss + gs;
                let n = cur.len() + stale.len() + garb.len();
                out += &format!(
                    "    {:<32} {:>9}  ({n} items; stale {}, garbage {})\n",
                    fam.path,
                    human(cs + ss + gs),
                    human(ss),
                    human(gs)
                );
            } else {
                let (bytes, files) = du(&dir);
                total += bytes;
                out += &format!(
                    "    {:<32} {:>9}  ({files} items)\n",
                    fam.path,
                    human(bytes)
                );
            }
        }
    }
    // Nothing in the store may be invisible: report what no family claims, both at the root (which
    // is where a store written by an older layout shows up) and inside each class root.
    let mut unclaimed: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(root) {
        for entry in rd.flatten() {
            let path = entry.path();
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if Class::all().iter().any(|c| c.path() == name) {
                if let Ok(inner) = std::fs::read_dir(&path) {
                    for child in inner.flatten() {
                        let child_path = child.path();
                        let rel = format!("{name}/{}", child.file_name().to_string_lossy());
                        if !claimed.contains(&rel) {
                            unclaimed.push(child_path);
                        }
                    }
                }
            } else if !name.ends_with(".stamp") {
                unclaimed.push(path);
            }
        }
    }
    for path in &unclaimed {
        let (bytes, files) = du(path);
        total += bytes;
        out += &format!(
            "  {:<36} {:>9}  ({files} items, unclaimed)\n",
            path.strip_prefix(root).unwrap_or(path).display(),
            human(bytes)
        );
    }
    out += &format!("  {:<36} {:>9}\n", "total", human(total));
    if total_stale > 0 {
        out += &format!(
            "stale and garbage could be cleared: {} (`mirvm cache purge`)\n",
            human(total_stale)
        );
    }
    out
}

/// Execute cleanup, return full report. dry_run lists actions without touching anything.
pub fn purge(root: &Path, plan: Purge) -> String {
    let mut out = String::new();
    let (mut freed, mut acted) = (0u64, 0u64);
    let dry = plan.dry_run;
    let rm_file = |p: &Path, why: &str, freed: &mut u64, acted: &mut u64, out: &mut String| {
        let sz = p.metadata().map(|m| m.len()).unwrap_or(0);
        *out += &format!(
            "  {} {} ({}, {why})\n",
            if dry { "to be deleted" } else { "deleted" },
            p.display(),
            human(sz)
        );
        if (!dry && std::fs::remove_file(p).is_ok()) || dry {
            *freed += sz;
            *acted += 1;
        }
    };
    let rm_dir = |d: &Path, label: &str, dry: bool, out: &mut String| -> u64 {
        let (bytes, _) = du(d);
        *out += &format!(
            "  {} {} ({}, {label})\n",
            if dry { "to be deleted" } else { "deleted" },
            d.display(),
            human(bytes)
        );
        if !dry {
            let _ = std::fs::remove_dir_all(d);
        }
        bytes
    };

    for fam in families() {
        let dir = root.join(&fam.path);
        match fam.class {
            // Generational cache: a stale generation is individually removable, which is what
            // `purge` does by default.
            Class::Cache if !fam.entry_ext.is_empty() => {
                if named_cache_flag(&fam.path, &plan) {
                    freed += rm_dir(&dir, "all generations cleared", dry, &mut out);
                    acted += 1;
                    continue;
                }
                if plan.stale || plan.all {
                    let (_, stale, garb) = split_generational(&dir, fam.entry_ext);
                    for p in stale.iter().chain(&garb) {
                        rm_file(p, "stale/garbage", &mut freed, &mut acted, &mut out);
                    }
                }
            }
            // Content-keyed cache: no generations to tell apart, so it is all or nothing.
            Class::Cache => {
                if plan.all {
                    freed += rm_dir(&dir, "content-keyed cache all cleared", dry, &mut out);
                    acted += 1;
                }
            }
            Class::Build => {
                if named_build_flag(&fam.path, &plan) || plan.all {
                    freed += rm_dir(&dir, "build space all cleared", dry, &mut out);
                    acted += 1;
                }
            }
            // Only --data reaches these: the crate store must be fetched again and the sysroot
            // rebuilt, so no other flag may take them as a side effect.
            Class::Data => {
                if plan.data {
                    freed += rm_dir(
                        &dir,
                        "data cleared (refetch/rebuild next run)",
                        dry,
                        &mut out,
                    );
                    acted += 1;
                }
            }
            Class::Run => {
                if plan.all {
                    freed += rm_dir(&dir, "process scratch cleared", dry, &mut out);
                    acted += 1;
                }
            }
        }
    }
    // Empty directory sweep: leaving shells behind is harmless, but an enumerable store reads
    // better, and it keeps a purged store from looking like a used one.
    if !dry {
        for fam in families() {
            let d = root.join(&fam.path);
            if d.is_dir() && std::fs::read_dir(&d).is_ok_and(|mut r| r.next().is_none()) {
                let _ = std::fs::remove_dir(&d);
            }
        }
        for class in Class::all() {
            let d = root.join(class.path());
            if d.is_dir() && std::fs::read_dir(&d).is_ok_and(|mut r| r.next().is_none()) {
                let _ = std::fs::remove_dir(&d);
            }
        }
    }
    if acted == 0 {
        out += "  nothing to be cleared\n";
    }
    out += &format!(
        "{}release {}\n",
        if dry { "(dry-run) estimated" } else { "" },
        human(freed)
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mirvm-cachectl-test-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Same shape as the three families' files: first field String (postcard varint + UTF-8).
    fn fake_entry(dir: &Path, name: &str, build_id: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join(name);
        let mut bytes = postcard::to_stdvec(&build_id.to_string()).unwrap();
        bytes.extend_from_slice(b"payload-bytes-after-header");
        std::fs::write(&p, &bytes).unwrap();
        p
    }

    #[test]
    fn peek_reads_postcard_first_string() {
        let root = temp_root("peek");
        let p = fake_entry(&root, "a.bin", "0123456789abcdef");
        assert_eq!(file_build_id(&p).as_deref(), Some("0123456789abcdef"));
        assert_eq!(file_build_id(&root.join("missing.bin")), None);
        let junk = root.join("junk.bin");
        std::fs::write(&junk, b"\xff\xff\xff\xff").unwrap();
        assert_eq!(file_build_id(&junk), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn classify_covers_current_stale_and_garbage() {
        let root = temp_root("classify");
        let cur = fake_entry(&root, "cur.bin", crate::options::build::BUILD_ID);
        let old = fake_entry(&root, "old.bin", "0000000000000000");
        let tmp = root.join(".cur.bin.tmp-123");
        std::fs::write(&tmp, b"orphan").unwrap();
        assert_eq!(classify(&cur), Staleness::Current);
        assert_eq!(classify(&old), Staleness::Stale);
        assert_eq!(classify(&tmp), Staleness::Garbage);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn purge_stale_keeps_current_and_dry_run_touches_nothing() {
        let root = temp_root("purge");
        let deps = root.join("cache/deps");
        let cur = fake_entry(&deps, "cur.img", crate::options::build::BUILD_ID);
        let old = fake_entry(&deps, "old.img", "0000000000000000");
        // non-entry files (build byproducts) do not enter classification, are left untouched and unreported
        let log = deps.join("build.log");
        std::fs::write(&log, b"build output").unwrap();
        // dry-run: list only, no action
        let report = purge(
            &root,
            Purge {
                stale: true,
                dry_run: true,
                ..Default::default()
            },
        );
        assert!(report.contains("to be deleted"));
        assert!(!report.contains("build.log"));
        assert!(old.exists() && cur.exists());
        // real cleanup: stale goes, current stays, byproducts untouched
        let report = purge(
            &root,
            Purge {
                stale: true,
                ..Default::default()
            },
        );
        assert!(report.contains("deleted") && !report.contains("to be deleted"));
        assert!(!old.exists() && cur.exists() && log.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    /// `--all` may take everything that needs no network: cache/, build/ and run/. `data/` is
    /// reachable only through `--data`, because losing it costs a re-fetch and a sysroot rebuild.
    fn purge_all_reaches_cache_build_and_run_but_not_data() {
        let root = temp_root("purge-all");
        let sysroot = root.join(format!("data/sysroot-{}", crate::options::build::HOST));
        std::fs::create_dir_all(sysroot.join("lib")).unwrap();
        std::fs::write(sysroot.join("lib/x.rlib"), b"x").unwrap();
        fake_entry(&root.join("cache/ir"), "a.bin", "0000000000000000");
        std::fs::create_dir_all(root.join("build/scripts/h/target")).unwrap();
        std::fs::create_dir_all(root.join("run/engine")).unwrap();
        std::fs::write(root.join("run/engine/scratch"), b"x").unwrap();
        std::fs::create_dir_all(root.join("data/registry/git/db")).unwrap();
        std::fs::write(root.join("data/registry/git/db/object"), b"git").unwrap();

        purge(
            &root,
            Purge {
                all: true,
                ..Default::default()
            },
        );
        assert!(!root.join("cache/ir").exists());
        assert!(!root.join("build/scripts").exists());
        assert!(!root.join("run").exists());
        assert!(sysroot.exists(), "--all must not take data/");
        assert!(
            root.join("data/registry").exists(),
            "--all must not take data/"
        );

        purge(
            &root,
            Purge {
                all: true,
                data: true,
                ..Default::default()
            },
        );
        assert!(!sysroot.exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
