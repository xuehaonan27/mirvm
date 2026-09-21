# tests/data/fixtures/tsan/ — the execution phase under ThreadSanitizer

`mirvm-tsan` is a standalone crate (the empty `[workspace]` keeps it out of the root crate's
dependency graph). It exists for two structural reasons, and it is the only place in this
repository where either one is enforced.

## 1. The race verdict

The product binary links `rustc_private` dynamic libraries and installs its own allocator, so
it cannot be instrumented as a whole. But `src/vm` — the execution phase — is deliberately
pure Rust with no `rustc` types, so this crate compiles it **source-for-source**
(`#[path = "../../../src/vm/mod.rs"]`) under `-Zsanitizer=thread -Zbuild-std` and runs the engine's
concurrency cases for real.

Verdict: **exit 0 and not a single `WARNING: ThreadSanitizer`.** Cases keep guest memory
race-free by construction (guest races are out of contract), so any report is an engine bug,
not a test artifact.

This matters because every other gate in the repository compares output — stdout, stderr, exit
codes. Those checks are structurally blind to a data race: a racy engine produces correct
output until it doesn't. The architecture's central claim ("no GIL, 1:1 real threads, one
read-only `Shared` plus a per-thread `Ctx`") can only be falsified here.

## 2. The purity fence

Building this crate is what keeps `rustc_private` out of the execution phase. Only `blake3`,
`libc`, `libffi`, `libmimalloc-sys`, `serde`, `postcard` and `memmap2` are in scope, so a single
`rustc` type leaking into `src/vm` fails this build. `cargo check --all-features` can never
catch that: the root crate has `rustc_private` by design. This property is what makes the L2 IR
cache and the `.mirvm` package self-contained, tcx-free artifacts; it has already been broken
once (a `CodeDomain` enum placed behind the `cranelift` gate), and `runtime.semantics` runs the
compile half as a "purity gate" step for exactly that reason.

## Layout

```
tests/data/fixtures/tsan/
  src/main.rs            crate root: the shared-source adapters, then cases::run_all()
  src/product_adapters.rs  stubs for the rustc-dependent leaves src/vm references
  src/telemetry.rs       adapter: keeps the name `telemetry` so src/vm's crate::telemetry resolves
  src/cases/mod.rs       the case registry (ids + run order) and the verdict printer
  src/cases/*.rs         one file per area of the engine
  src/cases/mixed_stack/ four mixed interp/compiled-stack cases with their private bytecode,
                         frame and memory helpers (originally spikes 3 and 4)
```

Crate-root modules named `arch`, `diag`, `native`, `options`, `os`, `os_arch`, `store`, `telemetry`,
`utils`, `vm` are **not** a style choice: `src/vm` is compiled verbatim and refers to exactly those
paths.
`crate::telemetry` is the source-shared adapter, `crate::diag` is the std-only diagnostic
vocabulary, `crate::native` carries only the pure-Rust symbol reader and byte layouts, and the rustc-dependent
leaves (`lower`, and with them `sysroot`) are stubbed in `src/product_adapters.rs`.

## Cases

- `mixed-stack-fib`: 8 threads run interpreted and compiled frames in parallel on one native stack,
  and the shared read-only program is read lock-free.
- `atomic-cross-tier`: interpreted `AtomicRmw` and compiled `fetch_add` hammer the same real address,
  so the interpreter must issue a real host atomic.
- `blocking-io-liveness`: a real blocking `read(2)` stalls only its own OS thread (the isomorphic
  program deadlocks under cooperative scheduling).
- `mixed-stack-unwind`: 8 threads unwind mixed stacks concurrently — per-thread panic, `Drop` in guest
  order, catch — with per-thread logs.
- `engine-atomics-thunk-cache`: the product engine: 8 threads attach, run guest atomics, and race the
  thunk factory's `Mutex` cache (the same key must yield the same code).
- `capture-session-lifecycle`: two live producers, page publish/return/rescue, and a committed capture
  ledger.
- `engine-close-race`: `close()` racing 8 threads inside `run_export`: every result is a value or a
  structured `EngineClosed`, the seal rejects everything later, the engine leaves the registry, and
  both deferred windows (hold, in-flight TSD op) survive the close.
- `guest-threads`: 6 rounds × 8 threads attaching, interpreting and retiring, with tracked
  `pthread_setspecific` set/clear through the deferred TSD registry; the thread count reads back to
  the baseline after the joins.
- `signal-delivery`: cross-thread `pthread_kill` into a per-thread inbox — the handler runs once per
  raise on the owning pthread, a neighbour's inbox stays empty, and a blocked raise stays pending until
  unblocked.
- `fork-guard`: `fork()` with guest threads live — the child's generation advances once, its fork
  baseline is repaired to the child, the recording syscall returns the child's pid, and the parent's
  engine keeps interpreting afterwards.

Every case prints exactly one `PASS <id> ...` / `FAIL <id> ...` line. `tests/lib/modes/tsan.sh`
holds the list of ids that must appear, so a case that stops running or gets renamed fails the suite
instead of quietly looking like a pass.

## What this does not cover

* **The JIT.** The shared `src/vm` gates its JIT half on `feature = "cranelift"`, and this crate
  does not provide cranelift, so `src/vm/jit/**` is compiled out here. The JIT worker's
  slot/`trace_enter` publication and the trace-domain pinned-register path are therefore **not**
  instrumented. Closing that gap means adding the cranelift dependency set plus a `cranelift`
  feature to `Cargo.toml`; the build gets slower, which is why it is a separate decision.
* **Two things sanitizer mode makes unreachable**, measured rather than assumed:
  * *TSD teardown rounds.* `ctx` installs the `Ctx` pthread-key destructor as `None` under
    `cfg(sanitize = "thread")`, so the destructor's re-hang rounds never run here; the case
    churns attach/run/retire instead. The rounds are covered by `threads_panic` in the
    unsanitized differential suite.
  * *The fork child's capture rebuild.* TSan refuses to start threads after a multi-threaded
    fork (`ThreadSanitizer: starting new threads after multi-threaded fork is not supported.
    Dying`, exit 66 — reproduced both with a `clang -fsanitize=thread` probe and in-harness).
    The engine's only post-fork thread creation is the capture writer that
    `rebuild_session_from_recipe` spawns, so `fork-guard` deliberately runs without an armed
    session and stops before `fork_child_guard` sets the rebuild flag. Everything else the
    engine does in a fork child — generation advance, service-thread reset, baseline repair —
    is asserted.
* **Guest races.** Cases must not create unsynchronized guest memory access; concurrency
  *through engine APIs* (close racing a call, cross-thread signal delivery) is the point.
* **Platforms other than Linux/x86_64**, like the rest of the repository. `src/os_arch` is the
  pair this build is for, and the crate root declares it next to `os` for the same reason.
* **Everything below the engine**: `src/os`, `src/arch` are compiled in (the engine calls
  through them) but the harness does not exercise their concurrency on its own.

## Running it

```bash
make case C=tsan     # the gate; runtime.semantics calls it too
```

Formatting: this crate is outside the root workspace, so `cargo fmt` at the repository root
does not cover it. `rustfmt --edition 2024 tests/data/fixtures/tsan/src/cases/*.rs tests/data/fixtures/tsan/src/product_adapters.rs`
works; `tests/data/fixtures/tsan/src/main.rs` and `tests/data/fixtures/tsan/src/telemetry.rs` must be checked with
`--config skip_children=true`, because rustfmt cannot resolve the module tree behind the
`#[path]` includes (it looks for the children of `capture.rs` in the wrong directory).

`runtime.semantics` is in the `smoke` and `gate` profiles, not in `fast`. `SKIP_TSAN=1` skips
the execution (the purity compile still runs) and the gate reports it as a SKIP rather than a
PASS. The harness needs `MIRVM_BUILD_ID` at compile time (the capture module reads it with
`env!`); the suite supplies zeros.

## Adding a case

1. Add `src/cases/<area>.rs` with `pub(crate) fn run_<id>() -> bool` that prints exactly one
   `PASS <id> ...` / `FAIL <id> ...` line.
2. Register it in `src/cases/mod.rs` (module, `IDS`, and the `run_all` list).
3. Add the id to `EXPECTED` in `tests/lib/modes/tsan.sh`.
4. Prefer hand-built `Module`s and real OS threads, as the existing cases do: lowering needs
   `rustc`, which this crate cannot link.
5. Verify: the case must pass, and it must **fail** when the invariant it pins is broken —
   a case that cannot fail is not evidence.
