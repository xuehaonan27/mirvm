//! Inventory and cleanup of the mirvm local store: `mirvm cache status` / `mirvm cache purge`.
//!
//! The store itself — its lifetime classes, its families and the publication rules — is
//! `crate::store`. This module is the report over `store::FAMILIES`: it sizes each family, groups it
//! by lifetime, and removes what the plan selects. No directory is named here, so a family added to
//! the register is reported and cleanable without a second edit.
//!
//! Each report is data first: `text` and `json` are two renderings of one structure, so a field can
//! neither exist in one and be missing from the other, nor be computed twice.

use std::path::{Path, PathBuf};

use crate::diag::json::{self, Writer};
use crate::diag::table::{Cell, Table, human_bytes};
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

// ===== `mirvm cache status` =====

/// One family's size and item count.
struct FamilyStatus {
    path: String,
    bytes: u64,
    items: u64,
    /// `Some((stale, garbage))` for a generational family: what `purge` can take back without
    /// touching the current generation.
    generations: Option<(u64, u64)>,
    /// For the others, how the family is keyed — which is why it cannot be cleaned partially.
    kind: &'static str,
}

impl FamilyStatus {
    fn detail(&self) -> String {
        match self.generations {
            Some((stale, garbage)) => format!(
                "({} items; stale {}, garbage {})",
                self.items,
                human_bytes(stale),
                human_bytes(garbage)
            ),
            None => format!("({} items, {})", self.items, self.kind),
        }
    }
}

/// One lifetime class (`cache/`, `data/`, …).
struct ClassStatus {
    name: &'static str,
    contract: &'static str,
    families: Vec<FamilyStatus>,
}

/// A directory no family claims, at the store root or inside a class root.
struct Unclaimed {
    path: String,
    bytes: u64,
    items: u64,
}

/// The `mirvm cache status` report.
pub struct Status {
    pub root: PathBuf,
    classes: Vec<ClassStatus>,
    unclaimed: Vec<Unclaimed>,
    total_bytes: u64,
    /// What `mirvm cache purge` would reclaim: stale generations and garbage.
    pub stale_bytes: u64,
}

impl Status {
    /// Human text. The first line is parsed by the test harness for the build id; its shape is a
    /// contract, so it is built here rather than by the table renderer.
    pub fn text(&self) -> String {
        let mut out = format!(
            "mirvm local cache {} (build {})\n",
            self.root.display(),
            crate::options::build::BUILD_ID
        );
        for class in &self.classes {
            out.push_str(&format!("  {}/  ({})\n", class.name, class.contract));
            let mut table = Table::new(4);
            for family in &class.families {
                table.row(vec![
                    Cell::left(&family.path),
                    Cell::right(human_bytes(family.bytes)),
                    Cell::left(family.detail()),
                ]);
            }
            out.push_str(&table.render());
        }
        // Nothing in the store may be invisible: report what no family claims, both at the root
        // (which is where a store written by an older layout shows up) and inside each class root.
        if !self.unclaimed.is_empty() {
            let mut table = Table::new(2);
            for row in &self.unclaimed {
                table.row(vec![
                    Cell::left(&row.path),
                    Cell::right(human_bytes(row.bytes)),
                    Cell::left(format!("({} items, unclaimed)", row.items)),
                ]);
            }
            out.push_str(&table.render());
        }
        let mut total = Table::new(2);
        total.row(vec![
            Cell::left("total"),
            Cell::right(human_bytes(self.total_bytes)),
        ]);
        out.push_str(&total.render());
        if self.stale_bytes > 0 {
            out.push_str(&format!(
                "stale and garbage could be cleared: {} (`mirvm cache purge`)\n",
                human_bytes(self.stale_bytes)
            ));
        }
        out
    }

    /// The same report as one versioned JSON document.
    pub fn json(&self) -> String {
        let classes: Vec<String> = self
            .classes
            .iter()
            .map(|class| {
                let families: Vec<String> = class
                    .families
                    .iter()
                    .map(|family| {
                        let mut out = Writer::new();
                        out.string("path", &family.path);
                        out.number("bytes", family.bytes);
                        out.number("items", family.items);
                        match family.generations {
                            Some((stale, garbage)) => {
                                out.number("stale_bytes", stale);
                                out.number("garbage_bytes", garbage);
                            }
                            None => {
                                out.string("kind", family.kind);
                            }
                        };
                        out.finish()
                    })
                    .collect();
                let mut out = Writer::new();
                out.string("name", class.name);
                out.string("contract", class.contract);
                out.raw("families", &json::array(&families));
                out.finish()
            })
            .collect();
        let unclaimed: Vec<String> = self
            .unclaimed
            .iter()
            .map(|row| {
                let mut out = Writer::new();
                out.string("path", &row.path);
                out.number("bytes", row.bytes);
                out.number("items", row.items);
                out.finish()
            })
            .collect();
        let mut out = Writer::document();
        out.string("root", &self.root.display().to_string());
        out.string("build_id", crate::options::build::BUILD_ID);
        out.raw("classes", &json::array(&classes));
        out.raw("unclaimed", &json::array(&unclaimed));
        out.number("total_bytes", self.total_bytes);
        out.number("stale_bytes", self.stale_bytes);
        out.finish()
    }
}

/// Size every family, group it by lifetime, and report what no family claims.
pub fn status(root: &Path) -> Status {
    let mut total = 0u64;
    let mut total_stale = 0u64;
    let mut claimed: Vec<String> = Vec::new();
    let mut classes: Vec<ClassStatus> = Vec::new();
    for class in Class::ALL {
        let mut families: Vec<FamilyStatus> = Vec::new();
        for family in store::FAMILIES.iter().filter(|f| f.class == class) {
            let name = family.path();
            claimed.push(name.clone());
            let dir = family.dir_in(root);
            let status = match family.shape {
                Shape::Generation { ext } => {
                    let (current, stale, garbage) = store::split_generational(&dir, ext);
                    let (current_bytes, stale_bytes, garbage_bytes) =
                        (size_of(&current), size_of(&stale), size_of(&garbage));
                    let bytes = current_bytes + stale_bytes + garbage_bytes;
                    total += bytes;
                    total_stale += stale_bytes + garbage_bytes;
                    FamilyStatus {
                        path: name,
                        bytes,
                        items: (current.len() + stale.len() + garbage.len()) as u64,
                        generations: Some((stale_bytes, garbage_bytes)),
                        kind: "content-keyed",
                    }
                }
                // A keyed family is cleaned exactly like a single artifact; naming the shape is what
                // tells a reader why it cannot be cleaned partially.
                _ => {
                    let (bytes, files) = du(&dir);
                    total += bytes;
                    FamilyStatus {
                        path: name,
                        bytes,
                        items: files,
                        generations: None,
                        kind: match family.shape {
                            Shape::Keyed => "content-keyed",
                            _ => "one artifact",
                        },
                    }
                }
            };
            families.push(status);
        }
        classes.push(ClassStatus {
            name: class.name(),
            contract: class.contract(),
            families,
        });
    }

    let mut unclaimed: Vec<Unclaimed> = Vec::new();
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
                            unclaimed.push(unclaimed_row(root, &child.path()));
                        }
                    }
                }
            } else if !name.ends_with(".stamp") {
                unclaimed.push(unclaimed_row(root, &path));
            }
        }
    }
    for row in &unclaimed {
        total += row.bytes;
    }
    Status {
        root: root.to_path_buf(),
        classes,
        unclaimed,
        total_bytes: total,
        stale_bytes: total_stale,
    }
}

fn unclaimed_row(root: &Path, path: &Path) -> Unclaimed {
    let (bytes, items) = du(path);
    Unclaimed {
        path: path
            .strip_prefix(root)
            .unwrap_or(path)
            .display()
            .to_string(),
        bytes,
        items,
    }
}

fn size_of(paths: &[PathBuf]) -> u64 {
    paths
        .iter()
        .map(|path| path.metadata().map(|m| m.len()).unwrap_or(0))
        .sum()
}

// ===== `mirvm cache purge` =====

/// One thing `cache purge` did, or would do under `--dry-run`.
pub struct PurgeAction {
    pub deleted: bool,
    pub path: String,
    pub bytes: u64,
    /// Why this path is removable: the class contract, or `stale/garbage`.
    pub reason: String,
}

/// The `mirvm cache purge` report.
pub struct PurgeReport {
    pub dry_run: bool,
    pub actions: Vec<PurgeAction>,
    pub freed_bytes: u64,
}

impl PurgeReport {
    pub fn text(&self) -> String {
        let verb = |deleted: bool| {
            if self.dry_run {
                "to be deleted"
            } else if deleted {
                "deleted"
            } else {
                "kept"
            }
        };
        let mut out = String::new();
        let mut table = Table::new(2);
        for action in &self.actions {
            table.row(vec![
                Cell::left(verb(action.deleted)),
                Cell::left(&action.path),
                Cell::left(format!(
                    "({}, {})",
                    human_bytes(action.bytes),
                    action.reason
                )),
            ]);
        }
        if self.actions.is_empty() {
            table.row(vec![Cell::left("nothing to be cleared")]);
        }
        out.push_str(&table.render());
        out.push_str(&format!(
            "{}{}\n",
            if self.dry_run {
                "(dry-run) estimated release "
            } else {
                "release "
            },
            human_bytes(self.freed_bytes)
        ));
        out
    }

    pub fn json(&self) -> String {
        let actions: Vec<String> = self
            .actions
            .iter()
            .map(|action| {
                let mut out = Writer::new();
                out.boolean("deleted", action.deleted);
                out.string("path", &action.path);
                out.number("bytes", action.bytes);
                out.string("reason", &action.reason);
                out.finish()
            })
            .collect();
        let mut out = Writer::document();
        out.boolean("dry_run", self.dry_run);
        out.raw("actions", &json::array(&actions));
        out.number("freed_bytes", self.freed_bytes);
        out.finish()
    }
}

/// Execute cleanup and return the report. `dry_run` lists actions without touching anything.
pub fn purge(root: &Path, plan: Purge) -> PurgeReport {
    let dry = plan.dry_run;
    let mut report = PurgeReport {
        dry_run: dry,
        actions: Vec::new(),
        freed_bytes: 0,
    };
    for family in store::FAMILIES {
        let dir = family.dir_in(root);
        match family.shape {
            // Generational cache: a stale generation is individually removable, which is what
            // `purge` does by default. Naming the family is the user asking for all of it; `--all`
            // keeps the current generation, which is what makes the next run fast.
            Shape::Generation { ext } => {
                if plan.names(family) {
                    remove_dir(&mut report, &dir, "all generations cleared", dry);
                } else if plan.stale || plan.all {
                    let (_, stale, garbage) = store::split_generational(&dir, ext);
                    for path in stale.iter().chain(&garbage) {
                        remove_file(&mut report, path, "stale/garbage", dry);
                    }
                }
            }
            // No generations to tell apart, so it is all or nothing. The class contract is the
            // reason it is deletable, and it is the label the report states.
            _ => {
                if plan.takes(family) {
                    let reason = family.class.contract();
                    remove_dir(&mut report, &dir, reason, dry);
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
    report
}

fn remove_file(report: &mut PurgeReport, path: &Path, reason: &str, dry: bool) {
    let bytes = path.metadata().map(|m| m.len()).unwrap_or(0);
    let deleted = dry || std::fs::remove_file(path).is_ok();
    if deleted {
        report.freed_bytes += bytes;
    }
    report.actions.push(PurgeAction {
        deleted,
        path: path.display().to_string(),
        bytes,
        reason: reason.to_string(),
    });
}

fn remove_dir(report: &mut PurgeReport, dir: &Path, reason: &str, dry: bool) -> u64 {
    let (bytes, _) = du(dir);
    if !dry {
        let _ = std::fs::remove_dir_all(dir);
    }
    report.freed_bytes += bytes;
    report.actions.push(PurgeAction {
        deleted: true,
        path: dir.display().to_string(),
        bytes,
        reason: reason.to_string(),
    });
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mirvm-cache-report-test-{}-{}",
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
        let text = report.text();
        assert!(text.contains("to be deleted"));
        assert!(!text.contains("build.log"));
        assert!(old.exists() && current.exists());
        // real cleanup: stale goes, current stays, byproducts untouched
        let report = purge(
            &root,
            Purge {
                stale: true,
                ..Default::default()
            },
        );
        let text = report.text();
        assert!(text.contains("deleted") && !text.contains("to be deleted"));
        assert!(!old.exists() && current.exists() && log.exists());
        assert_eq!(report.actions.len(), 1);
        assert!(report.freed_bytes > 0);
        assert!(report.json().contains("\"deleted\":true"));
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
        assert!(report.text().contains("all generations cleared"));
        assert!(!current.exists() && other.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    /// The status report's first line is a machine contract (the harness reads the build id from it),
    /// and every family in the register appears in both renderings.
    fn status_reports_every_family_and_the_build_id() {
        let root = temp_root("status");
        let report = status(&root);
        let text = report.text();
        let first = text.lines().next().unwrap();
        assert!(
            first.starts_with("mirvm local cache ") && first.ends_with(')'),
            "{first}"
        );
        assert!(first.contains(crate::options::build::BUILD_ID));
        for family in store::FAMILIES {
            assert!(text.contains(&family.path()), "{} missing", family.path());
            assert!(
                report.json().contains(&family.path()),
                "{} missing from json",
                family.path()
            );
        }
        // A store written by an older layout is visible rather than silently dropped.
        std::fs::create_dir_all(root.join("base")).unwrap();
        std::fs::write(root.join("base/stale.img"), b"old").unwrap();
        let report = status(&root);
        assert!(report.text().contains("base/stale.img") && report.text().contains("unclaimed"));
        assert!(
            report
                .json()
                .contains("\"unclaimed\":[{\"path\":\"base/stale.img\"")
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
