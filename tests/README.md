# mirvm Test Suite

This document is the single source of truth for the test entry point, the suite inventory, and the rules for adding tests. Historical design documents may record old file names.

## Layout

Everything mirvm needs in order to run or test a guest program lives under `tests/`. There is no second script tree anywhere in the repository.

```text
Makefile                    the interface: make test|smoke|gate|list|suite|projects|clean
tests/
  run.sh                    the implementation every target forwards to (product build, tiers, verdicts)
  support/harness.sh        shared library: suite_init, accounting, manifest parsing, corpus runner
  suites/<category>/*.sh    leaf suites; the suite id is derived from the path
  scripts/                  every guest program mirvm runs, in one namespace
    c_<name>.rs             corpus driver: registered in cases.manifest, judged by exit/oracle/diff
    vmcall_<name>.rs        exported-entry probe: driven by `mirvm run --vm-call`
    <name>.rs               differential program: native rustc against mirvm, three-way
  projects/                 real Cargo projects: git submodules pinned to one upstream commit
  fixtures/                 committed fixture crates, oracles and input data
  tsan/                     standalone crate compiling src/vm under ThreadSanitizer
```

`Makefile` is the interface and `tests/run.sh` is the implementation; the tier logic exists in exactly one place, so CI and a developer run the same thing.

New test material goes under `tests/`; nothing test-related is added at the repository root. A guest program is exactly one of the three kinds above — the `c_` and `vmcall_` prefixes are conventions the suites rely on (`differential.programs` skips both, and the corpus manifest names `c_<name>.rs`). Real Cargo projects are submodules, never vendored into this tree: each one has its own history upstream, and `tests/projects/README.md` records the pins.

## Entry Point

```bash
make test                     # daily commit check (same as fast)
make smoke                    # test + small real workloads + runtime semantics
make gate                     # the full final gate: strict corpus, deps image, perf limits
make list                     # every suite id
make suite S=<id> [ARGS="..."]  # one suite
make projects                 # fetch the tests/projects/ submodules
```

- `test` / `fast`: daily commit check.
- `smoke`: adds small real workloads and runtime checks on top of `fast`.
- `gate`: final gating, covering everything in `fast` and `smoke`, plus full corpus, performance, dependency image, and extra run modes.
- `list`: list all active suites and their purposes.
- `suite`: run a single suite. Example:
  `make suite S=corpus.run ARGS="--tier smoke"`.

`tests/run.sh` accepts the same commands (`fast`, `smoke`, `gate`, `list`, `suite <id> [args...]`) if make is not available; nothing else may be invoked directly.

The entry point switches to the repository root first, so it can be invoked from any working directory. When `MIRVM` is not set, suites that need the product binary first run `cargo build --release --locked`; when `MIRVM` is set explicitly, the specified binary is used. The pinned Rust toolchain comes from `rust-toolchain.toml`.

When the default Cargo home is not writable, the entry point uses `target/test-state/cargo-home`. When the default mirvm home is not writable, it uses `${TMPDIR:-/tmp}/mirvm-contract-home`. These can be overridden explicitly with `CARGO_HOME`, `MIRVM_HOME`, and `MIRVM_CONTRACT_HOME`. The entry point is responsible for creating writable directories; it does not require users to manually prepare the environment for each suite.

## Tier Contents

| Suite Group | `fast` | `smoke` | `gate` |
|---|:---:|:---:|:---:|
| Rust fmt, clippy, unit tests | yes | yes | yes |
| program, Cargo, cargoless three-way differential | yes | yes | yes |
| cargoless test/workspace/git/source contract | yes | yes | yes |
| pack default self and Cargo fallback contract | yes | yes | yes |
| build.rs incremental contract | yes | yes | yes |
| C/C-unwind cross-language exception contract | yes | yes | yes |
| harness anti-false-green regression | yes | yes | yes |
| corpus smoke exploratory batch | no | yes | covered by strict corpus |
| x86 and runtime semantics | no | yes | yes |
| full strict corpus | no | no | yes |
| dependency image, JIT stats, performance limits | no | no | yes |

`gate` does not rerun the same suite redundantly for formality, but the behavior coverage it checks must be a complete superset of the first two tiers. The default product path is cargoless; `differential.cargo` always keeps the Cargo fallback path, and `differential.cargoless` always compares both paths. For the full Cargo compatibility track, use:

```bash
MIRVM_DEPS=cargo ./tests/run.sh gate
```

## Suite Directory

| Suite ID | Purpose and Behavioral Authority | Main Fixtures |
|---|---|---|
| `quality.rust` | fmt, clippy, Rust unit tests; embedded regression locks for signal guest `oldact`/query address, non-LIFO close recovery, inactive owner routing, safe-point dispatch only registered after close/JIT lock, and self-produced archive `signal`/`sigaction`/`raise` owner bridge; subprocess regression also locks that any exception in native fini must be diagnosed and then `abort`, never stuck in `Closing` | `src/` |
| `differential.programs` | native stdout, stderr, and exit code of the differential programs in `tests/scripts/` are authoritative (the `c_` and `vmcall_` kinds are excluded by name); `signal_probe` also requires synchronous nested `raise` order, count, and SIG_IGN behavior to match | `tests/scripts/` |
| `differential.cargo` | fixed Cargo's behavior for scripts, projects, and plain/workspace rustc wrapper combinations is authoritative | `tests/scripts/ecosystem.rs`, `tests/scripts/ffi_zlib.rs`, `rustc_wrapper_probe.sh`, other `tests/fixtures/` |
| `differential.cargoless` | Cargo path and cargoless path must match byte-for-byte | `tests/fixtures/cless_*` |
| `contracts.cargoless-test` | fixed Cargo/rustdoc's selection of test/bench/doctest, compilation shape, diagnostics, output, and exit code are authoritative | `cless_test_contract/`, `cless_proc_macro_test_contract/`, `cless_doctest_contract/` |
| `contracts.cargoless-workspace` | fixed Cargo's resolver 1/2/3, complex member globs, workspace lint, package spec, features, and failure propagation are authoritative | `cless_workspace_contract/`, `cless_workspace_remaining_contract/` |
| `contracts.cargoless-git` | fixed Cargo lock format plus local Git repo commit contents are authoritative | generated at runtime |
| `contracts.cargoless-sources` | fixed Cargo source config merging, alternate registry, credential provider, source replacement, patch/replace, and lock | locally generated sparse/local/directory registry at runtime |
| `contracts.pack` | default pack must use zero Cargo; explicit fallback must enter fixed Cargo; v4 artifacts must run without build cache; the process's own immutable snapshot hot-order prefetch must not change second-run results; the same `Package` can be instantiated concurrently and repeatedly, with static/TLS, P1 (guest native entry) address, native ctor/fini, global_asm/C2 bridge isolated per instance; old pointers after close must not point to a new Engine | path-dependency projects generated at runtime, `package_embed.rs` |
| `contracts.build-script-rerun` | build.rs input changes and Cargo directives decide whether to rerun | `cless_br/`, `cless_libc.rs` |
| `contracts.deps-image` | fixed output, cache file count, and set time limits | temporary copy of `a2_ws/` |
| `corpus.run` | real-dependency exploratory batch; checks exit code, and XFAIL also locks diagnostics | `tests/suites/corpus/cases.manifest`, `tests/scripts/`, `tests/projects/` |
| `corpus.deps-pair` | three-way consistency of Cargo/cargoless for each corpus entry | `tests/suites/corpus/cases.manifest`, `tests/scripts/` |
| `corpus.contract` | strict verdict per manifest exit/oracle/diff/xfail | `tests/suites/corpus/cases.manifest`, `tests/fixtures/oracles/` |
| `runtime.semantics` | math constants or same-source native results are the runtime semantics authority for exported entries, value/memory digests, unwind, and threads; the unwind segment also separates a real `lang_start` main panic from a normal `Termination` 101, in both the interpreter and the JIT | `tests/scripts/vmcall_*.rs`, `tests/tsan/` |
| `runtime.c-unwind` | fixed rustc+C++ is the cross-language exception authority; 13 items require interpreter and forced-sync JIT to preserve exception identity, Drop, ordinary C termination boundary, C++ typed exception can pass through the whole Engine, C++ exception terminates when reaching guest catch, and reject non-C/System ABI | `c_unwind_contract/` |
| `runtime.diagnostics` | direct, cargoless, and Cargo runner default stderr keep original bytes; capture independently saves compiler and MIRVM control diagnostics from command-arg parsing, must not mix in guest same-text, NUL, non-UTF-8, or ANSI bytes | `diagnostic_router_*.rs` |
| `runtime.x86-features` | current host native result is the authority for each x86 sub-capability | `tests/fixtures/x86_*.rs` |
| `runtime.tsan` | TSan exit code is zero and no data-race warnings | `tests/tsan/` |
| `runtime.jit-stats` | JIT exit stats must exist and key buckets must be non-zero | `tests/scripts/jit_unwind_probe.rs` |
| `performance.limits` | existing load, rayon, fib time limits and cache budget | `tests/scripts/c_rayon.rs`, `tests/scripts/vmcall_pure.rs` |
| `harness.truth` | deterministic fake programs prove the framework cannot false-green or swallow failures | `tests/fixtures/gate_truth/` |

`tests/support/harness.sh` is shared implementation, not a test. It owns the suite bootstrap (`suite_init`), PASS/FAIL/SKIP/XFAIL counting, the unified summary, corpus selection and the corpus driver runner, stderr normalization, the pinned-toolchain and pinned-Cargo checks, the temporary sysroot, timing, and disk protection. A suite that repeats any of that is a bug in the suite.

## Verdict Rules

Each test item may use only the following states:

| State | Meaning | Causes Suite Failure |
|---|---|:---:|
| `PASS` | requirement was actually executed and passed | no |
| `FAIL` | product behavior, behavioral authority, or test framework does not meet the requirement | yes |
| `SKIP` | host genuinely lacks the capability, with reason stated | no |
| `XFAIL` | registered product gap fails with exact exit code and diagnostics | no |
| `XPASS` | registered gap unexpectedly turns green, contract must be updated | yes |

Missing `strace`, fixed Cargo, Rust toolchain, or test sysroot is an environment error, not a SKIP. Explicit unimplemented boundaries must be written as XFAIL with precise reasons.

Suite exit codes:

- `0`: no unexpected failures.
- `1`: FAIL or XPASS exists.
- `64`: command argument error.
- `69`: required tool or test environment unavailable.
- `77`: entire suite skipped only due to missing host capability.

Each leaf suite must output a uniform trailing line with `suite_summary <suite-id>`. The top entry judges suite results by exit code, not by grepping arbitrary explanatory text. Suite output is printed in full when that suite ends; failure messages are not kept only in the last few lines.

## Writing Requirements

1. New suite scripts go in `tests/suites/<category>/`; file names describe behavior; no milestone codenames. This applies to fixtures too: `x86_addcarry.rs`, not `m51_addcarry.rs`.
2. New guest programs go in `tests/scripts/` and pick one of the three kinds by prefix: `c_<name>.rs` for a corpus driver (register it in `tests/suites/corpus/cases.manifest` in the same change — an unregistered driver silently never runs, and a registered name without a driver fails the batch), `vmcall_<name>.rs` for a `--vm-call` exported-entry probe, and `<name>.rs` for a differential program. New real Cargo projects go in `tests/projects/` as a pinned submodule, never vendored, and their manifest row carries `needs=tests/projects/<name>/Cargo.toml` so an uninitialized checkout SKIPs instead of failing.
3. Suite IDs are derived automatically from the path: e.g. `contracts/cargoless_git.sh` becomes `contracts.cargoless-git`. The second line of the script must be a single-line purpose description; suites that do not depend on the product binary additionally declare `# product: no`. Do not create a manual registry.
4. Scripts must source `tests/support/harness.sh` and then call `suite_init` (`suite_init --no-product` when the suite declares `# product: no`). That is the whole bootstrap: it enters the repository root, resolves the pinned toolchain and the product binary, and provides `$TMP` with cleanup armed, so no suite resolves a toolchain, validates the binary, or creates a temporary directory itself. The product binary defaults to `$REPO_ROOT/target/release/mirvm`, so a suite behaves the same standalone and under `run.sh`.
5. The file header must state what is being tested, why it is needed, who is the behavioral authority, and which inputs are compared. Describe the present contract only: no milestone history, no per-change narration.
6. When native Rust or fixed Cargo is the authority, the authority side must first reach the explicit expected exit code. Both sides failing to build or run the same way does not count as passing.
7. Differential comparison defaults to stdout, stderr, and exit code. Only filter fields that are genuinely unstable such as time and thread IDs; each filter rule must explain why in the script. The one allowed default filter is `normalize_stderr`.
8. Committed fixtures are read-only. When a file needs to be modified, first copy it to a directory created by `mktemp -d` and clean up with trap. After the test ends, `git status --short` must not show modifications produced by the test.
9. Suites must be runnable standalone and cannot rely on a previous suite leaving locks, sysroot, environment variables, or files. Shared caches may speed things up, but cache misses must not change the verdict standard.
10. Aggregate suites must not use `set -e`, because expected non-zero exit codes may themselves be the contract. Every external command must explicitly capture and judge its exit code.
11. SKIP is only for host capability differences. Missing required tools, lost fixtures, or invalid manifests must fail loudly. A `needs=` path that names an optional asset (a submodule, a machine-local tool) is the one honest way to express a capability SKIP.
12. Run serially by default. Performance, cache, and some system capabilities share global state; there is currently no trustworthy parallel contract.

## Steps to Add a New Suite

1. First write assertions that can fail due to the target defect, and verify the failure reason is correct.
2. Add a leaf script under `tests/suites/`, reusing the shared harness.
3. Confirm `make list` has automatically discovered it without manual registration.
4. Describe its purpose, behavioral authority, and fixtures in the suite table in this file.
5. Explicitly add it to the acceptance policy of `fast`, `smoke`, or `gate` in `tests/run.sh`, or explain why it can only be run manually.
6. If modifying entry, state propagation, or summary, first extend `harness.truth`.
7. Run `bash -n`, then `make suite S=<id>`, then `make test`, and `make smoke` or `make gate` depending on scope.

Auto-discovery depends only on directories and file names, while tier contents still explicitly state acceptance coverage in `tests/run.sh`, so test infrastructure does not become a new product engineering effort. If a rule above has to be repeated inside a suite, it belongs in `tests/support/harness.sh` instead.
