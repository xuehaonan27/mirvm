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
  fields. An exhaustive verifier checks raw IR and immutable archives; internal ownership evidence carries
  validated code into Engine construction without repeating the function walk.
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
  header, so an edit reuses the bodies and frozen chunks it did not change. A disposable bound image
  in `cache/execution` loads before stack discovery and carries its own complete frozen domains and
  immutable function archive. Mirvm-owned sysroots publish content-identified generations with local
  integrity receipts and process leases; ordinary external inputs retain full content hashing.
  [startup-cache-design.md](designs/startup-cache-design.md) owns these contracts. `MIRVM_TIMING`
  prints a phase ledger.
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

Linux x86_64 remains the authoritative execution target. Focused verification of the JIT's call boundaries,
frame storage and constant-copy paths covers:

- `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --all-features -- -D warnings`,
  and all 14 repository source-boundary checks.
- All 13 compiler tests on Linux x86_64 and macOS aarch64, including escaped pointer mutation,
  nested dereferences, unaligned and overlapping copies, execution from reloaded artifacts,
  pre-callee-frame stack checks, and rebinding callee headroom across programs.
- Linux release differentials with compilation forced on first call: `jit_abi_probe`,
  `jit_call_probe`, `jit_builtin_probe`, `jit_foreign_probe`, `jit_unwind_probe`, `simd_probe`,
  `panic_exit`, `catch` and `signal_probe`; also `recursion-stack-overflow`, `fib32-reload` and
  `fib32-jit-cache`. The cache check covers cold/warm/bypass, corrupt and missing entries, loaded
  frame unwinding, and artifact equality across processes and frontend thread counts.
- Store and input-validation checks: 35 focused unit tests on macOS aarch64 and 36 on Linux x86_64
  (including manifest-driven collection), covering catalogue reconciliation, damaged and missing
  packs, file replacement, directory relocation and content changes with restored timestamps.

Focused validation of the archive/load path passes 18 tests on Linux x86_64 and macOS aarch64.
They cover archive layout round trips, invalid bounds and references, retained immutable ownership,
concurrent demand decoding, fork preparation, isolated frozen domains, package mutation and
bound-image invalidation. Managed-input tests cover generation identity, local receipt recovery and
publication/collection leases. Linux release checks pass `fork_exec_probe`, `jit_unwind_probe`,
`fib32-jit-cache`, all eight package assertions and the unchanged `fib32` timing case at 52 ms
against its 80 ms ceiling. Package checks include concurrent public Engine embedding,
constructors/destructors and closed callback behavior. The full sanitizer suite is not included.

Compiled callers check the callee's stack headroom before entering its body through the PLT.
Interpreter/native packed entries retain their guarded wrapper. This removes a wrapper frame and
call/return from each compiled-to-compiled call without changing the calling convention, return
safepoints or unwind registration. Persisted callers name callee headroom through a canonical
relocation: another program can bind the same caller fragment to a different callee frame layout.
Focused Linux release checks pass `jit_abi_probe`, `jit_call_probe`, `jit_unwind_probe`, `signal_probe`,
`recursion-stack-overflow`, `fib32-jit-cache`, `fib32-reload`, `telemetry` and `fib32` (65 ms against
80 ms). The corruption check flips a payload byte in every stored record and requires zero hits;
writing a fixed byte at an arbitrary pack offset does not reliably damage the entry being loaded.

Call-overhead measurements compare alternating release binaries with and without caller-side
checks. Both use the compact immutable archive, managed generations of identical sysroot contents,
local derived stores, absolute source paths, the default JIT policy and `MIRVM_OFFLINE=1`. Each
workload has two preparation runs per binary; every measured run starts a fresh process with
application caches populated. These are not OS page-cache flushes or empty-cache startup readings.
All 44 timing, preparation and counter runs completed successfully with expected stdout.

| Workload / measure | Samples per binary | Guarded-call median | Caller-check median | Caller-check range |
|---|---:|---:|---:|---:|
| `fib(32)`, whole process | 9 | 53.7 ms | 45.7 ms | 44.7–52.1 ms |
| `fib(32)`, engine phase | 9 | 22.3 ms | 14.8 ms | — |
| `flate2`, whole process | 7 | 1476.1 ms | 1336.7 ms | 1179.4–1937.6 ms |
| `flate2`, engine phase | 7 | 1412.6 ms | 1251.3 ms | — |

The fib whole-process median falls 14.8% and its engine median 33.6%. Image/input loading is
essentially unchanged at 17.2 versus 17.0 ms; user/system CPU falls from 52.6 to 44.6 ms. Work outside
the driver ledger contributes about 13.7 ms to the current process time. The `flate2` whole-process
median falls 9.4% and CPU falls from 1508.4 to 1342.9 ms, but its larger variance does not establish
a tail-latency improvement. All samples, including the 1937.6 ms run, remain in the comparison.

Four CPU captures have no lost samples. Short fib captures have roughly 125–150 samples, so small
percentages are approximate. Before the change, fib's guarded wrapper accounts for about 12% of
sampled CPU; generated code confirms an extra frame and body call. Checks remain in the caller
and their cost is attributed to the body after the change. Archive hashing/validation and dynamic
library initialization remain visible; the `flate2` profiles retain small compiled operations and
compiled/interpreted transitions. Hardware counters were heavily multiplexed and are not used to
quantify instruction savings. Telemetry syscall events have no timestamps and do not time host
startup; phase ledgers, whole-process measurements and CPU stacks serve different parts of the
breakdown.

The code-size tradeoff is explicit: in a synchronous compiled fib probe, the body grows from 429 to
710 bytes; body plus guarded and packed entries grows from 574 to 846 bytes. A separate repeated
in-process probe reduces the last ten calls' median from 17.37 to 10.31 ms, but this diagnostic is
not an alternating whole-process comparison. Selective inlining and smaller call sequences require
joint time/code-size measurements.

The bound `fib(32)` execution image is approximately 21 MB, with a 17.7 MB function section retaining
9,486 bodies. Archived blocks occupy 104 bytes and statements 128 bytes. An aligned immutable file
owner avoids a second archive copy, and an execution-cache hit consumes metadata into its validated
instance. Reusable packages restore independent frozen domains. Archive pruning, runtime-only
compiler-library loading, and empty-cache frontend/base preparation remain separate optimization
questions; caller-side checks do not establish improvements to those paths.

These focused checks do not certify the full fast, smoke or gate tiers. The existing `fib32` case and
all measured cache-hit fib32 samples meet 80 ms; this does not guarantee that ceiling under arbitrary
storage or scheduling delays. Large dependency closures retain the fixed-address layout conflict
recorded in E52. A completely green gate is not claimed.

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

### Frontend research

The current binary's driver ledger includes base preparation in `frontend`; its `lower` includes
rustc queries triggered by MIRVM. These labels do not partition rustc work from MIRVM work.
An external callback probe linked to the release library uses the pinned `nightly-2026-07-02`, the
managed MIR sysroot, `-Zmir-opt-level=2` and one frontend thread. Three runs per mode use rustc's
coarse `time-passes` instrument; a separate self-profile capture provides query attribution.
The flate probe uses the same driver source with frontmatter removed and an existing MIR dependency
rlib. Dependency compilation, base serialization and guest execution are outside these probe times.

| Input | Entry through `after_analysis` | Additional native mono collection | MIRVM lowering from an empty image stack | Resulting functions |
|---|---:|---:|---:|---:|
| Empty main / std base | 7.3 ms | 4.2 ms | 215.9 ms | 9,480 |
| Pure integer exports, including fib | 10.7 ms | 4.3 ms | 218.3 ms | 9,486 |
| flate2 driver | 24.0 ms | 15.9 ms | 253.4 ms | 10,930 |

The columns are medians from separate probe modes, not additive stages: lowering includes its own
mono collection. Native mono item counts are 10, 16 and 355 respectively. Native dependency code
already compiled into libraries does not enter that set in the same way as MIRVM's guest execution
closure; the count difference is not proof that the extra guest functions can be removed.

One self-profiled base lowering takes 255.6 ms: 111.7 ms of outer activity self time and 143.9 ms in
nested compiler activities. Prominent query self times are symbol names (25.2 ms), optimized-MIR
metadata decoding (21.3 ms), codegen candidate selection (11.4 ms), layout (7.8 ms), constant
allocation evaluation (6.8 ms) and instance resolution (6.4 ms). Inclusive query durations overlap.
The measured outer self time includes allocations and other uninstrumented work, not just emission.

Production-binary controls run fib and flate2 in three isolated homes per cache state and preserve
guest output. Fib's `frontend` medians are 326.8 ms with empty derived stores, 32.9 ms with a supplied
base, and 9.4 ms with base bypassed; bypass shifts 234.3 ms into `lower`. A new home can also recover
managed receipts before driver entry. Bypass disables bound-image caching, and those runs report no
successful program-cache publication, so whole-run differences do not establish an equivalent
optimization. All 24 production runs and 36 successful compiler probes are retained. Three CPU
captures have zero lost samples; query summaries, timelines and flamegraphs provide the attribution.

The release executable directly depends on `librustc_driver`, including when no compiler session is
needed. The shared engine has a compiler-free test build, but its product adapters and omitted JIT
mean a complete compiler-independent runtime remains unproven. No product source was changed by
this investigation and no repository test suite was run. The proposed experiments and compatibility
requirements are in [frontend-boundary.md](designs/frontend-boundary.md).

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
- **Logging / profile**: syscall events have no timestamps and do not cover host startup I/O.
  `make profile` records CPU call stacks and can render a flame graph, but JIT caller unwinding can
  remain incomplete. No 1-byte raw syscall site, separate trace interpreter loop or 4→64 KiB
  adaptation; the trace code domain is frozen at Engine construction rather than chosen at the
  outermost activation. A fork capture's files can inspect clean individually while directory
  inspect/export rejects their different session IDs.
- **Embedding**: `instantiate` stays `unsafe` — the native/FFI ABI, hand-written Modules and raw
  two-machine-word exports must be trusted, `Shared` is not public, and no safe typed export bindings
  are generated. Third-party callbacks have no universal revocation, so published closure/JIT/MC/native
  code lives to process end.
- **Distribution**: `.mirvm` is at unstable format v5 with direct archive verification and demand
  body decoding. Cross-build_id/target compatibility, fat artifacts and format freeze are unfinished. No daemon, REPL, checked mode, resource governance or real sandbox.
- **Cargo config**: Git source replacement, patches targeting a Git URL, and parts of the Cargo config
  surface such as `paths`/`HOST_RUSTFLAGS` are not implemented.
- **`mirvm doc`**: doctests work through a fixed rustdoc front end; independent `mirvm doc`/HTML
  generation is not claimed.

Architectural ambition is not qualification. "A reference implementation that runs in RAM" is a
long-term semantic contract; while the gaps above and a limited corpus remain, mirvm is not a finished
product covering all of Rust.

## 5. Development order

1. **Performance (D16).** Close the `fib(32)` RED by making it faster, never by relaxing the gate.
   Address pack discovery and input/image validation first for startup; measure interpreted calls,
   compilation throughput and publication latency separately for execution-heavy workloads. Use
   unprofiled wall-time controls, the timing ledger, CPU call stacks and syscall counts together.
   A local small-cache result does not qualify a populated or network-backed store.
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
   fd1/fd2, the rustc emitter's own rendering and the cargo-compatibility lines never pass through it;
   `src/out.rs` is the one writer of product output, and the `MIRVM_*_DEBUG` instruments are the other
   raw channel. Every fallible signature names `crate::error::Error`, and the four source checks that
   keep a second spelling out live in `repo-quality` ([output-grammar.md](designs/output-grammar.md)).
6. **Product capability.** Direct archive semantic verification needs a new offset-based read-only
   representation before a format freeze can be reviewed. OS-level sandboxing is deliberately paused.
   Remaining stage boundaries and acceptance criteria are in `open-issues.md`.
7. **Harness budget.** Touch the harness only when a current RED cannot be reproduced or judged, and
   never harden it for a hypothetical future.
8. **Remote work stays paused** until the maintainer explicitly resumes it (`open-issues.md` G1).
