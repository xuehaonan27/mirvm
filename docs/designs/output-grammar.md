# One output grammar and one error vocabulary

> Status: Implemented · Scope: what mirvm writes where, the line grammar and its machine rendering,
> the failure root every fallible signature names, and the source checks that keep a second spelling
> from appearing.

## 1. Contract

- **Two destinations, decided by what a line is.** The command's own result — a survey, a report,
  `--dump-mir`, `--vm-call`, the version and usage blocks — is product output: inherited fd 1, the
  renderer's own bytes, `src/out.rs` the only writer. Everything mirvm says *about* the work is a
  diagnostic: inherited fd 2 through `src/diag`, which owns the grammar.
- **The grammar is spelled once.** Text mode renders `mirvm[component]: severity: message`, or
  `mirvm: severity: message` when no component and no command scope is set. Machine mode
  (`MIRVM_OUTPUT=json`) renders one JSON object per line with the same fields plus the failure's
  registered `code`, its structured `details`, its `causes` and, for a failure, `exit_code`. The
  literal `mirvm[` outside `src/diag` is a second spelling of the grammar and a check rejects it.
- **Two sinks, and the difference is a correctness constraint.** Routed `emit` writes fd 2 and hands
  the same bytes to the capture tee, so `mirvm capture` records control diagnostics byte-for-byte; it
  takes the tee lock and is legal only on ordinary thread context. Direct `emit_direct` writes fd 2
  alone and takes no lock: the engine's Drop chain, the syscall trampoline and the target-pthread
  teardown run where that lock can deadlock, so their lines are deliberately absent from
  `diagnostics.log`.
- **A line's severity decides whether it is written at all.** The threshold (`-v`, `MIRVM_LOG`) has a
  default of `Warning`: a run reports what the user must act on and says nothing about the work mirvm
  did to get there. `Error` is the most serious discriminant, so no threshold can suppress a failure.
  A flag-gated development channel is not an event and must not be silenced by this threshold.
- **Four writers never pass through the vocabulary, deliberately.** Guest fd 1/fd 2 are the guest's
  own channels; the rustc emitter's diagnostics belong to rustc; the cargo-shim's mirror lines are
  Cargo's own text and a frozen surface; and the `MIRVM_*_DEBUG` instruments carry no component or
  severity, so they keep the shape and the destination their raw `eprintln!` had. The last two are the
  only raw writers, and both live in `src/diag` (`instrument`, `mirror`).
- **One failure type.** `crate::error::Error` is what every fallible signature names.
  `Error::Plain` carries a failure mirvm authors in place — its component, its class and its message,
  written as `fail!(Component, "…")` — and `Error::Owned` carries a module enum registered with
  `diag_codes!`, which is what a caller acts on or a machine consumer matches. A failure type spelled
  `String` loses that identity, which is the same loss that bans `anyhow`.
- **A process status is chosen once.** `src/diag/exit.rs` names every code mirvm chooses (`SUCCESS`,
  `FAILURE`, `USAGE`, `SOFTWARE`, `TEST_FAILED`); `Kind` maps a failure class onto them and
  `Error::report` is the only place a failure becomes a status. A code mirvm did not choose — the
  guest's, rustc's, the `SIGABRT` a signal fault must reproduce — travels through unchanged and is
  never renumbered.

## 2. Why

A machine consumer reads lines, not prose. `MIRVM_OUTPUT=json` is a stream of objects with a stable
`code`; the text rendering is the same fields for a human; the capture tee is a byte-exact copy of one
of them. That only holds if every line is produced by one renderer from typed inputs, so the checks in
§3 exist to keep a second spelling — a raw print, a digit at an exit site, a `String` error, a
hand-written `mirvm[…]:` prefix — from appearing beside it.

## 3. Enforcement

`repo-quality` runs these beside `cargo fmt`, `cargo clippy` and the unit tests. Each scans the
*product half* of a file: lines from the top, stopping where an inline `#[cfg(test)] mod … {` begins,
with test modules and test drivers skipped, because a test that pins the rendered bytes is evidence.

- **no string errors** — counts angle brackets to tell an error position from a success one:
  `Result<Vec<String>, E>` passes, `Result<Result<(), String>, E>` does not.
- **no bare exit code** — a digit at an exit site is a second spelling of `diag::exit`; `0` and the
  `SIGABRT` re-raise are the two declared exceptions.
- **no raw print** — `print!`/`println!`/`eprint!`/`eprintln!` under `src/` are refused outright;
  product output goes through `src/out.rs`, a diagnostic through `src/diag`.
- **one output grammar** — the `mirvm[…]:` prefix is spelled only in `src/diag`. Message *content* may
  repeat where two paths must report the same fact (the interpreter and the JIT share a trap
  contract), which is why the check is on the shape, not the words.

`diag purity` (already present) keeps `src/diag` std-only, because the TSan harness compiles it
source-for-source next to the engine.
