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
    /// The home rule's ledger: which unit each stored body would belong to (`None` = residue), and
    /// how much of the layer each home accounts for.
    homes: FxHashMap<Option<u32>, Home>,
}

#[derive(Default)]
struct Home {
    bodies: u64,
    /// Bodies that name no unit at all: std residue the base lacks, which any unit's closure covers.
    unattached: u64,
    canonical_bytes: u64,
    fragments: FxHashMap<[u8; 32], (u64, u64)>,
}

#[derive(Default)]
struct Layer {
    name: String,
    bodies: u64,
    canonical_bytes: u64,
    /// The frozen bytes stored beside the fragments: a layer's data, which no fragment contains.
    frozen_bytes: u64,
    /// fragment id -> (bodies that hashed to it, canonical bytes one copy takes)
    fragments: FxHashMap<[u8; 32], (u64, u64)>,
}

impl FragStats {
    pub(super) fn new() -> Self {
        Self {
            layers: Vec::new(),
            homes: FxHashMap::default(),
        }
    }

    /// Record one stored body against the unit the home rule gives it.
    pub(super) fn home(&mut self, home: Option<crate::image::units::Home>, body: &ir::FuncBody) {
        let Ok(bytes) = ir::frag::encode(body) else {
            return;
        };
        let entry = self.homes.entry(home.map(|home| home.unit)).or_default();
        entry.bodies += 1;
        if home.is_some_and(|home| home.unattached) {
            entry.unattached += 1;
        }
        entry.canonical_bytes += bytes.len() as u64;
        let fragments = entry
            .fragments
            .entry(ir::frag::id_of(&bytes))
            .or_insert((0, 0));
        fragments.0 += 1;
        fragments.1 = bytes.len() as u64;
    }

    /// Record one stored layer. A body that cannot be encoded is skipped: it could not become a
    /// fragment either.
    pub(super) fn layer(&mut self, name: &str, funcs: &ir::FuncTable, frozen_bytes: u64) {
        let mut layer = Layer {
            name: name.to_string(),
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

    pub(super) fn dump(&self, base_key: Option<&str>) {
        self.dump_homes(base_key);
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

impl FragStats {
    /// Per home: the unit's name (or `residue`), how many bodies it accounts for, and what they would
    /// take as fragments. This is the unit-size ledger the design prices itself against.
    fn dump_homes(&self, base_key: Option<&str>) {
        if self.homes.is_empty() {
            return;
        }
        let table = crate::image::units::current();
        let mut rows: Vec<(Option<u32>, String, &Home)> = self
            .homes
            .iter()
            .map(|(home, entry)| {
                let name = home
                    .and_then(|index| table.and_then(|table| table.get(index)))
                    .map(|unit| unit.name.to_string())
                    .unwrap_or_else(|| "residue".to_string());
                (*home, name, entry)
            })
            .collect();
        rows.sort_by(|a, b| a.1.cmp(&b.1));
        let mut bodies = 0;
        let mut unique_bytes = 0;
        let mut keys_ms = 0.0f64;
        for (home, name, entry) in rows {
            let unique = entry.fragments.len() as u64;
            let kept: u64 = entry.fragments.values().map(|(_, bytes)| *bytes).sum();
            // A unit's key is what its stored manifest will be named by: the rlib stamp the
            // pre-session load path pays for. Measured here because it is the design's own cost
            // question — every unit's rlib, not just the direct externs.
            let key = match (base_key, home, table) {
                (Some(base), Some(index), Some(table)) => {
                    let start = std::time::Instant::now();
                    let key = table.key_of(index, base);
                    keys_ms += start.elapsed().as_secs_f64() * 1e3;
                    key
                }
                _ => None,
            };
            eprintln!(
                "[frag] home {name}: {} bodies ({} name no unit) -> {unique} fragments, canonical {} B -> {kept} B{}",
                entry.bodies,
                entry.unattached,
                entry.canonical_bytes,
                key.map(|key| format!(", key {key}")).unwrap_or_default()
            );
            bodies += entry.bodies;
            unique_bytes += kept;
        }
        eprintln!(
            "[frag] homes: {bodies} image bodies homed, {unique_bytes} B of unique fragments"
        );
        if keys_ms > 0.0 {
            eprintln!("[frag] unit keys: {keys_ms:.1}ms of rlib stamping");
        }
    }
}

fn ratio(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        part as f64 / whole as f64
    }
}
