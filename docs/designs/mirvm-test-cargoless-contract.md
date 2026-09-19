# The cargoless contract for `mirvm test`

> Status: Contract · Scope: `mirvm test` for single packages and resolver 1/2/3 workspaces, no Cargo process at run time · Effective 2026-08-11.
> Implementation facts are the code plus `tests/suites/contracts/cargoless_test.sh` and `tests/suites/contracts/cargoless_workspace.sh`.

## 1. Contract

- **C1.** The pinned `nightly-2026-07-02` Cargo and rustdoc decide correct behaviour; mirvm must never copy their internals nor treat its own earlier output as the standard. For uncertain behaviour, first run `cargo test -vv` with the pinned toolchain on a minimal project, then implement the actual rustc/rustdoc arguments, environment and observable results.
- **C2.** Every behaviour in section 2 must align with Cargo; everything outside section 3's scope must produce an explicit error.
- **C3.** An out-of-scope input must never be silently ignored so that execution continues.
- **C4.** Inputs Cargo itself rejects — nested workspaces, duplicate member names — must error as Cargo does; mirvm adds no permissive rule.
- **C5.** Edition 2024 implies resolver 3; a virtual or old-edition workspace with no `resolver` key implies resolver 1.
- **C6.** A doctest must never masquerade as a normal test target: pinned rustdoc keeps owning code-block extraction, source line numbers and diagnostics, and mirvm only compiles and runs the temporary crate rustdoc generates.

## 2. Model

### 2.1 Target selection and compilation shape
lib, bin, integration test, example and bench targets are read, honouring `test`, `harness`, `required-features` and auto-discovery. Root lib/bin see normal dependencies only; test units additionally see dev-dependencies, which enter the lockfile but are not built by `mirvm run`. Tests use `[profile.test]`; libtest targets carry `--test`; `harness=false` targets keep the user `main` and set `cfg(test)`. An example is compiled by default but not run, while explicit `--example` runs it as a test target. A bench uses dev-dependencies, `cfg(test)` and the libtest shape, and `--bench`, `--benches`, `--all-targets` are supported. When an integration test exists, normal bins are additionally compiled. Integration tests get `CARGO_TARGET_TMPDIR` and an executable `CARGO_BIN_EXE_<name>`, which still starts the in-VM target program.

\#\#\#\ 2\.2\ Diagnostics\,\ output\,\ exit\ codes
lib/bin/test/example/bench selection, a single filter string, `--no-run`, `--no-fail-fast`, `--quiet`, `--locked`, `--offline` are supported, and harness arguments after `--` pass through verbatim. Default selection and explicit `--doc` run a root lib with `doctest` enabled; as in Cargo, `--doc --no-run` and `--doc` mixed with another target selection are rejected. rustdoc owns edition, cfg, source line numbers, `no_run`, `ignore`, `compile_fail`/error codes and `should_panic(expected)`; the temporary crate uses the same MIR sysroot, root library, normal/dev dependencies and build.rs output, and successful tests are executed by the VM. Each test artifact runs in a separate mirvm subprocess, and failure exit code is 101, as in Cargo.

### 2.3 Root proc-macro packages
With a proc-macro root package, the normal root library and normal dependencies compile host-side into dynamic libraries. The root library's own unit tests still enter the VM under `--test`; integration tests consume the host proc-macro expansion through `--extern`. Normal host dependencies of the root proc-macro and dev-dependencies of the test keep their separate Cargo dependency purposes.

### 2.4 Workspace discovery and resolution
The nearest workspace is found from the workspace root or a member directory. Virtual/root-package manifests, `members`, `exclude`, `default-members`, `*`/`**`/`?`/`[]` globs, and path dependencies inside the workspace joining members automatically are supported; starting from an `exclude`d package treats it as standalone, while a missing member and a pattern matching no member must error.
`[workspace.package]`, `[workspace.dependencies]`, `[workspace.lints]`, target-conditional dependencies and the root `[profile]` are materialized. Workspace lints reach rustc in Cargo's level/priority/check-cfg shape; workspace dependency features sum; paths always resolve against the workspace root.
Default members, the current member, `--workspace`/`--all`, repeated `-p`/`--package` and `--exclude` are supported. A package spec may be a package name, `name@version` or `path+file:///...#name@version`, where version and path disambiguate; with several packages selected, all compilation finishes before execution follows Cargo's fail-fast rule.

### 2.5 Features
`--features`/`-F`, `--all-features`, `--no-default-features`, `package/feature` and a direct dependency's `dependency/feature` are supported. Resolver 2/3 unify features for the same package and dependency category while keeping build dependencies separate from normal/dev; resolver 1 unifies across normal/dev/build purposes as Cargo does. Host and target artifacts are still produced separately per purpose.

### 2.6 Version and lockfile
`rust-version` is read from the package, workspace inheritance and the registry index. Resolver 3 prefers candidates compatible with the workspace's lowest Rust version; Cargo configuration may explicitly set `allow`, and `--ignore-rust-version` disables both candidate preference and the pre-compile version rejection. A fresh lock writes v3 when the lowest version is at or below 1.82, and v4 from 1.83. All members share the workspace-root `Cargo.lock`; without one, Cargo's rules apply — solve once over all features reachable from all members, then write the unified lock atomically — while actual compilation still enables only the user's selected features. `--locked` with no lock fails immediately, and the produced result must also be accepted by the pinned Cargo under `--locked --offline`.

## 3. Boundaries

- Cross-compilation targets: not in the committed scope, so they must error explicitly (C2, C3).
- Git source replacement, Git URL target patches, and the remaining full Cargo config surface: D15 has not implemented them.
- Inputs Cargo rejects, e.g. nested workspaces and duplicate member names: follow Cargo's error instead of adding a permissive mirvm rule.
- Resolver scope is 1/2/3 only.

## 4. Verification

Suites run through `./tests/run.sh`:

| Suite ID | Behavioural authority | Fixtures |
|---|---|---|
| `contracts.cargoless-test` | Fixed Cargo/rustdoc selection of test/bench/doctest, compilation shape, diagnostics, output, exit code | `tests/fixtures/cless_test_contract`, `cless_proc_macro_test_contract`, `cless_doctest_contract` |
| `contracts.cargoless-workspace` | Fixed Cargo resolver 1/2/3, complex member globs, workspace lint, package spec, features, failure propagation | `tests/fixtures/cless_workspace_contract`, `cless_workspace_remaining_contract` |
| `differential.cargoless` | Cargo path and cargoless path must match byte-for-byte | `tests/fixtures/cless_*` |
| `quality.rust` | fmt, clippy, Rust unit tests | `src/` |

`cless_test_contract` covers library tests, bin tests, integration tests, a plain example, bench, custom harness, build script, path dev-dependency, panic, ignored, failure propagation, bin subprocess and test profile; `cless_proc_macro_test_contract` covers root proc-macro normal dependencies, dev-dependencies, unit test and integration expansion; `cless_doctest_contract` covers plain, `no_run`, `ignore`, `compile_fail` and `should_panic` doctests plus root library, dev-dependencies, build.rs cfg/env, default/explicit selection, failure and Cargo argument conflicts. `cless_workspace_contract` uses resolver 3 and covers virtual manifest, four members, excluded package, inheritance, target-conditional dev-dependencies, default/explicit/dependency features, cross-root transitive dependency convergence, optional dependencies activated by features, automatic integration tests, unified build and unified lockfile; `cless_workspace_remaining_contract` uses an old-style workspace with no `resolver` key and covers the resolver 1 default, feature unification across purposes, complex globs, workspace lints and full-path package specs.

Current fixed contract result: single-package/test/bench/doctest scripts **34/34**, workspace scripts **31/31**.

Both contract scripts keep three judgment layers only: (1) **structure** — read the pinned Cargo `-vv` rustc/rustdoc lines and pin `--test`, `cfg(test)`, dependency kinds, example/bench, the root proc-macro dynamic library, resolver 1 feature merging, workspace lints, `CARGO_BIN_EXE`, the doctest root library/dev-dependencies and build.rs cfg placement; (2) **result** — compare stdout, stderr and exit codes across native Cargo, `MIRVM_DEPS=cargo` and `MIRVM_DEPS=self`, normalizing only thread ids, durations and Cargo's own build progress lines; (3) **mechanism** — the self leg replaces `cargo` on `PATH` with a must-fail sentinel and audits every `execve` with `strace` on the Linux baseline, also catching absolute-path invocations, and a hot re-run checks that build scripts still execute exactly 1 time, preventing "same result but quietly fell back to Cargo" and increment invalidation. The workspace contract additionally deletes the lock, has self rebuild it, then hands it to the pinned Cargo for `--locked --offline` review.

Pure-function assertions about argument shapes stay as unit tests in `resolve/` and `schedule.rs`. A contract script freezes once it can judge the real current workload; no manifest entries, snapshot formats or provenance metadata are added for future Cargo fields.

### 4.1 Cargo upgrade procedure

Raising `rust-toolchain.toml` must not simply refresh expected values:

1. With both the old and the new Cargo, run `test -vv --no-run`, the bench/proc-macro structure probes and the contract scripts' native legs against the fixed fixtures.
2. Explain every difference in rustc arguments, environment, target selection or output by hand, separating a Cargo behaviour change from a pure diagnostic-text change.
3. For a behaviour change, first fix manifest/resolve/schedule/driver and their unit tests, then the contract assertions; loosening normalization rules to make tests green is not allowed.
4. Run `./tests/run.sh suite quality.rust`, both contract suites, then `./tests/run.sh suite differential.cargoless`, and finally `./tests/run.sh fast`. Only after all three tracks pass does the new pinned Cargo become authoritative, and the workspace contract's Cargo `-vv` structure checks must also be re-reviewed by hand.

Cargo's mainline continuing to evolve never changes a released mirvm automatically: every mirvm version binds one explicit toolchain, and an upgrade moves the whole contract forward through the review above rather than guessing the Cargo version at run time.

## 5. Open items

- Cross-compilation targets, Git source replacement, Git URL target patches and the remaining Cargo config surface stay unimplemented; reopen this contract when D15 implements them, and until then a request must fail loudly rather than silently ignore the argument.
- The full Cargo compatibility track grows, but this contract's boundary does not move with it: a new field is implemented before it is asserted.
- The resolver scope stays at 1/2/3; a new resolver number is a reopen trigger.
