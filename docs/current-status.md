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
  Cargo's own composition and ordering. `MIRVM_DEPS` defaults to `self`: `mirvm run`, `mirvm prepare`,
  `mirvm install` and `mirvm test` resolve, schedule and build with no cargo in the process, using the in-tree
  `src/cargoless/` stack.
  `MIRVM_DEPS=cargo` remains the user fallback and behaviour referee, and both tracks are compared
  continuously. Both preparation commands enter the selected root's runner on the Cargo track,
  including when its launcher is fresh, and stop after bound-image publication. `mirvm test` covers
  lib/bin/test/bench/example with libtest or a custom harness, and a
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
  prints a phase ledger. Std products are currently built locally; release delivery remains
  unimplemented. Project `install` and `prepare` publish a complete bound image before succeeding,
  independently of optional symbolic delta publication. Required image failures fail preparation;
  ordinary runs retain best-effort caching. The installation boundary is
  specified in [distribution-design.md](designs/distribution-design.md#27-installed-execution-products).
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
- Compiler tests on Linux x86_64, including escaped pointer mutation, nested dereferences,
  unaligned and overlapping copies, execution from reloaded artifacts, pre-callee-frame stack
  checks, and rebinding callee headroom across programs. macOS aarch64 has runtime/Cranelift
  Clippy coverage.
- Linux release differentials with compilation forced on first call: `jit_abi_probe`,
  `jit_call_probe`, `jit_builtin_probe`, `jit_foreign_probe`, `jit_unwind_probe`, `simd_probe`,
  `panic_exit`, `catch` and `signal_probe`; also `recursion-stack-overflow`, `fib32-reload` and
  `fib32-jit-cache`. The cache check covers cold/warm/bypass, corrupt and missing entries, loaded
  frame unwinding, and artifact equality across processes and frontend thread counts.
- Store and input-validation checks: 35 focused unit tests on macOS aarch64 and 36 on Linux x86_64
  (including manifest-driven collection), covering catalogue reconciliation, damaged and missing
  packs, file replacement, directory relocation and content changes with restored timestamps.

Focused archive/base/package checks pass on Linux x86_64 and macOS aarch64. They cover archive
layout round trips, chunk bounds, cross-segment references, integrity and generation rejection,
fixed-domain conflicts, demand decoding through merge and publication, fork preparation, isolated
frozen domains, package mutation and bound-image invalidation. The package format rejects the
previous unchunked generation. Managed-input tests cover generation identity, local receipt recovery and
publication/collection leases. Linux release checks pass `fork_exec_probe`, `jit_unwind_probe`,
`fib32-jit-cache`, all eight package assertions and the unchanged `fib32` timing case at 52 ms
against its 80 ms ceiling. Package checks include concurrent public Engine embedding,
constructors/destructors and closed callback behavior. The full sanitizer suite is not included.

Compiled callers check the callee's stack headroom before entering its body through the PLT.
Interpreter/native packed entries check headroom before calling the body; no separate guarded
symbol is emitted. Persisted callers name callee headroom through a canonical relocation, so
another program can bind the same caller fragment to a different callee frame layout. Both paths
retain return safepoints and registered unwind metadata. Current timings, profiles and focused
validation are reported with the startup measurements below. Selective inlining and smaller call
sequences still require joint time/code-size measurements.

The bound `fib(32)` execution image is approximately 21 MB, with a 17.7 MB function section retaining
9,486 bodies. Archived blocks occupy 104 bytes and statements 128 bytes. An aligned immutable file
owner avoids a second archive copy, and an execution-cache hit consumes metadata into its validated
instance. Reusable packages restore independent frozen domains. Archive pruning, runtime-only
compiler-library loading, and empty-cache frontend/base preparation remain separate optimization
questions requiring separate measurements.

These focused checks do not certify the full fast, smoke or gate tiers. The existing `fib32` case has met
80 ms in focused checks; broader fresh-process measurements include startup and scheduling outliers
above it. This is not a ceiling under arbitrary storage or scheduling delays. Large dependency closures retain the fixed-address layout conflict
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

### Preparation and startup

Successful bound-image hits expose six sequential load intervals with `MIRVM_TIMING` and a nested
instance ledger separating restoration, semantic verification and lazy function-table initialization.
[startup-cache-design.md](designs/startup-cache-design.md#bound-execution-images) owns their scope.
File reading, command routing and guest execution remain outside these load intervals.

Archive verification builds one immutable module context, checks every body, and accumulates main
panic boundary/catcher counts during that walk. One-shot images validate addresses without retaining
obligations for a later instance; reusable images retain those obligations and recheck each new
address context. Structural byte checks, input freshness, integrity and semantic rules remain intact.

Dependency loading borrows one symbol view owned by `ImageStack`; validated layers extend it once,
and truncation rebuilds the retained prefix. Projection, restoration and reverse-projection checks
share that view. The [dependency-sharing design](designs/dep-sharing-design.md#2-contract) owns its
collision, scope and invalidation rules. Cache formats and external-reference validation are unchanged.

The interpreter keeps scalar transfers, slot/immediate operands and local addresses with zero or
one dereference in small inline entry paths. Recursive projections retain the same fallback semantics.
A separate census with JIT disabled counts 24.42 million operands on flate and 496,069 on serde;
slot/immediate operands make up 89.0% / 88.1%. This instrumented census is not a timing measurement.
The Linux x86_64 release block loop grows from 9,336 to 11,270 bytes. Unique text ranges whose
symbols name the interpreter grow from 51,692 to 64,593 bytes, including instantiated helpers;
this 12.6 KiB increase is the code-size cost of direct execution, not the whole binary's growth.

The packed entry performs the boundary stack check and calls the fast body directly. A fresh JIT
compile finalizes its body, packed entry and missing C2I trampolines together. C2I backend output is
reused by flattened ABI shape within each compiler. The
[JIT design](designs/jit-code-cache-design.md#25-load-time-linking-and-publication) owns template
isolation, relocation, publication, failure handling and scratch lifetimes. Guest ABI, verification,
optimization levels and admission thresholds are unchanged. C2I templates are not persisted; artifact
encoding remains 3.

The performance priority is ordinary prepared execution. The focused measurements below use the
compiler-independent CLI with its matching worker, offline prepared inputs and generic templates
enabled; the experimental shared compiled tier is disabled. Ordinary JIT thresholds are retained
unless stated otherwise. This does not qualify the full default binary/configuration. C2I reuse
reduces strict threshold-one compilation ledgers by 13.0% on serde and 11.0% on flate, but does not
establish general ordinary-run payback: flate's process median regresses 3.7% in the alternating
comparison. Ordinary C2I definition
cost is only 0.235 ms / 0.856 ms, including template misses, copying and rebinding. These are small
parts of the end-to-end budget; forced compilation throughput is not an ordinary-run acceptance metric.

An instrumented startup decomposition uses ten native-matching fresh processes per input, with
prepared-image hits and persistent JIT caching disabled. Medians are independent and do not form
an exactly additive wall-time partition:

| Interval | serde JSON | flate2, 2 KiB |
|---|---:|---:|
| Main entry to driver clock | 20.32 ms | 18.01 ms |
| Dependency resolution within that interval | 10.42 ms | 6.06 ms |
| Prepared-image loading | 27.01 ms | 29.34 ms |
| Image loaded to exit request, including engine execution | 23.30 ms | 93.85 ms |
| Process time outside main-entry-to-atexit markers | 7.19 ms | 7.12 ms |

Serde pays about 47 ms before engine execution even with preparation complete. Its uninstrumented
CPU capture attributes 57.24% of user cycles to the main CLI thread, 28.87% to the guest and 10.73%
to JIT. Dependency resolution accounts for 11.09% of total user cycles; index-entry lookup beneath
feature unification accounts for 9.08%. A locked run still materializes the full crate version history
from JSON before selecting its exact version. Parsed entries are already shared within the command.
Registry opening, lock/links checks and sysroot selection, and dependency scheduling take medians
of 1.35, 3.65 and 1.23 ms respectively. The main startup opportunity spans the prepared launch path
and image materialization; an isolated decoder improvement cannot remove that whole cost.

A separate cache matrix uses five rotating rounds per workload, with native stdout agreement and
fixed per-workload heat snapshots. Prepared IR, installed dependencies and OS caches remain. Cold JIT
starts with an empty machine-code store; warm JIT uses three ordinary prior runs and retains pack
file identities so catalogue entries remain valid. Timings exclude result-file setup and cache resets;
no builds, CPU sampling or correctness suites overlap. Shared-host load is uncontrolled. These are
fresh processes with prepared inputs, not empty-installation or cold-filesystem measurements:

| JIT configuration | serde process / engine | flate process / engine |
|---|---:|---:|
| Persistence disabled | 75.06 / 21.30 ms | 148.14 / 98.00 ms |
| Persistence enabled, empty JIT store | 83.66 / 30.90 ms | 166.32 / 105.20 ms |
| Persistence enabled, populated JIT store | 84.37 / 23.90 ms | 161.77 / 108.50 ms |
| Interpreter only | 79.45 / 19.70 ms | 251.56 / 198.60 ms |

The populated JIT store reduces flate child CPU from 169.20 to 154.39 ms relative to disabled
persistence, without a process-latency gain. A separate diagnostic ledger links 142 functions in
40.30 ms and compiles 63 in 18.25 ms, with zero cache refusals. The no-persistence ledger compiles
205 functions in 47.72 ms. These ledger intervals are instrumented and overlap guest execution;
they do not partition wall time. A cache hit is not evidence of cheaper execution.

A fresh flate CPU capture contains 5,560 samples over twelve processes with no lost samples. Guest,
JIT and main-thread shares are 54.44%, 33.76% and 11.66%; the block loop and complex statement
handler alone consume 28.99% of total user cycles. Sampling uses `cycles:u` at 1999 Hz, DWARF stacks
and PID-specific JIT maps. It excludes kernel work and I/O/scheduling waits.

A diagnostic interpreter census attributes statements and terminators to guest functions. The first
ordinary flate run executes 8,554,170 such units; these counts are not CPU time. Four functions own
49.5% of that interpreted work:

| Function | Interpreted units | Interpreted calls | JIT request |
|---|---:|---:|---|
| `deflate::core::compress_normal` | 17.5% | 320 | None |
| `DictOxide::find_match` | 15.7% | 1,250 | 54.38 ms after probe start |
| `inflate::core::init_tree` | 8.6% | 22 | None |
| `deflate::stored::compress_stored` | 7.7% | 6 | None |

Across three runs, observed interpreted work after publication of the same function is at most
0.00012%. The probe checks publication every 256 blocks, so this is a lower bound, not a complete
OSR opportunity estimate. Instrumentation changes scheduling. These observations nominate admission
candidates; they do not establish that compiling them will repay its cost.

Two diagnostic binaries request those four functions on first entry. One waits for publication;
the other continues interpreting asynchronously. Compilation, selection and waiting costs remain
inside the engine and process clocks. Ten alternating pairs per flate configuration give baseline /
probe medians:

| First-entry policy | Persistence | Engine | Complete process | Child CPU |
|---|---|---:|---:|---:|
| Wait for publication | Disabled | 100.45 / 102.85 ms | 167.78 / 176.86 ms | 173.02 / 198.70 ms |
| Wait for publication | Enabled, empty | 123.55 / 124.00 ms | 192.89 / 199.85 ms | 185.34 / 209.28 ms |
| Continue interpreting | Disabled | 98.70 / 88.00 ms | 169.27 / 154.25 ms | 179.73 / 190.46 ms |
| Continue interpreting | Enabled, empty | 115.25 / 114.70 ms | 180.82 / 189.55 ms | 195.28 / 186.22 ms |

Waiting fails to improve either configuration and raises CPU 13–15%. Separate CPU captures show
25.7% fewer guest user cycles but 25.3% more JIT user cycles; waiting also changes the critical path.
Asynchronous admission improves all ten no-persistence engine pairs and eight process pairs, at
6.0% more CPU. With persistence enabled, only five of ten engine/process pairs improve. This batch
also contains large non-CPU stalls and variation on serde, where none of the four functions is
selected. It establishes no general admission policy or persistence-enabled speedup. The
workload-specific selectors remain diagnostic artifacts; production policy is unchanged.

Program-miss profiles retain image absorption's symbol-table copying/hashing, compiler metadata
allocation and semantic verification. Intermittent non-CPU waits remain unresolved under D7.
The telemetry stream records guest syscalls and capture lifecycle; CPU samples, phase clocks and
the function census supply compiler and pre-driver attribution. The
[preparation harness](../experiments/preparation/README.md) owns the reproduction and profiling tools.

C2I correctness coverage includes zero/register/stack parameters, zero/one/two return words, high
callee IDs, both tiers/domains, execution after template release, failure isolation, persisted reload,
stack checks, unwind and native differentials. The bottleneck experiments check native stdout on
every measured run; they do not rerun the full runtime suite or change runtime code.

Registry index lines decode their schema directly into resolver fields. Object-only visitors reject
positional arrays; unusual shapes use the value parser to retain tolerant field rules and diagnostics.
Frozen snapshots use bulk byte decoding with unchanged postcard bytes and a sequence fallback for
JSON. An isolated 277,240-byte index probe records 38,182 → 8,749 allocation/reallocation calls and
1.849 → 0.758 ms parse/drop medians over 32 alternating pairs with counting disabled during timing.
The 1 MiB synthetic snapshot probe falls from 557.063 to 17.882 microseconds with one allocation.
These isolate decoder costs and do not establish complete startup payback.

Pinned compiler version, target and host cfg facts are embedded in the build identity. Prepared
script syscall captures show one executable start instead of MIRVM plus two rustc query children;
[D15](designs/d15-cargoless-design.md#1-contract) owns the selection contract. Bound-image hits in the
optional lightweight CLI also avoid compiler linkage, evaluated separately below.

### Frontend research

The driver ledger exposes base loading and construction separately. `base-load` is nested in
`frontend` or `cache-load`; a miss's `base-build` follows analysis and precedes program `lower`.
Both lowering phases include the compiler queries they drive; the labels do not partition rustc
work from MIRVM work.
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

The std base is constructed from canonical std roots in the program's compiler session. The
isolated preparation command uses the same roots; program-specific main, mono items, native inputs
and allocator shims stay in the delta. Stable identities and semantic profile checks make the base
reusable. Fresh mappings and immutable function archives survive publication in memory, so a miss
needs neither a second compiler session nor a readback of the new base. The contract is specified
in [startup-cache-design.md](designs/startup-cache-design.md#std-base-loading).

Alternating release controls use five samples per fib state and three per flate2 state. Isolated
derived stores share managed sysroot contents and existing dependency metadata, with valid receipts;
OS page caches are not flushed. Both binaries publish program caches, and unchanged reruns must
report hits. An edit changes fib's result and is checked for invalidation. The reference already
uses archived base bodies and the same small-call JIT optimization.

| State | Isolated builder, whole process | Single session, whole process | Maximum RSS, reference → single session |
|---|---:|---:|---:|
| fib, empty derived caches | 432.1 ms | 400.1 ms | 261.8 → 260.4 MiB |
| fib, only a compatible base prepared | 119.5 ms | 126.3 ms | 149.7 → 147.8 MiB |
| Another fib program sharing the base | 104.3 ms | 106.7 ms | 149.3 → 148.5 MiB |
| Edited fib program | 174.0 ms | 165.0 ms | 150.5 → 149.5 MiB |
| Unchanged fib rerun | 43.2 ms | 42.8 ms | 69.7 → 66.8 MiB |

These are medians, not latency guarantees. Empty-cache ranges are 418.4–442.8 ms and 394.8–403.2 ms;
prepared-base ranges are 117.7–138.5 ms and 121.3–137.6 ms. Cold wall time falls by 7.4%, while a
prepared compatible base is 6.8 ms slower in this sample. Second-program and unchanged reruns remain
close to the reference. The cold driver total is 415.1 → 375.4 ms. Reference base construction is
324.8 ms plus a 14.1 ms load; in-session construction is 267.4 ms with no readback. These build fields
have different boundaries: the reference includes child startup and teardown, while the combined
session's compiler finalization remains outside its build field. The wall-time comparison includes
that cost on both sides. Maximum RSS is the process-tree maximum reported by GNU time, not aggregate
simultaneously resident parent and child memory.

The base artifact is 19.98 → 19.61 MiB. Cold peak memory remains about 260 MiB: compiler queries,
owned bytecode and archive buffers still coexist during construction. flate2 with a compatible base
has a median wall time of 2081.4 → 2035.2 ms and driver total of 1607.1 → 1579.8 ms. An 8.23 s
whole-process outlier in the candidate is retained in the evidence; guest execution and filesystem
or scheduling variability dominate this workload, so no runtime speedup is claimed. All 62 recorded
runs preserve expected output and publication. Four separate CPU captures report zero lost samples;
std lowering, compiler queries, allocation and destruction remain the main cold costs.

The `base-image` contract checks isolated/in-session byte equality, different main return types,
custom allocation and caught panic, native-input isolation, cross-program reuse, edits, corruption,
failed publication, compiler errors and profile compatibility. Focused unit, release fork/unwind,
package and capture probes cover the affected ownership paths. No full test tier or sanitizer gate
is claimed for this change.

### Runtime boundary prototype

The default CLI links `librustc_driver`. Building the library without its default `compiler` feature
and with `cranelift` produces a real artifact runner with no rustc/LLVM dependency. Both its dynamic
dependency list and mappings captured during guest execution confirm the absence. It uses the
ordinary package/bound-image readers, input validation, assembly materialization, JIT and shared
guest stack/exit implementation. Compiler-only base/dependency layers and program-delta preparation
are excluded from that build. The compiler-linked reference runner uses the same protocol and
source/build identity. Ordinary `mirvm run` is a separate control.

Two miss routes use the existing compiler driver and artifact contract. Prepare/reload starts a
worker, waits for diagnostics and publication, then reopens and validates its image before guest
entry. Exec replaces the runtime with the worker and executes in that process after compiler
finalization. Exec avoids parent waiting and image reload but retains rustc/LLVM mappings for that
run, confirmed during guest execution. Hits remain compiler-independent. Warning-bearing sessions
cannot publish an image: prepare/reload reports that absence without entering the guest; exec emits
the same diagnostics as the direct driver and executes normally, without caching those sessions.
The explicit recipe still requires complete compiler arguments and a prepared sysroot. Reproduction
is in the [experiment README](../experiments/runtime-boundary/README.md).

The timing matrix contains 100 native-matching fresh-process measurements, five alternating rounds
per case/mode, collected separately from this experiment's builds, linting and profiling. OS caches
and heat orders are retained; persistent JIT caching is disabled. Hits cannot launch a worker. Misses remove only the
probe's bound image, retaining std, dependency and program products; they are not empty-store builds.
Packages are identical across runners. Complete-time medians are:

| Workload/path | Ordinary CLI | Compiler-linked runner | Independent runtime, prepare/reload miss | Independent runtime, exec miss |
|---|---:|---:|---:|---:|
| Minimal output, image hit | 44.7 ms | 47.0 ms | 37.2 ms | Same hit path |
| Minimal output, package | 35.8 ms | 35.4 ms | 29.5 ms | Same package path |
| Minimal output, image miss | 106.5 ms | 127.1 ms | 115.6 ms | 109.6 ms |
| Recursive fib(32), image hit | 55.5 ms | 59.4 ms | 52.3 ms | Same hit path |
| Recursive fib(32), package | 49.6 ms | 47.7 ms | 43.7 ms | Same package path |
| Recursive fib(32), image miss | 108.4 ms | 142.8 ms | 133.3 ms | 118.6 ms |

Independent hits/packages are cheaper in this matrix. Exec misses improve on prepare/reload by
6.0 ms for minimal output and 14.7 ms for fib, while still trailing ordinary in-process preparation.
Minimal-output miss ranges are 82.3–111.9 / 105.5–129.0 / 88.0–110.7 ms for ordinary CLI,
prepare/reload runtime and exec runtime; fib ranges are 95.4–130.9 / 127.9–144.4 / 106.1–121.0 ms.
All valid timing outliers remain in the data. These small workloads support the fallback direction,
not default routing or a representative-workload speedup. This fib runs through `main`, not the
repository's `--vm-call fib(32)` gate. The experiment establishes no executable-size or RSS saving.

Handoff probes preserve cwd, spaced/empty arguments, runtime environment and SIGTERM status.
Runtime-only environment changes hit with an unavailable worker; source and compile-time `env!`
changes invalidate. A compilation error produces no guest stdout. Repeated warning-bearing exec
runs preserve the direct driver's stdout, stderr and exit status without publishing an image.
Seven separate strict-JIT invocations cover inline assembly, syscall rewriting, global assembly
calling guest code, generic templates with callbacks/concurrency and separate fork, and main
return/panic classification. The latter two also run through a compiler miss with IR caching
disabled. The generic artifact is prepared with sharing enabled and executed without the exporter
flag. Native stdout and exit status agree.

Native archive materialization now has a compiler-independent byte/link layer and a compiler-only
metadata/symbol-recovery layer. Runtime unit tests use the same native fixture producer, preserving
native interposition and lifecycle coverage. The release runtime inventory passes 375 checks with
one existing ignored check in one batch. Command-report tests belong to the compiler build; a
portable assembly fixture verifies generated symbol bytes and comment handling on both supported
hosts. Compiler-build validation also covers native, sysroot and trace boundaries. `repo-quality`
includes strict all-target runtime Clippy and release runtime library tests alongside the compiler gates.

A C-archive/Rust-callback fixture forces the real compiler symbol-recovery path in reference and
candidate binaries. Both packages match native output, and emitted trampoline assembly and its
content-addressed filenames are byte-identical. The candidate package also runs in the independent
runtime. The existing package embedding fixture, linked to the no-compiler library, passes concurrent
instance creation, constructors/finalizers, native and assembly callbacks, explicit close, stale
callback rejection and instance-address isolation. Compiler/runtime build identities agree and the
runtime executable's dynamic dependencies contain neither rustc nor LLVM.
With JIT caching enabled, a compiler-built runner writes 610 entries for the native fixture; the
independent runtime and then the compiler-built runner each reuse all 610 with zero misses and
native-matching output. This checks cache sharing across the feature boundary as well as fresh JIT execution.

The release checks use one captured native-handler address across codegen units and distinct restorer
code, avoiding function-pointer duplication/merging assumptions. The trace ABI check observes the
reserved register in a compiled caller immediately after normal return and at its unwind landing
pad, rather than comparing values across unrelated Rust statements. Store flag checks allow the two
declared multi-family purge groups. Managed input copy/full-hash validation compares complete names,
directory inode identities and file revisions; delayed directory timestamps cannot falsely reject
an unchanged snapshot. Receipt hits still compare full metadata and content checks remain mandatory
on a mismatch.

Both compiler and compiler-independent builds pass all-target Clippy with warnings denied.
The runtime feature excludes dependency resolution/fetching, command reports and unused artifact
producers; tests retain the producers needed by native, archive and managed-input fixtures. The
Linux runtime's normal dependency graph has 72 packages rather than 109, excluding the root package:
seven direct CLI dependencies remove 37 packages from this graph, including HTTP/TLS,
compression and dependency-solving libraries. Build/dev dependencies are outside that count. This
establishes a smaller build dependency surface, not a measured startup improvement. Runtime instance
layouts, serialized formats and emitted assembly remain shared across feature configurations.
The optional ordinary CLI uses `--no-default-features --features cli,cranelift`. Its `cli` feature
retains scheduling and reports while `compiler` owns rustc linkage and session-dependent producers.
Prepared images and packages execute directly; misses exec a same-build `mirvm-compiler`, preserving
PID, cwd, arguments, diagnostic scope, preparation state, unit inputs and Cargo's saved guest
environment. The managed-input lease stays held through exec and becomes close-on-exec again in the
worker. A private single-use request carries no compiler allocations or native pointers. The worker
inherits capture ownership and consumes rejected requests before reporting incompatibility.

The ordinary CLI contract batch covers both dependency tracks with a target dependency, host proc
macro and build script; frontmatter; source/compile/runtime environment changes; absent-worker hits;
native environment bytes on the self track; cwd and spaced/empty arguments; private requests and
lease continuity; warnings/errors; capture; packaging; interpreter and strict-JIT panic status; and
SIGTERM/PID continuity. The executing lightweight hit has no rustc/LLVM mappings; the miss does.
The Cargo build sidecar still panics on non-UTF-8 environment values before handoff, so that track
uses UTF-8 values. The request serializer itself has native-byte round-trip and malformed-input
checks. Linux's lightweight CLI library batch passes 518 checks with one existing ignored check;
the two request checks pass again after rejection handling changed. All three configurations pass
Linux warnings-denied Clippy; the lightweight CLI also passes it on macOS. The native archive,
assembly-key and embedded lifecycle fixtures pass for these exact binaries.

An ordinary CLI matrix compares identical-source full and lightweight builds sharing prepared
products: ten alternating rounds of image hits and five rounds with program IR caching disabled,
over five workloads (150 native-matching samples). Installed std/dependency products, OS caches and
heat orders remain; persistent JIT caching is disabled. These are fresh processes, not empty-store
or page-cache-cold runs. Whole time brackets process creation through exit, with result persistence
outside the clock. Experiment builds, checks and profiling are separate; shared-host load is
uncontrolled. Complete-time medians are:

| Workload | Full CLI, image hit | Lightweight CLI, image hit | Full CLI, program miss | Lightweight CLI, program miss |
|---|---:|---:|---:|---:|
| Minimal output | 58.0 ms | 47.4 ms | 119.1 ms | 137.1 ms |
| Recursive fib(32) | 75.6 ms | 64.1 ms | 138.6 ms | 138.4 ms |
| Generic calls | 186.1 ms | 177.6 ms | 297.0 ms | 273.2 ms |
| Serde | 89.0 ms | 82.0 ms | 326.9 ms | 313.3 ms |
| Flate | 243.3 ms | 222.0 ms | 461.2 ms | 425.5 ms |

The hit-path result supports removing compiler linkage from ordinary prepared execution. It does
not establish a universal miss improvement: the minimal-output miss regresses by 18.0 ms, and miss
ranges overlap for every workload. Flate hit ranges are 191.2–516.5 ms full and 178.6–347.3 ms
lightweight; its median alone is insufficient to attribute the difference to linkage. All valid
outliers remain. Separate serde CPU captures (four fresh-process hits per binary, user cycles at
1999 Hz with DWARF stacks) retain hashing, archived structural/semantic verification, allocation and
interpreter guest calls among the main costs. They confirm remaining work after compiler isolation;
the instrumented samples are excluded from latency measurements and do not establish a total CPU
reduction. Distribution/default routing, true cold preparation, peak memory and combined
lifecycle qualification remain open in [frontend-boundary.md](designs/frontend-boundary.md).
The default build still includes the compiler.

### Runtime generics evaluation

A separate research build records each lowered instance, its definition, phase costs and canonical
fragment, plus pure-interpreter function entries and executed IR statements/terminators. Every
runtime function joins a lowering record. The production reference executable is unchanged. Counts
below exclude its prepared 9,479-function std base and include all newly lowered program/dependency
instances with type or const arguments; lifetime-only parameters are excluded. A definition group
is `DefPathHash` plus instance-kind discriminator, not a proven shareable template.

| Input | New generic instances | Definition groups | Entered | Unentered | Instances covering 90% / 99% of generic IR operations |
|---|---:|---:|---:|---:|---:|
| serde_json corpus | 1,318 | 599 | 786 | 532 (40.4%) | 311 / 619 |
| flate2, input reduced to 2 KiB | 1,081 | 498 | 728 | 353 (32.7%) | 38 / 289 |
| Eight-type serde matrix, one type used | 1,048 | 575 | 259 | 789 (75.3%) | 99 / 218 |
| Same matrix, all eight types used | 1,048 | 575 | 469 | 579 (55.2%) | 100 / 303 |

The matrix executes two roundtrips per selected type; it is a controlled sparse-use case, not a
representative application. Operation counts are unweighted and exclude native work: they establish
execution concentration, not CPU-time percentages or safe reachability pruning. Existing fragments
already deduplicate storage: the serde instances have 645 unique canonical fragments, flate2 616,
and the matrix 652. Instance reduction cannot be claimed as proportional artifact-size reduction.

One instrumented lowering pass attributes 74.8 / 53.1 / 49.7 ms to the new generic bodies in
serde / flate / matrix. Of that, 30.0 / 18.8 / 35.7 ms belongs to bodies unentered in serde / flate /
the one-type matrix; using all matrix types reduces that exposure to 28.8 ms. Direct MIR parameter
substitution takes only 6.3 / 4.9 / 6.9 ms. The rest includes metadata, further layout/constant/call
queries and bytecode construction. These are attribution samples, affected by instrumentation and
query sharing; required validation and descriptor/template preparation prevent treating them as
predicted savings. These census timings do not include shared execution or specialization costs.

Uninstrumented controls use one fixed installation per input, prepared dependency MIR and std base,
and three first/repeated pairs, clearing derived execution/sharing/JIT products before each first
run. Whole-process medians are 371.3 / 87.1 ms for serde and 418.4 / 147.7 ms for small flate. Root
lowering medians are 148.3 and 127.9 ms. The one-type matrix is 335.9 / 250.4 ms, but its first
repeated run still performs analysis/lowering, so that second number is not a bound-image-hit
baseline. A later diagnostic run reaches the bound image. A fresh-installation-directory control
also rebuilds dependency/host artifacts; its whole-process times are excluded from the isolated
comparison. OS caches are not flushed, and these sample medians are not latency guarantees.

All 54 uninstrumented samples and five final census runs match independently compiled native output.
A separate prepared-dependency serde CPU capture has 866 samples and zero lost samples; metadata,
query execution, hashing, allocation and archive verification remain visible. This assessment
supports the execution prototype below. No whole-program generic execution improvement or full
test tier is established by the census.

### Production shared generic execution

Setting `MIRVM_GENERIC_TEMPLATES=1` enables reusable templates for scalar/pair and
opaque by-value locals, slice metadata and patterns, sized struct/tuple field projections and
construction, checked slice/array indexing, pointer/const operations and resolved direct guest calls. Each admitted definition is
exported once; binding fixes ABI, frame storage, dictionary values and guest-call adapters without
copying its ordinary instructions. The interpreter executes the shared template directly. The existing
JIT service requests concrete instructions through a per-Engine publication slot when compiling an
instance. Unsupported functions retain the original lowerer, and default enablement remains gated
on whole-program measurements.

Dependency compiler sessions also export unbound templates before a concrete consumer exists.
Checksummed per-crate products retain owned instructions, MIR recipe locations and negative export
probes under exact compiler/crate/target/profile identity. Loading validates the schema and IR before
restoring compiler recipes and binding current types/callees. A binary-only analysis session without
HIR hashes keeps local templates in memory. Concurrent writers merge definition sets without
retaining replaced bodies. `cache purge --units` removes these disposable products.

The preparation contract exports an unused dependency generic on both dependency tracks, then
changes only the consumer and verifies reuse with a new aggregate containing a by-value array. It also checks
bound-image hits, tracked-input invalidation and failed required publication. Template unit checks
cover exact identity, damaged/truncated archives, recipe bounds, instruction/schema validation,
stale publishers and failed publication. `MIRVM_TIMING` separates accepted export, rejected probes,
product loading, restore/verification, concrete binding and publication.

The `generic-templates` contract checks five actual Rust type/const instances against one template,
including an aligned ZST, distinct element widths and const lengths. It also covers signed consts,
negative pointer offsets, slice parameters/returns, indexed references, field references, distinct
same-layout trait witnesses, reference cloning, drop glue and guest panic propagation. Bounds checks
cover empty slices, zero-length arrays, maximum indices, nested slices/arrays, ZSTs and different element widths; panic
messages, source line/column and caller cleanup agree with native. Compound pointee sizes bind both
type and const parameters. First/last/middle patterns and mutable subslices preserve pointer identity,
length and writes across empty and nonempty inputs. By-value identity, forwarding, selection and Clone
share templates across ZST, scalar, pair, oversized scalar, array and over-aligned aggregate layouts.
Field and constructor checks cover Rust and C representations, nested projections, mutable
references, tuples and moving a field out across eight concrete layouts. Offsets, alignment, values
and pointer identity agree with native. The contract checks mixed concrete/opaque arguments, both
selection branches, Clone failure, a subsequent witness failure and a destructor failure before
return; value identity, alignment and drop
order agree with native. Direct JIT calls write scalar/pair results to either slots or memory
destinations through the same writeback path. Indirect calls, unsupported constant/aggregate
operations and caller-location bindings retain eager fallback; an invalid unentered instantiation is
rejected before entry. Eager interpretation, shared interpretation and synchronous JIT agree with
native output, including concurrent entries, recursive calls, native qsort callbacks and fork in a
separate single-threaded invocation. Each reloads its bound image without re-entering the frontend.
Export/binding diagnostics assert actual coverage; specialization diagnostics distinguish shared
execution from the JIT's concrete requests. Program manifests derive callback code ownership from
the actual frozen domain, so a program-owned native entry can be published and reloaded.

`FuncTable` exposes ABI/frame headers independently of instruction decoding. JIT callee admission,
trampoline preparation, stack-headroom relocation, FFI marshalling and signal-handler validation
borrow those headers. Compiling or reloading a caller in either plain or trace code leaves its
callee's instructions archived; the mixed-part header check covers every parameter/return shape
without initializing archived decode cells. Shared instructions and schema are deduplicated within
function archive chunks and stored as independent content-addressed fragments in unit/program
manifests. Descriptors include the exact template identity and concrete adapter relocations.
Borrowed verification checks dictionary arity/accesses, frame storage, call indices and the admitted
vocabulary without specializing. Focused checks cover concurrent publication, template sharing,
Engine isolation, republishing after specialization, fork preparation and malformed descriptors.
The original
`ffi_agg_probe` remains blocked by E53's constant-aggregate lowering gap; a diagnostic variant with
addressable arguments agrees with native under synchronous JIT and bound-image reload, including
aggregate parameters, pair returns and indirect returns through native callbacks.

Three alternating rounds on one release executable use prepared std/dependency MIR. Each miss
removes execution/sharing/JIT products except the std base; the prepared-template mode also retains
the generic-template store. Immediate repeats exercise the bound image. All 54 timed samples agree
with native, every repeat omits frontend/lowering/layer loading, and diagnostics are captured
separately. OS page caches are retained. Whole-process / lowering medians are:

| Workload | Eager | Fresh shared templates | Prepared shared templates |
|---|---:|---:|---:|
| serde JSON | 362.4 / 187.3 ms | 379.8 / 207.1 ms | 382.5 / 208.6 ms |
| flate2, 2 KiB input | 444.1 / 168.8 ms | 463.6 / 175.1 ms | 435.9 / 175.8 ms |
| generic matrix, one selected type | 358.9 / 162.2 ms | 386.0 / 176.5 ms | 385.5 / 167.0 ms |

These samples do not establish a whole-program improvement. Three rounds cannot isolate small
median differences, and process/I/O variation is substantial: flate's fresh-template range includes
446.3–2,181.2 ms. Bound-image-hit medians for eager / fresh / prepared are 70.5 / 78.3 / 79.2 ms
(serde), 132.9 / 130.0 / 128.1 ms (flate), and 84.6 / 92.7 / 84.6 ms (matrix). A separate install
check with only std base retained publishes the complete matrix image before guest entry and reaches
it on the first run. Optional symbolic-delta failure does not block this image.

Diagnostic runs admit 245 definitions / 423 instances for serde, 196 / 292 for flate, and 230 / 330
for matrix. They reject 312 / 257 / 307 definitions respectively; concrete binding additionally falls
back for 90 / 77 / 17 instances. Remaining field-projection refusals are 10 / 12 / 7; aggregate
refusals are 55 / 40 / 60. Rvalues, casts and binary operations also retain substantial missed
coverage. These counts identify admission work, not time saved.

The prepared runs reuse 244 / 195 / 227 accepted templates and 312 / 257 / 300 negative export probes.
In a representative serde run, product loading takes 4.805 ms, restoration/verification 2.426 ms,
concrete binding 2.172 ms and residual export 0.101 ms. The same run's telemetry reports base load
13.8 ms, layer load 2.1 ms, frontend 36.2 ms, lowering 209.1 ms, cache publication 27.7 ms and engine
19.7 ms within a 333.8 ms measured driver total. Generic-stage timings are nested within preparation,
not additional wall time. Driver total and whole-process time have different boundaries; the
median difference is 46.0 ms in this mode. Reuse removes export work but still pays loading,
verification, metadata queries and concrete binding.

A separate prepared serde CPU capture at 997 Hz has 535 samples and zero lost samples. An exclusive
self-time grouping assigns about 24.2% of sampled cycles to compiler/metadata symbols, 20.9% to
allocation, 14.5% to hashing/hash tables and 6.4% to bytecode verification. Allocation and hashing
are classified before compiler symbols. The generic export/restore/bind call chain accounts for
about 4.1% inclusively and overlaps these groups. CPU cycles exclude waits; profile overhead is
excluded from the timing table. Metadata, allocation, hashing and repeated verification remain
relevant startup targets.

The JIT ledger records sampled own-body interpreter work, queue delay, concrete specialization,
worker time and final publication. `MIRVM_GENERIC_PAYBACK=1` enables an experimental compile-work
budget; it remains off by default. A separate 36-sample comparison uses bound-image hits with the
persistent JIT cache disabled, three alternating rounds, native output comparison and separate
ledger captures. Its whole-process clock includes diagnostic-file creation and closing as well as execution.
Whole-process / engine medians are:

| Workload | Stay interpreted | Direct specialization | Work budget |
|---|---:|---:|---:|
| Five briefly used instances | 51.6 / 5.8 ms | 46.4 / 6.4 ms | 52.9 / 6.9 ms |
| Two repeated hot instances | 106.3 / 66.7 ms | 65.8 / 21.7 ms | 71.7 / 22.5 ms |
| Native-call-heavy wrappers | 81.4 / 48.1 ms | 82.2 / 47.7 ms | 77.4 / 47.6 ms |
| flate2, 2 KiB input | 333.5 / 258.7 ms | 176.4 / 108.3 ms | 196.2 / 111.5 ms |

The hot workload calls each instance 5,000 times with 64 loop iterations; native wrappers call a
50 µs sleep 200 times each. Neither the short nor the native-wrapper case requests a shared-instance
compilation; the latter stays below the existing entry trigger and does not prove profitable
admission after that trigger. The hot case compiles three shared entries under either JIT policy.
Flate's diagnostic capture reduces shared transitions from 30 to 14 and worker time from 8.344 to
3.529 ms under the budget, but its complete-time median rises by 19.8 ms. First stdout byte rises
from 75.9 to 90.3 ms there; synthetic cases print a startup marker before their measured work, so
that marker is not a completed useful result. The budget reduces compilation spending without an
established latency win. Default generic enablement and the compiled shared tier remain open.

The generic contract agrees with native under eager/shared interpretation, synchronous JIT and
bound-image reload, including concurrent entries, callbacks, unwind and fork. The budget mode also
matches native for normal and fork invocations. Focused template, cache-publication and compile-work
unit checks, all-target/all-feature Clippy, formatting and repository boundary checks pass. The
preparation contract passes 86 assertions across both dependency tracks. No full runtime test tier
was run for this change; OSR remains deferred.

### Runtime generics prototype

`experiments/compiled-shared` evaluates a compiled intermediate tier using production `SharedBody`
objects. It reserves private ABI headers from borrowed template identities and translates groups
only when the compile worker needs them. Call bridges reuse the interpreter's direct-call
implementation; concrete promotion publishes the original ABI for subsequent calls. Active frames
retain their body and code. Production archives retain immutable template content IDs; the
experiment groups those IDs across archive copies without eager decoding or payload hashing.
Persisted shared JIT code and trace are refused, and there is no OSR. The compiled shared tier
remains an experiment with no production option or default enablement.

Identity qualification compares content IDs with address grouping in the same release binary and
prepared images. One hundred samples cover five workloads, fixed nomination and work policy, with
five alternating rounds per mode. Every sample hits its image and agrees with native; persistent
JIT caching is disabled, OS caches and heat orders are retained, and builds/lints/profiles do not
overlap timing. Its complete-time clock includes diagnostic-file creation and closing; long gaps cannot be
attributed to guest startup from that clock alone. Complete / engine medians are:

| Workload | Address, fixed | Content, fixed | Address, policy | Content, policy |
|---|---:|---:|---:|---:|
| Many instances | 84.9 / 41.4 ms | 91.4 / 42.1 ms | 82.0 / 42.5 ms | 89.8 / 44.2 ms |
| Hot loop | 78.4 / 39.0 ms | 76.4 / 37.7 ms | 75.6 / 35.9 ms | 73.1 / 35.1 ms |
| Tiny calls | 62.7 / 18.6 ms | 136.2 / 21.0 ms | 70.7 / 20.7 ms | 62.7 / 19.1 ms |
| serde JSON | 120.0 / 24.6 ms | 119.2 / 25.4 ms | 119.9 / 29.1 ms | 121.4 / 28.3 ms |
| flate2, 2 KiB | 222.7 / 139.6 ms | 219.4 / 136.0 ms | 213.1 / 125.5 ms | 212.9 / 128.4 ms |

Content grouping removes duplicate compilation but has no established whole-program latency win.
Separate flate ledgers show fixed-nomination shared compilation falling from 24 to 20 templates
and 10,021 to 9,377 code bytes. Merging also changes admission: linked flate groups fall from 278
to 234 while admitted descriptors grow from 2,096 to 2,439, including former singletons. With work
policy, shared compilation grows from 11 to 18 and concrete compilation inside those groups falls
from 24 to 19. Serde changes from no shared compilations to three under fixed nomination. Group
preparation in these separate ledgers costs 0.19–0.61 ms. All valid timing outliers remain, including
a 1,122.1 ms tiny-call run; engine time does not explain those startup stalls.

Focused checks preserve fragment encoding/identity, reject invalid descriptors, and keep
concatenated archive decoding lazy while publishing one decoded template per content ID. A
forced-compilation fixture with different dictionaries and call targets in two independent archives
uses one shared entry instead of two: 421 versus 842 code bytes with frame memory, or 469 versus
938 with SSA. Results remain instance-specific in all four configurations. Eight native-matching
contract runs cover memory/SSA, normal/fork and image/package execution. A compiler-independent
runner also passes the package's normal and fork invocations with forced SSA; its dynamic linkage
contains neither rustc nor LLVM. Eleven generic checks, thirteen package checks, ten experimental
unit checks, four forced identity configurations, both builds' all-target/all-feature Clippy and
repository boundary checks pass. No full runtime suite was run.

Separate flate CPU captures contain 375 address-grouping and 428 content-grouping samples. The
remaining CPU work includes interpreter statements, block dispatch, guest calls/frame setup and
Cranelift. Their cycle-weighted flame graphs are diagnostic captures, not paired latency samples
or a precise attribution of small differences.

Optional scalar SSA retains nonaliasing values across compiled blocks. Address escapes, overlapping
or inconsistent-width accesses retain memory semantics. Entry, call adapters, cleanup and interpreter
return preserve the concrete frame through synchronization; ordinary scalar calls use their read and
write ranges, while cleanup and opaque destinations retain conservative boundaries. Cranelift owns
register allocation and spilling. Dynamic dictionaries and the concrete guest ABI are unchanged.
The [experiment README](../experiments/compiled-shared/README.md) owns the transformation details.

The work policy requires actual execution of two instances before shared nomination; a dominant
instance can specialize directly. Separate interpreted/shared samples exclude nested callee waits.
After shared publication, concrete admission uses shared execution credit. Worker calibration and
Engine credit budget compilation, without predicting future work or removable overhead. Frame setup
is outside the sample, and sampling cost/bias remains a qualification requirement.

Four contract invocations agree with native and hit bound images: synchronous SSA and asynchronous
SSA policy, each with normal and separate fork execution. The fixture covers mixed layouts, fields,
trait/drop and unwind behavior, recursion, concurrent entries and native callbacks. Six focused
checks cover escaping addresses, mixed/overlapping byte accesses, pointer dereferences, partial
adapter reads/writes, cleanup and opaque-return synchronization. These are bounded checks, not the
full lifecycle/signal/trace gate.

The scalar SSA qualification matrix isolates SSA with address-based groups; it does not establish
payback for SSA combined with content grouping. It contains 243 native-matching samples: nine workloads and five modes in five
alternating rounds, plus three forced-compilation pairs on many/hot/tiny inputs. Modes share one
release executable and one prepared bound image per input. Every timed process must hit its image;
persistent JIT caching is disabled and OS caches/heat orders are retained. Ledgers and CPU profiles
are separate captures. No build or lint work overlaps timing. Complete time includes compilation
and worker shutdown. These are fresh-process prepared-product runs, not cold frontend builds.
Whole-process / engine medians are:

| Workload | Direct specialization | Shared memory | Shared SSA | Policy, memory | Policy, SSA |
|---|---:|---:|---:|---:|---:|
| 64 sparse instances | 79.3 / 12.5 ms | 66.1 / 12.0 ms | 60.6 / 10.0 ms | 74.8 / 14.9 ms | 61.0 / 10.5 ms |
| 64 instances, 512 rounds, 32 iterations | 157.4 / 119.4 ms | 79.9 / 38.7 ms | 76.8 / 39.0 ms | 78.2 / 41.4 ms | 86.0 / 49.9 ms |
| Two instances, 5,000 rounds, 64 iterations | 84.2 / 42.8 ms | 74.8 / 37.7 ms | 76.8 / 38.7 ms | 78.4 / 36.6 ms | 76.3 / 36.4 ms |
| 64 instances, 512 rounds, zero iterations | 50.7 / 15.7 ms | 55.6 / 16.8 ms | 55.7 / 17.3 ms | 52.3 / 16.9 ms | 53.1 / 15.7 ms |
| Native-call-heavy wrappers | 96.1 / 56.5 ms | 96.2 / 56.3 ms | 100.4 / 54.0 ms | 112.0 / 53.9 ms | 110.4 / 53.9 ms |
| serde JSON | 115.4 / 25.9 ms | 134.7 / 30.6 ms | 118.3 / 28.0 ms | 130.7 / 24.7 ms | 123.8 / 33.1 ms |
| flate2, 2 KiB input | 195.3 / 115.2 ms | 199.6 / 129.0 ms | 210.9 / 131.8 ms | 191.1 / 119.0 ms | 204.8 / 123.0 ms |
| Two linked compute instances, one executed | 63.5 / 24.9 ms | 68.2 / 28.8 ms | 70.1 / 29.1 ms | 60.3 / 25.0 ms | 61.9 / 26.6 ms |
| Generic wrappers around native sleeps | 678.1 / 636.8 ms | 678.9 / 638.6 ms | 675.6 / 640.1 ms | 687.0 / 639.2 ms | 681.0 / 641.7 ms |

SSA has not established a whole-program improvement. Many-instance sharing still avoids concrete
nomination for instances below their individual trigger; that difference is not an isolated SSA
benefit. Hot-loop and flate engine medians do not improve with SSA. Forced hot-loop engine medians
are 333.5 / 324.5 ms for memory/SSA, while complete medians are 389.1 / 390.3 ms and include hundreds
of synchronous compilations. This control does not isolate steady-state loop throughput. All valid
outliers remain: sleep-mode whole-process maxima reach 2,442.8 ms and forced-tiny maxima reach
1,659.4 ms. Startup stalls and asynchronous compile trajectories limit small-difference attribution.

Separate ledgers show 51 promoted slots over ten hot-loop templates, and 55 over 23 flate templates.
Shared code grows from 4,535 to 5,882 bytes for hot-loop and 9,630 to 11,692 bytes for flate. Hot-loop
call bridges remain about 602,000–606,000; SSA does not remove the adapter calls. With policy, flate
still compiles ten shared templates and 23 concrete entries in admitted groups, totaling 212
functions in both memory and SSA modes. Their observed worker times are 61.5 and 61.7 ms.

Separate memory/SSA flate CPU profiles each contain 400 samples. Interpreter statement/operand
execution, the block loop, dispatch, shared call bridges and Cranelift compilation remain visible.
Perf maps are resolved offline for flame graphs. The captures identify remaining work, without
supporting precise causal percentages for the small latency differences. Timing and profiles use
the same executable; reproducible source hashes accompany the artifacts outside git.

The isolated patch builds in release and passes all-target/all-feature Clippy with warnings denied.
Its six focused unit checks and four native-matching contract invocations pass; no full runtime
suite was run. SSA remains opt-in. Remaining work includes profitable group selection, frame/call
cost attribution, archive identity, code-size/register-pressure control, RSS and the full
lifecycle/trace gates. The contract and release gates belong to
[runtime-generics-design.md](designs/runtime-generics-design.md); OSR remains deferred under T13.

## 4. Gaps and honest limits

What is refused or not yet claimed — not what is planned.

- **Platform**: Linux/ELF/x86_64 is the only claimed and tested baseline; the macOS/aarch64 axis is
  implemented but no test, `Makefile` target or CI job names it (E26).
- **Arbitrary Rust**: not supported. The corpus holds real projects and a small workload set; those
  are not a continuous gate and do not generalize.
- **JIT**: no OSR, no deopt, and no within-run tier upgrade — a function's tier is the previous run's
  heat order (E42; OSR is T13). Close stops and joins compile workers and releases `Shared`, but published JIT code
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
- **Distribution**: `.mirvm` is at unstable format v9 with direct archive verification and demand
  body decoding. Cross-build_id/target compatibility, fat artifacts and format freeze are unfinished. No daemon, REPL, checked mode, resource governance or real sandbox.
- **Cargo config**: Git source replacement, patches targeting a Git URL, and parts of the Cargo config
  surface such as `paths`/`HOST_RUSTFLAGS` are not implemented.
- **`mirvm doc`**: doctests work through a fixed rustdoc front end; independent `mirvm doc`/HTML
  generation is not claimed.

Architectural ambition is not qualification. "A reference implementation that runs in RAM" is a
long-term semantic contract; while the gaps above and a limited corpus remain, mirvm is not a finished
product covering all of Rust.

## 5. Development order

1. **Performance (D16).** Preserve the focused `fib(32)` ceiling and target the ordinary-run
   bottlenecks in E40. First address prepared launch and image materialization for short programs;
   then the combined interpretation, JIT compilation and cache-linking cost on execution-heavy
   workloads. Admission experiments must include their compilation and publication costs. Require
   alternating unprofiled process/engine controls under default persistence, empty and populated
   JIT stores, and a non-recursive workload. CPU samples and diagnostic ledgers explain those
   results; isolated throughput or hit counts do not establish payback. A local small-cache result
   does not qualify a populated or network-backed store. OSR remains deferred.
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
6. **Product capability.** Artifact-format freeze needs a compatibility and corruption policy under
   D4; direct immutable archive verification alone does not establish that promise. OS-level
   sandboxing is deliberately paused.
   Remaining stage boundaries and acceptance criteria are in `open-issues.md`.
7. **Harness budget.** Touch the harness only when a current RED cannot be reproduced or judged, and
   never harden it for a hypothetical future.
8. **Remote work stays paused** until the maintainer explicitly resumes it (`open-issues.md` G1).
