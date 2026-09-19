# mirvm tests

Three things, and nothing else:

- `manifest` — every case, one line each: the asset inventory **and** the metadata the runner needs.
- `lib/` — all the code that runs tests: the dispatcher, the shared library, one file per run method.
- `data/` — all the assets: guest programs, fixture crates, oracles, inputs, external projects.

Control and data never mix: no script lives under `data/`, and no asset path is spelled out in
`lib/`. `make inventory` enforces both, together with "every data file is referenced by some case".

```bash
make test        # fast tier: the daily check
make smoke       # fast + smoke tiers
make gate        # every tier except manual
make list        # every case, with its mode and tier
make modes       # the available run methods
make case C=<id> [ARGS="..."]
make mode M=<mode> [ARGS="..."]
make inventory
make projects    # fetch the data/projects submodules
```

`make` is the interface and `tests/run.sh` is the implementation. `tests/run.sh` takes the same
commands (`tier`, `case`, `mode`, `list`, `modes`, `inventory`) if make is unavailable.

## Layout

```text
tests/
  manifest                one line per case: <name> <mode> <tier> <timeout> [key=value ...]
  run.sh                  the dispatcher: select cases, run each through its mode, summarize
  lib/harness.sh          shared library: bootstrap, field access, execution, comparison, accounting
  lib/helpers/            tool scripts a mode hands to something else (the rustc proxies)
  lib/modes/<mode>.sh     one run method each -- the only place a method is named
  data/programs/          guest programs mirvm runs (c_*, vmcall_*, probe_*, the differential set)
  data/projects/          real Cargo projects as pinned submodules
  data/fixtures/          fixture crates, oracles, inputs, the fake product and fake Cargo
```

## A case

```text
fib                native-diff  fast   120   input=programs/fib.rs
blake3             corpus       smoke  90    input=programs/c_blake3.rs verdict=oracle:blake3
hexyl              project-diff gate   180   input=projects/hexyl needs={DATA}/projects/hexyl/Cargo.toml
vmcall-digest      vmcall       smoke  120   input=programs/vmcall_digest.rs calls=vec_digest(10):155;...
```

- `name` is the case id; `mode` names the run method; `tier` places it in the profiles
  (`fast` ⊂ `smoke` ⊂ `gate`; `manual` only when named); `timeout` is seconds per product invocation.
- Everything after column 4 is mode metadata. The runner refuses a field the mode does not declare
  and a missing required field, so a typo fails loudly instead of being ignored.
- Values cannot contain spaces: write `%20`. `{DATA}` is the data root, `{ROOT}` the repository root.
- `needs=` names an asset (or a machine-local path) whose absence makes the case **SKIP**, never FAIL.

## A mode

A mode is one file in `lib/modes/`. It declares

```bash
MODE_FIELDS="input env args"     # the fields it reads; anything else in a case line is an error
MODE_REQUIRED="input"            # fields a case must provide
MODE_PRODUCT=no                  # "no" when the case must not require the product binary
mode_run() { ... }               # the method itself
```

and nothing else runs at source time. `mode_run` gets the case in `CASE_NAME`, `CASE_MODE`,
`CASE_TIER`, `CASE_TIMEOUT` and its fields in `CASE_FIELDS`, and reads them with `field` /
`field_required`. It reports through `ok` / `bad` / `skip` / `red` and returns non-zero on failure.

A declarative mode is driven entirely by generic fields (`native-diff`, `corpus`, `pair`,
`project-diff`, `cargo-diff`, `vmcall`, `run-expect`, `timed-run`, `tsan`, `compile-crate`,
`repo-quality`, `metering`). A bespoke mode owns machinery a field list cannot express — generating
registries at run time, auditing `execve` with strace, building a C++ oracle, decoding capture files,
driving the framework with fakes — and is declared like any other case, so the inventory stays
complete and the asset paths stay in the manifest. Each mode's header states what it does and which
fields it reads.

## Verdicts

A checked item is one of:

- `PASS` — ran and met the requirement.
- `FAIL` — product, authority or harness does not meet it; fails the case.
- `SKIP` — the host genuinely lacks the capability, never a missing tool, fixture or manifest.
- `XFAIL` — a registered gap failing with an exact exit code and diagnostic.
- `XPASS` — a registered gap turned green, so the contract must be updated; fails the case.

Case exit codes: `0` clean, `1` FAIL or XPASS, `64` usage error, `69` environment unavailable,
`77` the case was skipped for lack of host capability. A profile run prints
`== <selection>: N passed, N skipped, N failed ==` and exits non-zero if anything failed.

## Rules

- A new guest program goes in `data/programs/` and gets a case line. One namespace, four kinds by
  prefix: `c_*` is a corpus driver, `vmcall_*` is driven through `--vm-call`, `probe_*` is owned by
  exactly one bespoke mode, and the rest are differential programs compared against native.
- A new run method goes in `lib/modes/` and is named only from the manifest. There is no separate
  notion of a suite: the mode is the method.
- No case-specific path, timeout, tier, env, argument or expectation outside `tests/manifest`; no
  case logic outside `lib/`. `make inventory` fails on a data file nobody references and on a script
  under `data/`.
- A differential compares stdout, stderr and exit code. When native Rust or fixed Cargo is the
  authority, the authority side must first reach its expected exit code; both sides failing the same
  way is not a `PASS`. `normalize_stderr` is the only default filter, and anything else a mode
  filters has to be justified in its header.
- Committed data is read-only: copy to `mktemp -d` before modifying, and leave `git status --short`
  clean when the case ends.
- Cases run standalone and serially, must not depend on state another case left behind, and must not
  use `set -e`: expected non-zero exits are part of the contract, so every external command's exit
  code is captured and judged explicitly.
- Adding a case: write the assertion that can fail first, add the line, run `make inventory`, then
  `make case C=<id>` and the tier it belongs to. Adding a mode: keep its fields declared, and extend
  `framework-self-test` when the change touches the dispatcher, the accounting or the field contract.
