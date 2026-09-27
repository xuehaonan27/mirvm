//! The fragment dedup probe (`MIRVM_FRAG_STATS`): what one session's lowered layers would store as
//! fragments, and how much of that is shared content.
//!
//! The design's sharing-acceptance ratio is this number, so the probe hashes exactly the at-rest form
//! the fragment store uses ([`crate::vm::ir::frag`]) and prints one line per body. Two sessions are
//! compared by intersecting their id sets: the share between two adjacent versions of a crate, or
//! between two feature sets, is that intersection over the union. What the summary adds per layer is
//! the size the layer would take — unique fragment bytes plus the frozen bytes stored beside them —
//! and how much of it is frozen, which is what prices the frozen-region extensions.

use super::*;

pub(super) struct FragStats {
    layers: Vec<Layer>,
}

#[derive(Default)]
struct Layer {
    name: &'static str,
    bodies: u64,
    canonical_bytes: u64,
    /// The frozen bytes stored beside the fragments: a layer's data, which no fragment contains.
    frozen_bytes: u64,
    /// fragment id -> (bodies that hashed to it, canonical bytes one copy takes)
    fragments: FxHashMap<[u8; 32], (u64, u64)>,
}

impl FragStats {
    pub(super) fn new() -> Self {
        Self { layers: Vec::new() }
    }

    /// Record one stored layer. A body that cannot be encoded is skipped: it could not become a
    /// fragment either.
    pub(super) fn layer(&mut self, name: &'static str, funcs: &ir::FuncTable, frozen_bytes: u64) {
        let mut layer = Layer {
            name,
            frozen_bytes,
            ..Default::default()
        };
        for body in funcs.iter() {
            let Ok(bytes) = ir::frag::encode(body) else {
                continue;
            };
            let id = ir::frag::id_of(&bytes);
            layer.bodies += 1;
            layer.canonical_bytes += bytes.len() as u64;
            layer
                .fragments
                .entry(id)
                .or_insert((0, bytes.len() as u64))
                .0 += 1;
            eprintln!(
                "[frag] {} {} {} {}",
                name,
                crate::utils::content::digest_hex(&id),
                bytes.len(),
                body.name
            );
        }
        self.layers.push(layer);
    }

    pub(super) fn dump(&self) {
        let mut session: FxHashMap<[u8; 32], u64> = FxHashMap::default();
        for layer in &self.layers {
            let unique = layer.fragments.len() as u64;
            let unique_bytes: u64 = layer.fragments.values().map(|(_, bytes)| *bytes).sum();
            let unit_bytes = unique_bytes + layer.frozen_bytes;
            eprintln!(
                "[frag] {}: {} bodies -> {} fragments (x{:.2}), canonical {} B -> {} B (x{:.2})",
                layer.name,
                layer.bodies,
                unique,
                ratio(layer.bodies, unique),
                layer.canonical_bytes,
                unique_bytes,
                ratio(layer.canonical_bytes, unique_bytes)
            );
            eprintln!(
                "[frag] {}: unit {} B = {} B fragments + {} B frozen ({:.1}% frozen)",
                layer.name,
                unit_bytes,
                unique_bytes,
                layer.frozen_bytes,
                100.0 * ratio(layer.frozen_bytes, unit_bytes)
            );
            for (id, (_, bytes)) in &layer.fragments {
                session.insert(*id, *bytes);
            }
        }
        let bytes: u64 = session.values().sum();
        eprintln!(
            "[frag] session: {} fragments, {} B unique canonical bytes",
            session.len(),
            bytes
        );
    }
}

fn ratio(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        part as f64 / whole as f64
    }
}
