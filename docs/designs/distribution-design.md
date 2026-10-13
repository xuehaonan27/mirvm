# Distribution and Product Surface: Load Ingestion, Cache Layering, Packaging and Release

> Status: Decided RFC · Scope: how mirvm ingests a load (single file, script frontmatter, Cargo
> project), how its caches are layered, and how it would be packaged and released. Siblings:
> [modeb-mirvmar-design.md](modeb-mirvmar-design.md) (mode B package format, which names this
> document as its parent), [d15-cargoless-design.md](d15-cargoless-design.md) (resolution and compile
> scheduling without Cargo), [mirvm-test-cargoless-contract.md](mirvm-test-cargoless-contract.md)
> (`mirvm test`), [concurrency-arch.md](concurrency-arch.md) (engine and thread model).

## 1. Contract

- **Unified entry point.** One `mirvm` binary plus subcommands (`run`, later `pack`, …). `mirvmc` is
  at most a hard-link alias of that binary; a separate binary never appears, because the
  `rustc_private` link is huge and two copies are pure waste.
- **A custom package format, built in two layers.** "Stable MIR as the format" is rejected: Stable
  MIR is an in-process API with no on-disk stability guarantee. The layers are the polymorphic layer
  (approximately rmeta already) and the monomorphic layer (post-mono engine IR). Build the L2
  monomorphic engine-IR cache first, for this machine; the mode B `.mirvm` package is then that cache
  made portable, with a version header, checksum and relocation section. Freezing the external format
  waits until the IR stabilizes — no external promise is frozen while `program.rs` is still gaining
  statement kinds.
- **Cache layering.** The L2 key is mirvm build id + sysroot hash + crate-graph content hash; a
  mismatch rebuilds the whole package with no partial reuse, in the AppCDS manner. Because the
  toolchain is self-pinned and the format is ours, a rebuild is always possible and that brittleness
  is harmless.
- **Dependency metadata and execution products have separate roles.** Target dependencies use
  `-Zalways-encode-mir` and `-Zno-codegen`; their rlibs carry the compiler inputs needed to instantiate
  code. Installation must also prepare MIRVM bytecode. Host build scripts and proc-macros still
  require native compilation.
- **The toolchain is bundled, not discovered.** Runtime discovery of a local toolchain is
  architecturally impossible — `rustc_private` is nightly-only, the binary is ABI-locked to its
  toolchain, and rmeta is unstable across versions — so the right analogy is a JDK that bundles
  `javac`. Release in two phases: first miri-style, one build per toolchain, where "switching
  toolchain" means switching to the matching mirvm build; later a self-contained tarball bundling
  rustc, rust-src and Cargo with no rustup dependency for the user. `--sysroot` keeps its boundary: it
  controls where guest std comes from, never who the compiler is. Guest code is not subject to the
  nightly restriction.
- **Preparation belongs to distribution and installation.** Supported releases provide the MIR
  sysroot and a matching pre-lowered std base. Project installation prepares the selected target's
  concrete dependency instances and execution image before its first run. The preparation boundary
  and remaining generic work are specified in §2.7.
- **Release scope.** Local execution caches and mode B packages supply artifact machinery, but do
  not themselves deliver an installed runtime. Release production, artifact import and installation
  lifetime must satisfy §2.7. Kit naming remains open, with MDK or "mirvm toolkit" as candidates;
  MRsDK is rejected.

## 2. Model

### 2.1 Three run forms

1. `mirvm run <file.rs>`, pure single file: the zero-cargo fast path — the in-process rustc front end
   compiles and runs directly.
2. `mirvm run <file.rs>` with frontmatter: RFC 3503 syntax (shebang tolerance, `---`/`---cargo`
   fences, body line numbers preserved), materialized into a cached Cargo project, then the Cargo
   channel.
3. `mirvm run <dir | Cargo.toml>`: the Cargo channel directly.

### 2.2 The Cargo three-phase mechanism

- **phase_cargo** drives a real `cargo run` and injects `RUSTC_WRAPPER=mirvm`,
  `target.runner=["mirvm","runner"]` and the unified `$MIRVM_HOME/build/target/mirvm`. It forces
  `--target <host>` as the host/target crate separation switch. Resolution, download and fail-fast are
  native Cargo behaviour, so `Cargo.lock` and the `~/.cargo` registry cache are free-ridden.
- **phase_wrapper**: host crates (`build.rs`, proc-macros) compile with the real rustc and really
  execute; target dependencies compile with the real rustc, the MIR sysroot and `-Zalways-encode-mir`
  for an rlib with full MIR; the final bin is **not** compiled — a JSON fake binary plus a stub `.d`
  file prevent a rebuild.
- **phase_runner**: when Cargo "runs" the fake binary, control returns to mirvm, which starts the
  front end in-process with Cargo's original rustc arguments and monomorphizes, lowers and interprets.

Dependency MIR production is parallelized over the crate DAG. On an image miss, the root compiler
session determines concrete instances and lowers the missing execution closure; persisted images
avoid this work on subsequent runs. Installation must drive that session too: `cargo build` alone
only produces dependency metadata and the root runner recipe. The guest execution phase uses no
compiler session.

### 2.3 Sysroot and toolchain pinning

`src/sysroot.rs` rebuilds std from rust-src with the Miri arrangement and `-Zalways-encode-mir`,
cached at `~/.mirvm/data/sysroot-<target>` and keyed by content hash. `build.rs` bakes
`MIRVM_DEFAULT_SYSROOT` and an rpath to that sysroot's `librustc_driver.so`, so the binary is
ABI-locked to the nightly that built it. The wrapper phase ignores the rustc name Cargo passes and
always uses the pinned rustc, because proc-macro dylibs and rmeta must be the same compiler version as
the interpreting session. The layout inside that sysroot has one owner: `sysroot::toolchain` derives
`bin/rustc` and its siblings from the constant, and a consumer names it rather than joining those
paths again.

### 2.4 Store layout

The store root is `$HOME/.mirvm`, relocatable through `MIRVM_HOME`. It is split by **lifetime**,
because that is what decides whether a tree may be deleted:

| sub-root | contract | contents |
|---|---|---|
| `cache/` | derived; safe to delete at any time | `base/`, `deps/`, `ir/` (generational: `build_id` is the first field of every entry), `asm-stubs/`, `global-asm/`, `native-archives/`, `package-native/`, `package-heat/` |
| `data/` | fetched or built once; expensive to lose | `registry/` (crate and git sources, read-through to `~/.cargo`), `sysroot-<host>/` (MIR-rich std) |
| `build/` | project and session space | `scripts/` (frontmatter materialization, keyed by path), `target/mirvm` (Cargo track), `target/cargoless/` (own scheduler), `target/native` (differential builds), `sysroot-build/` |
| `run/` | process scratch | `runtime-native/` (per-Engine copies of required native libraries) and `lower-native/` (private copies of self-produced objects) |

The layout is a register in `src/store/mod.rs`, one line per family: a lifetime class, the directory, the
shape of its entries, and — for a family that a flag names — that flag. `mirvm cache status` and
`mirvm cache purge` iterate that register, so a family is reported and cleanable by construction, and
a directory no family claims is reported rather than silently ignored (an older layout, or a leftover
of an interrupted publish, both show up that way).

Two rules belong to the register rather than to each writer, because every writer needs them:

- **Publish atomically.** A producer fills a staging path next to the target (dot-prefixed, unique
  per process and call) and renames it into place; `publish` moves an existing directory target aside
  first, because `rename(2)` cannot replace a non-empty one. A reader therefore never sees a
  half-written artifact, and a crash leaves one orphan.
- **Record the generation.** A generational entry's first serialized field is the `build_id` that
  wrote it, so a stale generation is recognisable with a zero-decode peek — a full decode would
  restore the entry's frozen region and map memory.

`mirvm cache purge` defaults to stale generations and those orphans; `--deps/--base/--ir` take one
generational family whole, `--scripts` and `--target` one build family, `--all` every class but
`data/` (which keeps the current generation, the part that makes the next run fast), and `--data`
adds `data/` — `--all --data` is a full cold start.

### 2.5 Cache layers

- **L0** — registry and git sources (`~/.cargo`), cross-project, free-ridden from Cargo.
- **L1** — dependency rlib and MIR-rlib plus fingerprints (`~/.mirvm/build/target/...`), machine-wide shared.
- **L1.5** — sysroot, native `.so`, asm stubs, script materialization; all content-hash keyed.
- **L1.7 / L1.8** — per-crate unit manifests and the machine-wide content-addressed fragment store,
  owned by [dep-sharing-design.md](dep-sharing-design.md).
- **L2** — the post-mono engine-IR program cache and bound execution image. A valid bound image
  loads before shared-layer discovery or compiler entry; otherwise the program entry reuses those
  layers. [startup-cache-design.md](startup-cache-design.md) owns their loading contract.
- **L3** — the JIT machine-code cache, owned by [jit-code-cache-design.md](jit-code-cache-design.md):
  relocatable per-function artifacts keyed by fragment id × jit-key, unlocked by the relocation
  choke-point work recorded there.

L2 and the two layers below it that mirror the runtime stack are one module tree: `src/image` holds
the stack itself (`mod.rs`) beside the three persisted layers (`base.rs`, `deps.rs`, `program.rs`), and
the entry mechanism all three share — key, generation, fixed-domain guards — is `src/store/entry.rs`.
Everything else in the store belongs to `src/store`: the family register, publication, and the
`cache status`/`purge` report.

L2 design points:

- Serializability follows from the founding property that the execution phase is tcx-free and the
  bytecode self-contained.
- **Live relink section**: dlopen'd handles, the FnPtr table, string and constant-pool host pointers,
  and thunk factory products do not serialize; they are rebuilt at load time and need an independent
  relocation subsection.
- Against silent wrong values: after load, a mismatched FnPtr or handle must prefer Trap. A dangling
  old value is never allowed.
- Verification is the existing gate: one workload run cold and warm must produce byte-identical
  output, catching "the cache returned old semantics" as a false green. No new harness mechanism.

### 2.6 Measurement boundary

[current-status.md](../current-status.md) owns measured timings; the frontend/base attribution is
explained in [frontend-boundary.md](frontend-boundary.md). Report installation, the first program
with an installed std base, the first run after project installation, and repeated runs separately.
Empty derived caches measure a reconstruction path, not the intended installed-product experience.
Moving work into installation improves first-run latency without reducing total preparation work.

### 2.7 Installed execution products

Release delivery remains unimplemented. Project `install` and `prepare` share a preparation
pipeline on both dependency tracks: they lower the selected root, bind and validate its complete
execution image, publish it and stop before guest entry. The Cargo track uses its runner even when
Cargo's artifacts are already fresh, because the root's MIRVM products may still need preparation.

**Runtime distribution** provides the pinned compiler inputs and pre-lowered std together. Building
the MIR-rich sysroot produces compiler metadata; building the std base translates a canonical
runtime closure into MIRVM bytecode, frozen data and binding recipes. The base is not all possible
std instantiations and contains no portable promise for JIT machine code. Its roots, format and
identity are owned by [startup-cache-design.md](startup-cache-design.md#std-base-loading).

For every advertised target and default semantic profile, a release bundles or fetches a matching
MIR sysroot generation and base artifact. The release manifest binds their content digests to the
MIRVM build, target/ABI and profile. The installed sysroot goes through the managed-generation
owner's verification and publication; copying files into a managed-looking path is insufficient.
Relocation must preserve content identity and reconstruct local receipts. Runtime native bindings
must not depend on the producer's absolute library paths or live process state. Download/import and
cross-installation relocation need validation before the local base file is treated as distributable.

Downloaded std products are immutable installation data, outside disposable execution caches.
Cache-only purging must leave a loadable installed base; it must not make the next supported-default
run lower std again. This requires a registered installed-artifact owner and lookup path. Local base
construction remains the recovery path for development builds, custom sysroots and unshipped
profiles, rather than the default release installation path.

**Project installation** means preparing a script or project with a selected target, resolved
dependencies, features and compiler profile. It resolves/fetches sources, runs necessary host build
scripts and proc-macros, produces MIR metadata, then analyzes the selected root and prepares its
validated execution closure. Reusable dependency products follow
[dep-sharing-design.md](dep-sharing-design.md); instances involving application types remain in the
program delta. Installation binds and validates the complete image, publishes the required products
and records their input identity before reporting success. It does not invoke the program entry.
`install` and `prepare` should share this preparation pipeline on both dependency tracks.

For cacheable programs, the first unchanged run after successful installation must load the bound
image without frontend or lowering work. Changes to source, lockfile, features, target/profile,
build-script inputs or compiler inputs invalidate the affected products under the existing identity
rules. Failure to publish required installation products is an installation failure, even though
ordinary `run` may still execute after a best-effort cache publication failure.

The complete execution image is the required installed product. Symbolic program deltas and
dependency manifests are reusable accelerators: a projection or publication failure there does not
prevent the self-contained image from being published. Cache writers preserve the runnable module
on failure. Compiler warnings deliberately prevent caching so subsequent runs retain diagnostic
behavior; preparation still validates such a program and succeeds without invoking its entry.

Installing a library without a consumer can prepare its non-generic roots and concrete instances
reachable from them. It cannot enumerate future `Vec<UserType>` or `deserialize::<UserType>`
instances. Retain generic MIR metadata and prepare those instances when a consuming project is
installed. The default path prepares concrete bytecode; the opt-in
[shared-generics path](runtime-generics-design.md) prepares owned templates and validated instance
descriptors, with concrete instructions generated when the JIT requests them. Exporting library
templates without a concrete consumer remains preparation work.
Installation must still settle known semantic errors and runtime bindings before guest entry;
deferred specialization must not require a new rustc session. JIT compilation and per-process
native binding remain governed by their runtime contracts.

## 3. Boundaries

- **proc-macros and `build.rs` are always really compiled and really executed.** Every "lower
  everything ahead" assumption stops here; mode B also runs them once at packaging time.
- Stable MIR is not a serialization format.
- Runtime toolchain discovery is impossible.
- The L3 JIT code cache lands only through [jit-code-cache-design.md](jit-code-cache-design.md)'s
  relocation contract; ad-hoc persistence of JIT output stays forbidden.
- Full cross-project sharing of the L1 target dir is not delivered beyond same-workspace cross-bin
  smoke.

## 4. Verification

- **L2 acceptance**: one workload run cold and warm must be byte-identical through the existing gate's
  diff channel, which catches a cache hit returning stale semantics.
- **Installed std**: relocate a release payload into a fresh installation with empty derived
  caches; a supported-default program loads std with no sysroot or base build. Repeat after a
  cache-only purge, and reject mismatched target/profile/build or damaged artifacts.
- **Project installation**: on both dependency tracks, install a cacheable generic-heavy program
  without invoking its entry; the first run must hit its bound image and match native output.
  Source/feature/input edits must invalidate it, and failed publication must fail installation.
- **Gate**: `make test` / `make smoke` / `make gate`, plus `make case C=<id>`; L2 adds an acceptance
  dimension to this gate rather than a parallel harness.
- **Phase timing**: use `MIRVM_TIMING` for the preparation/execution split described in §2.6.
- **Cache management**: `mirvm cache status|purge`, with the root at `$HOME/.mirvm`. `MIRVM_BUILD_ID`
  keys every layer, so a rebuild invalidates base, deps and L2 images by design.
- **Baseline reproduction**: release builds and explicitly recorded installation/cache states;
  measure ordinary wall time separately from instrumented profiles.

## 5. Open items

[open-issues.md](../open-issues.md) owns release delivery, project installation and compatibility
work. Format stability and cache invalidation remain constrained by exact build identity and the
validation contracts above.
