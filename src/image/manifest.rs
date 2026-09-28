//! The stored form of one lowered layer: its functions as shared fragments, plus the binding table
//! that gives each fragment's ordinals meaning.
//!
//! A fragment ([`crate::vm::ir::frag`]) is a body written without the ids lowering assigned or the
//! addresses it baked, so the same body in two crate versions, two feature sets or two projects is
//! one fragment. What remains per function is this module's job: the **binding table**, one entry per
//! ordinal, saying whether that ordinal is local — an index in one of the unit's own tables, an offset
//! inside one of its own fixed domains — or symbolic: a target a layer below owns, named by its v0
//! symbol and resolved through the image stack's union lookup at load.
//!
//! Both directions live here so they cannot drift: [`project`] writes a lowered body down (canonical
//! bytes, fragment id, bindings) and [`rehydrate`] turns those back into the body the runtime needs.
//! They share the exhaustive site walk in [`crate::vm::ir::frag`], so a new reference site cannot be
//! handled in one direction only.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::vm::ir::frag::{self, SiteVisitor, Target};
use crate::vm::ir::{AsmStubId, FuncBody, FuncId, LinkAddr, TlsId};

/// What one ordinal of a canonical body stands for.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum Binding {
    /// Index in this unit's own function table; the loader adds the stack's prefix.
    Func(u32),
    Tls(u32),
    Asm(u32),
    /// Offset inside this unit's own frozen region.
    Frozen(u64),
    /// Offset inside this unit's own code arena (an executable entry stub).
    Stub(u64),
    /// A target a layer below owns, named by its v0 symbol.
    Symbol {
        kind: SymbolKind,
        name: Box<str>,
    },
}

/// Which table of a lower layer a symbolic binding resolves in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum SymbolKind {
    Func,
    Tls,
    Static,
    Entry,
}

/// The id ranges and fixed domains one layer occupies, in the session that stores it (publish) or the
/// process that loads it. Both callers build it from the same rules, which is what makes a binding's
/// local half identical on both sides.
///
/// `layers` is what tells an id's *owner* apart from "inside this session's home": a function of this
/// unit that a loaded manifest already provided is a hit whose id lies in that loaded layer's range,
/// and it is still this unit's function. Each entry is `(absolute start, count)` from the bottom up;
/// `self_layer` indexes this unit's own.
#[derive(Clone, Debug)]
pub(crate) struct Unit {
    /// Absolute id ranges of this unit's own functions, TLS slots and asm stubs.
    pub funcs: (u32, u32),
    pub tls: (u32, u32),
    pub asm: (u32, u32),
    /// The unit's own frozen region and code arena: `(home, len)`.
    pub frozen: (u64, u64),
    pub code: (u64, u64),
    /// The function and TLS ranges of every layer below and this unit's own, bottom first.
    pub layers: Vec<((u32, u32), (u32, u32))>,
    /// Index of this unit's own entry in `layers`.
    pub self_layer: usize,
}

/// One layer's function and TLS ranges in the absolute id space of a session.
pub(crate) type LayerRanges = ((u32, u32), (u32, u32));

impl Unit {
    /// The layer map of a unit that sits above `below` (every layer of the stack, bottom first): its
    /// own ids start where the last layer below ends.
    pub(crate) fn above(mut self, below: &[LayerRanges]) -> Unit {
        // `own` holds this unit's own ranges as recorded in its image; only the start moves, to the
        // end of the last layer below. With no layer below, the image's own start is already right
        // (the first home of a session).
        let shift =
            |last: Option<&LayerRanges>, own: (u32, u32), pick: fn(&LayerRanges) -> (u32, u32)| {
                let (start, len) = own;
                let start = match last {
                    Some(range) => pick(range).0 + pick(range).1,
                    None => start,
                };
                (start, len)
            };
        let (funcs, tls) = (
            shift(below.last(), self.funcs, |r| r.0),
            shift(below.last(), self.tls, |r| r.1),
        );
        self.layers = below.to_vec();
        self.layers.push((funcs, tls));
        self.self_layer = self.layers.len() - 1;
        self.funcs = funcs;
        self.tls = tls;
        self
    }

    /// Which layer owns `id` in `space`, and the ordinal inside it.
    fn owner(&self, space: Space, id: u32) -> Option<(usize, u32)> {
        self.layers.iter().enumerate().find_map(|(index, ranges)| {
            let (start, len) = match space {
                Space::Func => ranges.0,
                Space::Tls => ranges.1,
            };
            id.checked_sub(start)
                .filter(|at| at < &len)
                .map(|at| (index, at))
        })
    }
}

/// The symbol tables of the layers below a unit.
///
/// Publish names a target the unit does not own; load resolves that name back to a value. One table
/// pair, built from the same layers, is what keeps the two from disagreeing about a name.
#[derive(Default)]
pub(crate) struct Symbols {
    /// Reverse: the value a lower layer owns, as the symbol that names it.
    func_names: HashMap<FuncId, Box<str>>,
    tls_names: HashMap<TlsId, Box<str>>,
    entry_names: HashMap<u64, Box<str>>,
    static_names: HashMap<u64, Box<str>>,
    /// Forward: the loaded layers' own tables.
    funcs: HashMap<Box<str>, FuncId>,
    tls: HashMap<Box<str>, TlsId>,
    entries: HashMap<Box<str>, u64>,
    statics: HashMap<Box<str>, u64>,
}

impl Symbols {
    /// Collect the tables of the layers below one unit. A name that two layers both define resolves
    /// to the bottom-most one, which is the same rule the stack's union lookup uses.
    pub(crate) fn of<'a>(layers: impl IntoIterator<Item = &'a crate::image::BaseImage>) -> Symbols {
        let mut symbols = Symbols::default();
        for layer in layers {
            for (name, &id) in &layer.fn_by_sym {
                symbols.funcs.entry(name.clone()).or_insert(id);
                symbols.func_names.entry(id).or_insert_with(|| name.clone());
            }
            for (name, &id) in &layer.tls_by_sym {
                symbols.tls.entry(name.clone()).or_insert(id);
                symbols.tls_names.entry(id).or_insert_with(|| name.clone());
            }
            for (name, &addr) in &layer.entry_by_sym {
                symbols.entries.entry(name.clone()).or_insert(addr);
                symbols
                    .entry_names
                    .entry(addr)
                    .or_insert_with(|| name.clone());
            }
            for (name, &addr) in &layer.static_by_sym {
                symbols.statics.entry(name.clone()).or_insert(addr);
                symbols
                    .static_names
                    .entry(addr)
                    .or_insert_with(|| name.clone());
            }
        }
        symbols
    }

    fn func(&self, name: &str) -> Option<FuncId> {
        self.funcs.get(name).copied()
    }

    fn tls(&self, name: &str) -> Option<TlsId> {
        self.tls.get(name).copied()
    }

    fn address(&self, kind: SymbolKind, name: &str) -> Option<u64> {
        match kind {
            SymbolKind::Entry => self.entries.get(name).copied(),
            SymbolKind::Static => self.statics.get(name).copied(),
            _ => None,
        }
    }

    /// The symbol naming a value a lower layer owns, for the publish direction.
    fn name_of(&self, target: Target) -> Option<Binding> {
        match target {
            Target::Func(id) => self.func_names.get(&id).map(|name| Binding::Symbol {
                kind: SymbolKind::Func,
                name: name.clone(),
            }),
            Target::Tls(id) => self.tls_names.get(&id).map(|name| Binding::Symbol {
                kind: SymbolKind::Tls,
                name: name.clone(),
            }),
            Target::Link(addr) => {
                if let Some(name) = self.entry_names.get(&addr.0) {
                    return Some(Binding::Symbol {
                        kind: SymbolKind::Entry,
                        name: name.clone(),
                    });
                }
                self.static_names.get(&addr.0).map(|name| Binding::Symbol {
                    kind: SymbolKind::Static,
                    name: name.clone(),
                })
            }
            // Asm-stub recipes are per-module: one module's stub id never names another's site.
            Target::Asm(_) => None,
        }
    }
}

/// The stored manifest of one layer: header material, the module without its bodies and without its
/// id-bearing tables, and one record per function in FuncId order (parallel to
/// `module.function_names`). The bodies are fragments in the shared store and the tables are
/// canonical ([`Tables`]), so nothing in the file depends on how many layers happen to sit below it.
///
/// The same format serves both layers the design has: a **closure manifest** (one program's whole
/// dependency closure, keyed by the `--extern` stamps, in `cache/deps/`) and a **unit manifest** (one
/// crate, keyed by its rlib, in `cache/units/`). A unit manifest also records the spline slot its
/// arenas were built in, so the loader restores them where their baked link addresses point.
#[derive(Deserialize, Serialize)]
pub(crate) struct File {
    pub build_id: String,
    /// The layer below the stack: the base image's key. Exact equality is required — a mismatch is
    /// wrong at every value.
    pub base_key: String,
    /// A unit manifest's key (`build id ⊕ base key ⊕ rlib stamp`); `None` for a closure manifest,
    /// whose key material is the `--extern` stamps.
    pub unit_key: Option<String>,
    /// The function, TLS and asm counts of each layer this manifest was written above, in order.
    ///
    /// A layer's own ids start where that stack ends, and a canonical binding's ordinal is resolved
    /// against those boundaries, so the *layout* of the stack below — not just the symbols it offers —
    /// is part of what makes a manifest valid. The counts are what the layout is: a unit's ids and its
    /// slot are its position and its own size, and neither changes when a lower manifest is rewritten,
    /// which is why this is the invariant rather than the digest chain.
    pub below: Vec<(u32, u32, u32)>,
    /// The frozen/code spline slot the layer's arenas were built in (`0` for a closure manifest).
    pub home: usize,
    pub lowering_fp: (bool, bool, bool),
    /// Content stamps of the artifacts the key material was built from (hash-collision immune).
    pub extern_stamps: Vec<crate::utils::content::FileStamp>,
    /// The module with its bodies and id-bearing tables removed.
    pub module: crate::vm::ir::Module,
    pub funcs: Vec<Record>,
    pub tables: Tables,
    pub fn_entry_syms: Vec<(Box<str>, u64)>,
    pub static_syms: Vec<(Box<str>, u64)>,
}

/// Borrowed shape for writing (`ir::Module` is not `Clone`).
#[derive(Serialize)]
pub(crate) struct FileRef<'a> {
    pub build_id: &'a str,
    pub base_key: &'a str,
    pub unit_key: Option<&'a str>,
    pub below: &'a [(u32, u32, u32)],
    pub home: usize,
    pub lowering_fp: (bool, bool, bool),
    pub extern_stamps: &'a [crate::utils::content::FileStamp],
    pub module: &'a crate::vm::ir::Module,
    pub funcs: &'a [Record],
    pub tables: &'a Tables,
    pub fn_entry_syms: &'a [(Box<str>, u64)],
    pub static_syms: &'a [(Box<str>, u64)],
}

/// Serialize one manifest. The bytes are what a unit manifest is named by, so the caller that wants a
/// content address hashes exactly these.
pub(crate) fn encode(file: &FileRef<'_>) -> Result<Vec<u8>, String> {
    postcard::to_stdvec(file).map_err(|error| error.to_string())
}

/// Project every function of `module` into `session` (the fragment store's staging set) and return the
/// manifest records. The bodies come back into the module before returning: the layer this session
/// runs is the one it just wrote.
pub(crate) fn project_module(
    module: &mut crate::vm::ir::Module,
    unit: &Unit,
    symbols: &Symbols,
    session: &mut crate::store::frags::Session,
) -> Result<Vec<Record>, String> {
    let mut bodies = Vec::new();
    module.funcs.drain_into(&mut bodies);
    let mut records = Vec::with_capacity(bodies.len());
    let mut failure = None;
    for (index, body) in bodies.iter().enumerate() {
        match project(body, unit, symbols) {
            Ok(projected) => {
                records.push(projected.record);
                session.add(projected.bytes);
            }
            Err(error) => {
                if crate::options::a2_debug() {
                    let name: &str = module
                        .function_names
                        .get(index)
                        .map_or("<unnamed>", |name| name);
                    eprintln!(
                        "[a2-debug] project failed at record {index} of {} ({name}): {error}",
                        bodies.len()
                    );
                }
                failure = Some(error);
                break;
            }
        }
    }
    module.funcs = bodies.into();
    match failure {
        Some(error) => Err(error),
        None => Ok(records),
    }
}

/// The manifest's records back into bodies, in FuncId order. Every fragment must be in the store and
/// every binding must resolve: anything else is a miss, never a partially assembled layer.
pub(crate) fn rehydrate_module(
    module: &mut crate::vm::ir::Module,
    records: &[Record],
    unit: &Unit,
    symbols: &Symbols,
) -> Result<(), String> {
    if module.function_names.len() != records.len() {
        return Err(format!(
            "manifest stores {} functions but the module names {}",
            records.len(),
            module.function_names.len()
        ));
    }
    let ids: Vec<[u8; 32]> = records.iter().map(|record| record.fragment).collect();
    let fragments = crate::store::frags::Index::load().read_many(&ids);
    let mut bodies = Vec::with_capacity(records.len());
    for (index, record) in records.iter().enumerate() {
        let fragment = fragments
            .get(&record.fragment)
            .ok_or_else(|| "a fragment the manifest names is not in the store".to_string())?;
        let name = module.function_names.get(index).map_or("?", |name| name);
        bodies.push(rehydrate(fragment, &record.bindings, name, unit, symbols)?);
    }
    module.funcs = bodies.into();
    Ok(())
}

/// Which table of the unit an [`Owned`] reference is about: the two spaces a manifest names by
/// ordinal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Space {
    Func,
    Tls,
}

/// A reference from a manifest table to something a layer owns: this layer's own ordinal, or a symbol a
/// layer below owns. Absolute ids are the sum of the layers under the manifest, which a shared
/// manifest cannot know, so its tables store ordinals and symbols and the loader rebuilds the ids.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum Owned {
    Local(u32),
    Symbol { kind: SymbolKind, name: Box<str> },
}

/// The two symbol indexes a load rebuilds from the canonical tables: exported function symbols and TLS
/// symbols, both under the prefix this layer was loaded at.
pub(crate) type SymbolIndexes = (
    HashMap<Box<str>, crate::vm::ir::FuncId>,
    HashMap<Box<str>, crate::vm::ir::TlsId>,
);

/// The module's id-bearing tables in canonical form. [`File`] serializes the module with these
/// drained, so the manifest carries no absolute id at all.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct Tables {
    /// Exported symbol → the function it names.
    pub exports: Vec<(Box<str>, Owned)>,
    /// Entry link address (inside this layer's own domains) → the function the entry names.
    pub fn_entry_links: Vec<(LinkAddr, Owned)>,
    /// Entry-stub recipe functions, parallel to `module.entry_stub_sites`.
    pub entry_stub_funcs: Vec<Owned>,
    /// TLS symbol → the slot it names.
    pub tls_syms: Vec<(Box<str>, Owned)>,
}

pub(crate) fn project_owned(
    space: Space,
    id: u32,
    unit: &Unit,
    symbols: &Symbols,
) -> Result<Owned, String> {
    // An id this unit owns is local, whether it came from this session's lowering or from a loaded
    // manifest of the same unit; only another layer's id becomes a symbol.
    if let Some((owner, local)) = unit.owner(space, id)
        && owner == unit.self_layer
    {
        return Ok(Owned::Local(local));
    }
    let target = match space {
        Space::Func => Target::Func(id),
        Space::Tls => Target::Tls(id),
    };
    match (symbols.name_of(target), space) {
        (Some(Binding::Symbol { kind, name }), Space::Func) if kind == SymbolKind::Func => {
            Ok(Owned::Symbol { kind, name })
        }
        (Some(Binding::Symbol { kind, name }), Space::Tls) if kind == SymbolKind::Tls => {
            Ok(Owned::Symbol { kind, name })
        }
        _ => Err(format!(
            "no symbol names id {id}, which this unit does not own"
        )),
    }
}

fn resolve_owned(space: Space, owned: &Owned, unit: &Unit, symbols: &Symbols) -> Option<u32> {
    match owned {
        Owned::Local(local) => {
            let (start, len) = match space {
                Space::Func => unit.funcs,
                Space::Tls => unit.tls,
            };
            (*local < len).then(|| start + local)
        }
        Owned::Symbol { kind, name } => match (space, kind) {
            (Space::Func, SymbolKind::Func) => symbols.func(name),
            (Space::Tls, SymbolKind::Tls) => symbols.tls(name),
            _ => None,
        },
    }
}

impl Tables {
    /// Take the module's id-bearing tables out, in canonical form: the module is left with them empty
    /// (or zeroed) so serializing it carries no absolute id.
    pub(crate) fn capture(
        module: &mut crate::vm::ir::Module,
        unit: &Unit,
        symbols: &Symbols,
        tls_by_sym: &std::collections::HashMap<Box<str>, crate::vm::ir::TlsId>,
    ) -> Result<Tables, String> {
        let mut tables = Tables::default();
        for (name, id) in module.exports.drain() {
            tables
                .exports
                .push((name, project_owned(Space::Func, id, unit, symbols)?));
        }
        for (addr, id) in std::mem::take(&mut module.fn_entry_links) {
            tables
                .fn_entry_links
                .push((addr, project_owned(Space::Func, id, unit, symbols)?));
        }
        for site in &mut module.entry_stub_sites {
            tables
                .entry_stub_funcs
                .push(project_owned(Space::Func, site.func, unit, symbols)?);
            site.func = 0;
        }
        for (name, id) in tls_by_sym {
            tables
                .tls_syms
                .push((name.clone(), project_owned(Space::Tls, *id, unit, symbols)?));
        }
        Ok(tables)
    }

    /// Put the absolute ids back, against the prefix this layer was loaded at and the layers below it.
    pub(crate) fn restore(
        &self,
        module: &mut crate::vm::ir::Module,
        unit: &Unit,
        symbols: &Symbols,
    ) -> Result<SymbolIndexes, String> {
        let mut exports = HashMap::with_capacity(self.exports.len());
        for (name, owned) in &self.exports {
            let id = resolve_owned(Space::Func, owned, unit, symbols).ok_or_else(|| {
                format!("export `{name}` does not resolve against the stack below")
            })?;
            exports.insert(name.clone(), id);
        }
        let mut links = Vec::with_capacity(self.fn_entry_links.len());
        for (addr, owned) in &self.fn_entry_links {
            let id = resolve_owned(Space::Func, owned, unit, symbols).ok_or_else(|| {
                format!(
                    "entry link {:#x} does not resolve against the stack below",
                    addr.0
                )
            })?;
            links.push((*addr, id));
        }
        if module.entry_stub_sites.len() != self.entry_stub_funcs.len() {
            return Err("entry-stub table size changed between write and load".into());
        }
        for (site, owned) in module
            .entry_stub_sites
            .iter_mut()
            .zip(&self.entry_stub_funcs)
        {
            site.func = resolve_owned(Space::Func, owned, unit, symbols)
                .ok_or_else(|| "entry stub does not resolve against the stack below".to_string())?;
        }
        let mut tls = HashMap::with_capacity(self.tls_syms.len());
        for (name, owned) in &self.tls_syms {
            let id = resolve_owned(Space::Tls, owned, unit, symbols).ok_or_else(|| {
                format!("TLS symbol `{name}` does not resolve against the stack below")
            })?;
            tls.insert(name.clone(), id);
        }
        module.exports = exports.clone();
        module.fn_entry_links = links;
        Ok((exports, tls))
    }
}

/// One function's stored form: the fragment, and what its ordinals mean.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct Record {
    pub fragment: [u8; 32],
    pub bindings: Vec<Binding>,
}

/// One projected body: what the manifest records, plus the bytes the fragment store needs.
#[derive(Debug)]
pub(crate) struct Projected {
    pub record: Record,
    pub bytes: Vec<u8>,
}

/// Write one lowered body down. `Err` names the target that could not be expressed; the caller treats
/// that as "this layer is not storable" rather than inventing a value.
pub(crate) fn project(
    body: &FuncBody,
    unit: &Unit,
    symbols: &Symbols,
) -> Result<Projected, String> {
    let canonical = frag::canonical(body);
    let mut bindings = Vec::with_capacity(canonical.targets.len());
    for target in &canonical.targets {
        bindings.push(project_target(*target, unit, symbols)?);
    }
    let bytes = frag::encode_canonical(&canonical.body)?;
    Ok(Projected {
        record: Record {
            fragment: frag::id_of(&bytes),
            bindings,
        },
        bytes,
    })
}

fn project_target(target: Target, unit: &Unit, symbols: &Symbols) -> Result<Binding, String> {
    let in_range =
        |value: u32, (start, len): (u32, u32)| value.checked_sub(start).is_some_and(|at| at < len);
    let in_domain =
        |value: u64, (home, len): (u64, u64)| value.checked_sub(home).is_some_and(|at| at < len);
    match target {
        Target::Func(id) if in_range(id, unit.funcs) => Ok(Binding::Func(id - unit.funcs.0)),
        Target::Tls(id) if in_range(id, unit.tls) => Ok(Binding::Tls(id - unit.tls.0)),
        Target::Asm(id) if in_range(id, unit.asm) => Ok(Binding::Asm(id - unit.asm.0)),
        Target::Link(addr) if in_domain(addr.0, unit.frozen) => {
            Ok(Binding::Frozen(addr.0 - unit.frozen.0))
        }
        Target::Link(addr) if in_domain(addr.0, unit.code) => {
            Ok(Binding::Stub(addr.0 - unit.code.0))
        }
        other => symbols
            .name_of(other)
            .ok_or_else(|| format!("no symbol names {other:?}, which this unit does not own")),
    }
}

/// The dependency-image slot an address inside a frozen region occupies, if the region that contains
/// it is one.
///
/// [`crate::os_arch::addrspace::image_slot`] classifies a region *base*; a link address is an offset
/// inside one, so carrying a layer's addresses needs this. The span is the region's own — the spline
/// step is far wider than [`FROZEN_CAP`] — which is what keeps an address that is nobody's region (a
/// `dlsym` result in a foreign slot's fixpoint, say) from being mistaken for a layer's.
pub(crate) fn frozen_slot_of(addr: usize) -> Option<usize> {
    use crate::os_arch::addrspace as layout;
    let offset = addr.checked_sub(layout::IMAGE_SPLINE_BASE)?;
    let slot = offset / layout::IMAGE_SPLINE_STEP;
    (slot < layout::IMAGE_SPLINE_COUNT
        && offset - slot * layout::IMAGE_SPLINE_STEP < crate::vm::frozen::FROZEN_CAP)
        .then_some(slot)
}

/// The code-spline counterpart of [`frozen_slot_of`]: the slot an entry-stub address inside a code
/// region occupies.
fn code_slot_of(addr: usize) -> Option<usize> {
    use crate::os_arch::addrspace as layout;
    let offset = addr.checked_sub(layout::IMAGE_CODE_SPLINE)?;
    let slot = offset / layout::IMAGE_CODE_STEP;
    (slot < layout::IMAGE_CODE_COUNT
        && offset - slot * layout::IMAGE_CODE_STEP < crate::vm::codearena::CODE_CAP)
        .then_some(slot)
}

/// Whether a stored layer may carry this *data* address.
///
/// The delta region is this program's own: its base is fixed, but what lives at an offset inside it is
/// decided by this session's lowering, so a layer that carries one is not a layer another process can
/// use. Otherwise the address's frozen slot has to be at or below the layer's own.
fn above_frozen(target: u64, home: usize) -> Option<u64> {
    use crate::os_arch::addrspace as layout;
    let addr = target as usize;
    if (layout::DELTA_FIXED_ADDR..layout::IMAGE_SPLINE_BASE).contains(&addr) {
        return Some(target);
    }
    let slot = frozen_slot_of(addr)?;
    (slot > home).then_some(target)
}

/// Whether a stored layer may carry this *executable entry* address: the delta's own code region is
/// refused like its data region, and the slot test is the code spline's. The two splines are separate
/// spaces with the same numbering and the same step, so an address names a slot only under the kind it
/// is — the code spline's base line is numerically a frozen slot's, hundreds of slots above the layer
/// that owns it.
fn above_entry(target: u64, home: usize) -> Option<u64> {
    use crate::os_arch::addrspace as layout;
    let addr = target as usize;
    if (layout::DELTA_CODE_ADDR..layout::BASE_CODE_ADDR).contains(&addr) {
        return Some(target);
    }
    let slot = code_slot_of(addr)?;
    (slot > home).then_some(target)
}

/// The first link address in a *higher* dependency-image spline slot than this layer's own — the one
/// shape a stored layer cannot carry.
///
/// A layer's slot is its position in the unit table of the program that built it, so a reference to a
/// slot below is pinned by the layout the manifest records, and its own slot is where it will be
/// restored. A reference *above* it would mean the layer reaches a layer that is not below it at all,
/// which is wrong above every stack; frozen relocations and the TLS/GOT tables are what can express
/// it, because the bodies and the id-bearing tables are canonical.
pub(crate) fn cross_layer_target(module: &crate::vm::ir::Module, home: usize) -> Option<u64> {
    use crate::vm::ir::FrozenRelocTarget;
    // Every table of the serialized module that holds a bare link address: a relocation's target, a
    // TLS template, and a GOT slot. Bodies and the id-bearing tables are canonical (`Tables`), but
    // these three are stored as they are, so they are what the layer has to be able to explain.
    module
        .frozen_relocs
        .iter()
        .find_map(|reloc| match reloc.target {
            FrozenRelocTarget::Frozen(addr) => above_frozen(addr.0, home),
            FrozenRelocTarget::Entry(addr) => above_entry(addr.0, home),
        })
        .or_else(|| {
            module
                .tls
                .iter()
                .find_map(|slot| above_frozen(slot.template.0, home))
        })
        .or_else(|| {
            module
                .got_fixups
                .iter()
                .find_map(|fixup| above_frozen(fixup.addr.0, home))
        })
}

/// The first name a layer's own fn-entry/static index binds to an address it may not carry, with that
/// address.
///
/// The module check covers what the layer's *data* reads; a layer also publishes name→address indexes,
/// and the session above binds those names to exactly those addresses. An index entry pointing above
/// the layer is as unusable as a relocation pointing there — worse, because nothing downstream would
/// re-derive it. The two indexes hold different kinds of address, so each is classified as its own.
pub(crate) fn index_target<'a>(
    home: usize,
    entries: impl IntoIterator<Item = (&'a str, u64)>,
    statics: impl IntoIterator<Item = (&'a str, u64)>,
) -> Option<(&'a str, u64)> {
    entries
        .into_iter()
        .find_map(|(name, addr)| above_entry(addr, home).map(|addr| (name, addr)))
        .or_else(|| {
            statics
                .into_iter()
                .find_map(|(name, addr)| above_frozen(addr, home).map(|addr| (name, addr)))
        })
}

/// One function's fragment bytes and bindings, turned back into the body the runtime needs.
pub(crate) fn rehydrate(
    fragment: &[u8],
    bindings: &[Binding],
    name: &str,
    unit: &Unit,
    symbols: &Symbols,
) -> Result<FuncBody, String> {
    let mut body = frag::decode(fragment)?;
    body.name = name.into();
    let mut binder = Binder {
        bindings,
        unit,
        symbols,
        failed: None,
    };
    frag::visit_sites(&mut body, &mut binder);
    // The walk is infallible so that one exhaustive match serves both directions; a binding that does
    // not resolve records the reason here and the body is discarded.
    match binder.failed {
        Some(error) => Err(error),
        None => Ok(body),
    }
}

/// Resolves the ordinals a stored body carries back into ids and addresses.
struct Binder<'a> {
    bindings: &'a [Binding],
    unit: &'a Unit,
    symbols: &'a Symbols,
    failed: Option<String>,
}

impl Binder<'_> {
    fn at(&self, ordinal: u32, want: &str) -> Result<&Binding, String> {
        self.bindings
            .get(ordinal as usize)
            .ok_or_else(|| format!("ordinal {ordinal} is outside the binding table"))
            .and_then(|binding| match binding {
                Binding::Symbol { .. } => Ok(binding),
                Binding::Func(_) if want == "function" => Ok(binding),
                Binding::Tls(_) if want == "TLS" => Ok(binding),
                Binding::Asm(_) if want == "asm stub" => Ok(binding),
                Binding::Frozen(_) | Binding::Stub(_) if want == "address" => Ok(binding),
                _ => Err(format!("ordinal {ordinal} does not name a {want}")),
            })
    }

    fn bind_func(&self, id: &mut FuncId) -> Result<(), String> {
        *id = match self.at(*id, "function")? {
            Binding::Func(local) => self.unit.funcs.0 + local,
            Binding::Symbol {
                kind: SymbolKind::Func,
                name,
            } => self
                .symbols
                .func(name)
                .ok_or_else(|| format!("function symbol `{name}` is not in the stack below"))?,
            other => return Err(format!("function ordinal bound to {other:?}")),
        };
        Ok(())
    }

    fn bind_tls(&self, id: &mut TlsId) -> Result<(), String> {
        *id = match self.at(*id, "TLS")? {
            Binding::Tls(local) => self.unit.tls.0 + local,
            Binding::Symbol {
                kind: SymbolKind::Tls,
                name,
            } => self
                .symbols
                .tls(name)
                .ok_or_else(|| format!("TLS symbol `{name}` is not in the stack below"))?,
            other => return Err(format!("TLS ordinal bound to {other:?}")),
        };
        Ok(())
    }

    fn bind_asm(&self, id: &mut AsmStubId) -> Result<(), String> {
        *id = match self.at(*id, "asm stub")? {
            Binding::Asm(local) => self.unit.asm.0 + local,
            other => return Err(format!("asm-stub ordinal bound to {other:?}")),
        };
        Ok(())
    }

    fn bind_link(&self, addr: &mut LinkAddr) -> Result<(), String> {
        let ordinal = u32::try_from(addr.0).unwrap_or(u32::MAX);
        *addr = match self.at(ordinal, "address")? {
            Binding::Frozen(offset) => LinkAddr(self.unit.frozen.0 + offset),
            Binding::Stub(offset) => LinkAddr(self.unit.code.0 + offset),
            Binding::Symbol { kind, name } => LinkAddr(
                self.symbols
                    .address(*kind, name)
                    .ok_or_else(|| format!("address symbol `{name}` is not in the stack below"))?,
            ),
            other => return Err(format!("address ordinal bound to {other:?}")),
        };
        Ok(())
    }
}

impl SiteVisitor for Binder<'_> {
    fn func(&mut self, id: &mut FuncId) {
        if let Err(error) = self.bind_func(id) {
            self.failed.get_or_insert(error);
        }
    }
    fn tls(&mut self, id: &mut TlsId) {
        if let Err(error) = self.bind_tls(id) {
            self.failed.get_or_insert(error);
        }
    }
    fn asm(&mut self, id: &mut AsmStubId) {
        if let Err(error) = self.bind_asm(id) {
            self.failed.get_or_insert(error);
        }
    }
    fn link(&mut self, addr: &mut LinkAddr) {
        if let Err(error) = self.bind_link(addr) {
            self.failed.get_or_insert(error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::ir::{
        Block, CallRole, Operand, PlaceBase, PlaceExpr, RetAbi, RetDest, Rvalue, ScalarPlace, Slot,
        Stmt, Terminator, UnwindAction, Width,
    };

    /// A unit whose ids start away from zero, so a local binding has to add the base back.
    fn unit() -> Unit {
        Unit {
            funcs: (10, 4),
            tls: (100, 2),
            asm: (200, 1),
            frozen: (0x6a00_0000_0000, 0x1000),
            code: (0x6e00_0000_0000, 0x1000),
            layers: Vec::new(),
            self_layer: 0,
        }
        .above(&[])
    }

    fn static_place(addr: u64) -> PlaceExpr {
        PlaceExpr {
            base: PlaceBase::Static(LinkAddr(addr)),
            steps: Box::new([]),
        }
    }

    /// A body that names one target of every kind the walk knows, in walk order: statements before
    /// the terminator, and a call's callee before its arguments.
    fn body_with_sites(tls: TlsId, static_addr: u64, callee: FuncId, addr_imm: u64) -> FuncBody {
        FuncBody {
            frame_size: 8,
            frame_align: 8,
            ret: RetAbi::Zst,
            params: Vec::new(),
            caller_loc_off: None,
            blocks: vec![Block {
                stmts: vec![
                    Stmt::Assign {
                        dst: ScalarPlace::Slot(Slot {
                            off: 0,
                            width: Width::W64,
                        }),
                        rv: Rvalue::TlsRef(tls),
                    },
                    Stmt::Copy {
                        dst: static_place(static_addr),
                        src: PlaceExpr {
                            base: PlaceBase::Local(0),
                            steps: Box::new([]),
                        },
                        size: 8,
                    },
                ],
                term: Terminator::Call {
                    callee,
                    args: vec![Operand::AddrImm(LinkAddr(addr_imm))],
                    ret: RetDest::Ignore,
                    target: 0,
                    unwind: UnwindAction::Continue,
                    role: CallRole::Normal,
                },
            }],
            name: "sym".into(),
        }
    }

    fn asm_body(stub: AsmStubId) -> FuncBody {
        FuncBody {
            frame_size: 8,
            frame_align: 8,
            ret: RetAbi::Zst,
            params: Vec::new(),
            caller_loc_off: None,
            blocks: vec![Block {
                stmts: Vec::new(),
                term: Terminator::InlineAsm {
                    stub,
                    buf_size: 0,
                    ins: Vec::new(),
                    outs: Vec::new(),
                    target: 0,
                },
            }],
            name: "asm".into(),
        }
    }

    fn symbols_with_func() -> Symbols {
        let mut symbols = Symbols::default();
        symbols.func_names.insert(7, "base::fn".into());
        symbols.funcs.insert("base::fn".into(), 7);
        symbols
    }

    fn sig() -> crate::vm::ir::ForeignSig {
        crate::vm::ir::ForeignSig {
            args: Vec::new(),
            ret: crate::vm::ir::FfiKind::U64,
            fixed: None,
            thunk_args: Vec::new(),
            unwind: true,
        }
    }

    #[test]
    fn the_canonical_tables_rebuild_under_a_different_prefix() {
        // A unit whose functions start at 100 and TLS slots at 20, with an export, an entry link, an
        // entry stub and a TLS symbol that name its own items, plus one export naming a lower layer's
        // function by symbol.
        let unit = unit();
        let mut symbols = Symbols::default();
        symbols.func_names.insert(7, "base::fn".into());
        symbols.funcs.insert("base::fn".into(), 7);
        let mut module = crate::vm::ir::Module::default();
        module.exports.insert("mine".into(), 11);
        module.exports.insert("theirs".into(), 7);
        module
            .fn_entry_links
            .push((LinkAddr(unit.frozen.0 + 0x40), 12));
        module.entry_stub_sites.push(crate::vm::ir::EntryStubSite {
            link_addr: LinkAddr(unit.code.0 + 0x20),
            func: 13,
            sig: sig(),
        });
        let tls_by_sym = HashMap::from([(Box::<str>::from("T"), 101u32)]);

        let tables = Tables::capture(&mut module, &unit, &symbols, &tls_by_sym).unwrap();
        assert!(module.exports.is_empty(), "exports stay in the manifest");
        assert!(
            module.fn_entry_links.is_empty(),
            "links stay in the manifest"
        );
        assert_eq!(module.entry_stub_sites[0].func, 0);
        let stored: HashMap<&str, &Owned> = tables
            .exports
            .iter()
            .map(|(name, owned)| (name.as_ref(), owned))
            .collect();
        assert_eq!(stored["mine"], &Owned::Local(1));
        assert_eq!(
            stored["theirs"],
            &Owned::Symbol {
                kind: SymbolKind::Func,
                name: "base::fn".into()
            }
        );

        // The same stack: every id comes back where it was.
        let (exports, tls) = tables.restore(&mut module, &unit, &symbols).unwrap();
        assert_eq!(exports["mine"], 11);
        assert_eq!(exports["theirs"], 7);
        assert_eq!(
            module.fn_entry_links,
            vec![(LinkAddr(unit.frozen.0 + 0x40), 12)]
        );
        assert_eq!(module.entry_stub_sites[0].func, 13);
        assert_eq!(tls["T"], 101);

        // A stack with 50 more functions and 5 more TLS slots below: the local ids move with the
        // prefix, the lower layer's symbol does not.
        let moved = Unit {
            funcs: (60, 4),
            tls: (105, 2),
            ..unit.clone()
        }
        .above(&[]);
        let (exports, tls) = tables.restore(&mut module, &moved, &symbols).unwrap();
        assert_eq!(exports["mine"], 61);
        assert_eq!(exports["theirs"], 7);
        assert_eq!(
            module.fn_entry_links,
            vec![(LinkAddr(unit.frozen.0 + 0x40), 62)]
        );
        assert_eq!(module.entry_stub_sites[0].func, 63);
        assert_eq!(tls["T"], 106);
    }

    #[test]
    fn local_targets_bind_to_indices_and_domain_offsets() {
        let unit = unit();
        let symbols = Symbols::default();
        let body = body_with_sites(101, unit.frozen.0 + 0x40, 12, unit.code.0 + 0x80);
        let projected = project(&body, &unit, &symbols).unwrap();
        assert_eq!(
            projected.record.bindings,
            vec![
                Binding::Tls(1),
                Binding::Frozen(0x40),
                Binding::Func(2),
                Binding::Stub(0x80),
            ]
        );
        assert_eq!(projected.record.fragment, frag::id_of(&projected.bytes));
    }

    #[test]
    fn a_target_the_unit_does_not_own_is_recorded_by_symbol() {
        let unit = unit();
        let symbols = symbols_with_func();
        let body = body_with_sites(100, unit.frozen.0, 7, unit.code.0);
        let projected = project(&body, &unit, &symbols).unwrap();
        assert_eq!(
            projected.record.bindings,
            vec![
                Binding::Tls(0),
                Binding::Frozen(0),
                Binding::Symbol {
                    kind: SymbolKind::Func,
                    name: "base::fn".into()
                },
                Binding::Stub(0),
            ]
        );
    }

    #[test]
    fn a_target_no_layer_names_is_not_storable() {
        let unit = unit();
        let symbols = Symbols::default();
        let body = body_with_sites(0, 0, 7, 0);
        let error = project(&body, &unit, &symbols).unwrap_err();
        assert!(error.contains("no symbol names"), "{error}");
    }

    #[test]
    fn every_site_comes_back_the_same_after_a_round_trip() {
        let unit = unit();
        let symbols = symbols_with_func();
        let bodies = [
            body_with_sites(101, unit.frozen.0 + 0x40, 13, unit.code.0 + 0x80),
            asm_body(200),
        ];
        for body in bodies {
            let projected = project(&body, &unit, &symbols).unwrap();
            let rebuilt = rehydrate(
                &projected.bytes,
                &projected.record.bindings,
                "sym",
                &unit,
                &symbols,
            )
            .unwrap();
            // Canonicalizing both sides compares every reference site: the ordinals are positional,
            // so equal targets mean every id and address came back where it was.
            assert_eq!(
                frag::canonical(&body).targets,
                frag::canonical(&rebuilt).targets
            );
            assert_eq!(rebuilt.name.as_ref(), "sym");
            assert_eq!(rebuilt.frame_size, body.frame_size);
        }
    }

    #[test]
    fn a_binding_of_the_wrong_kind_is_rejected() {
        let unit = unit();
        let symbols = Symbols::default();
        let body = body_with_sites(101, unit.frozen.0, 13, unit.code.0);
        let projected = project(&body, &unit, &symbols).unwrap();
        let mut bindings = projected.record.bindings.clone();
        bindings[0] = Binding::Func(0);
        let error = rehydrate(&projected.bytes, &bindings, "sym", &unit, &symbols).unwrap_err();
        assert!(error.contains("does not name a TLS"), "{error}");
    }

    #[test]
    fn a_symbol_the_stack_below_lost_is_rejected_instead_of_guessed() {
        let unit = unit();
        let symbols = symbols_with_func();
        let body = body_with_sites(100, unit.frozen.0, 7, unit.code.0);
        let projected = project(&body, &unit, &symbols).unwrap();
        let error = rehydrate(
            &projected.bytes,
            &projected.record.bindings,
            "sym",
            &unit,
            &Symbols::default(),
        )
        .unwrap_err();
        assert!(error.contains("is not in the stack below"), "{error}");
    }

    /// A layer's addresses are classified by the region that contains them: its own slot and every slot
    /// below it are carried as offsets *inside* a region, the base image it is stacked on is carried,
    /// and a higher slot, this program's delta, or an address no region owns is not. The two kinds of
    /// address are read in their own spline — a frozen pointer and an executable entry.
    #[test]
    fn a_link_address_is_classified_by_the_region_containing_it() {
        use crate::os_arch::addrspace as layout;
        let home = 3;
        let data = |addr: usize| index_target(home, [], [("static", addr as u64)]).is_none();
        let entry = |addr: usize| index_target(home, [("entry", addr as u64)], []).is_none();
        assert!(data(layout::image_addr(2) + 0x18));
        assert!(data(layout::image_addr(3) + 0x4238));
        assert!(data(layout::BASE_IMAGE_FIXED_ADDR + 0x40));
        // A `dlsym` result, or any other host address: no region contains it.
        assert!(data(0x0000_7f2b_c000_1234));
        assert!(entry(
            layout::IMAGE_CODE_SPLINE + 3 * layout::IMAGE_CODE_STEP + 0x30
        ));
        assert!(entry(layout::BASE_CODE_ADDR + 0x40));
        assert!(entry(0x0000_7f2b_c000_1234));
        assert!(!data(layout::image_addr(4) + 0x8));
        assert!(!data(layout::DELTA_FIXED_ADDR + 0x8));
        assert!(!entry(
            layout::IMAGE_CODE_SPLINE + 4 * layout::IMAGE_CODE_STEP + 0x8
        ));
        assert!(!entry(layout::DELTA_CODE_ADDR + 0x8));
    }
}
