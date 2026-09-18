//! Inventory and cleanup of the mirvm local repository (`$HOME/.mirvm`, address changeable via `MIRVM_HOME`):
//! `mirvm cache status` / `mirvm cache purge` (decision-history §7.14).
//!
//! Family tree and generational semantics:
//! - sysroot-<host>: MIR-rich std (content-keyed stable; cleared only by --sysroot)
//! - scripts/: frontmatter materialized projects + two cargo targets (local dependency libraries without packaging the world +
//!   B-dimensional native cross-build; largest; cleared by --scripts/--all)
//! - registry/: cargoless crates.io and Git source/checkouts (cleared by --all)
//! - deps/ base/ ir/: lowering accelerators, filenames = fnv(build_id, …) opaque hash —
//!   generational staleness checks read the first field build_id from files (first field of all three families,
//!   postcard varint encoding, peek zero-decode has no side effects; full module decode would trigger frozen-region base mmap, disabled)
//! - native-archives/ global-asm/ asm-stubs/: content-keyed .so (runtime dlopen objects)
//!
//! All components self-heal: purging any family only affects next-run speed, not correctness.

use std::path::{Path, PathBuf};

/// Cleanup plan (product of CLI flag parsing).
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Purge {
    /// Three generational families (files in deps/base/ir whose build_id ≠ current build + tmp orphans/junk)
    pub stale: bool,
    pub deps: bool,
    pub base: bool,
    pub ir: bool,
    pub scripts: bool,
    /// Unified target dir (D14 shared dependency storage; one of the largest)
    pub target: bool,
    /// Clear everything except sysroot (all generations of three families + scripts + three .so families)
    pub all: bool,
    /// Clear sysroot too (full cold start; only stacked on --all semantics)
    pub sysroot: bool,
    pub dry_run: bool,
}

struct Family {
    /// Display name (= directory name)
    name: String,
    /// Generational semantics (build_id first field determines generation) family entry extension; empty = non-generational family
    entry_ext: &'static str,
}

fn families() -> Vec<Family> {
    let host = env!("MIRVM_HOST");
    [
        (format!("sysroot-{host}"), ""),
        ("scripts".into(), ""),
        ("target".into(), ""),
        ("registry".into(), ""),
        ("deps".into(), "img"),
        ("base".into(), "img"),
        ("ir".into(), "bin"),
        ("native-archives".into(), ""),
        ("global-asm".into(), ""),
        ("asm-stubs".into(), ""),
    ]
    .into_iter()
    .map(|(name, entry_ext)| Family { name, entry_ext })
    .collect()
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
        Some(id) if id == env!("MIRVM_BUILD_ID") => Staleness::Current,
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
    let mut out = format!(
        "mirvm local cache {} (build {})\n",
        root.display(),
        env!("MIRVM_BUILD_ID")
    );
    let (mut total, mut total_stale) = (0u64, 0u64);
    let mut known = Vec::new();
    for fam in families() {
        let dir = root.join(&fam.name);
        known.push(fam.name.clone());
        if !fam.entry_ext.is_empty() {
            let (cur, stale, garb) = split_generational(&dir, fam.entry_ext);
            let (cs, ss, gs) = (size_of(&cur), size_of(&stale), size_of(&garb));
            let sum = cs + ss + gs;
            total += sum;
            total_stale += ss + gs;
            let n = cur.len() + stale.len() + garb.len();
            out += &format!(
                "  {:<36} {:>9}  ({n} items; stale {}, garbage {})\n",
                fam.name,
                human(sum),
                human(ss),
                human(gs)
            );
        } else {
            let (bytes, files) = du(&dir);
            total += bytes;
            out += &format!("  {:<36} {:>9}  ({files} items)\n", fam.name, human(bytes));
        }
    }
    // non-family miscellaneous items (e.g. tests project-suite)
    if let Ok(rd) = std::fs::read_dir(root) {
        for e in rd.flatten() {
            let p = e.path();
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if !known.iter().any(|k| k == &name) && !name.ends_with(".stamp") {
                let (bytes, files) = du(&p);
                total += bytes;
                out += &format!("  {:<36} {:>9}  ({files} items)\n", name, human(bytes));
            }
        }
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

    // whole-family flag union semantics with --all
    let whole = |name: &str, flag: bool| flag || (plan.all && name != "sysroot");
    for fam in families() {
        let name = fam.name.as_str();
        let dir = root.join(name);
        if !fam.entry_ext.is_empty() {
            let (cur, stale, garb) = split_generational(&dir, fam.entry_ext);
            if whole(
                name,
                matches!(name, "deps" if plan.deps)
                    || matches!(name, "base" if plan.base)
                    || matches!(name, "ir" if plan.ir),
            ) {
                freed += rm_dir(&dir, "all cleared", dry, &mut out);
                acted += 1;
                continue;
            }
            if plan.stale || plan.all {
                for p in stale.iter().chain(&garb) {
                    rm_file(p, "stale/garbage", &mut freed, &mut acted, &mut out);
                }
                let _ = cur;
            }
        } else if name == "scripts" && whole(name, plan.scripts) {
            freed += rm_dir(&dir, "scripts caches all cleared", dry, &mut out);
            acted += 1;
        } else if name == "target" && whole(name, plan.target) {
            freed += rm_dir(&dir, "target cache all cleared", dry, &mut out);
            acted += 1;
        } else if name == "registry" && plan.all {
            freed += rm_dir(&dir, "registry/Git caches all cleared", dry, &mut out);
            acted += 1;
        } else if name.starts_with("sysroot-") && plan.all && plan.sysroot {
            freed += rm_dir(
                &dir,
                "sysroot all cleared (next run will be cold start)",
                dry,
                &mut out,
            );
            acted += 1;
        } else if plan.all && matches!(name, "native-archives" | "global-asm" | "asm-stubs") {
            freed += rm_dir(&dir, ".so all cleared", dry, &mut out);
            acted += 1;
        }
    }
    // empty directory sweep (leaving empty family directory shells after purge is harmless, keeps repo root enumerable)
    if !dry {
        for fam in families() {
            let d = root.join(&fam.name);
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
        let cur = fake_entry(&root, "cur.bin", env!("MIRVM_BUILD_ID"));
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
        let deps = root.join("deps");
        let cur = fake_entry(&deps, "cur.img", env!("MIRVM_BUILD_ID"));
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
    fn purge_all_leaves_sysroot_unless_flagged() {
        let root = temp_root("purge-all");
        let sysroot = root.join(format!("sysroot-{}", env!("MIRVM_HOST")));
        std::fs::create_dir_all(sysroot.join("lib")).unwrap();
        std::fs::write(sysroot.join("lib/x.rlib"), b"x").unwrap();
        fake_entry(&root.join("ir"), "a.bin", "0000000000000000");
        std::fs::create_dir_all(root.join("scripts/h/target")).unwrap();
        std::fs::create_dir_all(root.join("registry/git/db")).unwrap();
        std::fs::write(root.join("registry/git/db/object"), b"git").unwrap();
        purge(
            &root,
            Purge {
                all: true,
                ..Default::default()
            },
        );
        assert!(sysroot.exists());
        assert!(!root.join("scripts").exists());
        assert!(!root.join("ir").exists());
        assert!(!root.join("registry").exists());
        purge(
            &root,
            Purge {
                all: true,
                sysroot: true,
                ..Default::default()
            },
        );
        assert!(!sysroot.exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
