//! The program layer: one program's post-mono engine IR, cached as the L2 entry keyed by the rustc
//! arguments.
//!
//! Cold path (miss): rustc frontend → lower → **store** (clean snapshot before guest runs) → run.
//! Hot path (hit): **lookup** → asm-stub rematerialization → argv finalization → run — the entire
//! rustc session (frontend + metadata + mono + lower) is skipped.
//!
//! Key = digest(MIRVM_BUILD_ID, rustc_args); entry header = full args replay (hash-collision proof)
//! + the input manifest ([`crate::depinfo`]).
//!
//! Guard against silent wrong values: any validation mismatch is a miss (cold path rebuilds and
//! overwrites); frozen area not at fixed base is rejected for serialization/restore (see
//! frozen.rs); missing required .so is a miss (self-heal rather than runtime error).
//! `MIRVM_NO_IR_CACHE=1` bypasses the cache entirely.

use std::path::PathBuf;

use rustc_middle::ty::TyCtxt;
use serde::{Deserialize, Serialize};

use crate::depinfo::InputManifest;
use crate::vm::ir;

#[derive(Serialize, Deserialize)]
struct Header {
    build_id: String,
    args: Vec<String>,
    /// The compilation's input manifest (files + `env!` dependencies), replayed on lookup.
    inputs: crate::depinfo::InputManifest,
    /// S4 layering: base key referenced by the delta module (None = full module with no base).
    /// Delta bytecode/frozen area embeds base absolutes (FuncId offset, base addresses) — a
    /// mismatched base load is globally wrong, so keys must match exactly.
    base_key: Option<String>,
}

fn disabled() -> bool {
    crate::options::get().no_ir_cache
}

fn entry_path(rustc_args: &[String]) -> PathBuf {
    let mut key = crate::store::entry::Key::new();
    for arg in rustc_args {
        key.part(arg);
    }
    crate::store::IR.dir().join(format!("{}.bin", key.digest()))
}

/// Header triple equality: build id (stale across builds) + full args replay (hash-collision proof) +
/// base key exact equality (S4: delta embeds base absolutes — FuncId offsets / base addresses — a
/// base replacement or presence change makes a mismatched load globally wrong; None side must also
/// match exactly, a no-base session must not consume a base delta).
fn header_matches(header: &Header, rustc_args: &[String], base_key: Option<&str>) -> bool {
    crate::store::entry::is_current_generation(&header.build_id)
        && header.args == rustc_args
        && header.base_key.as_deref() == base_key
}

/// Hot-path lookup. The returned Module already has its frozen area restored to a fixed base;
/// asm_stub_addrs are stale addresses from serialization, the caller **must** rematerialize and
/// overwrite via asm_sites before executing.
pub fn lookup(
    rustc_args: &[String],
    base_key: Option<&str>,
    prefix: crate::vm::verify::Prefix,
) -> Option<ir::Module> {
    if disabled() {
        return None;
    }
    let data = std::fs::read(entry_path(rustc_args)).ok()?;
    let (header, module_bytes) = postcard::take_from_bytes::<Header>(&data).ok()?;
    if !header_matches(&header, rustc_args, base_key) {
        return None;
    }
    // Every recorded input must still hold its recorded value (content digest, not mtime).
    if !header.inputs.is_current() {
        return None;
    }
    // Module deserialization includes frozen-area fixed-base restoration; failure (base occupied
    // etc.) → miss
    let mut module: ir::Module = postcard::from_bytes(module_bytes).ok()?;
    // Correct shape does not guarantee index and frame range safety, so a bad cache is a miss that
    // the cold path self-heals. Materialized .so files (native archive / global_asm) that were
    // removed are a miss for the same reason.
    if !crate::store::entry::revive(&mut module, prefix) {
        return None;
    }
    if !crate::store::entry::native_libs_present(&module) {
        return None;
    }
    Some(module)
}

/// Cold-path store (clean state right after lower finishes and before guest runs). Returns whether
/// the write actually happened.
pub fn store(
    tcx: TyCtxt<'_>,
    rustc_args: &[String],
    module: &ir::Module,
    base_key: Option<&str>,
    prefix: crate::vm::verify::Prefix,
) -> bool {
    if disabled() {
        return false;
    }
    if crate::vm::verify::module_with_prefix(module, prefix).is_err() {
        return false;
    }
    // The frozen area must be at a fixed base (a concurrent preempt or an ASLR conflict leaves
    // embedded addresses cross-process invalid); a delta may sit at any fixed base, since the file
    // records its own domain.
    // Foreign symbols (environ-like extern static / extern fn address-taking) are indirected
    // through GOT slots since P2 (decision-history §7.5c): the GOT table travels with the snapshot
    // and is refilled with this process's real values at startup, so it is not a cache blocker.
    if !crate::store::entry::snapshot_is_publishable(module, None) {
        return false;
    }
    let Some(inputs) = InputManifest::collect(tcx) else {
        return false; // some input could not be stamped (missing/unusual) — prefer not to cache
    };

    let header = Header {
        build_id: crate::options::build::BUILD_ID.to_string(),
        args: rustc_args.to_vec(),
        inputs,
        base_key: base_key.map(str::to_owned),
    };
    let Ok(mut buf) = postcard::to_stdvec(&header) else {
        return false;
    };
    match postcard::to_stdvec(module) {
        Ok(m) => buf.extend(m),
        Err(_) => return false,
    }

    // Atomic publish: a reader sees either the previous entry or this one, never a half-written file.
    let path = entry_path(rustc_args);
    let Some(dir) = path.parent() else {
        return false;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    crate::store::publish_bytes(&path, &buf).is_ok()
}

#[cfg(test)]
mod tests {
    #[test]
    fn base_key_must_match_exactly_including_absence() {
        let args = vec!["mirvm".to_string(), "x.rs".to_string()];
        let mk = |base_key: Option<&str>| super::Header {
            build_id: crate::options::build::BUILD_ID.to_string(),
            args: args.clone(),
            inputs: crate::depinfo::InputManifest::default(),
            base_key: base_key.map(str::to_owned),
        };
        // same key ✓; replacement ✗; presence change (some→none / none→some) both ways ✗
        assert!(super::header_matches(&mk(Some("k1")), &args, Some("k1")));
        assert!(!super::header_matches(&mk(Some("k1")), &args, Some("k2")));
        assert!(!super::header_matches(&mk(Some("k1")), &args, None));
        assert!(!super::header_matches(&mk(None), &args, Some("k1")));
        assert!(super::header_matches(&mk(None), &args, None));
        // existing axis regression: args drift still rejected
        let other = vec!["mirvm".to_string(), "y.rs".to_string()];
        assert!(!super::header_matches(&mk(None), &other, None));
    }
}
