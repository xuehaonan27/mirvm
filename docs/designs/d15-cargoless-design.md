# D15: Dropping cargo — own dependency resolution and compilation scheduling

> Status: Implemented · Scope: mirvm resolves manifests, versions, topology, build.rs and proc-macros itself, so `mirvm run` spawns zero cargo processes end to end; the Cargo compat track is retained as an explicit dual-track judge.

## 1. Contract

mirvm is the compilation scheduler for every dep crate: it parses manifests itself, solves versions itself, orders the topology itself, runs build.rs itself and compiles proc-macros itself. Bin crates still lower through the existing `MirvmCallbacks` channel; `mirvm run` spawns zero cargo processes end to end.

Solve semantics were finalized 2026-07-27 (P1) and are implemented in `resolve.rs`. Every rule below was established empirically against the pinned Cargo; the evidence chain is in git history.

- **C1 resolve graph vs build graph.** Cargo.lock's resolve graph is the union across all platforms (never-true `cfg(any())` edges included; windows-sys enters the lock on a Linux host). The build graph is filtered by evaluating the host `rustc --print cfg`. Version solving, feature activation and lock dependency lines use the resolve graph; compilation units use the build graph (`unify_features`'s `include_weak` two-state).
- **C2 multi-version fork (lazy-bucket).** Same-named crates may coexist at semver-incompatible versions (hashbrown 0.14/0.15, syn 1/2/3 in one graph). Package id = (name, bucket): when a dep edge arrives and a common candidate exists within an existing bucket's accumulated interval (the index has a version satisfying both), it merges; otherwise a new bucket opens. pubgrub backtracks independently per bucket. A reachable-set filter clears orphan buckets left by pubgrub backtracking.
- **C3 optional dependency gate.** An optional dependency enters version solving and lock dependency lines iff ① it is activated by (parent package, dependency key) (strong forms: `dep:` / implicit / `x/y`), or ② it is referenced by an enabled feature in weak `?/` form — reference alone puts it in the graph (resolve graph semantics) and features propagate as usual (it cascades with strong activation: rust_decimal std → borsh?/std → borsh std → bytes?/std → bytes enters the lock). The build graph keeps only ①. The gate is keyed on (parent package, dependency key); a global package-name gate would misattribute a same-named dependency activated by package A to package B (zerovec's yoke → litemap's yoke ^0.8).
- **C4 feature unification.** resolver v2/v3 keep normal and build edges separate (one crate carrying different feature sets on the two edges is two compilation units). A feature reference must point at a feature or an optional dependency, otherwise reject loudly. When a feature flag arriving on an edge has no table entry to expand and is itself a non-hidden optional dependency key, it activates that dependency (final cleanup sweep).
- **C5 pre-release exact rule.** A pre-release version is selectable only when named by a comparator in some req of that package whose major/minor/patch are all equal and which carries a pre (`req_to_ranges` is self-written and preserves the lower-bound pre; ark-ff-asm 0.5.0-alpha.0 mis-selection is the evidence).
- **C6 yanked.** The lock accepts yanked versions (same as cargo). A fresh solve skips them and a no-solution is loud.
- **C7 lock shape.** Canonical v3/v4: every dependency line ends with a trailing comma (cargo `--locked` declares any non-canonical lock "needs rewrite" and refuses it). With multiple versions of one name, dependency lines carry a `name version` disambiguation hint. Cargo writes v3 for projects whose minimum Rust version is not above 1.82, and v4 from 1.83.
- **C8 no silent fallback.** Constructs outside the implemented scope are rejected loudly with the construct named; mirvm never silently falls back to cargo.
- **C9 dev-deps are never solved.** mirvm never runs test, stated up front.
- **C10 profile semantics are pinned.** Profile semantics replicate the cargo dev profile: debug-assertions=on, overflow-checks=on. jiff's debug_assert is the precedent — these two flags enter MIR semantics, and a mismatch is differential drift.
- **C11 fingerprint v1.** hash(manifest subtree + lock version set + features + rustflags + build.rs rerun-if output + rustc version + source-tree dep-info). No cargo fingerprint compatibility is needed, but the profile semantics above is pinned.
- **C12 accounted boundaries are not closure.** Approximations and deferrals are declared up front (see §3, §5) and may not be passed off as closed.

## 2. Model

### 2.1 Why drop cargo

1. **Runtime de-toolchaining.** `mirvm run` implicitly depends on a complete cargo plus an online registry, which conflicts with the mode B (`.mirvm` self-contained package distribution) worldview. The three package-format slices have landed; the production-side cargo dependency is the next stake to pull.
2. **Scheduling autonomy.** cargo's fingerprints, scheduling and artifact naming are a black box; mirvm's L2 cache, deps-image and D14 store all read cargo artifacts and reverse-infer intent, and every reverse-inference layer is a potential drift surface (the batch-11 double-manifest drift is the isomorphic lesson).
3. **Institutional removal of detours.** C4's precedent: dep global_asm needed an intervention point during dep compilation, and only a `RUSTC_WRAPPER` bypass provided it; two rounds of detour were rejected. Any design where wanting a hook means parasitizing someone else's scheduling is led by cargo's shape. Own scheduler = hooks for free.

### 2.2 What cargo does for mirvm today

| Responsibility | Who computes it today | Evidence |
|---|---|---|
| manifest parsing (package/deps/features/targets/workspace/profile) | cargo | mirvm makes zero TOML parsing calls |
| version resolution (semver req → concrete version; lock read/write) | cargo (re-solves every time without a lock, G7 on record) | `MIRVM_CARGO_LOCKED` is the only pinning mechanism (cargo_shim.rs:107-115) |
| registry fetching (sparse index, .crate download/verify/unpack) | fully delegated to cargo | zero places in source read the registry |
| feature unification (resolver v2: normal/build edge separation) | cargo | — |
| build.rs full lifecycle (host compile → execute → directive parse → propagate) | cargo | mirvm reads zero lines of OUT_DIR (§7.22 explicitly forbids detours) |
| proc-macro host dylib compilation | cargo (host without `--target`, wrapper passes through) | cargo_shim.rs:198,207-212 |
| per-crate rustc args (--extern closure/-L/-l/--cfg/edition/metadata hash/profile flags) | cargo | bin side passes through in full (agent-178 checklist) |
| fingerprints and incrementality (mtime/dep-info/flags hash) | cargo (the D14 shared target depends on it) | cargo_shim.rs:123-131 |
| bin artifact location and run protocol | cargo runner + fake-binary JSON | cargo_shim.rs:214-222,340-383 |

### 2.3 Existing assets (half already there; not rebuilt)

`run_dep_compiler` (in-process rustc_driver, `-Zno-codegen` + MIR sysroot + DepCallbacks global_asm extraction, cli.rs:493-517); the `MirvmCallbacks` bin lowering channel; ircache L2 (input stamp = sess.file_depinfo + used_crate_source, content-addressed and self-computed); baseimage/depsimage; the D14 unified target store; the self-built sysroot (rustc-build-sysroot, self-managed from P4).

### 2.4 Hand-written scope list (dependency order)

| # | Item | Content |
|---|---|---|
| 1 | manifest model | `Cargo.toml` parsing (package/lib/`[[bin]]`/`[dependencies]`/`[build-dependencies]`/`[features]`/`[profile]`/`[workspace]` basic inheritance/target.'cfg()'.dependencies platform evaluation — mirvm's target is always the host triple, so the cfg evaluation surface is limited); frontmatter scripts keep the existing parse_frontmatter (cli.rs:1039-1074) but feed the own model instead of materializing a cargo project |
| 2 | lock read/write | read (v3/v4 format) preferred; write (drop a lock from the self-solved graph for reproducibility) |
| 3 | version resolution | lock present → follow the lock (closed); lock absent → semver solve (decision point ④) |
| 4 | registry access layer | sparse index read (HTTP) + .crate download/sha256 verify/unpack → own store `~/.mirvm/registry/{cache,src}` (decision points ①②) |
| 5 | feature unification | resolver v2 semantics subset — normal deps vs build deps edge separation, optional/`dep:`/weak(`?`), default-features; dev-deps are not solved at all |
| 6 | topological scheduling + per-crate rustc args | edition / `--cfg` features / `--extern` full closure (including proc-macro `.so`) / `-L` / `-l` / crate-name; artifact naming hash uses an own scheme (cargo's `-C metadata` algorithm is unstable and is not chased — cargo has left, internal consistency suffices); args feed the existing `run_dep_compiler` (deps) and the `MirvmCallbacks` session (bin); the fake binary and runner protocol are retired wholesale (E36 cwd semantics is closed on the new path along the way: guest cwd = caller cwd, same as cargo run) |
| 7 | build.rs full lifecycle | largest new responsibility, 151 crates evidence of universality: host real compile (codegen) → execute with cargo-compatible env (CARGO_PKG_*/OUT_DIR/TARGET/HOST/PROFILE/CARGO_CFG_*) → parse `cargo::rustc-link-lib/-search/-cfg/-flags/metadata` directives → propagate (`-l`/`-L`/`--cfg` into dependent rustc args; OUT_DIR/CARGO_PKG_* into the bin session env; DEP_* into downstream build.rs env). build.rs is arbitrary code: its semantics is "execute it", and mirvm executes it too (external tool dependencies such as cc/pkg-config remain, same terms as today) |
| 8 | proc-macro | host compile to dylib (real codegen), `--extern` into dependents; the mechanism is straightforward (the host/target criterion already has a wrapper prototype) |
| 9 | fingerprint | own coarse-grained v1 (C11); no cargo fingerprint compatibility is needed, but profile semantics are pinned to the cargo dev-profile equivalent (C10); opt-level has no semantic effect on `-Zno-codegen` deps (pass it through without misjudging) |
| 10 | config subset | `.cargo/config.toml`'s build.rustflags/target.*.rustflags; source replacement and alternative registry go to P5 as needed |

### 2.5 Module shape

New namespace `src/cargoless/` (plainly: build without cargo):

```
src/cargoless/
  manifest/     # Cargo.toml model + frontmatter integration + cfg platform evaluation
  lockfile.rs   # Cargo.lock read/write
  registry.rs   # sparse index + .crate download/verify/unpack + own store
  resolve/      # version solving + feature unification -> compilation unit graph
  schedule/     # topological sort + fingerprint + per-crate rustc arg computation
  buildrs.rs    # build.rs compile/execute/directive parse/propagate; host proc-macro dylib scheduling
  driver.rs     # mirvm run new path (replaces phase_cargo; compat path retained)
```

New direct dependencies: `toml` (already in the lock at 1.1.2), `semver` (already in the lock at 1.0.28); HTTP/tar per decision point ①.

### 2.6 Decisions (ruled 2026-07-23)

1. **HTTP/unpack** → **pure Rust crates** (ureq + flate2(miniz_oxide) + tar; self-containment takes priority over a minimal dependency tree; the TLS backend lands as ureq's default rustls).
2. **registry store** → **own `~/.mirvm/registry` + read-through reuse of `~/.cargo/registry`** (read-only, no pollution; read-through order = own src → own cache → cargo src → cargo cache → HTTP).
3. **phasing axis** → P1→P5 as in §2.7.
4. **lock-absent solver** → the **`pubgrub` crate** (0.4; principled closure with manageable engineering).

### 2.7 Phase plan and closure contracts

Each phase states the observable boundary it closes to. Things that cannot be closed in principle are stated up front; a detour may not impersonate closure.

| Phase | Scope | Closure contract | Acceptance | Closed |
|---|---|---|---|---|
| P1 groundwork: parsing library + audit tool (not wired into the run path) | scope items 1-5 (manifest/lock/registry/resolve/feature graph) | for all in-repo corpus entries (164 + projects): where a lock is present, the self-solved version set **== lock version set** (the audit tool reconciles item by item); where the lock is absent (frontmatter scripts), after the self-solve writes a lock, `cargo build --locked --offline` accepts it as-is (compatibility counter-proof). build.rs/proc-macro are not in this phase | audit tool all green; unit tests cover the manifest/feature shape matrix | 2026-07-27 |
| P2 full mechanism: scheduling + build.rs + proc-macro (coarse fingerprint v1) | scope items 6-10; coarse fingerprint (build.rs reruns every time, rerun-if refinement belongs to P3) | **corpus smoke tier, all 24 entries** (including build.rs heavy hitters blake3/crossbeam/mimalloc/libgit2/rusqlite/mlua/tree_sitter) run with zero cargo processes, stdout/stderr/exit byte-identical to the cargo path (new differential axis: self path vs cargo path self-consistency + the original three-dimension check green as usual). Constructs outside the subset (complex workspace shapes/alt registry) are **rejected loudly with the construct named**, never silently falling back to cargo | `MIRVM_DEPS=self ./tests/run.sh suite corpus.run --tier smoke` 24/24 | 2026-07-27 (corpus smoke 24 dual-track byte-identical 24/24) |
| P3 migration: fingerprint refinement + full corpus + dual-track gate | rerun-if fine-grained incrementality (build.rs not-rerun semantics aligned); full corpus tier migration; gate gains a DEPS axis (self full run + cargo compat path retained for smoke) | `MIRVM_DEPS=self ./tests/run.sh gate` all green; every entry's self path byte-identical to its cargo path | gate DEPS=self all green; cold/hot L2 behavior invariants green as usual | 2026-07-28 (corpus full 138 pass 1 p5 0 fail + gate DEPS=self dual-track green, git history) |
| P4 sysroot self-management + cargo exit (default flip) | the sysroot build switches to D15 self-managed scheduling (rust-src fully local sources + a fixed ~27 crate graph; incidentally cutting an accidental crates.io dependency — agent-177 evidence: `.d` referenced `~/.cargo/registry`, changed to rust-src `library/vendor/`); `MIRVM_DEPS` default flips to self, the cargo path is retained as explicit compat (`MIRVM_DEPS=cargo`) and kept long-term per §7.37 (explicit user fallback and behavior differential, not silent rescue) | sysroot build zero cargo processes; `mirvm run` (project/script) default path zero cargo throughout; compat path gate smoke retained | after purge --sysroot, cold build all green; gate dual-track green | 2026-07-29 (sysroot self-managed + default flipped to self + compat dual-track decided) |
| P5 complex semantics as needed (resolver 2/3 workspace done) | resolver 1, complex member glob/nested workspace/workspace lints, [patch]/[replace], alt registry, source replacement — **filed item by item as corpus expansion demands**; no commitment to "cargo full semantics" (an up-front non-closure surface; on encounter reject loudly, register, and expand as needed) | on encounter, reject loudly with the construct named; no silent fallback (C8) | filed item by item as needed | resolver 2/3 common workspace, rust-version-aware selection and Git dependencies completed 2026-08-08 to 08-10 (§7.35/§7.36/§7.38); alternative registry/Cargo config, common source replacement/patch/replace and pack self also completed (§7.39-§7.41); resolver 1, complex member glob/package spec and workspace lints completed with D17 remaining items (§7.44); remaining items filed per actual need |

## 3. Boundaries

- **semver solving** — lock present = closed (read the lock); lock absent is decision point ④.
- **feature resolver v2 corners** (union rules when one crate has different feature sets on normal/build edges) — implemented per cargo book semantics with full-corpus empirical backing; exotic shapes (weak dep feature chains) are covered by a unit test matrix, not claimed exhaustive.
- **build.rs arbitrariness** — it can reach the network and write arbitrary paths; same terms as cargo (no sandbox, the semantics is execution). Sandboxing belongs to the D10 product surface and is not mixed into D15.
- **profile semantics** — debug-assertions/overflow-checks enter MIR semantics (jiff precedent) and are hard-pinned to the dev-profile equivalent flags from P2 onward; opt-level has no semantic effect on `-Zno-codegen` deps and is passed through without misjudging.
- **cargo version behavior differences** — the compat dual track only backs the pinned toolchain's cargo, same as today.
- **rust-version-aware version preference** was originally a P1-period boundary and was completed 2026-08-10: resolver 3, fallback/allow, workspace minimum version, `--ignore-rust-version` and lock v3/v4 are all implemented against fixed-Cargo evidence (git history).
- **req meets a new semver operator** unknown to this repo's pinned version → falls back to `Ranges::from_req` (the pre is lost; loudly accounted in code).
- **Git source replacement and similar remaining boundaries** are rejected loudly (see §5).
- **dev-deps and test targets** are never solved, and mirvm never runs test; cross-target and full Cargo config coverage are not claimed here.

## 4. Verification

Every finalized solve rule was established empirically against the pinned Cargo; the evidence chain is in git history. The differential tracks are:

- **P1 audit chain.** The audit tool walks all in-repo corpus entries (164 + projects). Where a lock is present: the self-solved version set must equal the lock version set, reconciled item by item. Where the lock is absent (frontmatter scripts): after the self-solve writes a lock, `cargo build --locked --offline` must accept it as-is. Unit tests cover the manifest/feature shape matrix.
- **P2 track.** `MIRVM_DEPS=self ./tests/run.sh suite corpus.run --tier smoke` → 24/24; stdout/stderr/exit byte-identical to the cargo path; the new axis (self path vs cargo path self-consistency) plus the original three-dimension byte-equality check stay green.
- **P3 track.** `MIRVM_DEPS=self ./tests/run.sh gate` all green; every entry's self path byte-identical to its cargo path; cold/hot L2 behavior invariants unchanged. The gate DEPS axis = self full run + cargo compat smoke. Recorded result: corpus full 138 pass 1 p5 0 fail + gate DEPS=self dual-track green.
- **P4 track.** `mirvm run` (project/script) default path zero cargo processes; sysroot build zero cargo processes; after `purge --sysroot`, cold build all green; gate dual-track green; the compat path stays under its own smoke with `MIRVM_DEPS=cargo`.
- **Long-term dual track.** Cargo compat was ruled on 2026-08-10 to be retained long-term; default self and explicit Cargo fallback keep running the dual-track differential (§7.37).

## 5. Open items

- **P5 residual semantics** are filed item by item only when corpus expansion demands; the up-front non-closure surface is resolver 1, complex member glob/nested workspace/workspace lints, [patch]/[replace], alt registry and source replacement. Reopen trigger: a corpus entry needs one of them. On encounter: reject loudly, register, expand as needed.
- **Already closed on this surface** (kept for traceability, no further work): resolver 2/3 common workspace, rust-version-aware selection and Git dependencies (2026-08-08 to 08-10, §7.35/§7.36/§7.38); alternative registry/Cargo config, common source replacement (including registry/local/directory), patch/replace and pack self (§7.39-§7.41); resolver 1, complex member glob/package spec and workspace lints (§7.44, with D17 remaining items).
- **Undetermined semver operator fallback.** When a req hits a semver operator unknown to this repo's pinned version, `Ranges::from_req` is used and the pre is lost. Reopen trigger: the pinned version gains that operator; until then the loss is loudly accounted in code.
- **Fresh-solve skips yanked versions** and a no-solution is loud (C6); the lock path keeps accepting them like cargo. Reopen trigger: a corpus entry requires selecting a yanked version during a fresh solve.
- **Git source replacement** and similar remaining boundaries are rejected loudly. Reopen trigger: corpus expansion demands them.
- **Fingerprint v1 is coarse** (build.rs reruns every time; rerun-if refinement lands with P3's semantics). Reopen trigger: a differential shows a stale artifact that cargo would have rebuilt, or a rebuild cost that P3's incrementality was meant to remove.
- **Project record:** [open-issues.md D15](../open-issues.md). The motivating precedent is C4 (two rounds of detour rejected — the situation "eating cargo's artifacts forces a detour" is to be eliminated institutionally).
