# mirvm — current status

> What mirvm does today, how a run flows, what has been measured, what is not claimed, and what comes
> next. Current code and reproducible tests win over any plan; open debt and refusal boundaries live
> only in [open-issues.md](open-issues.md).

## 1. Implemented today

mirvm runs whole Rust programs built for the pinned toolchain `nightly-2026-07-02` on a
Linux/ELF/x86_64 baseline: rustc MIR is lowered into mirvm's own typed bytecode and that bytecode is
executed in a VM. The design contract is [docs/designs/README.md](designs/README.md); the per-topic
contracts are its siblings in `docs/designs/`.

- **Interpreter** — a tcx-free tree walk over the typed bytecode: scalar and SIMD intrinsics, true
  stack depth, `f16`/`f128`, atomic ordering, 128-bit forms, nested DSTs, `volatile`, `dyn` tail
  fields. An exhaustive verifier runs at the pack, image, L2 and final-execution entries.
- **JIT** — method-level Cranelift, on by default (`MIRVM_JIT=off` falls back to pure interpretation).
  The translator is exhaustive over the statement, rvalue and terminator tables: frame model v2 with
  memory operands, the full scalar set, 128-bit and atomics, all ABI shapes, the five call helpers,
  and productized unwind with a full LSDA over both CIEs. `MIRVM_JIT_SYNC` publishes synchronously, so
  `MIRVM_JIT_THRESHOLD=1` differentials prove compiled code really ran. Compiled symbols persist in the
  machine-local `cache/jit` store keyed by `(fragment, jit-key)`, with relocation deferred to load: a
  warm session links the heat order before serving a request and takes each function's tier from
  `cache/package-heat` (`MIRVM_NO_JIT_CACHE`, `MIRVM_JIT_RELOAD`, `MIRVM_JIT_LEDGER`).
- **FFI and unwind** — guest to native through `dlsym` + libffi with the source ABI (C or C-unwind);
  native to guest through libffi closures plus TLS attach; aggregates marshalled by value (frozen
  layout, libffi struct grouping, sret). Exceptions keep their source ABI: plain C aborts, C-unwind
  carries C++ typed exceptions or Rust panic payloads through interpreter and JIT frames and runs
  cleanup, and non-C/System ABIs are refused.
- **Threads, TLS, signals** — real guest threads and TLS, one signal inbox per owning Engine,
  pthread-exit TSD teardown, and a fork guard that admits `fork` only while the guest is
  single-threaded. `exec` passes through.
- **Cargo** — the compat track occupies Cargo's `RUSTC` slot, so ordinary and workspace wrappers keep
  Cargo's own composition and ordering. `MIRVM_DEPS` defaults to `self`: `mirvm run`, `mirvm prepare`
  and `mirvm test` resolve, schedule and build with no cargo in the process, using the in-tree
  `src/cargoless/` stack.
  `MIRVM_DEPS=cargo` remains the user fallback and behaviour referee, and both tracks are compared
  continuously; on that track `prepare` is `cargo build`, since Cargo's own cache is what a following
  run would reuse. `mirvm test` covers lib/bin/test/bench/example with libtest or a custom harness, and a
  rustdoc front end adds doctests without pretending they are ordinary tests.
- **Images and caches** — a byte-deterministic std base image plus a deps manifest make warm runs load
  instead of re-lower. The manifest stores each dependency function as a fragment in the shared
  content-addressed `cache/frags` store, its frozen region as 4 KiB chunks in `cache/frozen`, plus the
  binding table that gives that fragment's ordinals meaning, so identical bodies and an unchanged
  region are stored once machine-wide. On the cargoless track the closure is
  split by crate: `cache/units` holds one content-named manifest per unit (keyed by build id, base key
  and rlib stamp), a stack `[std base, unit…]` serves two programs that share a dependency, and a layer
  the stack provides is never rewritten. An image stack carries multi-source lookup, cumulative
  offsets and a key chain, and an L2 post-mono engine-IR entry is a manifest of fragments behind a
  header, so an edit reuses the bodies and frozen chunks it did not change. `MIRVM_TIMING` prints a
  phase ledger.
- **Packaging** — `.mirvm` packages (mode B) are validated program images: `Package::load` copies the
  source into a process-owned immutable snapshot and each `unsafe instantiate` builds an independent
  Engine, so one package instantiates repeatedly and concurrently. Artifacts use logical `LinkAddr`
  values with per-Engine mapping of frozen/TLS/native/MC images and callback entry closures.
- **Test and capture tooling** — `make` is the interface (`test`/`smoke`/`gate`, `list`,
  `case C=<id>`, `mode M=<mode>`, `inventory`, `projects`, `clean`) and `tests/run.sh` is the
  implementation it forwards to. `tests/` holds exactly three things: `manifest` (one line per case:
  name, mode, tier, timeout, and the metadata the runner needs), `lib/` (the dispatcher, the shared
  library, and one file per run method) and `data/` (guest programs, fixture crates, oracles, inputs,
  and the external projects as pinned submodules). No script lives under `data/`, no asset path is
  spelled out under `lib/`, and `make inventory` checks both, plus that every data file is referenced
  by some case. Telemetry captures internal syscall events, decodes and inspects them offline, keeps
  a process-wide page pool and registers JIT address ranges for perf-maps only at an explicit stop.

## 2. Implementation path

```text
.rs / Cargo project
  -> cli / cargo wrapper + runner
  -> rustc_driver::after_analysis (tcx is available during lowering; the callback only lowers)
       collector seed + call-site worklist -> monomorphization closure
       freeze layout / ABI / place / statics / vtable / TLS / FFI signatures
       materialize supported asm stubs and constrained static native archives;
       unknown constructs degrade to a Trap with diagnostics
  -> tcx-free ir::Module
  -> runner installs a narrow TRACK_DIAGNOSTIC filter only after lowering
  -> run_compiler finishes tcx/diagnostics/compiler drop, then the filter is restored
  -> the Module runs only on compiler success
  -> per-Engine Shared + one CtxSlot per host thread (heavy resources cleared on close)
  -> interp_frame / run_blocks
       guest call: host recursion
       guest -> native: dlsym + libffi (C / C-unwind by source ABI)
       native -> guest: libffi closure thunk + TLS attach
       inline asm: call the materialized fn(*mut u8) stub
       guest panic / EngineFault: MIRVM-owned exception class, classified per frame
       real lang_start main: MainPanicBoundary + a state stack per run_main
       exit: Returned / GuestPanic / RunErrorKind; normal Termination 101 is not a panic
       C++ foreign: not consumed or rewritten by the Engine, unwinds by its own type
  -> close: leases and deferred work drain -> native fini (escaping exception = diagnostic + abort)
       -> Ctx/Shared reclaimed; published closure/JIT/MC/native addresses leave tombstones only
```

Hot spots are `src/lower/func/` (call and calling-convention lowering), `src/vm/interp/`
(interpreter main loop) and `src/vm/jit/translate/` (translator). The implementation is
Linux/ELF/x86_64 first — it depends on pthread, `dlopen`, GNU link behaviour and x86 asm wrappers —
and that is the only platform baseline that may be claimed.

That baseline is named in three layers, and nothing outside them names a platform item. `src/arch/`
is the CPU: instruction encoding and execution, register and feature facts, the ELF machine identity
and the assembly vocabulary. `src/os/` is the platform outside mirvm, the C library and the kernel
together: page size, mappings, `dlopen`, `/proc`, process and signal primitives, `errno`. `src/os_arch/<os>_<arch>/` is the two at once, which in practice means the kernel ABI as the
CPU encodes it — signal frames and restorers, raw syscall sequences, the fixed-address layout, kernel
TLS. An object-file byte layout is deliberately none of the three: it does not vary with the CPU or the
kernel, so it lives in the layer that produces and parses those objects (`src/native/object/{elf,ar}.rs`),
with `e_machine` on the CPU axis and everything the loader does with an image on the platform axis. Each axis declares its surface
in its own `mod.rs` and dispatches through one `#[cfg]` ladder, so a call site names
`crate::os::signal::…` or `crate::arch::asm_text::…` on every target. `repo-quality`'s
`platform boundary` gate holds it: no `libc` constant or protocol function, no `asm!` site, no host
`cfg` and no `std::os::unix` outside those three trees.

## 3. Verified boundaries

Measured on this tree against the Linux x86_64 release build, with the cases that fetch run under
`the proxy helper` ([environment.md](environment.md)); `tests/README.md` documents the suites.

- `cargo fmt --check` and `cargo clippy --locked --all-targets --all-features -- -D warnings`: clean.
- `cargo test --locked --all-features`: 499 pass, 1 ignored.
- `make test` (the fast tier, 73 cases): 69 pass / 4 fail; one run measured 68 pass / 5 fail because
  `quality` was RED on E48's flaky test.
- `make smoke` (fast + smoke, 119 cases): 107 pass / 12 fail.
- `telemetry` PASS; `tsan` PASS with zero warnings and all ten concurrency cases; `quality` PASS,
  which is `repo-quality`'s 13 source checks.
- Base image byte-determinism: 6 builds (3 at `MIRVM_THREADS=1`, 3 at `=8`) produce one key.
- Timing gates: `load` 155ms against its 1000ms ceiling; `fib32` RED (E40).

The fast tier's failures are the four `cargo-diff` rows that report FAIL rather than SKIP when their
materialized project directory is absent (`ecosystem`, `ffi_zlib`, `ripgrep_regex`, `warning_return`;
G2).

The smoke tier adds eight: the seven real-project REDs of E47 — four rayon-family timeouts (`rayon`,
`flate2`, `brotli`, `tiny_skia`), two aborts (`mlua_lua`, `wasmtime_wat`) and one genuine trap
(`png_round`: `foreign llvm.x86.pclmulqdq.512`, the intrinsic queue) — plus `main-panic-jit` (E45).
`tokei` is a gate-tier case and is not in this tier.

`fib(32)` is RED: the best of three runs is about 8.7s against its 80ms gate (E40). Output is correct
and the JIT is effective, so the gate is not relaxed — a completely green `gate` must not be claimed.
The gate tier as a whole has not been re-run; the numbers above cover `make test`, `make smoke`, the
runtime suites and the base image.

Build notes: debug and release both build, and only the release binary is executed — under the pinned
LLVM 22 the release profile must keep `debug=2` with `strip="debuginfo"` and `codegen-units=256`, or
the release cleanup chain miscompiles.

Oracle discipline: an oracle must be observable output or an invariant; "both sides failed" or "exit
codes match" is never success. Deferred cases are refused loudly as a separate `p5` bucket instead of
counted as failures.

CI (`.github/workflows/ci.yml`) installs the pinned toolchain plus `strace` and `ripgrep`, then runs
`make test` and `make case C=tsan`. It stops there on purpose: `smoke` and `gate` add the
corpus and the timing gates, which need a machine larger than a shared runner. GitHub-side operation is
paused; the local equivalent is authoritative.

## 4. Gaps and honest limits

What is refused or not yet claimed — not what is planned.

- **Platform**: Linux/ELF/x86_64 is the only claimed and tested baseline; the macOS/aarch64 axis is
  implemented but no test, `Makefile` target or CI job names it (E26).
- **Arbitrary Rust**: not supported. The corpus holds real projects and a small workload set; those
  are not a continuous gate and do not generalize.
- **JIT**: no OSR, no deopt, and no within-run tier upgrade — a function's tier is the previous run's
  heat order (E42). Close stops and joins compile workers and releases `Shared`, but published JIT code
  and its `.eh_frame` live to process end.
- **Signals**: synchronous faults in a guest handler, realtime signals and
  `SA_SIGINFO`/`SA_ONSTACK`/`SA_NODEFER`/`SA_RESETHAND` are refused. Process-directed external signals
  are promised only at the owner Engine's next safe point.
- **backtrace**: the `_Unwind_Set/GetGR/SetIP/Resume` and beyond-CFA context family stays
  `Unsupported`. `atexit` is a builtin; `dl_iterate_phdr` still goes through native FFI.
- **fork / exec**: `fork` is admitted only while the guest is single-threaded; multithreaded fork and
  the `vfork`/`clone`/`setjmp` families are loud refusals.
- **IR / ABI**: `volatile` uses alignment=1 opaque `MaybeUninit` carriers and promises no atomicity
  beyond 16 bytes. Slices and `str` keep static formulas, other nested DSTs are refused, and not every
  128-bit ABI shape is scalarized. The pointer coercions (`Unsize`, `MutToConstPointer`,
  `UnsafeFnPointer`, `ArrayToPointer`, `ReifyFnPointer`, `ClosureFnPointer`) are all resolved.
- **Static archives**: non-PIC, thin archives, cross-archive dependency/ordering/duplicate exports and
  export-symbols are refused. There is no multi-archive link plan and this is not a general linker.
- **Logging / profile**: no 1-byte raw syscall site, no profile command, no separate trace interpreter
  loop, no 4→64 KiB adaptation; the trace code domain is frozen at Engine construction rather than
  chosen at the outermost activation.
- **Embedding**: `instantiate` stays `unsafe` — the native/FFI ABI, hand-written Modules and raw
  two-machine-word exports must be trusted, `Shared` is not public, and no safe typed export bindings
  are generated. Third-party callbacks have no universal revocation, so published closure/JIT/MC/native
  code lives to process end.
- **Distribution**: `.mirvm` is at unstable format v4; first load still decodes every function, and
  archive-direct verification, cross-build_id/target compatibility, fat artifacts and format freeze
  are unfinished. No daemon, REPL, checked mode, resource governance or real sandbox.
- **Cargo config**: Git source replacement, patches targeting a Git URL, and parts of the Cargo config
  surface such as `paths`/`HOST_RUSTFLAGS` are not implemented.
- **`mirvm doc`**: doctests work through a fixed rustdoc front end; independent `mirvm doc`/HTML
  generation is not claimed.

Architectural ambition is not qualification. "A reference implementation that runs in RAM" is a
long-term semantic contract; while the gaps above and a limited corpus remain, mirvm is not a finished
product covering all of Rust.

## 5. Development order

1. **Performance (D16).** Close the `fib(32)` RED by making it faster, never by relaxing the gate.
   `MIRVM_TIMING` and profile data decide JIT code persistence, direct archive verification/loading,
   background service threads and `-Cincremental`. Starting facts: lowering costs about
   `0.10 ms/instance` and behaves as a near-constant std tax (the executed set is only 7–29% of the
   lowered set, and 75% of the phase is rustc query/decoding machinery); the std base image takes a
   pure-cold script from 385ms to 104ms, a deps image takes an ecosystem cold run from 924ms to 66ms,
   and an L2 hit takes the warm load phase 11–15×. The gate itself is still RED: `fib(32)` is about
   8.7s against its 80ms ceiling (E40).
2. **Logging and telemetry.** Fork generations are closed. Next: the remaining direct hot path, the
   trace interpreter loop, then stateless inline-asm raw syscall sites — the first internal syscall
   slice must not be claimed complete before those raw sites land. Only variadic `libc::syscall` forms
   are recorded today; `std::fs` and `Command` go through their own builtins.
3. **Profile (parallel).** JIT address ranges and perf-map are done. Linux perf capture may run
   alongside the logging work, must report permissions, lost samples and missing mappings loudly, and
   must not switch the trace code domain.
4. **Data rulings.** Rule on 4/16/64 KiB page sizes, hard-pool numbers, writer batching and checksums
   under one memory budget, then implement 4→64 KiB auto-scaling.
5. **Diagnostics.** Preparation and execution are separate commands: `mirvm prepare` builds a guest —
   sysroot, dependencies, frontend, lowering, cache entry — reports the phases, and stops; `mirvm run`
   does the same work and starts the guest. A guest build's own compiler output is preparation detail,
   so `run` holds it back and releases it only when the build failed or the run asked for detail
   (`-v`, `MIRVM_LOG=info|debug`); the severity threshold behind the same option drops mirvm's own
   quieter lines, and never a failure. Guest fd1/fd2 are never held back. Capture tees
   compiler/control byte-for-byte into `diagnostics.log` from the command boundary, and guest fd2
   enters neither the router nor the event ring. Perf capture reuses that boundary. MIRVM-owned lines
   go through `src/diag`, which renders `mirvm[component]: severity: message` (one JSON object per
   line under `MIRVM_OUTPUT=json`) and owns the two sinks: routed (fd2 plus the capture tee) and
   direct (fd2 alone, for signal-adjacent and teardown paths where the tee lock would deadlock). Guest
   fd1/fd2, the rustc emitter's own rendering and the cargo-compatibility lines never pass through it.
   Remaining conversions are T13.
6. **Product capability.** Direct archive semantic verification needs a new offset-based read-only
   representation before a format freeze can be reviewed. OS-level sandboxing is deliberately paused.
   Remaining stage boundaries and acceptance criteria are in `open-issues.md`.
7. **Harness budget.** Touch the harness only when a current RED cannot be reproduced or judged, and
   never harden it for a hypothetical future.
8. **Remote work stays paused** until the maintainer explicitly resumes it (`open-issues.md` G1).
