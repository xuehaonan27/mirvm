# The cargoless contract for `mirvm test`

> Status: Contract · Scope: `mirvm test` for single packages and resolver 1/2/3 workspaces, with no
> Cargo process at run time. Implementation facts are the code plus
> `tests/suites/contracts/cargoless_test.sh` and `tests/suites/contracts/cargoless_workspace.sh`.

## 1. Contract

- **C1.** The pinned `nightly-2026-07-02` Cargo and rustdoc decide correct behaviour. mirvm never
  copies their internals and never treats its own earlier output as the standard. For uncertain
  behaviour, first run `cargo test -vv` with the pinned toolchain on a minimal project, then implement
  the actual rustc/rustdoc arguments, environment and observable results.
- **C2.** Every behaviour in §2 must align with Cargo; everything outside §3's scope must produce an
  explicit error.
- **C3.** An out-of-scope input must never be silently ignored so that execution continues.
- **C4.** Inputs Cargo itself rejects — nested workspaces, duplicate member names — must error as
  Cargo does; mirvm adds no permissive rule.
- **C5.** Edition 2024 implies resolver 3; a virtual or old-edition workspace with no `resolver` key
  implies resolver 1.
- **C6.** A doctest must never masquerade as a normal test target: pinned rustdoc keeps owning
  code-block extraction, source line numbers and diagnostics, and mirvm only compiles and runs the
  temporary crate rustdoc generates.

## 2. Model

### 2.1 Target selection and compilation shape

lib, bin, integration test, example and bench targets are read, honouring `test`, `harness`,
`required-features` and auto-discovery. Root lib and bin see normal dependencies only; test units also
see dev-dependencies, which enter the lockfile but are not built by `mirvm run`. Tests use
`[profile.test]`; libtest targets carry `--test`; `harness=false` targets keep the user `main` and set
`cfg(test)`. An example is compiled by default but not run, while an explicit `--example` runs it as a
test target. A bench uses dev-dependencies, `cfg(test)` and the libtest shape, and `--bench`,
`--benches` and `--all-targets` are supported. When an integration test exists, normal bins are
additionally compiled. Integration tests get `CARGO_TARGET_TMPDIR` and an executable
`CARGO_BIN_EXE_<name>`, which still starts the in-VM target program.

### 2.2 Diagnostics, output and exit codes

lib/bin/test/example/bench selection, a single filter string, `--no-run`, `--no-fail-fast`, `--quiet`,
`--locked` and `--offline` are supported, and harness arguments after `--` pass through verbatim.
Default selection and explicit `--doc` run a root lib with `doctest` enabled; as in Cargo,
`--doc --no-run` and `--doc` mixed with another target selection are rejected. rustdoc owns edition,
cfg, source line numbers, `no_run`, `ignore`, `compile_fail`/error codes and `should_panic(expected)`;
the temporary crate uses the same MIR sysroot, root library, normal and dev dependencies and build.rs
output, and successful tests are executed by the VM. Each test artifact runs in a separate mirvm
subprocess, and the failure exit code is 101, as in Cargo.

### 2.3 Root proc-macro packages

With a proc-macro root package, the normal root library and normal dependencies compile host-side into
dynamic libraries. The root library's own unit tests still enter the VM under `--test`, and
integration tests consume the host proc-macro expansion through `--extern`. Normal host dependencies
of the root proc-macro and dev-dependencies of the test keep their separate Cargo purposes.

### 2.4 Workspace discovery and resolution

The nearest workspace is found from the workspace root or a member directory. Virtual and root-package
manifests, `members`, `exclude`, `default-members`, `*`/`**`/`?`/`[]` globs, and path dependencies
inside the workspace joining members automatically are supported. Starting from an excluded package
treats it as standalone; a missing member and a pattern matching no member must error.

`[workspace.package]`, `[workspace.dependencies]`, `[workspace.lints]`, target-conditional
dependencies and the root `[profile]` are materialized. Workspace lints reach rustc in Cargo's
level/priority/check-cfg shape, workspace dependency features sum, and paths always resolve against
the workspace root.

Default members, the current member, `--workspace`/`--all`, repeated `-p`/`--package` and `--exclude`
are supported. A package spec may be a name, `name@version` or `path+file:///...#name@version`, where
version and path disambiguate. With several packages selected, all compilation finishes before
execution begins, following Cargo's fail-fast rule.

### 2.5 Features

`--features`/`-F`, `--all-features`, `--no-default-features`, `package/feature` and a direct
dependency's `dependency/feature` are supported. Resolver 2/3 unify features for the same package and
dependency category while keeping build dependencies separate from normal and dev; resolver 1 unifies
across normal, dev and build purposes as Cargo does. Host and target artifacts are still produced
separately per purpose.

### 2.6 Version and lockfile

`rust-version` is read from the package, workspace inheritance and the registry index. Resolver 3
prefers candidates compatible with the workspace's lowest Rust version; Cargo configuration may set
`allow` explicitly, and `--ignore-rust-version` disables both that preference and the pre-compile
version rejection. A fresh lock writes v3 when the lowest version is at or below 1.82 and v4 from
1.83. All members share the workspace-root `Cargo.lock`; without one, Cargo's rules apply — solve once
over every feature reachable from all members, then write the unified lock atomically — while actual
compilation enables only the user's selected features. `--locked` with no lock fails immediately, and
the produced result must also be accepted by the pinned Cargo under `--locked --offline`.

## 3. Boundaries

- Cross-compilation targets are not in the committed scope and must error explicitly (C2, C3).
- Git source replacement, Git URL target patches and the remaining full Cargo config surface are
  unimplemented.
- Inputs Cargo rejects, such as nested workspaces and duplicate member names, follow Cargo's error
  instead of a permissive mirvm rule.
- The resolver scope is 1/2/3 only.

## 4. Verification

- `make suite S=contracts.cargoless-test`: fixed Cargo/rustdoc selection of test, bench and doctest,
  compilation shape, diagnostics, output and exit code, against `tests/suites/contracts/fixtures/cargoless/test-contract`,
  `cless_proc_macro_test_contract` and `cless_doctest_contract`.
- `make suite S=contracts.cargoless-workspace`: fixed Cargo resolver 1/2/3, complex member globs,
  workspace lints, package specs, features and failure propagation, against
  `tests/suites/contracts/fixtures/cargoless/workspace-contract` and `cless_workspace_remaining_contract`.
- `make suite S=differential.cargoless`: the Cargo path and the cargoless path must match
  byte-for-byte.

The fixtures cover: library, bin and integration tests, a plain example, bench, custom harness, build
script, path dev-dependency, panic, ignored tests, failure propagation, bin subprocess and the test
profile; root proc-macro normal dependencies, dev-dependencies, unit test and integration expansion;
plain, `no_run`, `ignore`, `compile_fail` and `should_panic` doctests plus root library dev-dependency
and build.rs cfg/env placement and Cargo argument conflicts; and, for workspaces, resolver 3 with a
virtual manifest, four members, an excluded package, inheritance, target-conditional dev-dependencies,
default/explicit/dependency features, cross-root transitive convergence, optional dependencies
activated by features, automatic integration tests, a unified build and a unified lockfile — plus an
old-style workspace with no `resolver` key covering the resolver 1 default, feature unification across
purposes, complex globs, workspace lints and full-path package specs.

Both contract scripts keep three judgment layers:

- **structure** — read the pinned Cargo `-vv` rustc/rustdoc lines and pin `--test`, `cfg(test)`,
  dependency kinds, example and bench, the root proc-macro dynamic library, resolver 1 feature
  merging, workspace lints, `CARGO_BIN_EXE`, the doctest root library and dev-dependencies, and
  build.rs cfg placement;
- **result** — compare stdout, stderr and exit codes across native Cargo, `MIRVM_DEPS=cargo` and
  `MIRVM_DEPS=self`, normalizing only thread ids, durations and Cargo's own build progress lines;
- **mechanism** — the self leg replaces `cargo` on `PATH` with a must-fail sentinel and audits every
  `execve` with `strace`, which also catches absolute-path invocations, and a hot re-run checks that
  build scripts still execute exactly once. This prevents "same result but quietly fell back to Cargo"
  and increment invalidation. The workspace contract additionally deletes the lock, has the self path
  rebuild it, then hands it to the pinned Cargo for `--locked --offline` review.

Pure-function assertions about argument shapes stay as unit tests in `resolve/` and `schedule.rs`. A
contract script freezes once it can judge the real current workload; no manifest entries, snapshot
formats or provenance metadata are added for future Cargo fields.

### 4.1 Cargo upgrade procedure

Raising `rust-toolchain.toml` must not simply refresh expected values.

1. With both the old and the new Cargo, run `test -vv --no-run`, the bench and proc-macro structure
   probes, and the contract scripts' native legs against the fixed fixtures.
2. Explain every difference in rustc arguments, environment, target selection or output by hand,
   separating a Cargo behaviour change from a pure diagnostic-text change. For a behaviour change, fix
   manifest/resolve/schedule/driver and their unit tests first, then the contract assertions;
   loosening a normalization rule to make tests green is not allowed.
3. Run `make suite S=quality.rust`, both contract suites, `make suite S=differential.cargoless` and
   finally `make test`. Only after all three tracks pass does the new pinned Cargo become
   authoritative, and the workspace contract's `-vv` structure checks must be re-reviewed by hand.

Cargo's mainline evolving never changes a released mirvm automatically: every mirvm version binds one
explicit toolchain, and an upgrade moves the whole contract forward through the review above rather
than guessing the Cargo version at run time.

## 5. Open items

- Cross-compilation targets, Git source replacement, Git URL target patches and the remaining Cargo
  config surface stay unimplemented; until they land, a request must fail loudly rather than silently
  ignore the argument.
- The full Cargo compatibility track grows, but this contract's boundary does not move with it: a new
  field is implemented before it is asserted.
- The resolver scope stays at 1/2/3; a new resolver number is a reopen trigger.
