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

A **fragment** is one `FuncBody` in canonical form: the `name` field is lifted into the manifest, and
each of the five reference sites is rewritten to a dense ordinal in order of first appearance.
`CallForeign`/GOT names are stable C symbols and stay inline. Fragment bytes = one encoding-version
byte + postcard of the canonical body; fragment id = BLAKE3-256 of those bytes. Version
disambiguators, frozen-layout offsets and id assignments — everything that differs between tokio
1.x.y and 1.x.z when the code does not — thereby leaves the fragment and moves into the binding.

The **manifest** carries, per function in symbol-sorted order (which makes the manifest digest
deterministic; the base image's byte-determinism gate is the precedent): the v0 symbol, the fragment
id, and the binding table that assigns each ordinal one of: unit-local function/TLS/asm index ·
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
  offsets shifted) without stealing the manifest it would need to rebuild cheaply. The Cargo track
  writes the same manifest format with its closure-level key material, so one format serves both
  tracks and only the keying discipline differs.
- `cache/frags/` — append-only pack files, each an atomic publish of one session's new fragments
  with a trailing index; a store-level index memo is a rebuildable convenience. Readers mmap packs
  and decode lazily — `FuncTable::from_bytes` (per-function blob bounds + expected hash + heat-order
  prefetch) is the existing mechanism, extended to multiple maps and to running the binding walk
  after postcard decode. Writers dedupe against the index before appending; a lost race stores a
  duplicate that the next repack folds.

Fragments carry no generation: they are reached only through manifests, manifests are generational,
and collection is **mark-sweep from current-generation manifests** — refcounts are rejected as
crash-fragile. `cache purge` marks live fragment ids, rewrites packs whose live ratio is low, and
deletes prunable manifests (stale generations as today; superseded digests per unit_key beyond a
small keep-count). Sweep takes an exclusive lock on the family; publishers take it shared. If two
mirvm builds happen to produce byte-identical fragment bytes, the hash collides *correctly* — content
addressing is self-consistent — so cross-build sharing is free when the encoding is unchanged, and
never wrong when it is not.

### 3.5 Load path

Pre-session, per crate in topological order: compute `unit_key` (rlib stamp — no tcx), pick the
newest valid manifest, validate exact-equality header material and generation, mark the rest for
lowering. The stack becomes `[base, unit_1 … unit_n]` with k by load order; union lookups,
cumulative offsets, fingerprint prefix-truncation and the key chain are the existing `ImageStack`
mechanics. During the session the multi-way split lowers only missing homes and publishes them;
absorb at the end is unchanged. The L2 chain grows from `base ⊕ aggregate` to `base ⊕ (unit_key,
manifest_digest)*`, same mechanism.

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
- **Manifest overhead** duplicates per unit: symbols, bindings, frozen bytes. Symbols and frozen
  bytes exist per unit today; the net new cost is the binding tables, bought back many times over by
  fragment sharing.

## 4. Boundaries

- No cross-`build_id` compatibility is promised. Fragments may coincide across builds (§3.4); that is
  a bonus, not a contract.
- The L3 JIT code cache is not this document's topic: [jit-code-cache-design.md](jit-code-cache-design.md)
  owns it, keyed by this document's fragment ids.
- rlibs are rustc's artifacts and are not deduplicated here; the frozen region is stored whole per
  unit in v1 — chunk-level dedup of frozen bytes is an extension with a measurement trigger, not a
  commitment.
- The base image and L2 program entries do not move into the fragment store in v1. The L2 entry as a
  manifest of fragments (making per-edit IR caching incremental) is the highest-value extension and
  is deferred until the store exists.
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
- **Equivalence gate**: cold full lowering versus a warm unit stack must produce byte-identical guest
  output through the existing diff channel, per the L2 acceptance rule.
- **Sharing acceptance**: two projects with overlapping lockfiles produce one copy of every shared
  unit; two adjacent versions of the fixture crate keep ≥ 99% of their fragments (the `frag-share`
  gate); `cache status` reports live/dead fragment bytes and the dedup factor.
- **Collection safety**: purge under a concurrent publisher never leaves a manifest referencing a
  swept fragment (lock discipline §3.4); a manifest referencing a missing fragment is a miss that
  self-heals, never a runtime error.

## 6. Construction order

No intermediate formats and no migration paths: this is development, so the monolithic deps image is
deleted and replaced, not phased out.

1. Measurement: the dedup probe (`MIRVM_FRAG_STATS`, following the purity-stats pattern) plus
   unit-size and frozen-share numbers on real fixtures; this prices the §7 extensions and fixes the
   acceptance ratios of §5.
2. **Canonical fragment encoding and the binding walk** — the keystone this design shares with
   [jit-code-cache-design.md](jit-code-cache-design.md); fragment ids exist from here on, before and
   independent of the store.
3. **The fragment store**: `cache/frags/` packs, BLAKE3 ids, mark-sweep purge.
4. **Per-crate units as manifests** on the cargoless track: the reserved build path wired as a
   multi-way split with the home rule, symbolic cross-unit edges, `cache/units/`, k by load order;
   the monolithic deps image is removed in the same change, gated by the §5 equivalence gate.
5. **The Cargo track's closure manifest**: same manifest format, closure-level key, observable
   behaviour untouched.
6. Extensions, each behind its own measurement: frozen-region chunk dedup; the L2 entry as a
   fragment manifest; heat-order-driven pack layout.

## 7. Open items

- The residue rule (v1: delta) versus synthesized join units for hot sibling-spanning instances —
  decide on purity-ledger data, not up front.
- Binding-walk cost on the warm path is unmeasured; if it erodes the L2 gate, the counter-move is
  caching bound bodies in the L2 entry (space traded back for time, per program).
- open-issues G5 asks whether cross-project sharing is needed at all and whether tainted images
  should become project-local; this RFC is the "yes, and finer" answer to the first question and
  leaves the second untouched. Deciding this RFC closes that branch of G5.
- The store register rows, purge flags (`--units`, `--frags`) and `options.rs` entries land with
  their implementing steps; the parent document's store-layout table follows the code, per the
  "current code wins" rule.
