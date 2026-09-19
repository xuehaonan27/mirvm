# D15: Dropping cargo — own dependency resolution and compilation scheduling

> Status: Implemented · Scope: mirvm resolves manifests, versions, topology, build.rs and proc-macros
> itself, so `mirvm run` spawns zero cargo processes end to end. The Cargo compat track is retained
> as an explicit dual-track judge.

## 1. Contract

mirvm is the compilation scheduler for every dependency crate: it parses manifests, solves versions,
orders the topology, runs build.rs and compiles proc-macros itself. Bin crates still lower through the
existing `MirvmCallbacks` channel. Every rule below was established empirically against the pinned
Cargo.

- **C1 resolve graph vs build graph.** Cargo.lock's resolve graph is the union across all platforms,
  including never-true `cfg(any())` edges, so windows-sys enters the lock on a Linux host. The build
  graph is filtered by evaluating the host `rustc --print cfg`. Version solving, feature activation
  and lock dependency lines use the resolve graph; compilation units use the build graph.
- **C2 multi-version fork (lazy bucket).** Same-named crates may coexist at semver-incompatible
  versions (hashbrown 0.14/0.15, syn 1/2/3 in one graph). A package id is (name, bucket): when a
  dependency edge arrives and the index holds a version satisfying both the edge and an existing
  bucket's accumulated interval, the buckets merge; otherwise a new bucket opens. Backtracking is
  independent per bucket, and a reachable-set filter clears orphan buckets.
- **C3 optional dependency gate.** An optional dependency enters version solving and the lock iff it
  is activated by (parent package, dependency key) in a strong form (`dep:`, implicit, `x/y`), or it
  is referenced by an enabled feature in weak `?/` form — a reference alone puts it in the resolve
  graph, and features then propagate as usual (rust_decimal std → borsh?/std → borsh std →
  bytes?/std → bytes enters the lock). The build graph keeps only the strong form. The gate is keyed
  on (parent package, dependency key), because a global package-name gate would misattribute a
  same-named dependency activated by another package.
- **C4 feature unification.** Resolver v2/v3 keep normal and build edges separate, so one crate
  carrying different feature sets on the two edges is two compilation units. A feature reference must
  point at a feature or an optional dependency, and is otherwise rejected loudly. When a feature flag
  arriving on an edge has no table entry to expand and is itself a non-hidden optional dependency key,
  it activates that dependency.
- **C5 pre-release exact rule.** A pre-release version is selectable only when named by a comparator in
  some requirement of that package whose major, minor and patch are all equal and which carries a pre.
- **C6 yanked.** The lock accepts yanked versions, as Cargo does; a fresh solve skips them and a
  no-solution is loud.
- **C7 lock shape.** Canonical v3/v4: every dependency line ends with a trailing comma, since Cargo
  `--locked` declares a non-canonical lock "needs rewrite" and refuses it. With several versions of one
  name, dependency lines carry a `name version` disambiguation hint. v3 is written when the minimum
  Rust version is not above 1.82, v4 from 1.83.
- **C8 no silent fallback.** Constructs outside the implemented scope are rejected loudly with the
  construct named; mirvm never silently falls back to Cargo.
- **C9 dev-dependencies are never solved**, and mirvm never runs tests.
- **C10 profile semantics are pinned** to Cargo's dev profile: `debug-assertions=on` and
  `overflow-checks=on`. These two flags enter MIR semantics, so a mismatch is differential drift.
- **C11 fingerprint v1**: hash of the manifest subtree, lock version set, features, rustflags,
  build.rs rerun-if output, rustc version and source-tree dep-info. No Cargo fingerprint compatibility
  is needed.
- **C12 accounted boundaries are not closure.** Approximations and deferrals are declared up front and
  may not be passed off as closed.

## 2. Model

### 2.1 Why drop cargo

- **Runtime de-toolchaining.** `mirvm run` implicitly depending on a complete Cargo plus an online
  registry conflicts with the self-contained `.mirvm` package worldview.
- **Scheduling autonomy.** Cargo's fingerprints, scheduling and artifact naming are a black box;
  mirvm's L2 cache, deps image and dependency store all read Cargo artifacts and reverse-infer intent,
  and every reverse-inference layer is a drift surface.
- **Institutional removal of detours.** Dependency `global_asm` needed an intervention point during
  dependency compilation, and only a `RUSTC_WRAPPER` bypass provided it. Any design in which wanting a
  hook means parasitizing someone else's scheduling is led by Cargo's shape; owning the scheduler
  gives hooks for free.

### 2.2 What Cargo does for mirvm today

Manifest parsing (package, dependencies, features, targets, workspace, profile); version resolution
from a semver requirement to a concrete version, plus lock read and write; registry fetching (sparse
index, `.crate` download, verification and unpack); feature unification with normal/build edge
separation; the whole build.rs lifecycle (host compile, execute, parse directives, propagate); proc-
macro host dylib compilation; per-crate rustc arguments (`--extern` closure, `-L`, `-l`, `--cfg`,
edition, metadata hash, profile flags); fingerprints and incrementality; and bin artifact location
with the run protocol.

### 2.3 Existing assets reused

The in-process dependency compiler (rustc_driver with `-Zno-codegen`, the MIR sysroot and global_asm
extraction); the `MirvmCallbacks` bin lowering channel; the L2 IR cache with its self-computed,
content-addressed input stamp; the base and deps images; the unified target store; and the self-built
sysroot.

### 2.4 Scope, in dependency order

1. **Manifest model** — `Cargo.toml` parsing: package, lib, `[[bin]]`, `[dependencies]`,
   `[build-dependencies]`, `[features]`, `[profile]`, basic `[workspace]` inheritance, and
   `target.'cfg()'.dependencies` platform evaluation, where the cfg surface is limited because mirvm's
   target is always the host triple. Frontmatter scripts feed this model instead of materializing a
   Cargo project.
2. **Lock read and write** — read v3/v4; write a lock from the self-solved graph for reproducibility.
3. **Version resolution** — with a lock, follow it; without one, semver solve.
4. **Registry access** — sparse index over HTTP, `.crate` download, sha256 verification and unpack into
   an own store.
5. **Feature unification** — the resolver v2 subset: normal versus build edge separation, optional and
   `dep:` and weak `?` forms, `default-features`; dev-dependencies are not solved at all.
6. **Topological scheduling and per-crate rustc arguments** — edition, `--cfg` features, the full
   `--extern` closure including proc-macro `.so`, `-L`, `-l`, crate name. Artifact naming uses an own
   hash scheme, since Cargo's `-C metadata` algorithm is unstable and internal consistency suffices.
   The arguments feed the existing dependency compiler and the bin session. Guest cwd equals caller
   cwd, as with `cargo run`.
7. **build.rs full lifecycle** — the largest new responsibility: host real compile with codegen, then
   execute with Cargo-compatible environment (`CARGO_PKG_*`, `OUT_DIR`, `TARGET`, `HOST`, `PROFILE`,
   `CARGO_CFG_*`), parse `cargo::rustc-link-lib/-search/-cfg/-flags/metadata` directives, and
   propagate them (`-l`/`-L`/`--cfg` into dependent rustc arguments, `OUT_DIR`/`CARGO_PKG_*` into the
   bin session, `DEP_*` into downstream build.rs). build.rs is arbitrary code, so its semantics is
   "execute it", and external tool dependencies such as cc and pkg-config remain, on the same terms as
   today.
8. **proc-macro** — host compile to a dylib with real codegen, then `--extern` into dependents.
9. **Fingerprint** — the own coarse v1 above, with profile semantics pinned to the dev-profile
   equivalent; `opt-level` has no semantic effect on `-Zno-codegen` dependencies and is passed through.
10. **Config subset** — `.cargo/config.toml`'s `build.rustflags` and `target.*.rustflags`; source
    replacement and alternative registries only as needed.

### 2.5 Module shape

```text
src/cargoless/
  manifest/     # Cargo.toml model, frontmatter integration, cfg platform evaluation
  lockfile.rs   # Cargo.lock read/write
  registry.rs   # sparse index, .crate download/verify/unpack, own store
  resolve/      # version solving + feature unification -> compilation unit graph
  schedule/     # topological sort, fingerprint, per-crate rustc arguments
  buildrs.rs    # build.rs compile/execute/parse/propagate; host proc-macro dylib scheduling
  driver.rs     # the new `mirvm run` path (replaces phase_cargo; the compat path is retained)
```

New dependencies: `toml` and `semver`, already in the lock, plus an HTTP and tar stack.

### 2.6 Decisions

- **HTTP and unpack**: pure-Rust crates (`ureq` + `flate2`/miniz_oxide + `tar`), with self-containment
  taking priority over a minimal dependency tree; TLS is ureq's default rustls backend.
- **Registry store**: an own `~/.mirvm/registry` with read-through reuse of `~/.cargo/registry`,
  read-only and non-polluting. Read-through order is own src, own cache, Cargo src, Cargo cache, HTTP.
- **Lock-absent solver**: `pubgrub`.
- **Phasing**: P1 to P5 as below.

### 2.7 Phases and closure contracts

Each phase states the observable boundary it closes. What cannot be closed in principle is stated up
front; a detour may not impersonate closure.

- **P1, groundwork** — the parsing library and an audit tool, not wired into the run path; scope items
  1 to 5. Closes when, for every in-repo corpus entry, the self-solved version set equals the lock
  version set wherever a lock exists, and where it does not (frontmatter scripts) the self-solved lock
  is accepted as-is by `cargo build --locked --offline`. build.rs and proc-macros are out of this
  phase.
- **P2, full mechanism** — scheduling, build.rs and proc-macros, with a coarse fingerprint v1 (build.rs
  reruns every time; rerun-if refinement belongs to P3); scope items 6 to 10. Closes when the corpus
  smoke tier, all 24 entries including the build.rs-heavy ones, runs with zero cargo processes and
  byte-identical stdout, stderr and exit code to the cargo path. Constructs outside the subset are
  rejected loudly with the construct named, never silently falling back.
- **P3, migration** — fine-grained rerun-if incrementality, full corpus migration, and a DEPS axis in
  the gate (self full run, cargo compat path retained for smoke). Closes when the self path is
  byte-identical to the cargo path for every entry and the cold/hot L2 invariants stay green.
- **P4, sysroot self-management and the default flip** — the sysroot build switches to self-managed
  scheduling over rust-src's local sources and a fixed crate graph, `MIRVM_DEPS` flips to `self`, and
  the cargo path is retained as the explicit compat fallback and behavior differential. Closes when
  the sysroot build and the default `mirvm run` path spawn zero cargo processes and a cold build after
  `purge --sysroot` is green.
- **P5, complex semantics as needed** — resolver 1, complex member globs, nested workspaces, workspace
  lints, `[patch]`/`[replace]`, alternative registries and source replacement, filed item by item as
  corpus expansion demands. There is no commitment to full Cargo semantics: on encounter a construct
  is rejected loudly, registered, and expanded as needed.

## 3. Boundaries

- **Semver solving**: with a lock it is closed by reading the lock; without one it is the `pubgrub`
  solver.
- **Feature resolver v2 corners**: implemented per the Cargo book with full-corpus empirical backing;
  exotic shapes such as weak dependency feature chains are covered by a unit-test matrix rather than
  claimed exhaustive.
- **build.rs arbitrariness**: it can reach the network and write arbitrary paths, on the same terms as
  Cargo — the semantics is execution, with no sandbox. Sandboxing belongs to the product surface and
  is not mixed in here.
- **Profile semantics**: debug-assertions and overflow-checks enter MIR semantics and are hard-pinned
  to the dev-profile equivalent; `opt-level` is passed through without misjudging.
- **Cargo version differences**: the compat dual track backs only the pinned toolchain's Cargo.
- **rust-version-aware version preference** is implemented for resolver 3, fallback and `allow`,
  workspace minimum version, `--ignore-rust-version`, and lock v3/v4.
- **An unknown semver operator** falls back to `Ranges::from_req`, losing the pre; the loss is
  accounted loudly in code.
- **Git source replacement** and similar residual boundaries are rejected loudly.
- **dev-dependencies and test targets** are never solved, and mirvm never runs tests; cross-target
  coverage and full Cargo config coverage are not claimed here.

## 4. Verification

The differential tracks, all run through `make`:

- **P1 audit chain** — the audit tool walks every in-repo corpus entry. Where a lock is present, the
  self-solved version set must equal the lock version set, reconciled item by item. Where it is absent
  (frontmatter scripts), the self-solved lock must be accepted as-is by
  `cargo build --locked --offline`. Unit tests cover the manifest and feature shape matrix.
- **P2 track** — `MIRVM_DEPS=self make suite S=corpus.run ARGS="--tier smoke"`: 24/24 with stdout,
  stderr and exit code byte-identical to the cargo path, keeping both the self-versus-cargo
  self-consistency axis and the original three-dimension byte equality.
- **P3 track** — `MIRVM_DEPS=self make gate`: all green, every entry's self path byte-identical to its
  cargo path, cold and hot L2 invariants unchanged.
- **P4 track** — the default `mirvm run` path and the sysroot build spawn zero cargo processes; a cold
  build after `purge --sysroot` is green; the compat path stays under its own smoke with
  `MIRVM_DEPS=cargo`.
- **Long-term dual track** — Cargo compat is retained permanently: the default self path and the
  explicit Cargo fallback keep running the dual-track differential.

## 5. Open items

- **P5 residual semantics** — resolver 1, complex member globs, nested workspaces, workspace lints,
  `[patch]`/`[replace]`, alternative registries and source replacement. Reopens when a corpus entry
  needs one; on encounter, reject loudly, register, and expand as needed.
- **Unknown semver operator fallback** — a requirement hitting an operator unknown to the pinned
  semver loses its pre. Reopens when the pinned version gains that operator.
- **Yanked versions** are skipped by a fresh solve, with a loud no-solution, and accepted from a lock
  as Cargo does. Reopens if a corpus entry needs a yanked version selected during a fresh solve.
- **Git source replacement** and similar boundaries are rejected loudly. Reopens on corpus demand.
- **Fingerprint v1 is coarse**, with build.rs rerunning every time. Reopens if a differential shows a
  stale artifact Cargo would have rebuilt, or a rebuild cost that incrementality was meant to remove.
