# mirvm Test Suite

This document is the single source of truth for the current test entry point, suite purposes, and rules for adding new tests. Historical design documents may record old file names, but users, CI, and current documentation must run tests only through `tests/run.sh`.

## Entry Point

```bash
./tests/run.sh fast
./tests/run.sh smoke
./tests/run.sh gate
./tests/run.sh list
./tests/run.sh suite <suite-id> [suite-args...]
```

- `fast`: daily commit check.
- `smoke`: adds small real workloads and runtime checks on top of `fast`.
- `gate`: final gating, covering everything in `fast` and `smoke`, plus full corpus, performance, dependency image, and extra run modes.
- `list`: list all active suites and their purposes.
- `suite`: run a single suite. Example:
  `./tests/run.sh suite corpus.run --tier smoke`.

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
| `differential.programs` | native stdout, stderr, and exit code of `demo/*.rs` are authoritative; `signal_probe` also requires synchronous nested `raise` order, count, and SIG_IGN behavior to match | `demo/` |
| `differential.cargo` | fixed Cargo's behavior for scripts, projects, and plain/workspace rustc wrapper combinations is authoritative | `demo/`, `rustc_wrapper_probe.sh`, other `tests/fixtures/` |
| `differential.cargoless` | Cargo path and cargoless path must match byte-for-byte | `tests/fixtures/cless_*` |
| `contracts.cargoless-test` | fixed Cargo/rustdoc's selection of test/bench/doctest, compilation shape, diagnostics, output, and exit code are authoritative | `cless_test_contract/`, `cless_proc_macro_test_contract/`, `cless_doctest_contract/` |
| `contracts.cargoless-workspace` | fixed Cargo's resolver 1/2/3, complex member globs, workspace lint, package spec, features, and failure propagation are authoritative | `cless_workspace_contract/`, `cless_workspace_remaining_contract/` |
| `contracts.cargoless-git` | fixed Cargo lock format plus local Git repo commit contents are authoritative | generated at runtime |
| `contracts.cargoless-sources` | fixed Cargo source config merging, alternate registry, credential provider, source replacement, patch/replace, and lock | locally generated sparse/local/directory registry at runtime |
| `contracts.pack` | default pack must use zero Cargo; explicit fallback must enter fixed Cargo; v4 artifacts must run without build cache; the process's own immutable snapshot hot-order prefetch must not change second-run results; the same `Package` can be instantiated concurrently and repeatedly, with static/TLS, P1 (guest native entry) address, native ctor/fini, global_asm/C2 bridge isolated per instance; old pointers after close must not point to a new Engine | path-dependency projects generated at runtime, `package_embed.rs` |
| `contracts.build-script-rerun` | build.rs input changes and Cargo directives decide whether to rerun | `cless_br/`, `cless_libc.rs` |
| `contracts.deps-image` | fixed output, cache file count, and set time limits | temporary copy of `a2_ws/` |
| `corpus.run` | real-dependency exploratory batch; checks exit code, and XFAIL also locks diagnostics | `cases.manifest`, `corpus/` |
| `corpus.deps-pair` | three-way consistency of Cargo/cargoless for each corpus entry | `cases.manifest`, `corpus/` |
| `corpus.contract` | strict verdict per manifest exit/oracle/diff/xfail | `cases.manifest`, `oracles/` |
| `runtime.semantics` | math constants or same-source native results are runtime semantics authority; the `unwind` section has 13 items: 9 existing unwind/recovery semantics, plus one each for interpreter and JIT where an uncaught guest payload drops exactly once, cleanup then continues calling and second panic, plus one each for interpreter and JIT distinguishing real `lang_start` main panic from normal `Termination` 101 | `demo/m4/`, `tsan/` |
| `runtime.c-unwind` | fixed rustc+C++ is the cross-language exception authority; 13 items require interpreter and forced-sync JIT to preserve exception identity, Drop, ordinary C termination boundary, C++ typed exception can pass through the whole Engine, C++ exception terminates when reaching guest catch, and reject non-C/System ABI | `c_unwind_contract/` |
| `runtime.diagnostics` | direct, cargoless, and Cargo runner default stderr keep original bytes; capture independently saves compiler and MIRVM control diagnostics from command-arg parsing, must not mix in guest same-text, NUL, non-UTF-8, or ANSI bytes | `diagnostic_router_*.rs` |
| `runtime.x86-features` | current host native result is the authority for each x86 sub-capability | `tests/fixtures/m51_*.rs` |
| `runtime.tsan` | TSan exit code is zero and no data-race warnings | `tsan/` |
| `runtime.jit-stats` | JIT exit stats must exist and key buckets must be non-zero | `demo/jit_unwind_probe.rs` |
| `performance.limits` | existing load, rayon, fib time limits and cache budget | `corpus/c_rayon.rs`, `demo/m4/pure.rs` |
| `harness.truth` | deterministic fake programs prove the framework cannot false-green or swallow failures | `tests/fixtures/gate_truth/` |

`tests/support/harness.sh` is shared implementation, not a test. It handles root-directory location, PASS/FAIL/SKIP/XFAIL counting, unified summary, corpus manifest parsing, temporary sysroot preparation, timing, and disk protection. `tests/parked/` is parked material, not part of active suites, and cannot be reached from `run.sh list`.

## Verdict Rules

Each test item may use only the following states:

| State | Meaning | Causes Suite Failure |
|---|---|:---:|
| `PASS` | requirement was actually executed and passed | no |
| `FAIL` | product behavior, behavioral authority, or test framework does not meet the requirement | yes |
| `SKIP` | host genuinely lacks the capability, with reason stated | no |
| `XFAIL` | registered product gap fails with exact exit code and diagnostics | no |
| `XPASS` | registered gap unexpectedly turns green, contract must be updated | yes |

Missing `strace`, fixed Cargo, Rust toolchain, or test sysroot is an environment error, not a SKIP. The old `P5` accounting is no longer a test state; currently explicit unimplemented boundaries must be written as XFAIL with precise reasons.

Suite exit codes:

- `0`: no unexpected failures.
- `1`: FAIL or XPASS exists.
- `64`: command argument error.
- `69`: required tool or test environment unavailable.
- `77`: entire suite skipped only due to missing host capability.

Each leaf suite must output a uniform trailing line with `suite_summary <suite-id>`. The top entry judges suite results by exit code, not by grepping arbitrary explanatory text. Suite output is printed in full when that suite ends; failure messages are not kept only in the last few lines.

## Writing Requirements

1. New scripts go in `tests/suites/<category>/`; file names describe behavior; no milestone codenames.
2. Suite IDs are derived automatically from the path: e.g. `contracts/cargoless_git.sh` becomes `contracts.cargoless-git`. The second line of the script must be a single-line purpose description; suites that do not depend on the product binary additionally declare `# product: no`. Do not create a manual registry.
3. Scripts must source `tests/support/harness.sh`, then call `test_enter_repo`. Do not assume the caller's current directory.
4. The file header must state what is being tested, why it is needed, who is the behavioral authority, and which inputs are compared.
5. When native Rust or fixed Cargo is the authority, the authority side must first reach the explicit expected exit code. Both sides failing to build or run the same way does not count as passing.
6. Differential comparison defaults to stdout, stderr, and exit code. Only filter fields that are genuinely unstable such as time and thread IDs; each filter rule must explain why in the script.
7. Committed fixtures are read-only. When a file needs to be modified, first copy it to a directory created by `mktemp -d` and clean up with trap. After the test ends, `git status --short` must not show modifications produced by the test.
8. Suites must be runnable standalone and cannot rely on a previous suite leaving locks, sysroot, environment variables, or files. Shared caches may speed things up, but cache misses must not change the verdict standard.
9. Aggregate suites must not use `set -e`, because expected non-zero exit codes may themselves be the contract. Every external command must explicitly capture and judge its exit code.
10. SKIP is only for host capability differences. Missing required tools, lost fixtures, or invalid manifests must fail loudly.
11. Run serially by default. Performance, cache, and some system capabilities share global state; there is currently no trustworthy parallel contract.

## Steps to Add a New Suite

1. First write assertions that can fail due to the target defect, and verify the failure reason is correct.
2. Add a leaf script under `tests/suites/`, reusing the shared harness.
3. Confirm `./tests/run.sh list` has automatically discovered it without manual registration.
4. Describe its purpose, behavioral authority, and fixtures in the suite table in this file.
5. Explicitly add it to the acceptance policy of `fast`, `smoke`, or `gate`, or explain why it can only be run manually.
6. If modifying entry, state propagation, or summary, first extend `harness.truth`.
7. Run `bash -n`, the target single suite, `fast`, and then `smoke` or `gate` depending on scope.

The number of suites is currently small; no second machine inventory or result database is added. Auto-discovery depends only on directories and file names, while tier contents still explicitly state acceptance coverage in `tests/run.sh`, so test infrastructure does not become a new product engineering effort.
