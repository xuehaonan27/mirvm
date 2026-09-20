//! Trap-debt statistics (survey instrument; pure IR analysis).
//! `mirvm run --engine vm --vm-stats <file.rs>`.
//!
//! Reachability blind spots the lowerer closes: a failed statement becomes a `Stmt::Trap`
//! placeholder while its terminator still lowers (keeping Call edges), and a function with
//! non-scalar params gets an entry Trap statement while its body still lowers -- so the BFS
//! now sees the whole downstream cone.
//! Remaining blind spots: indirect calls (fn pointers) and whole-function trap bodies
//! (layout failure / intrinsic) still have no out-edges.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::diag::json::{self, Writer};
use crate::diag::table::{Cell, Table};

use super::ir::{Module, Stmt, Terminator};

/// Grouping key: truncate the diagnostic string. Debug payloads make every reason unique,
/// so reasons are classified by prefix instead.
fn group_key(reason: &str) -> String {
    let cut = reason
        .char_indices()
        .nth(40)
        .map(|(i, _)| i)
        .unwrap_or(reason.len());
    reason[..cut].to_string()
}

/// Phase bucket of a diagnostic string, keyed by its embedded label.
fn phase_of(reason: &str) -> &'static str {
    if reason.starts_with("foreign") {
        "foreign(os::seed)"
    } else if reason.contains("M4.1+") {
        "M4.1+"
    } else if reason.contains("M4.1") {
        "M4.1"
    } else if reason.contains("M4.2") {
        "M4.2"
    } else if reason.contains("M4.3") {
        "M4.3"
    } else if reason.contains("M4.4") {
        "M4.4"
    } else if reason.contains("M4.x") {
        "M4.x"
    } else {
        "unlabeled"
    }
}

/// All Trap reasons of one function body: statement-level plus terminator-level.
fn traps_of(f: &super::ir::FuncBody) -> impl Iterator<Item = &str> {
    f.blocks.iter().flat_map(|b| {
        let stmt_traps = b.stmts.iter().filter_map(|s| match s {
            Stmt::Trap(r) => Some(&**r),
            _ => None,
        });
        let term_trap = match &b.term {
            Terminator::Trap(r) => Some(&**r),
            _ => None,
        };
        stmt_traps.chain(term_trap)
    })
}
/// The `--vm-stats` survey: where the module still has Trap debt.
///
/// Data first, like every other report: `text` and `json` are two renderings of one structure, so a
/// number cannot appear in one and be missing from the other.
pub struct Survey {
    pub instances: usize,
    pub with_trap: usize,
    /// Trap counts per phase bucket, descending.
    pub phases: Vec<(String, usize)>,
    /// Reasons truncated to the TOP25 cutoff.
    pub top_reasons: Vec<(String, usize)>,
    /// Every foreign reason, untruncated: these are the seeds for the `os::` registry, so they must
    /// not compete with the TOP25 cutoff.
    pub foreign: Vec<(String, usize)>,
    /// Reachable-Trap analysis per unmangled export/entry.
    pub exports: Vec<ExportDebt>,
}

/// One export's reachable cone.
pub struct ExportDebt {
    pub name: String,
    pub reachable_fns: usize,
    /// Empty when the reachable cone is trap-free.
    pub reasons: Vec<(String, usize)>,
    /// `(phase, count)`, descending; empty exactly when `reasons` is.
    pub phases: Vec<(String, usize)>,
}

/// Analyse one module. The reachability blind spots and their closures are described in the module
/// note; this function only reads the IR.
pub fn survey(module: &Module) -> Survey {
    let total = module.funcs.len();
    let mut funcs_with_trap = 0usize;
    let mut histogram: HashMap<String, usize> = HashMap::new();
    let mut phases: HashMap<&'static str, usize> = HashMap::new();
    for f in &module.funcs {
        let mut has = false;
        for r in traps_of(f) {
            has = true;
            *histogram.entry(group_key(r)).or_default() += 1;
            *phases.entry(phase_of(r)).or_default() += 1;
        }
        if has {
            funcs_with_trap += 1;
        }
    }

    let mut foreign: HashMap<&str, usize> = HashMap::new();
    for f in &module.funcs {
        for r in traps_of(f) {
            if r.starts_with("foreign") {
                *foreign.entry(r).or_default() += 1;
            }
        }
    }

    // Unmangled exports (no_mangle / @entry): reachable Trap analysis, BFS over Call edges.
    let mut exports: Vec<(&str, u32)> = module
        .exports
        .iter()
        .filter(|(name, _)| !name.starts_with("_ZN") && !name.starts_with("_R"))
        .map(|(n, id)| (&**n, *id))
        .collect();
    exports.sort();
    let exports = exports
        .into_iter()
        .map(|(name, id)| {
            let mut seen: HashSet<u32> = HashSet::new();
            let mut queue = VecDeque::from([id]);
            let mut reasons: HashMap<String, usize> = HashMap::new();
            let mut reach_phases: HashMap<&'static str, usize> = HashMap::new();
            while let Some(fid) = queue.pop_front() {
                if !seen.insert(fid) {
                    continue;
                }
                let f = &module.funcs[fid as usize];
                for r in traps_of(f) {
                    *reasons.entry(group_key(r)).or_default() += 1;
                    *reach_phases.entry(phase_of(r)).or_default() += 1;
                }
                for b in &f.blocks {
                    if let Terminator::Call { callee, .. } = &b.term {
                        queue.push_back(*callee);
                    }
                }
            }
            ExportDebt {
                name: name.to_string(),
                reachable_fns: seen.len(),
                reasons: descending(reasons),
                phases: descending_phases(reach_phases),
            }
        })
        .collect();

    Survey {
        instances: total,
        with_trap: funcs_with_trap,
        phases: descending_phases(phases),
        top_reasons: descending(histogram).into_iter().take(25).collect(),
        foreign: descending(foreign)
            .into_iter()
            .map(|(reason, count)| (reason.to_string(), count))
            .collect(),
        exports,
    }
}

/// Counts sorted by count, descending; ties keep the spelling order so a report is stable.
fn descending<T: Ord>(counts: HashMap<T, usize>) -> Vec<(T, usize)> {
    let mut rows: Vec<(T, usize)> = counts.into_iter().collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    rows
}

fn descending_phases(counts: HashMap<&'static str, usize>) -> Vec<(String, usize)> {
    descending(counts)
        .into_iter()
        .map(|(phase, count)| (phase.to_string(), count))
        .collect()
}

impl Survey {
    pub fn text(&self) -> String {
        let mut out = format!(
            "instances {}, with Trap {} ({:.1}%)\n",
            self.instances,
            self.with_trap,
            self.with_trap as f64 / self.instances.max(1) as f64 * 100.0
        );
        if !self.phases.is_empty() {
            out.push_str("\n== Trap debt by phase (Trap count) ==\n");
            out.push_str(&counts(&self.phases));
        }
        if !self.top_reasons.is_empty() {
            out.push_str("\n== Trap reasons TOP25 ==\n");
            out.push_str(&counts(&self.top_reasons));
        }
        if !self.foreign.is_empty() {
            out.push_str("\n== full foreign symbol list (os:: seeds) ==\n");
            out.push_str(&counts(&self.foreign));
        }
        out.push_str("\n== reachable Traps per export/entry (BFS over Call edges) ==\n");
        for export in &self.exports {
            if export.reasons.is_empty() {
                out.push_str(&format!(
                    "  {}: trap-free ({} fns reachable)\n",
                    export.name, export.reachable_fns
                ));
                continue;
            }
            let phases: Vec<String> = export
                .phases
                .iter()
                .map(|(phase, count)| format!("{phase}:{count}"))
                .collect();
            out.push_str(&format!(
                "  {}: {} kinds of reachable Trap, {} fns reachable | {}\n",
                export.name,
                export.reasons.len(),
                export.reachable_fns,
                phases.join(" ")
            ));
            out.push_str(&counts(
                &export.reasons.iter().take(6).cloned().collect::<Vec<_>>(),
            ));
        }
        out
    }

    pub fn json(&self) -> String {
        let mut out = Writer::document();
        out.number("instances", self.instances as u64);
        out.number("with_trap", self.with_trap as u64);
        out.raw("phases", &count_objects("phase", &self.phases));
        out.raw("top_reasons", &count_objects("reason", &self.top_reasons));
        out.raw("foreign", &count_objects("reason", &self.foreign));
        let exports: Vec<String> = self
            .exports
            .iter()
            .map(|export| {
                let mut out = Writer::new();
                out.string("name", &export.name);
                out.number("reachable_fns", export.reachable_fns as u64);
                out.raw("phases", &count_objects("phase", &export.phases));
                out.raw("reasons", &count_objects("reason", &export.reasons));
                out.finish()
            })
            .collect();
        out.raw("exports", &json::array(&exports));
        out.finish()
    }
}

/// `(name, count)` rows rendered as a two-column table.
fn counts(rows: &[(String, usize)]) -> String {
    let mut table = Table::new(0);
    for (name, count) in rows {
        table.row(vec![Cell::right(count.to_string()), Cell::left(name)]);
    }
    table.render()
}

/// `(name, count)` rows rendered as an array of objects.
fn count_objects(key: &str, rows: &[(String, usize)]) -> String {
    let items: Vec<String> = rows
        .iter()
        .map(|(name, count)| {
            let mut out = Writer::new();
            out.string(key, name);
            out.number("traps", *count as u64);
            out.finish()
        })
        .collect();
    json::array(&items)
}

/// Print the survey on stdout in the mode `MIRVM_OUTPUT` selects.
pub fn print(module: &Module) {
    let survey = survey(module);
    let json = matches!(
        crate::options::get().output_format(),
        Ok(crate::options::OutputFormat::Json)
    );
    print!("{}", if json { survey.json() } else { survey.text() });
}
