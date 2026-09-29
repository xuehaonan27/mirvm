# Fine-Grained Dependency Sharing: Per-Crate Image Units and the Fragment Store

> Status: Decided RFC · Scope: how the lowered products of dependency crates are stored and shared
> on one machine — per-crate image units in place of the monolithic deps image, a content-addressed
> fragment store that shares identical function bodies across crate versions, feature sets and
> projects, and the collection that keeps that store bounded. Parent:
> [distribution-design.md](distribution-design.md) (cache layering; its "no partial reuse" rule stays
> true for the program entry and is refined here for the dependency layer). Siblings:
> [d15-cargoless-design.md](d15-cargoless-design.md) (the build graph and fingerprints the unit key
> is derived from), [frame-abi-bytecode.md](frame-abi-bytecode.md) (the in-memory bytecode; this
> document owns only its at-rest storage form), [modeb-mirvmar-design.md](modeb-mirvmar-design.md)
> (the per-Engine `LinkAddr` mapping discipline the load path reuses),
> [jit-code-cache-design.md](jit-code-cache-design.md) (the machine-code cache keyed by this
> document's fragment ids).

## 1. Problem

The deps image (`src/image/deps.rs`) is one file per `(build_id, base key, sorted --extern stamps of
the whole direct dependency set)`: the entire registry closure of one program, lowered, purified and
frozen as a single artifact. That shape has three costs, and they multiply:

- **No cross-project sharing.** A project using `tokio + serde` and a project using `tokio + axum`
  share zero bytes of lowered product, although every lowered tokio instance they have in common is
  identical. A machine with N projects stores up to N full closures.
- **No cross-version sharing.** Two closures differing in one patch version of one crate — or only in
  which features of one crate are enabled — are distinct keys, hence two full copies, although the
  overwhelming majority of the lowered function bodies are byte-identical after the version
  disambiguator is factored out.
- **All-or-nothing invalidation.** One lockfile edit orphans the whole image; the next run re-lowers
  the whole closure.

The layers below already share at fine granularity: L0 registry sources are machine-global, and L1
rlibs are per-crate, keyed by the cargoless fingerprint (d15 C11) in the shared target root. The
lowered layer is where sharing currently stops.

Half of the finer design is already reserved in the tree: the address space provides a spline of
1300 fixed 16 GiB windows, one per dependency image, with k assigned by lockfile topology
(`src/os_arch/*/addrspace.rs`), and `lower_for_image_build(tcx, stack, k)` is a reserved, uncalled
entry point (`src/lower/mod.rs`). This document is the design that line was reserved for, extended
one level further down, to function granularity.

## 2. Contract

- **The unit of dependency reuse is one crate, not one closure.** A *unit* is one build-graph
  compilation unit whose source is a registry or git crate. Path/workspace crates and the bin are
  never units; they stay delta-class (the same boundary as `Purity::Local`).
- **Unit identity is pre-session and transitive:** `unit_key = digest(build_id, base key, FileStamp
  of the unit's rlib)`. The rlib content hash pins version, resolved features, cfgs and — through
  rustc's SVH chain — the unit's entire dependency sub-closure, by the same rebuild-propagation
  argument the aggregate key uses today. No tcx is needed to compute it.
- **Units are demand-published and monotone.** A program session lowers what its stack is missing,
  classifies each pure instance to its *home* unit (§3.2), and publishes per unit. A unit's content
  grows as programs exercise more of the crate; manifests are immutable and content-named
  (`<unit_key>-<manifest_digest>.unit`), so a grown unit never invalidates a reader that pinned the
  older digest.
- **Cross-unit edges are symbolic; intra-unit edges are compact.** A stored unit never bakes another
  layer's `FuncId`s, `TlsId`s or frozen addresses. Calls, statics, TLS and fn-entry references that
  leave the unit are recorded by v0 symbol name and resolved through the image stack's union lookup
  at load; intra-unit references use unit-local ordinals. This dissolves today's "the stack below
  must be byte-identical" constraint: a unit is valid above any stack that satisfies its symbols.
- **Function bodies live once per machine, in a content-addressed fragment store.** The unit manifest
  maps each function symbol to `(fragment id, binding table)`; the fragment is the body in a
  canonical relocatable encoding (§3.3) whose id is BLAKE3-256 of its bytes. Two units that lower an
  identical body — adjacent versions of one crate, one crate under two feature sets, or two crates
  expanding the same macro — reference one fragment.
- **Correctness never depends on version similarity.** Sharing is opportunistic byte-identity of
  canonical encodings, decided by hash equality alone. A body that differs in any way (layout change,
  MIR change, different callee arity) hashes differently and is stored separately; there is no
  version-compatibility judgment anywhere.
- **Store discipline is unchanged.** Families are registered in `src/store/mod.rs`; publication is
  atomic; manifests are generational (`build_id` first field); any miss, race loss or validation
  failure degrades to session lowering and self-heals; loaded modules pass the existing
  verifier before use. `MIRVM_NO_DEPS_IMAGE=1` bypasses the whole dependency-share stack.
- **Track discipline.** The Cargo track's mission is Cargo's official default behaviour, so it gains
  no mirvm-specific resolution machinery: no unit graph, no per-crate loading. It keeps its
  closure-level key (base key + the sorted `--extern` stamps) and changes only its storage form —
  one closure-scoped manifest over the shared fragment store, which is behaviour-neutral and gives
  that track the storage dedup for free. Units, the home rule and load-order domains are
  cargoless-only, where the resolver owns the build graph. Unit keys never cross tracks (the rlibs
  differ byte-wise), but fragments dedupe across them by construction: symbol names are lifted out
  and profile semantics are pinned equal (d15 C10). Nothing is migrated: the monolithic deps-image
  format is deleted and replaced, not kept alongside.

## 3. Model

### 3.1 The sharing ladder

| layer | unit of reuse | key | shared across |
|---|---|---|---|
| L0 sources | crate archive | registry checksum | everything (exists) |
| L1 rlib/MIR | crate compilation | cargoless fingerprint (d15 C11) | projects with equal fingerprint (exists) |
| **L1.7 unit manifest** | **crate lowering** | **unit_key** | **projects sharing the crate's sub-closure (new)** |
| **L1.8 fragment** | **one function body** | **BLAKE3 of canonical bytes** | **everything: versions, features, projects, crates (new)** |
| L2 program entry | program delta | rustc args + dep-info | reruns of one program (exists) |
| L3 machine code | one compiled body | fragment id × jit-key | everything its fragment shares ([jit-code-cache-design.md](jit-code-cache-design.md)) |

Storage cost falls from Σ over program closures of |closure| (today) to Σ over distinct units of
|manifest| plus Σ over *unique bodies* of |fragment|. The first sum collapses the per-project
multiplier; the second collapses the per-version multiplier the first cannot reach.

### 3.2 Units and the home rule

`classify_purity` today splits instances two ways: image-class (pure) versus delta. Units refine the
image class:

- **home(inst)** = the minimal crate C in the build graph such that the defining crate of `inst` and
  every crate mentioned by its generic arguments lie in closure(C). For a non-generic tokio function
  that is tokio itself; for `tokio::f::<bytes::Bytes>` it is still tokio (bytes ∈ closure(tokio));
  for an instance whose arguments span two crates neither of which depends on the other, no dep-crate
  C exists and the instance is **residue**, which v1 sends to the delta (cached per program by L2, as
  today — nothing is lost cross-project, because the aggregate never shared it either).
- The home rule preserves the downward closure the split relies on: everything a unit-homed instance
  references is homed in the same unit or below it, by the same argument that keeps pure instances
  from reaching local ones. The existing post-rebase contamination self-check generalizes from
  "image must not touch LOCAL_CRATE" to "unit k must not touch anything above its closure".
- Two semver-forked versions of one crate in one graph (d15 C2) are two units; v0 symbol names carry
  the crate disambiguator, so their exports never collide in the union lookup.
- Multi-way split generalizes today's two-way `Split`: one tag bit becomes a per-instance home index,
  one frozen arena / id space / GOT / stub arena per home actually populated this session. Instances
  already covered by a loaded unit are reused through the stack exactly as base hits are today.

### 3.3 The stored form: symbolic edges, canonical fragments

The bytecode is already relocatable in principle; the design only exploits invariants that exist:

- Ids appear in exactly three places (`Call.callee`, `InlineAsm.stub`, `Rvalue::TlsRef` — the
  exhaustive `rebase.rs` walk); addresses appear in exactly two (`PlaceBase::Static(LinkAddr)`,
  `Operand::AddrImm(LinkAddr)`), and "an ordinary integer is never guessed to be an address".
  Frozen-to-frozen pointers are enumerated `FrozenReloc`s; foreign symbols go through the GOT and are
  re-resolved at startup; fn-ptr values are `EntryStubSite` recipes rebuilt per Engine.

The same rule binds the manifest's *tables*, and landing the unit store showed why: a unit's absolute
ids are the sum of the layers below it and its link addresses are the spline slot it was lowered in,
so a manifest that stored either could only be loaded above the exact stack that produced it. Storing
exports, entry links, entry-stub sites, frozen relocations, TLS templates, GOT slots and the export
indexes in the same local form (a unit-local ordinal, an offset inside one of the unit's own domains,
or a symbol a lower layer owns) is what makes "a unit is valid above any stack that satisfies its
symbols" literal rather than aspirational; the loader rebuilds them from the prefix and the slot it
chose. The closure manifest already looks position-independent only because its stack is fixed.

An index entry is a link address too: `fn_entry_syms` and `static_syms` bind a name to an address, and
the session above resolves those names straight to those addresses. A layer may publish only entries in
its own slot or in one below it — never a higher unit's slot and never the delta — and the writer
refuses a layer that carries one outside that span while the loader re-checks it, because the store
outlives the build that wrote it. A relocation that reaches too far is the milder shape of the two: the
loading session re-derives its meaning through the binding walk, whereas an index entry is taken
verbatim, so a session that loads this unit without the other has nothing to bind the name to. Entry
and static addresses are each classified in their own spline: the two splines share a numbering and a
step, so an address names a slot only under the kind it is.

A **fragment** is one `FuncBody` in canonical form: the `name` field is lifted into the manifest, and
each of the five reference sites is rewritten to a dense ordinal in order of first appearance.
`CallForeign`/GOT names are stable C symbols and stay inline. Fragment bytes = one encoding-version
byte + postcard of the canonical body; fragment id = BLAKE3-256 of those bytes. Version
disambiguators, frozen-layout offsets and id assignments — everything that differs between tokio
1.x.y and 1.x.z when the code does not — thereby leaves the fragment and moves into the binding.

The **manifest** carries, per function in symbol-sorted order, and every symbol-keyed table it adds
(the export table, the TLS table, and the entry-link table in address order) in sorted order too, so
the manifest digest is deterministic (the base image's byte-determinism gate is the precedent; a
map's own order is its hasher's, and one unit built twice must name one digest). Per function it
carries: the v0 symbol, the fragment id, and the binding table that assigns each ordinal one of: unit-local function/TLS/asm index ·
extern function/static/TLS symbol (interned in a per-manifest string table) · frozen offset +
addend. Beside the function table it stores what the aggregate file stores today: header material
compared for exact equality on load (`build_id`, base key, the rlib `FileStamp`, lowering
fingerprint), the frozen bytes, `FrozenReloc`s (with cross-unit targets in symbolic form), TLS
table, asm recipes, GOT, entry-stub sites and export/static/TLS indexes.

Because binding happens at decode, a unit is **position-independent**: k really is assigned by load
order per program, as the spline comment always said. The mechanics are the `.mirvm` package's, not
new: frozen bytes restore into `image_addr(k)` via `FrozenArena::restore`, and `LoadMap` translates
link-base ranges to the loaded base for relocs, TLS templates and entry links. The
fixed-domain publishability guard is replaced for units by "fully canonical: no raw address or id
survives encoding", which the verifier checks at publish and load.

### 3.4 Store families and collection

Two new `Class::Cache` families in the register:

- `cache/units/` — `<unit_key>-<manifest_digest>.unit`, generational shape. A new session prefers
  the newest current-generation manifest per `unit_key`; an L2 entry's key chain pins exact digests,
  so machine-wide unit growth caused by *other* projects misses that L2 entry (correct: absolute
  offsets shifted) without stealing the manifest it would need to rebuild cheaply. The closure track
  writes the same manifest format with its closure-level key material (it does so already, in
  `cache/deps/`), so one format serves both tracks and only the keying discipline differs.
- `cache/frags/` — append-only pack files, each an atomic publish of one session's new fragments with
  a trailing index (`id`, record offset, fragment length, sorted by id), named by the BLAKE3 of the
  pack's own bytes so republishing one fragment set is idempotent. A fragment averages ~131 bytes
  (measured by the probe below), which is why a pack and not one file per fragment: a filesystem
  block each would spend roughly thirty times the data. Readers read a pack's trailing index and then
  only the fragments they ask for, re-hashing each body against its id before the binding walk and
  postcard decode; writers dedupe against every pack's index before appending, so a re-lowered
  closure stores no fragment twice and a lost race stores a duplicate that the next repack folds.

Fragments carry no generation: they are reached only through manifests, manifests are generational,
and collection is **mark-sweep from current-generation manifests** — refcounts are rejected as
crash-fragile. The frozen region's chunks are the same kind of record in their own family
(`cache/frozen`, §3.6), marked from the same manifests and swept under the same lock discipline.
`cache purge` marks live ids and chunk ids, rewrites packs whose live ratio is low, and
deletes prunable manifests (stale generations as today; superseded digests per unit_key beyond a
small keep-count). A pack whose live share stays above half its bytes is left alone: copying it would
buy back few bytes, so the dead records it keeps are counted until a later sweep folds them. Sweep
takes an exclusive lock on the family; publishers take it shared, and a publisher holds it across the
fragment pack *and* the manifest that names it — a sweep between the two writes would see fragments
no manifest names yet and drop them. If two
mirvm builds happen to produce byte-identical fragment bytes, the hash collides *correctly* — content
addressing is self-consistent — so cross-build sharing is free when the encoding is unchanged, and
never wrong when it is not.

### 3.5 Load path

Pre-session, per crate in topological order: compute `unit_key` (rlib stamp — no tcx), pick the
newest valid manifest, validate exact-equality header material and generation. The stack becomes
`[base, unit_1 … unit_n]` with k by load order; union lookups, cumulative offsets, fingerprint
prefix-truncation and the key chain are the existing `ImageStack` mechanics.

A layer the stack provides is **immutable**, and that decides the whole load rule. A session that
loaded *any* layer lowers no home: the loaded layer answers some instances and statics by symbol, so
the homes such a session would lower are laid out differently from the manifests written above them,
and a stack the store cannot provide *whole* is not used at all — the session lowers everything into
the delta instead of assembling a stack no manifest describes. An instance a loaded layer does not
already name — rustc instantiates some dependency generics in the instantiating crate, so its mangled
name carries this program — is therefore residue in the delta rather than an addition to a shared
manifest. Without that rule a session that loaded a lower unit would republish it from one program's
view, and the next run would refuse the manifest it had just written. The store consequently converges
from cold sessions only; growing it incrementally (lowering exactly the missing units above the loaded
ones and publishing those) is deferred, and the stable home rule — a placement that does not consult
the load set — is its first half. Absorb at the end is unchanged. The L2 chain grows from
`base ⊕ aggregate` to `base ⊕ (unit_key, manifest_digest)*`, same mechanism.

Decode of one function costs postcard + one binding walk (the `rebase.rs` shape: a few table lookups
per reference). The lazy table amortizes it behind the heat order; the §2.6 baseline gates of the
parent document are the regression fence.

### 3.6 What deliberately duplicates

- **Anonymous allocations** (promoted constants, vtables) never cross a unit boundary; a referencing
  unit materializes its own copy, as image/delta promotion already does. Vtable and promoted-const
  address identity is unspecified in RAM, so duplication is as-if-legal; named statics and TLS have
  exactly one home (their defining crate) and are always reached by symbol, so `static mut` identity
  is preserved by construction.
- **Residue instances** duplicate per program (in the delta), bounded by the same purity ledger that
  measures them today.
- **Manifest overhead** duplicates per unit: symbols, bindings, frozen bytes. Symbols exist per unit
  today and the binding tables are the net new cost, bought back many times over by fragment sharing;
  the frozen region does not duplicate in the store, because a manifest names its region's chunks
  rather than carrying the bytes (§3.4).

## 4. Boundaries

- No cross-`build_id` compatibility is promised. Fragments may coincide across builds (§3.4); that is
  a bonus, not a contract.
- The L3 JIT code cache is not this document's topic: [jit-code-cache-design.md](jit-code-cache-design.md)
  owns it, keyed by this document's fragment ids.
- rlibs are rustc's artifacts and are not deduplicated here. The frozen region is not stored whole:
  it is cut into 4 KiB chunks under their own content addresses, because consecutive revisions of a
  layer share most of it (measured below).
- The base image does not move into the fragment store: it is one byte-deterministic artifact per
  build, with no sibling to share bodies with. An L2 program entry is a manifest of fragments, so an
  edit reuses the bodies it did not change.
- This is not a distribution format: `.mirvm` stays self-contained; `pack` absorbs a stack and never
  ships fragment references.

## 5. Verification

- **Dedup probe** (the parent document's "measure before cache design" discipline): `MIRVM_FRAG_STATS`
  prints one canonical fragment id per lowered body plus a per-layer summary (bodies → fragments,
  canonical and frozen bytes, frozen share), and the `frag-share` gate intersects the id sets of two
  sessions. Measured on `fixtures/frag-sharing`: one changed function keeps 99.4% of the fragments
  (98.8% of the union), one more feature keeps 99.4% (96.6%). Those numbers are what the
  sharing-acceptance ratio below asserts.
- **Unit determinism gate**: one unit built twice (threads 1 and 8, separate sessions) yields one
  manifest digest and one fragment set — the base-image byte-determinism gate, generalized.
  `unit-share`'s last step is it: the same package is built in two sessions whose scheduling differs
  (one job against eight) and the two runs must leave the unit manifests named identically (they are
  content-named, so equal names are equal digests) and store no new fragment pack. It caught three
  order leaks in the manifest's tables — the export and TLS tables were written in their hash maps'
  order and the entry-link table in lowering's — which are now sorted.
- **Equivalence gate**: cold full lowering versus a warm unit stack must produce byte-identical guest
  output through the existing diff channel, per the L2 acceptance rule.
- **Sharing acceptance**: two programs whose closures lower the same bodies produce one stored copy of
  every fragment — the `deps-image` gate drops a closure's manifest, lowers it again and requires that
  the store grew by no pack at all; two adjacent versions of the fixture crate keep ≥ 99% of their
  fragments (the `frag-share` gate); `cache status` reports the family's fragments, unique bytes and
  repeated bytes, which is the dedup factor until manifests make live/dead accounting possible.
- **Unit sharing gate**: two programs with overlapping closures — one on `memchr`, one on `memchr` and
  `itoa` — must produce one manifest for the shared unit. `unit-share` has the first program publish
  it, requires the second to load what the first wrote and to publish only its own unit, requires a
  warm rerun to write no manifest at all, and diffs the loaded-stack run's guest output against the
  same program with the stack bypassed.
- **Collection safety**: purge under a concurrent publisher never leaves a manifest referencing a
  swept record — each pack family is marked and swept under its own exclusive lock, a publisher holds
  the lock shared across the pack and the manifest that names it, and a compacted pack is published
  under its new name before the old file is removed. A manifest referencing a missing fragment or
  chunk is a miss that self-heals, never a runtime error. `frag-collect` gates the cycle in a store of
  its own for both families: publish, drop the manifests, purge, republish the same packs under the
  same names.

## 6. Construction order

No intermediate formats and no migration paths: this is development, so the monolithic deps image is
deleted and replaced, not phased out.

1. **Canonical fragment encoding and the binding walk**: one exhaustive site walk, dense ordinals and
   the BLAKE3 fragment id (`vm::ir::frag`), the binding projection and its inverse
   (`image::manifest`), and the probe that prices the design (`MIRVM_FRAG_STATS`, `frag-share`).
   Implemented, and measured before the store was designed (§5).
2. **The closure manifest over the shared fragment store**: the deps layer became a manifest — header
   material, the module without bodies, one record per function (fragment id + binding table) — with
   `cache/frags/` packs behind it, publish-side dedupe and `cache purge --frags`. Implemented; both
   tracks use the same format and key material as before, so behaviour is unchanged.
3. **Per-crate units as manifests** on the cargoless track: the reserved build path wired as a
   multi-way split with the home rule, symbolic cross-unit edges, `cache/units/`, k by load order.
   Implemented: the split is per home and the home rule is its placement rule; the id-bearing tables
   are canonical (§3.3), so a unit is readable above any stack that satisfies its symbols; manifests
   are content-named (`<unit_key>-<digest>.unit`, the newest three kept per unit) so a grown unit adds
   a digest instead of overwriting the one a running program pinned; `cache purge --units` covers the
   family. Gated by the §5 equivalence gate, the `deps-image` gate and the `unit-share` gate.
4. **Collection**: mark-sweep from current-generation manifests, the `--units` flag, the shared/
   exclusive lock discipline of §3.4, and the live/dead accounting `cache status` reports.
   Implemented: `image::collect` marks a fragment live exactly while a current-generation manifest
   names it, `store::frags::sweep` drops or compacts packs against that mark under the family's
   exclusive lock, the unit manifests are a generational family so a manifest another build wrote is
   pruned, and `cache status` reports the live/dead split — the `frag-collect` gate walks one session
   through publish, drop, purge and republish, for both pack families.
5. Extensions, each behind its own measurement: frozen-region chunk dedup; the L2 entry as a
   fragment manifest; heat-order-driven pack layout.
   The first measurement is taken. A clean `frag-sharing` run reports the delta — the program's own
   lowered bodies, which is exactly what an L2 entry stores inline today — as 196 bodies → 172
   fragments, 19 281 B canonical beside 6 120 B of frozen data (24.1% of its unit). Editing the
   program (one new function, one changed caller) keeps **171 of those 172 fragments, 99.4% retention
   and 98.3% of the union**, so an L2 entry written as a manifest would add one new fragment and reuse
   the rest of the previous entry's bodies instead of carrying its own copy: the trigger fired, and the
   extension is implemented. A program entry is now the same manifest of fragments the other two layers
   use — bodies in `cache/frags`, its frozen region chunked in `cache/frozen`, id-bearing tables and
   asm recipes behind the entry's header — and collection marks the fragments an entry names, so an L2
   entry keeps its bodies alive
   exactly like the closure and unit manifests do. Measured after the change: a clean `frag-sharing`
   run leaves 172 records and 19 281 B in `cache/frags`, the same 172 fragments and bytes the probe
   priced before it, and a cold and a warm run agree on the guest's output.
   The frozen region is chunk-addressed too, and its trigger is measured. A region is cut into 4 KiB
   chunks under their own BLAKE3 addresses in `cache/frozen`, the manifest carries the addresses and
   the region's length, and `MIRVM_A2_DEBUG` prints the region's size, its chunk count and what the
   write had to store. Two revisions of one program share most of the region: `frag-sharing`'s delta
   (6 120 B, 2 chunks) is byte-identical across an edit that only changes a body and keeps 1 of 2
   chunks when the edit adds a function, and a program whose region is 71 720 B (18 chunks, a 64 KiB
   static table) keeps 18 of 18 and 16 of 18 in the same two cases. The share therefore depends on how
   much of the region changed and on where the change lands, not on the region being rewritten: 50 to
   100% of it survives a revision, so the extension pays. What is stored is the chunks: a cold
   `frag-sharing` run leaves 3 chunks (6 113 B) in `cache/frozen`, a warm run loads the entry and the
   closure from them and produces the same guest output, and a purge with the manifests gone empties
   the family.
   Heat-order-driven pack layout is measured and closed. A fragment is reached by binary search plus
   one seek, so record order buys nothing for random access, and the only reader that walks a layer in
   heat order is the lazy decode worker of a package's function table — whose input is not paged at
   all: `pack::read` copies the whole package into one `Arc<[u8]>` before the worker starts, so no
   order of the walk is a page-in. Measured on a real package (`frag-sharing`: 9 583 bodies, 2 808 000 B
   of postcard, 293 B average): the FUNCS section spans 687 pages and one page holds 14 bodies, and the
   predicted walk decodes the whole learned order, so it touches that same page set whatever the record
   order is. A reordering could therefore only shrink the *span* of a partial walk, by at most the
   factor the bodies-per-page ratio bounds (14 here), over a postcard parse of bytes already in RAM.
   The order it would be driven by also cannot key the layout: the heat file is keyed by the FUNCS section's own
   hash (`pack::read`), so a reordered section looks up an empty key — the learned order that would
   justify reordering is the one the reordering invalidates.

## 7. Open items

- Residue stays in the delta (v1) in three cases: an instance that mentions the local crate, one that
  spans units no closure covers, and one whose mangled name carries this program as its instantiating
  crate (§3.5). The last is measured at 2 of memchr's 218 bodies; it is the price of the loaded layer
  being immutable, and it grows only with how much rustc chooses to instantiate locally.
- The fourth case is measured and still open: instances that name no unit at all — std residue the base
  lacks, which every closure covers — are 227 of the 288 image bodies in the `serde_json` fixture (196
  fragments) and 400 of 400 in a one-dependency closure. They land in the first unit whose closure
  covers them, which costs unit-manifest size rather than sharing (the fragments are shared either
  way). A base-owned home for them — the adaptive-base direction of open-issues D9 — would take them
  out of every unit manifest, and the same split machinery hosts either choice, so the decision is left
  to this ledger.
- Binding-walk cost on the warm path is unmeasured; if it erodes the L2 gate, the counter-move is
  caching bound bodies in the L2 entry (space traded back for time, per program).
- The store grows only from cold sessions (§3.5): a session that loaded any layer publishes nothing,
  because a home lowered beside a loaded layer is laid out differently from the manifests written
  above it. Growing it in place — lowering exactly the units whose manifest missed and publishing
  those above the loaded ones — needs the whole placement rule to be independent of the load set, of
  which the stable home rule is the first half; it is deferred until stored units per cold run is
  measured and too low, and the store's coverage per program is the number to watch.
- open-issues G5 asks whether cross-project sharing is needed at all and whether tainted images
  should become project-local; this RFC is the "yes, and finer" answer to the first question and
  leaves the second untouched. Deciding this RFC closes that branch of G5.
- The store's layout is the register in `src/store/mod.rs`: `cache/frags`, `cache/frozen` and
  `cache/units` are present, with `cache purge --frags|--frozen|--units`. The parent document's
  store-layout table follows the code, per the "current code wins" rule.
