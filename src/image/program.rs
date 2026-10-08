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

use std::collections::HashMap;
use std::path::PathBuf;

use rustc_middle::ty::TyCtxt;
use serde::{Deserialize, Serialize};

use crate::depinfo::InputManifest;
use crate::image::manifest;
use crate::vm::instance::Instance;
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
    crate::options::no_ir_cache()
}

fn entry_path(rustc_args: &[String]) -> PathBuf {
    crate::store::IR
        .dir()
        .join(format!("{}.bin", key_of(rustc_args).digest()))
}

/// The key of one program's IR, which is what its heat order is filed under.
fn key_of(rustc_args: &[String]) -> crate::store::entry::Key {
    let mut key = crate::store::entry::Key::new();
    for arg in rustc_args {
        key.part(arg);
    }
    key
}

/// The heat order of this program: where a previous run left its hot functions, and where this run
/// leaves its own.
///
/// It is filed under the same key as the IR entry, because the function ids it names are ids of that
/// module: an order read against another module would name other functions, which the entry check
/// turns into a miss rather than a wrong link.
pub(crate) fn heat(rustc_args: &[String]) -> crate::vm::jit::Heat {
    crate::vm::jit::Heat::read(
        crate::store::PACKAGE_HEAT
            .dir()
            .join(format!("{}.order", key_of(rustc_args).digest())),
    )
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

/// Hot-path lookup. The returned module and instance: the frozen area is restored to a fixed base,
/// asm_stub_addrs are stale addresses from serialization, the caller **must** rematerialize and
/// overwrite via asm_sites before executing.
pub fn lookup(
    rustc_args: &[String],
    stack: &crate::image::ImageStack,
) -> Option<(ir::Module, Instance)> {
    if disabled() {
        return None;
    }
    let base_key = stack.key();
    let below = stack.below();
    let data = std::fs::read(entry_path(rustc_args)).ok()?;
    let (header, manifest_bytes) = postcard::take_from_bytes::<Header>(&data).ok()?;
    if !header_matches(&header, rustc_args, base_key) {
        return None;
    }
    // Every recorded input must still hold its recorded value (content digest, not mtime).
    if !header.inputs.is_current() {
        return None;
    }
    // The bodies live in the fragment store, one canonical fragment per function, so a module this
    // edit did not change reuses the fragments the previous entry wrote instead of storing its own
    // copy. Everything else — the frozen area, the id-bearing tables, the asm recipes — is the
    // manifest's, in the same form the closure and unit layers use, and the frozen area is itself
    // shared chunk by chunk.
    let mut f: manifest::File = postcard::from_bytes(manifest_bytes).ok()?;
    if !crate::store::entry::is_current_generation(&f.build_id)
        || f.base_key != base_key.unwrap_or_default()
        || f.lowering_fp != lowering_fp(stack)
    {
        return None;
    }
    // A chunk the store no longer holds is a miss, never a partial layer.
    manifest::restore_frozen(&mut f).ok()?;
    let frozen = f
        .module
        .frozen
        .as_ref()
        .map(|snapshot| (snapshot.home() as u64, snapshot.bytes().len() as u64))?;
    let unit = crate::image::deps::unit_of(&[], below.prefix, &f.module, 0, frozen);
    let symbols = manifest::Symbols::of(stack.layers());
    // A missing fragment or an unresolvable symbol is a miss, never a partial module.
    manifest::rehydrate_module(&mut f.module, &f.funcs, &unit, &symbols).ok()?;
    f.tables.restore(&mut f.module, &unit, &symbols).ok()?;
    // Correct shape does not guarantee index and frame range safety, so a bad cache is a miss that
    // the cold path self-heals. Materialized .so files (native archive / global_asm) that were
    // removed are a miss for the same reason.
    let mut instance = crate::store::entry::revive(&mut f.module, below)?;
    instance.asm_stub_addrs = crate::lower::asm::materialize(&f.module.asm_sites);
    if !crate::store::entry::native_libs_present(&f.module) {
        return None;
    }
    Some((f.module, instance))
}

/// Mark the fragments and frozen chunks one L2 entry names, for collection. The entry is
/// `[Header][manifest]`, so the marker skips exactly what the loader skips, and an entry from another
/// build marks nothing.
pub(crate) fn mark_live(
    path: &std::path::Path,
    fragments: &mut std::collections::HashSet<[u8; 32]>,
    chunks: &mut std::collections::HashSet<[u8; 32]>,
) {
    let Ok(data) = std::fs::read(path) else {
        return;
    };
    let Ok((header, manifest_bytes)) = postcard::take_from_bytes::<Header>(&data) else {
        return;
    };
    if !crate::store::entry::is_current_generation(&header.build_id) {
        return;
    }
    let Ok(file) = postcard::from_bytes::<manifest::File>(manifest_bytes) else {
        return;
    };
    fragments.extend(file.funcs.iter().map(|record| record.fragment));
    if let Some(frozen) = &file.frozen {
        chunks.extend(frozen.chunks.iter().copied());
    }
}

/// The lowering fingerprint a delta above this stack was built with: the base's, because every
/// absolute the delta embeds is laid out against it.
fn lowering_fp(stack: &crate::image::ImageStack) -> (bool, bool, bool) {
    stack
        .base_image()
        .map(|base| base.lowering_fp)
        .unwrap_or((false, false, false))
}

/// Cold-path store (clean state right after lower finishes and before guest runs). Returns whether
/// the write actually happened.
pub fn store(
    tcx: TyCtxt<'_>,
    rustc_args: &[String],
    module: &mut ir::Module,
    instance: &Instance,
    stack: &crate::image::ImageStack,
) -> bool {
    if disabled() {
        return false;
    }
    let base_key = stack.key();
    let below = stack.below();
    if crate::vm::verify::module_below(module, instance, below).is_err() {
        return false;
    }
    // The frozen area must be at a fixed base (a concurrent preempt or an ASLR conflict leaves
    // embedded addresses cross-process invalid); a delta may sit at any fixed base, since the file
    // records its own domain.
    // Foreign symbols (environ-like extern static / extern fn address-taking) are indirected
    // through GOT slots since P2 (decision-history §7.5c): the GOT table travels with the snapshot
    // and is refilled with this process's real values at startup, so it is not a cache blocker.
    if !crate::store::entry::snapshot_is_publishable(module, instance, None) {
        return false;
    }
    let Some(inputs) = InputManifest::collect(tcx) else {
        return false; // some input could not be stamped (missing/unusual) — prefer not to cache
    };
    let Some(frozen) = module
        .frozen
        .as_ref()
        .map(|snapshot| (snapshot.home() as u64, snapshot.bytes().len() as u64))
    else {
        return false;
    };

    // The bodies move into the fragment store — which is what makes this entry incremental across an
    // edit, and what lets two programs share a body they both have — and the manifest keeps the rest.
    // The segments mirror the closure layer's exactly, because the delta *is* the closure above the
    // base in this track.
    let unit = crate::image::deps::unit_of(&[], below.prefix, module, 0, frozen);
    let symbols = manifest::Symbols::of(stack.layers());
    let mut session = crate::store::frags::Session::default();
    let projected =
        manifest::project_module(module, &unit, &symbols, &mut session).and_then(|records| {
            let tables = manifest::Tables::capture(module, &unit, &symbols, &HashMap::new())?;
            Ok((records, tables))
        });
    let Ok((records, tables)) = projected else {
        return false;
    };
    let fp = lowering_fp(stack);
    // A program's frozen region is the part of it that most edits leave alone and every edit
    // rewrites, so it goes to the chunk store rather than into the entry. The region comes back
    // before the entry is published: this session keeps running the module it just wrote.
    let mut chunks = crate::store::frags::Session::default();
    let taken = manifest::take_frozen(module, &mut chunks);
    let frozen_chunks = taken.as_ref().map_or(0, |f| f.reference().chunks.len());
    let encoded = {
        let file = manifest::FileRef {
            build_id: crate::options::build::BUILD_ID,
            base_key: base_key.unwrap_or_default(),
            unit_key: None,
            // The base key is checked exactly and the delta's ids start after the base's prefix, so
            // there is no prefix of *manifest* layers to pin: the stack below is one image.
            below: &[],
            home: 0,
            lowering_fp: fp,
            extern_stamps: &[],
            module,
            funcs: &records,
            tables: &tables,
            fn_entry_syms: &[],
            static_syms: &[],
            frozen: taken.as_ref().map(manifest::Frozen::reference),
        };
        manifest::encode(&file)
    };
    if let Some(taken) = taken {
        taken.put_back(module);
    }
    let Ok(manifest_bytes) = encoded else {
        return false;
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
    buf.extend(manifest_bytes);

    // Chunks and fragments first, the entry last: an entry that names what the store does not hold is
    // an entry the loader refuses. Both publish locks are held across the whole sequence, because a
    // sweep between the writes would see records no manifest names yet and drop them.
    let _publishing = crate::store::frags::publish_lock();
    let chunks_store = crate::store::frozen::System::open();
    let _chunk_publishing = chunks_store.publish_lock();
    let Ok(chunk_published) = chunks_store.publish(chunks) else {
        return false;
    };
    if session.publish().is_err() {
        return false;
    }
    if crate::options::a2_debug() {
        // What the entry stores beside its fragments: the frozen region, as the chunks it is cut into,
        // and how much of that the store already held — which is what an edit's frozen bytes cost.
        crate::diag::instrument(format_args!(
            "[a2-debug] program entry: {} bodies -> fragments, frozen {} B in {frozen_chunks} chunks \
             ({} stored, {} deduped)",
            records.len(),
            frozen.1,
            chunk_published.stored,
            chunk_published.deduped
        ));
    }
    // Atomic publish: a reader sees either the previous entry or this one, never a half-written file.
    let path = entry_path(rustc_args);
    let Some(dir) = path.parent() else {
        return false;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    if crate::store::publish_bytes(&path, &buf).is_err() {
        return false;
    }
    // The entry is written: put this module's own ids back, so the session keeps running the module it
    // just wrote.
    let _ = tables.restore(module, &unit, &symbols);
    true
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
