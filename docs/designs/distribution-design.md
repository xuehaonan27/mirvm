# Track C: Distribution and Product Surface — Load Ingestion, Cache Layering, Packaging and Release (D9)

> Status: Decided RFC · Scope: track C — how mirvm ingests a load (single file, script frontmatter, cargo project), how its caches are layered, and how it would be packaged and released.
> D9a–D9f were approved 2026-07-14; construction is not chartered. The milestone number is fixed at charter time (candidate M6) and does not take the M5.3–5.5 JIT numbers. In the construction order, ①–③ may interleave with the JIT period; ④ sits explicitly after M5.3.
> This document refines DESIGN.md mode B and the ".mirvm-ified sysroot" sketch (git history); it is cross-indexed with the "distribution and product surface" row of current-status.md.
> Sibling contracts: [modeb-mirvmar-design.md](modeb-mirvmar-design.md) (mode B package format, which names this document as its parent), [d15-cargoless-design.md](d15-cargoless-design.md) (dependency resolution and compile scheduling without Cargo), [mirvm-test-cargoless-contract.md](mirvm-test-cargoless-contract.md) (`mirvm test`), [concurrency-arch.md](concurrency-arch.md) (engine and thread model).

## 1. Contract

- **D9a Unified entry point.** Keep the single `mirvm` binary plus subcommands (existing `run`, future `pack`, ...). `mirvmc` is at most a hard-link alias of that same binary; a separate binary never appears, because the rustc_private link is huge and two copies are pure waste.
- **D9b Custom package format, two-layer route.** Reject "StableMIR as the format": StableMIR (`rustc_smir`, upstream moving toward `rustc_public`) is an in-process API with no on-disk stability guarantee. The two layers are the polymorphic layer (approximately rmeta already) and the monomorphic layer (post-mono engine IR). Route: build the L2 monomorphic engine-IR cache first (this machine); the mode B `.mirvm` package is that cache made portable (version header / checksum / relocation section). Freezing the external format waits until the IR stabilizes in the M5.3 JIT period — M5.2 just added about 20 statement kinds to `ir.rs`, and no external promise is frozen during churn.
- **D9c Cache-layering ledger.** See section 2.5. L2 key = mirvm build id + sysroot hash + crate graph content hash; a mismatch rebuilds the whole package with no partial reuse (the AppCDS analogy). The toolchain is self-pinned and the format is our own, so a rebuild is always possible and the brittleness is harmless.
- **D9d Trim codegen from dependency builds.** Target dependencies move to the check shape (metadata only, a native cargo profile), cutting the rlib machine code compiled for nothing. Landing must establish empirically, against cargo-miri's build phases (`phases.rs`), how far cargo tolerates its artifact-existence checks (the miri sources are not vendored with `rustc-src`; obtain them separately at implementation time). This saves the first build (one-time), not every run — lower priority than L2.
- **D9e Toolchain model: a bundled compiler.** Runtime discovery of a local toolchain is architecturally impossible (rustc_private is nightly-only; the binary is ABI-locked to its toolchain; rmeta is unstable across versions), so the correct analogy is a JDK that bundles javac, not one that discovers javac. Release in two phases: first miri-style — one build per toolchain (a rustup component later), where "switching toolchain" means switching to the matching mirvm build; later JDK-style — a self-contained tarball bundling the necessary toolchain subset (rustc + rust-src + cargo), with zero rustup dependency for the user. `--sysroot` keeps its semantic boundary: it controls where guest std comes from, never who the compiler is. Guest code is not subject to the nightly restriction (stable syntax runs as-is).
- **D9f Construction order and naming.** After charter: ① phase timing (a time dimension on the existing `--vm-stats` instrumentation; no new harness mechanism) -> ② L2 engine-IR cache -> ③ dependency codegen trimming -> ④ mode B `.mirvm` package + `mirvm pack` subcommand (after M5.3) -> ⑤ release form and naming wrap-up. Naming: MRsDK is rejected (mixed script, hard to say); "kit" naming waits for a mode B physical artifact, with MDK / "mirvm toolkit" the candidates then. **2026-07-14 order revision (user ruling):** once ①② are done and M5.3 is marked Pending, insert cold-start leverage construction (cold-start research, git history): S1 (sysroot ceremony stamp + cache blind-spot repairs P1/P2/P3) -> S2 (= ③ dependency codegen trimming) -> S4 (std pre-lowering base, the mode B sysroot-side special case, design review first) -> S3 (lazy lowering) starts only after a joint layered design review with the M5.3 JIT; ④⑤ keep their positions.

Assumption against status (checked 2026-07-14):

| Assumption | Status |
|---|---|
| raw cargo project runs directly with `mirvm run` | implemented (`src/cargo_shim.rs` three-phase, the cargo-miri mechanism ported) |
| Rust script with dependencies ships as one file | implemented (cargo `-Zscript`-style `---` frontmatter, materialized to `~/.mirvm/scripts/<hash>`, same cargo channel) |
| unified entry point versus standalone `mirvmc` | settled as fact: `mirvm` is one binary in three forms (run / `RUSTC_WRAPPER` / runner, busybox-style); D9a confirms |
| pre-lowered `mirvmc` packaging and distribution | untouched = DESIGN.md **mode B**; D9b route: L2 cache first, package = cache made portable |
| local caches | partial: five content-hash caches plus a per-project `target/mirvm`; the L2 monomorphic engine-IR cache (highest value) is missing |

## 2. Model

### 2.1 Three run forms (`src/cli/`)

1. `mirvm run <file.rs>` (pure single file): zero-cargo fast path; the in-process rustc front end compiles and runs directly.
2. `mirvm run <file.rs>` (with frontmatter): RFC 3503 syntax (shebang tolerance, `---`/`---cargo` fences, body line numbers preserved); materialized into a cached cargo project, then the cargo channel.
3. `mirvm run <dir | Cargo.toml>`: the cargo channel directly.

### 2.2 The cargo three-phase mechanism (`src/cargo_shim.rs`, ported from cargo-miri)

- **phase_cargo** drives a real `cargo run` and injects `RUSTC_WRAPPER=mirvm` + `target.runner=["mirvm","runner"]` + the unified `$MIRVM_HOME/target/mirvm` (D14 near-term slice; 2026-07-18 changed from a per-project target dir to a shared one, §7.15). It forces `--target <host>` as the host/target crate separation switch. Dependency resolution, download and fail-fast are native cargo behaviour: `Cargo.lock` and the `~/.cargo` registry cache are all free-ridden.
- **phase_wrapper**: host crates (`build.rs`, proc-macros) compile with the real rustc and are really executed; target dependencies compile with the real rustc + the MIR sysroot + `-Zalways-encode-mir` (an rlib with full MIR); the final bin is **not** compiled — it writes a JSON fake binary plus a stub `.d` to prevent a rebuild.
- **phase_runner**: when cargo "runs" the fake binary, control returns to mirvm, which starts the front end in-process with cargo's original rustc arguments and monomorphizes + lowers + interprets.

Division of labour: dependency **MIR-ification** is front-loaded and parallel in cargo (crate DAG + `-j` + pipelining, a downstream crate starts as soon as a dependency's rmeta appears) and incremental (fingerprints); dependency **bytecode-ification (lowering)** happens in the load phase of every run, driven on demand by monomorphization (lowering only what is used, by construction). The guest runtime compiles no dependencies.

### 2.3 Sysroot and toolchain pinning

- `src/sysroot.rs` rebuilds std from rust-src with rustc-build-sysroot (the Miri arrangement) using `-Zalways-encode-mir`, cached at `~/.mirvm/sysroot-<target>`, fresh by content hash.
- `build.rs` bakes `MIRVM_DEFAULT_SYSROOT` and an rpath to that sysroot's `librustc_driver.so`, so the mirvm binary is **ABI-locked to the nightly toolchain that built it**. The wrapper phase ignores the rustc name cargo passes and always uses the pinned rustc, because proc-macro dylibs and rmeta must be the same compiler version as the interpreting session.

### 2.4 Existing cache inventory

| Location | Content | Key |
|---|---|---|
| `~/.cargo` | registry/git sources (cross-project) | native cargo |
| `~/.mirvm/target/{mirvm,native}` | dependency rlib/MIR-rlib + fingerprints (**machine-wide shared**, D14 near-term slice §7.15) | cargo fingerprints |
| `~/.mirvm/sysroot-<target>` | MIR-rich std | content hash (rustc-build-sysroot) |
| `~/.mirvm/native-archives` | `.a` -> `.so` products | content hash |
| `~/.mirvm/asm-stubs`, `global-asm` | asm factory `.so` | content hash |
| `~/.mirvm/scripts` | frontmatter script materialization projects | path + content hash |

> 2026-07-18: the cache root moved from `~/.cache/mirvm` to `$HOME/.mirvm` (`MIRVM_HOME` relocates it), plus the management surface `mirvm cache status|purge` (stale-generation GC / whole family / scripts / full clear); deps/base/ir stale generations are decided by peeking the first field of build_id.

### 2.5 Cache-layering ledger (D9c)

| Layer | Content | Status |
|---|---|---|
| L0 | registry/git sources (`~/.cargo`, cross-project) | free-ridden from cargo |
| L1 | dependency rlib/MIR-rlib + fingerprints (`~/.mirvm/target/{mirvm,native}`, machine-wide shared) | exists; **cross-project sharing landed (D14 near-term slice)** |
| L1.5 | sysroot / native `.so` / asm-stubs / script materialization | exists (content hash) |
| **L2** | **post-mono engine-IR whole-package cache** | **highest-value gap**: turns the ~3s paid on every run into "deserialize + run" |
| L3 | JIT code cache | forbidden until M5.3–5.5 fixes CFI/PLT/relocation |

L2 design points (pre-research checklist for construction ②):

- Serializability follows from the M4 founding property: the execution phase is tcx-free and the bytecode is self-contained.
- **Live relink section**: dlopen'd `.so` handles, the FnPtr table, string/constant-pool host pointers, thunk factory products — none of these serialize; they are rebuilt at load time and need an independent relocation subsection.
- Against silent wrong values: after load, a mismatched FnPtr/handle must prefer Trap (validation section); a dangling old value is never allowed.
- Verification channel = the existing gate: one workload run cold and warm must produce **byte-identical** output (the diff channel), catching "the cache returned old semantics" false green. No new harness mechanism (AGENTS.md budget discipline).

### 2.6 Measured baseline (2026-07-14, EPYC 7773X, release, warm cache)

| Workload | Wall clock | Meaning |
|---|---|---|
| `demo/args_env.rs` (std-only fast path) | **0.40s** | front end + monomorphization + lowering + run, std-only graph |
| `demo/ecosystem.rs` (serde+serde_json+rand+regex) | **3.07s** (reproducible) | dependency rmeta all cached; these 3s are **paid on every run**: leaf front end + pulling dependency MIR + whole-graph monomorphization + lowering + run |

Conclusion: the cold-start pain is **not** in dependency resolution (one-time), nor mainly in dependency compilation (one-time, parallel, incremental), but in the load phase paid on every run. The ripgrep tier at 3.7–4.7s (historical real-project measurements) behaves the same. The split inside that phase (front end / metadata / mono / lowering / guest) is **unmeasured** — the task of construction ①; the ledger comes before cache design (research-first discipline).

### 2.7 JVM mapping cheat sheet

| JVM | mirvm |
|---|---|
| jar | source + rmeta package (polymorphic layer) |
| CDS / AppCDS | L2 engine-IR cache |
| AOT / JIT code cache | L3 (after M5.3+) |
| javac annotation processor | proc-macro / build.rs (always really executed) |
| JDK bundling javac | mirvm's compiler-bound release (miri-style -> self-contained tarball) |

## 3. Boundaries

1. **proc-macro / build.rs are always really compiled and really executed** (host crates). Every "lower everything ahead" assumption stops here: mode B also runs them once at packaging time (the javac annotation-processor analogy).
2. StableMIR is not a serialization format (D9b).
3. Runtime toolchain discovery is impossible (the three hard constraints of D9e).
4. The L3 JIT code cache is forbidden until M5.3–5.5 fix CFI/PLT/relocation.
5. Cross-project sharing of the L1 target dir: registered as not-done (low priority, many cargo semantic pitfalls).

## 4. Verification

- **L2 acceptance (construction ②):** one workload run cold and warm must be byte-identical through the existing gate's diff channel; this catches a cache hit returning stale semantics. No new harness mechanism.
- **Gate:** `./tests/run.sh` (`fast` / `smoke` / `gate`, plus `suite <id>` and `list`), suites under `tests/suites/`; L2 adds an acceptance dimension to this gate, not a parallel harness.
- **Construction ① ledger:** a time dimension on the existing `--vm-stats` instrumentation; it produces the per-phase numbers section 2.6 lacks.
- **Cache management:** `mirvm cache status|purge` (stale-generation GC / whole family / scripts / full clear); the root is `$HOME/.mirvm`, relocated by `MIRVM_HOME`. `MIRVM_BUILD_ID` keys every layer, so a rebuild invalidates base/deps/L2 images by design.
- **Baseline reproduction:** release build (a debug build distorts every timing gate), warm cache, EPYC 7773X, `demo/args_env.rs` 0.40s and `demo/ecosystem.rs` 3.07s.

## 5. Open items

### 5.1 Unimplemented sub-items

- **④ mode B `.mirvm` package + `mirvm pack` subcommand** — explicitly after M5.3; not adopted.
- **⑤ release form and naming wrap-up** — MRsDK rejected; "kit" naming waits for a mode B physical artifact (candidates then: MDK / "mirvm toolkit"); not adopted.
- The approved but unstarted path before them: ① phase timing -> ② L2 engine-IR cache -> ③ dependency codegen trimming, with the 2026-07-14 revision inserting S1 -> S2 (= ③) -> S4 -> S3 once ①② finish and M5.3 goes Pending; ④⑤ keep their positions.
- The per-phase split inside the load phase is unmeasured; it is the input to ① and the precondition for L2 cache design.

### 5.2 Re-estimation triggers

| Risk | Handling | Trigger |
|---|---|---|
| `ir.rs` churn invalidates the L2 cache often | the key contains the build id, so it is immune by construction (only the hit rate suffers); the external format stays unfrozen | re-estimate the format freeze after M5.3 wraps up |
| cargo does not tolerate artifact-existence checks after emit trimming | establish it empirically on landing (against cargo-miri); if it fails, keep the status quo | construction ③; retest when the pinned toolchain is upgraded |
| a dangling FnPtr/handle in the relink section yields silent wrong values | validation section + prefer Trap | construction ② design review |
| semantic drift on the cache-hit path (false green) | cold/warm double run with byte diff wired into the gate | construction ② acceptance criterion |
