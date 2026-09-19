# mirvm Test Suite

All test assets live under `tests/`; nothing test-related sits at the repository root. `make` is the
interface and `tests/run.sh` is the implementation it forwards to — those two are the only entry
points.

```text
tests/
  run.sh                  implementation: product build, tier selection, suite discovery, verdicts
  support/harness.sh      the shared library every suite uses
  suites/<category>/*.sh  leaf suites; the suite id is derived from the path
  scripts/                guest programs mirvm runs, one namespace:
                            c_<name>.rs       corpus driver, registered in cases.manifest
                            vmcall_<name>.rs  --vm-call exported-entry probe
                            probe_<name>.rs   a program exactly one suite owns
                            <name>.rs         differential program
  projects/               real Cargo projects as git submodules, pinned to one upstream commit
  suites/<category>/fixtures/
                          the non-program assets a category owns: fixture crates, oracles,
                          input data, the fake runners harness.truth drives
  tsan/                   standalone crate compiling src/vm under ThreadSanitizer
```

## Commands

```bash
make test       # daily commit check
make smoke      # + small real workloads and runtime semantics
make gate       # full gate: strict corpus, dependency image, performance limits
make list       # every suite id with its one-line purpose
make suite S=<id> ARGS="..."
make projects   # fetch the tests/projects submodules
make clean      # drop disposable caches
```

`make list` is the suite inventory: ids come from `tests/suites/` and each script's second line is
its purpose. `gate` must cover everything the lower tiers cover. For the Cargo compatibility track,
run `MIRVM_DEPS=cargo make gate`.

## Verdicts

An item is:
- `PASS`: ran and met the requirement.
- `FAIL`: product, authority or harness does not meet it (fails the suite).
- `SKIP`: the host genuinely lacks the capability, never a missing tool, fixture or manifest.
- `XFAIL`: a registered gap failing with an exact exit code and diagnostic
- `XPASS`: a registered gap turned green, so the contract must be updated (fails the suite).

Suite exit codes: `0` clean, `1` FAIL or XPASS, `64` usage error, `69` environment unavailable,
`77` the whole suite skipped for lack of host capability. Every leaf suite ends with
`suite_summary <id>`; the entry point judges by exit code, never by grepping output.

## What the harness owns

`tests/support/harness.sh` holds everything more than one suite needs; a suite that repeats any of
it is a bug in the suite.

- `suite_init [--no-product]` is the whole bootstrap: repository root, pinned toolchain, product
  binary, and a private `$TMP` with cleanup already armed.
- `corpus_select` and `corpus_run` handle corpus batch selection and driver execution.
- `normalize_stderr` is the only stderr filter allowed by default — it hides the panic header's
  thread name and TID. Anything else a suite filters, it must justify.
- `rustc_host`, `require_pinned_cargo`, `require_executable`, `ensure_test_sysroot`, accounting,
  timing, and the disk and cache guards.

Suites keep their own comparison logic on purpose: each encodes a different contract (native
baseline with an expected exit code, xfail frontier, warm rerun, capability SKIP), so one shared
comparator would need an option per contract.

## Rules

- A new suite script goes in `tests/suites/<category>/`. Its second line is a one-line purpose; a
  suite that does not need the product binary declares `# product: no` on line 3. Nothing is
  registered by hand.
- A suite sources `tests/support/harness.sh` and then calls `suite_init`. It must not resolve a
  toolchain, validate the binary, or create its own temporary directory.
- The header states what is tested, why it is needed, who the behavioral authority is, and which
  inputs are compared — in the present tense. No milestone history, no per-change narration, and no
  milestone codenames anywhere, fixtures included.
- A differential compares stdout, stderr and exit code. When native Rust or fixed Cargo is the
  authority, the authority side must first reach its expected exit code; both sides failing the same
  way is not a `PASS`.
- Committed fixtures are read-only. Copy to `mktemp -d` before modifying, and leave
  `git status --short` clean when the suite ends.
- Suites run standalone and serially, must not depend on state another suite left behind, and must
  not use `set -e`: expected non-zero exits are part of the contract, so every external command's
  exit code is captured and judged explicitly.
- A new guest program goes in `tests/scripts/` as one of the four kinds above. A new corpus driver
  is registered in `cases.manifest` in the same change; a driver nobody registers silently never
  runs, and a registered name without a driver fails the batch loudly.
- A new real project becomes a pinned submodule under `tests/projects/`, never vendored, with
  `needs=tests/projects/<name>/Cargo.toml` in its manifest row so an uninitialized checkout records
  `SKIP` instead of failing.
- Adding a suite starts with the assertions that can fail; confirm `make list` discovers it, state
  its tier in `tests/run.sh`, then run `bash -n`, `make suite S=<id>`, `make test`, and `make smoke`
  or `make gate` as the scope requires. A change to the entry point, state propagation or the
  summary must extend `harness.truth` first.
