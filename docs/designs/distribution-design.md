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
  waits until the IR stabilizes — no external promise is frozen while `ir.rs` is still gaining
  statement kinds.
- **Cache layering.** The L2 key is mirvm build id + sysroot hash + crate-graph content hash; a
  mismatch rebuilds the whole package with no partial reuse, in the AppCDS manner. Because the
  toolchain is self-pinned and the format is ours, a rebuild is always possible and that brittleness
  is harmless.
- **Trim codegen from dependency builds.** Target dependencies move to the check shape (metadata
  only), cutting rlib machine code compiled for nothing. Landing must establish empirically how far
  Cargo tolerates its artifact-existence checks. This saves the first build only, not every run, so it
  ranks below L2.
- **The toolchain is bundled, not discovered.** Runtime discovery of a local toolchain is
  architecturally impossible — `rustc_private` is nightly-only, the binary is ABI-locked to its
  toolchain, and rmeta is unstable across versions — so the right analogy is a JDK that bundles
  `javac`. Release in two phases: first miri-style, one build per toolchain, where "switching
  toolchain" means switching to the matching mirvm build; later a self-contained tarball bundling
  rustc, rust-src and Cargo with no rustup dependency for the user. `--sysroot` keeps its boundary: it
  controls where guest std comes from, never who the compiler is. Guest code is not subject to the
  nightly restriction.
- **Construction order.** Phase timing on the existing `--vm-stats` instrumentation, then the L2
  engine-IR cache, then dependency codegen trimming, then the mode B `.mirvm` package with `mirvm
  pack` after the JIT work, then release form and naming. Naming: MRsDK is rejected; "kit" naming
  waits for a mode B physical artifact, with MDK or "mirvm toolkit" the candidates then.

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
  `target.runner=["mirvm","runner"]` and the unified `$MIRVM_HOME/target/mirvm`. It forces
  `--target <host>` as the host/target crate separation switch. Resolution, download and fail-fast are
  native Cargo behaviour, so `Cargo.lock` and the `~/.cargo` registry cache are free-ridden.
- **phase_wrapper**: host crates (`build.rs`, proc-macros) compile with the real rustc and really
  execute; target dependencies compile with the real rustc, the MIR sysroot and `-Zalways-encode-mir`
  for an rlib with full MIR; the final bin is **not** compiled — a JSON fake binary plus a stub `.d`
  file prevent a rebuild.
- **phase_runner**: when Cargo "runs" the fake binary, control returns to mirvm, which starts the
  front end in-process with Cargo's original rustc arguments and monomorphizes, lowers and interprets.

The division of labour: dependency MIR-ification is front-loaded and parallel in Cargo (crate DAG,
`-j`, pipelining, fingerprints), while dependency lower-to-bytecode happens in the load phase of every
run, driven on demand by monomorphization. The guest runtime compiles no dependencies.

### 2.3 Sysroot and toolchain pinning

`src/sysroot.rs` rebuilds std from rust-src with the Miri arrangement and `-Zalways-encode-mir`,
cached at `~/.mirvm/sysroot-<target>` and keyed by content hash. `build.rs` bakes
`MIRVM_DEFAULT_SYSROOT` and an rpath to that sysroot's `librustc_driver.so`, so the binary is
ABI-locked to the nightly that built it. The wrapper phase ignores the rustc name Cargo passes and
always uses the pinned rustc, because proc-macro dylibs and rmeta must be the same compiler version as
the interpreting session.

### 2.4 Caches

- `~/.cargo` — registry and git sources, cross-project, keyed natively by Cargo.
- `~/.mirvm/target/{mirvm,native}` — dependency rlib and MIR-rlib plus fingerprints, machine-wide
  shared, keyed by Cargo fingerprints (the D14 near-term slice).
- `~/.mirvm/sysroot-<target>` — MIR-rich std, keyed by content hash.
- `~/.mirvm/native-archives`, `~/.mirvm/asm-stubs`, `~/.mirvm/global-asm` — `.a`→`.so` products and asm
  factory `.so`, keyed by content hash.
- `~/.mirvm/scripts` — frontmatter script materialization projects, keyed by path plus content hash.

The cache root is `$HOME/.mirvm`, relocatable through `MIRVM_HOME`, with `mirvm cache
status|purge` for stale-generation GC, whole-family purge, scripts and full clear. Stale generations
are decided by the first field of the build id.

### 2.5 Cache layers

- **L0** — registry and git sources (`~/.cargo`), cross-project, free-ridden from Cargo.
- **L1** — dependency rlib and MIR-rlib plus fingerprints (`~/.mirvm/target/...`), machine-wide shared.
- **L1.5** — sysroot, native `.so`, asm stubs, script materialization; all content-hash keyed.
- **L2** — the post-mono engine-IR whole-package cache. This is the highest-value gap: it turns the
  load cost paid on every run into "deserialize and run".
- **L3** — the JIT code cache, forbidden until the CFI/PLT/relocation work lands.

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

### 2.6 Measured baseline

Release build, warm cache, on an EPYC 7773X:

- `tests/scripts/args_env.rs`, std-only: **0.40s** — front end, monomorphization, lowering and run over
  a std-only graph.
- `tests/scripts/ecosystem.rs`, serde + serde_json + rand + regex: **3.07s**, reproducible, with all
  dependency rmeta cached. Those 3s are **paid on every run**: leaf front end, pulling dependency MIR,
  whole-graph monomorphization, lowering and run.

So the cold-start pain is neither dependency resolution (one-time) nor dependency compilation
(one-time, parallel, incremental) but the load phase paid on every run. The split *inside* that phase
— front end, metadata, monomorphization, lowering, guest — is unmeasured; measuring it precedes cache
design.

## 3. Boundaries

- **proc-macros and `build.rs` are always really compiled and really executed.** Every "lower
  everything ahead" assumption stops here; mode B also runs them once at packaging time.
- Stable MIR is not a serialization format.
- Runtime toolchain discovery is impossible.
- The L3 JIT code cache stays forbidden until CFI, PLT and relocation are fixed.
- Full cross-project sharing of the L1 target dir is not delivered beyond same-workspace cross-bin
  smoke.

## 4. Verification

- **L2 acceptance**: one workload run cold and warm must be byte-identical through the existing gate's
  diff channel, which catches a cache hit returning stale semantics.
- **Gate**: `make test` / `make smoke` / `make gate`, plus `make suite S=<id>`; L2 adds an acceptance
  dimension to this gate rather than a parallel harness.
- **Phase timing**: a time dimension on the existing `--vm-stats` instrumentation, producing the
  per-phase numbers §2.6 lacks.
- **Cache management**: `mirvm cache status|purge`, with the root at `$HOME/.mirvm`. `MIRVM_BUILD_ID`
  keys every layer, so a rebuild invalidates base, deps and L2 images by design.
- **Baseline reproduction**: release build (a debug build distorts every timing gate), warm cache, the
  two workloads above.

## 5. Open items

- The mode B `.mirvm` package and `mirvm pack` are explicitly scheduled after the JIT CFI/unwind work.
- Release form and naming are open: MRsDK is rejected, and "kit" naming waits for a mode B physical
  artifact.
- The approved but unstarted order is phase timing → L2 engine-IR cache → dependency codegen trimming
  → mode B package → release form, with cold-start leverage construction inserted once the first two
  finish.
- The per-phase split inside the load phase is unmeasured, and it is the precondition for L2 design.
- Reopens and their triggers: frequent `ir.rs` churn hurting the L2 hit rate (the key contains the
  build id, so it stays correct; re-estimate the format freeze once the IR settles); Cargo not
  tolerating artifact-existence checks after emit trimming (establish empirically on landing, else
  keep the status quo); a dangling FnPtr or handle in the relink section yielding silent wrong values
  (validation section, prefer Trap); semantic drift on the cache-hit path (cold/warm byte diff in the
  gate).
