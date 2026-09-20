//! Inventory and cleanup of the mirvm local store: `mirvm cache status` / `mirvm cache purge`.
//!
//! The store itself — its lifetime classes, its families and the publication rules — is
//! `crate::store`. This module is the report over `store::FAMILIES`: it sizes each family, groups
//! it by lifetime, and removes what the plan selects. No directory is named here, so a family added
//! to the register is reported and cleanable without a second edit.

use std::path::{Path, PathBuf};

use crate::store::{self, Class, Family, FamilyFlag, Shape};

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

impl Purge {
    /// Whether the user named this family through its own `--flag`.
    fn names(&self, family: &Family) -> bool {
        match family.flag {
            Some(FamilyFlag::Base) => self.base,
            Some(FamilyFlag::Deps) => self.deps,
            Some(FamilyFlag::Ir) => self.ir,
            Some(FamilyFlag::Scripts) => self.scripts,
            Some(FamilyFlag::Target) => self.target,
            None => false,
        }
    }

    /// Whether the plan takes this whole family, flag or class flag alike.
    fn takes(&self, family: &Family) -> bool {
        self.names(family)
            || match family.class {
                Class::Data => self.data,
                Class::Cache | Class::Build | Class::Run => self.all,
            }
    }
}

/// Recursively accumulate directory size and file count; nonexistent ⇒ (0, 0).
fn du(path: &Path) -> (u64, u64) {
    let mut bytes = 0u64;
    let mut files = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in read.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(metadata) = path.metadata() {
                bytes += metadata.len();
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

fn size_of(paths: &[PathBuf]) -> u64 {
    paths
        .iter()
        .map(|path| path.metadata().map(|m| m.len()).unwrap_or(0))
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
    for class in Class::ALL {
        out += &format!("  {}/  ({})\n", class.name(), class.contract());
        for family in store::FAMILIES.iter().filter(|f| f.class == class) {
            let name = family.path();
            claimed.push(name.clone());
            let dir = family.dir_in(root);
            if let Shape::Generation { ext } = family.shape {
                let (current, stale, garbage) = store::split_generational(&dir, ext);
                let (cs, ss, gs) = (size_of(&current), size_of(&stale), size_of(&garbage));
                total += cs + ss + gs;
                total_stale += ss + gs;
                let count = current.len() + stale.len() + garbage.len();
                out += &format!(
                    "    {name:<36} {:>9}  ({count} items; stale {}, garbage {})\n",
                    human(cs + ss + gs),
                    human(ss),
                    human(gs)
                );
            } else {
                let (bytes, files) = du(&dir);
                total += bytes;
                // A keyed family is cleaned exactly like a single artifact; naming the shape is what
                // tells a reader why it cannot be cleaned partially.
                let kind = match family.shape {
                    Shape::Keyed => "content-keyed",
                    _ => "one artifact",
                };
                out += &format!(
                    "    {name:<36} {:>9}  ({files} items, {kind})\n",
                    human(bytes)
                );
            }
        }
    }
    // Nothing in the store may be invisible: report what no family claims, both at the root (which
    // is where a store written by an older layout shows up) and inside each class root.
    let mut unclaimed: Vec<PathBuf> = Vec::new();
    if let Ok(read) = std::fs::read_dir(root) {
        for entry in read.flatten() {
            let path = entry.path();
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if Class::ALL.iter().any(|class| class.name() == name) {
                if let Ok(inner) = std::fs::read_dir(&path) {
                    for child in inner.flatten() {
                        let relative = format!("{name}/{}", child.file_name().to_string_lossy());
                        if !claimed.contains(&relative) {
                            unclaimed.push(child.path());
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
            "  {:<38} {:>9}  ({files} items, unclaimed)\n",
            path.strip_prefix(root).unwrap_or(path).display(),
            human(bytes)
        );
    }
    out += &format!("  {:<38} {:>9}\n", "total", human(total));
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
    let rm_file = |path: &Path, why: &str, freed: &mut u64, acted: &mut u64, out: &mut String| {
        let size = path.metadata().map(|m| m.len()).unwrap_or(0);
        *out += &format!(
            "  {} {} ({}, {why})\n",
            if dry { "to be deleted" } else { "deleted" },
            path.display(),
            human(size)
        );
        if (!dry && std::fs::remove_file(path).is_ok()) || dry {
            *freed += size;
            *acted += 1;
        }
    };
    let rm_dir = |dir: &Path, label: &str, dry: bool, out: &mut String| -> u64 {
        let (bytes, _) = du(dir);
        *out += &format!(
            "  {} {} ({}, {label})\n",
            if dry { "to be deleted" } else { "deleted" },
            dir.display(),
            human(bytes)
        );
        if !dry {
            let _ = std::fs::remove_dir_all(dir);
        }
        bytes
    };

    for family in store::FAMILIES {
        let dir = family.dir_in(root);
        match family.shape {
            // Generational cache: a stale generation is individually removable, which is what
            // `purge` does by default. Naming the family is the user asking for all of it; `--all`
            // keeps the current generation, which is what makes the next run fast.
            Shape::Generation { ext } => {
                if plan.names(family) {
                    freed += rm_dir(&dir, "all generations cleared", dry, &mut out);
                    acted += 1;
                } else if plan.stale || plan.all {
                    let (_, stale, garbage) = store::split_generational(&dir, ext);
                    for path in stale.iter().chain(&garbage) {
                        rm_file(path, "stale/garbage", &mut freed, &mut acted, &mut out);
                    }
                }
            }
            // No generations to tell apart, so it is all or nothing. The class contract is the
            // reason it is deletable, and it is the label the report states.
            _ => {
                if plan.takes(family) {
                    freed += rm_dir(&dir, family.class.contract(), dry, &mut out);
                    acted += 1;
                }
            }
        }
    }
    // Empty directory sweep: leaving shells behind is harmless, but an enumerable store reads
    // better, and it keeps a purged store from looking like a used one.
    if !dry {
        for family in store::FAMILIES {
            let dir = family.dir_in(root);
            if dir.is_dir() && std::fs::read_dir(&dir).is_ok_and(|mut r| r.next().is_none()) {
                let _ = std::fs::remove_dir(&dir);
            }
        }
        for class in Class::ALL {
            let dir = root.join(class.name());
            if dir.is_dir() && std::fs::read_dir(&dir).is_ok_and(|mut r| r.next().is_none()) {
                let _ = std::fs::remove_dir(&dir);
            }
        }
    }
    if acted == 0 {
        out += "  nothing to be cleared\n";
    }
    out += &format!(
        "{}release {}\n",
        if dry { "(dry-run) estimated " } else { "" },
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
    fn purge_stale_keeps_current_and_dry_run_touches_nothing() {
        let root = temp_root("purge");
        let deps = store::DEPS.dir_in(&root);
        let current = fake_entry(&deps, "cur.img", crate::options::build::BUILD_ID);
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
        assert!(old.exists() && current.exists());
        // real cleanup: stale goes, current stays, byproducts untouched
        let report = purge(
            &root,
            Purge {
                stale: true,
                ..Default::default()
            },
        );
        assert!(report.contains("deleted") && !report.contains("to be deleted"));
        assert!(!old.exists() && current.exists() && log.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    /// `--all` may take everything that needs no network: cache/, build/ and run/. `data/` is
    /// reachable only through `--data`, because losing it costs a re-fetch and a sysroot rebuild.
    fn purge_all_reaches_cache_build_and_run_but_not_data() {
        let root = temp_root("purge-all");
        let sysroot = store::SYSROOT.dir_in(&root);
        std::fs::create_dir_all(sysroot.join("lib")).unwrap();
        std::fs::write(sysroot.join("lib/x.rlib"), b"x").unwrap();
        fake_entry(
            &store::IR.dir_in(&root),
            "a.bin",
            crate::options::build::BUILD_ID,
        );
        std::fs::create_dir_all(store::SCRIPTS.dir_in(&root).join("h/target")).unwrap();
        std::fs::create_dir_all(store::SYSROOT_BUILD.dir_in(&root).join("root")).unwrap();
        let native = store::RUNTIME_NATIVE.dir_in(&root);
        std::fs::create_dir_all(&native).unwrap();
        std::fs::write(native.join("scratch"), b"x").unwrap();
        let registry = store::REGISTRY.dir_in(&root).join("git/db");
        std::fs::create_dir_all(&registry).unwrap();
        std::fs::write(registry.join("object"), b"git").unwrap();

        purge(
            &root,
            Purge {
                all: true,
                ..Default::default()
            },
        );
        assert!(!store::SCRIPTS.dir_in(&root).exists());
        assert!(!store::SYSROOT_BUILD.dir_in(&root).exists());
        assert!(!root.join("run").exists());
        assert!(sysroot.exists(), "--all must not take data/");
        assert!(
            store::REGISTRY.dir_in(&root).exists(),
            "--all must not take data/"
        );
        // The current generation is what makes the next run fast, so `--all` keeps it.
        assert!(store::IR.dir_in(&root).join("a.bin").exists());

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

    #[test]
    /// A family flag takes that family whole, including its current generation, and nothing else.
    fn named_family_flag_takes_exactly_that_family() {
        let root = temp_root("purge-named");
        let current = fake_entry(
            &store::DEPS.dir_in(&root),
            "cur.img",
            crate::options::build::BUILD_ID,
        );
        let other = fake_entry(
            &store::IR.dir_in(&root),
            "cur.bin",
            crate::options::build::BUILD_ID,
        );
        let report = purge(
            &root,
            Purge {
                deps: true,
                ..Default::default()
            },
        );
        assert!(report.contains("all generations cleared"));
        assert!(!current.exists() && other.exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
