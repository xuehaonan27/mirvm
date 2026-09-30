# mirvm Open Issues Register

The only register of unresolved work: open defects, approved work awaiting construction, engine and
architecture debt, distribution and product direction, corpus-driven product debt, maintenance work,
refusal boundaries and condition-triggered reopens. Every entry here is open; an item is deleted from
this file in the same change that closes it, and an id is never reused, so a gap in a sequence marks a
closed entry. What is currently true lives in [current-status.md](current-status.md); the analysis
behind a deferral inside a design stays in that design's own "Open items" section.

Status words:

- `OPEN`: a diagnosed defect or a red gate, with no agreed repair yet.
- `APPROVED`: the blueprint is accepted, only construction remains.
- `UNSCHEDULED`: diagnosis and a repair path exist, the work is not approved yet.
- `WORKAROUND`: a legitimate workaround is in service.
- `ACCEPTED`: a known, knowingly accepted limitation.
- `REFUSED`: a finalized boundary; reopening needs hard evidence.

Design references: [ram-spec.md](designs/ram-spec.md),
[concurrency-arch.md](designs/concurrency-arch.md),
[frame-abi-bytecode.md](designs/frame-abi-bytecode.md),
[vmctx-passing.md](designs/vmctx-passing.md), [ffi-boundary.md](designs/ffi-boundary.md),
[c-unwind-contract.md](designs/c-unwind-contract.md),
[c1-ffi-agg-design.md](designs/c1-ffi-agg-design.md),
[c2-rlib-symbols-design.md](designs/c2-rlib-symbols-design.md),
[modeb-mirvmar-design.md](designs/modeb-mirvmar-design.md),
[distribution-design.md](designs/distribution-design.md),
[dep-sharing-design.md](designs/dep-sharing-design.md),
[jit-code-cache-design.md](designs/jit-code-cache-design.md),
[d15-cargoless-design.md](designs/d15-cargoless-design.md),
[mirvm-test-cargoless-contract.md](designs/mirvm-test-cargoless-contract.md).

## Open defects

- **E38** `OPEN`: the cargoless resolver cannot read back a lock naming serde 1.0.229. A fresh-mode run
  writes a lock whose `serde` depends on the newly split-out `serde_core`, and the next (lock-mode) run
  fails deterministically with "dependency serde_core of serde …@1.0.229 has no edge assignment record
  (internal inconsistency)" (`src/cargoless/resolve/units.rs`, also `features.rs`). Repro: clear the
  materialized project under `build/scripts/<key>`, run `fixtures/cargoless/serde.rs` (it succeeds),
  run it again (it fails). The defect is the resolver round trip — its own fresh resolution produces a
  graph its lock reader rejects — not the fixture.
- **E40** `OPEN`: `fib32`'s 80ms ceiling is unreachable, and has been: the best of three runs is about
  8.7s on the Linux verify host, against 9.1s in the round-7 full gate. The `--vm-call fib(32)` path
  raises one compilation request per function, and the baseline/optimized tier policy with heat-order
  pre-linking now exists (`src/vm/jit/state/mod.rs`, `src/vm/jit/compiler/mod.rs`), so the open question
  is which tier `fib` is built at and where the interpreted calls come from. The ceiling is the right
  number — 80ms is what compiled recursion costs — so this closes with a measurement of the interpreted
  calls, never by raising it. The same question is what makes the JIT probes expensive: 30000
  iterations of `jit_builtin_probe` cost 129s on that host with 525 functions compiled and every
  publish accounted for, against 140s with `MIRVM_JIT=off`, and the cost is linear in the iteration
  count. Its counters at 3000 iterations (`MIRVM_JIT_STATS=1`) read `c2i=3146740 alloc=42930
  call_terminate=974353 tls_ref=3503 call_indirect=13524 call_builtin=16070`, so the time is not in the
  compiled bodies.
- **E43** `OPEN`: a JIT batch's `.eh_frame` registration races guest unwinding. The compile worker
  registers each batch with `__register_frame` while guest threads may already be unwinding, and libgcc
  mutates that list under a lock it does not take on the lookup path, so a panic racing a registration
  can end as `_URC_END_OF_STACK` instead of a caught exception. Registration is serialized with the
  other registrations and always happens before the entries are published; closing the rest means one
  merged section per Engine re-registered at a point the guest cannot be unwinding at. The repository
  gate runs its tests on one thread for the same reason. Analysis in
  [jit-code-cache-design.md](designs/jit-code-cache-design.md) §6.
  A probe was built to measure the window before changing the registration path: 8 guest threads
  unwinding 512 caught panics while `MIRVM_JIT_THRESHOLD=1` had 64 first calls compile and register
  batches, three runs plus a `MIRVM_JIT=off` control, all clean. That is the expected outcome for a
  window of a few instructions inside `__register_frame`, not evidence that the window is closed — the
  race has to be removed by construction, and widening it would need an interposer inside libgcc.
  The gate that removes it is a process-wide read/write pair: an unwind holds the read side for the
  dynamic extent of its `_Unwind_RaiseException`/`_Unwind_Resume_or_Rethrow` call (the whole walk
  happens inside it) and registration takes the write side. Two hazards need the ruling first: the
  write side must not block a compile the unwinding thread is synchronously waiting for in
  `MIRVM_JIT_SYNC` mode (dispatch waits for publication inside a cleanup pad), and the read side has to
  be released on the paths where the unwinder installs a context instead of returning, so the counter
  behind it needs its own release point in mirvm's landing-pad bookkeeping.
- **E47** `OPEN`: five real-project corpus cases are RED on the Linux verify host — four rayon-family
  timeouts (`rayon`, `flate2`, `brotli`, `tiny_skia`) and one genuine Trap (`png_round`,
  `llvm.x86.pclmulqdq.512`, the intrinsic queue C6, whose repair is the intrinsic queue). The smoke
  tier runs them and counts them, so they are RED in every `make smoke` and `make gate`; the four
  timeouts are wall-clock.
- **E49** `OPEN`: `wasmtime_wat` still does not finish. Its guest work completes (stdout reaches its
  last oracle line, `trap null = uninitialized element`) and then the Engine close waits out the case
  budget for leases the driver's own parked threads hold, where native would exit the process and take
  them with it (`mlua_lua` is green now, and both drivers used to abort with `mirvm[m4-engine]: Engine
  activation exited out of order`). The abort is fixed: a native `longjmp` (Lua's error raise, a wasm
  trap) unwinds the host stack over mirvm's frames without running their destructors, and two
  boundaries recover from it — `ActivationGuard` restores the activation stack to the length recorded
  at its entry instead of aborting, and a libffi-closure callback frame's execution lease lives in a
  thread registry that `ffi::call_addr` truncates back when the native call returns. The registry
  entry is held by a guard, so a Rust unwind releases it and only a foreign `longjmp` can leave it
  behind; two engine-lifecycle tests were the witness that the unwind case matters.
  What remains is the close: either idle guest threads keep their own leases and the close waits for
  threads native would not wait for, or a callback skipped on a thread with no guest-to-native call
  frame (a wasmtime worker) has no truncation point. Closing needs that ruling — a bounded wait with a
  loud report, a detach, or process-exit semantics — plus a corpus witness that exits with guest
  threads parked.


## Approved, awaiting construction

- **T8** `APPROVED`: HostSyscall direct hot path. Three pieces remain — dropping the per-record cold
  sequence update on the healthy pair (needs a separate ruling, since `next_sequence` is also the
  producer-end ledger's `attempted` and deriving it in-page changes the v0 file/sequence contract);
  code-domain selection at the outermost activation entry, which needs one Engine materializing both
  plain and trace code plus both interpreter loops; and the inline 24B Exit write, which still goes
  through the shared cold `record_syscall_exit` (`src/telemetry/capture.rs`). The inline 64B Enter
  write is delivered.
- **T9** `APPROVED`: the 1B stateless raw syscall site and the double-materialized inline-asm raw
  sites. Depends on T8; closes with differential agreement on RFLAGS, GPRs, red zone, stack, full
  vector state and raw return semantics.
- **T11** `APPROVED`: Linux perf capture — a profile command and a thin script, first version
  user-space/IP-only/inherit. The perf-map plumbing exists (`src/vm/jit/state/perfmap.rs`, currently
  reached only through the internal hooks); what is missing is the command, the script and a fork reset
  for the registry. Permissions, lost samples and missing maps must fail loudly or be marked
  incomplete; rerun fib and the D16 real workloads.
- **T12** `APPROVED`: adaptive page pool and writer parameters. Under one memory budget, measure
  4/16/64 KiB pages, 24/32B Exit, return gap, drop, guest cycles, RSS and writer CPU, then implement
  4→64 KiB auto-scaling and rule on batch/checksum. The pool is fixed at 4 KiB and 16/64 KiB are
  accepted wire values only (`src/telemetry/capture.rs`, `src/telemetry/format/`). No pre-filled numbers.
- **T13** `APPROVED`: one output grammar and one error vocabulary. `src/diag` is the vocabulary
  (component, severity, the `diag_codes!` register, two sinks, `src/diag/table.rs` as the one report
  renderer, `MIRVM_OUTPUT=text|json`), `src/error.rs` is the failure root that composes module enums
  and turns one into the process status, `src/sysroot.rs` and `src/options.rs` are typed, and the CLI
  and entry layer speak the grammar with named exit codes; `mirvm_log!` and the `log`/`anyhow`
  dependencies are gone. Remaining: the `Result<_, String>` tail (vm, cargoless, native, image, plus
  `src/os_arch/bridge.rs` and one cli site) and the raw print sites that go with it, rustc
  `--error-format=json`, and the repo-quality gates not yet written (no `Result<_, String>`, no bare
  exit code, no raw print, no duplicated prose). The `#![allow(dead_code)]` in `src/diag/mod.rs` is
  deleted by the last print conversion.

## Engine and architecture debt

- **E14** `WORKAROUND`: allocation goes through the mimalloc crate instead of a hand-rolled TLAB, so
  chunk, size-class and remote-free-queue details are absent (`src/vm/heap.rs`).
- **E15** `WORKAROUND`: `--vm-stats` cannot see indirect fn-pointer out-edges, so its debt reading is
  permanently "at least this much". Closes with an out-edge discovery mechanism beyond incremental hits.
- **E16** `UNSCHEDULED`: io_uring pass-through is unproven and has no corpus comparison. No io_uring (or
  tokio-uring) code or design text exists in the tree to compare against.
- **E17** `UNSCHEDULED`: sessions carrying warnings or errors are still refused admission and
  diagnostics are never replayed, so warning programs get no cache hits (`src/cli/driver.rs`). Entries
  are evicted only by `mirvm cache purge`: its no-flag default mark-and-sweeps every fragment and
  frozen chunk no current-generation manifest names (`src/image/collect.rs`, `src/store/frags.rs`),
  while naming a family purges that family whole.
- **E19** `ACCEPTED`: syscall interception covers FFI libc wrappers, `libc::syscall` varargs, guest
  inline-asm raw syscalls, `global_asm`/naked and JIT. The residual is a vendored-C form that writes the
  `syscall` instruction itself, plus adversarial self-modifying `.byte 0x0f,0x05`; only OS-level
  seccomp covers those. Virtualized semantics belong to D10.
- **E22** `ACCEPTED + UNSCHEDULED`: two gaps in embedding's trust surface. (1) Not a fully safe typed
  API: `Package::load` is safe but `instantiate` must be `unsafe`, because the verifier cannot prove
  that packaged native libraries, host symbols and FFI signatures agree, and manual Module and raw
  two-word exports are unsafe as well; closing it needs generated, verified typed bindings for concrete
  export signatures. (2) Raw addresses have no general revocation protocol — any third-party library
  may hold a callback indefinitely, so published closures, JIT code and unwind tables, and committed
  images live to process end; strict in-process bounds need a completion or revocation contract for the
  specific native registration API hit, or subprocess isolation.
- **E23** `UNSCHEDULED`: checked mode (L3) is unbuilt. Double-ended `PROT_NONE` operand guards and
  pre-entry JIT stack checks exist, but pointer provenance does not: IR Deref distinguishes neither raw
  from reference nor frame/frozen/allocator/FFI/mmap ownership, so mapping checks alone would wrongly
  admit VM metadata. Closes when lowering and IR preserve provenance and the runtime maintains
  ownership ranges automatically. The limit is the declared raw-dereference check, not a sandbox.
- **E26** `ACCEPTED`: Linux/ELF/x86_64 is the only claimed and tested baseline — it depends on pthread,
  `dlopen`, GNU linking and x86 asm wrappers. macOS/aarch64 is an implemented platform axis
  (`src/os/macos/`, `src/os_arch/macos_aarch64/`, `src/arch/aarch64/`, selected by the `#[cfg]`
  ladders), but it is unverified: no test, `Makefile` target or CI job names it, the JIT's macOS pair
  items (`MAP_JIT`, `pthread_jit_write_protect_np`, the code-arena placement rules, the `Arm64Call`
  veneer) are absent, and per-platform unwinding is unmeasured.
- **E32** `ACCEPTED`: inline-asm setjmp/longjmp captured-frame memory reuse hazard. The capture point
  sits in the asm-stub wrapper frame, and the interpreter frame's synthetic protocol can collide when
  that host stack memory is reused between capture and restore (confirmed by a v2 spike; the boundary
  is recorded at `src/lower/func/asm.rs`). Real workloads, including the full wasmtime trap surface, do
  not hit it, and it disappears for a function with true native frame identity; the interpreter-frame
  path keeps the limitation.
- **E33** `UNSCHEDULED`: the unsafe trust-boundary audit is not prioritized — 1126 `unsafe {`
  occurrences under `src/`, 92 `unsafe fn`, 10 `unsafe impl` and 124 `unsafe extern`, against 28
  `SAFETY` comments. Closes by auditing the six named boundaries (FFI,
  global `Shared`, ELF parsing, fixed-address mapping, thunks, unwinding) with a minimal proof or test
  per real invariant, not by mechanical commenting. ASan/fuzz-class guarantees are unscheduled without
  hard evidence.
- **E35** `WORKAROUND`: the pinned toolchain's release-build miscompile of the interpreter's cleanup
  chain is more sensitive than the workaround in `Cargo.toml`
  (`[profile.release] debug = 2`, `strip = "debuginfo"`, `codegen-units = 256`) implies: a guest panic
  stays at the native-matching
  101 only for some code shapes of `run_vm_engine`. Evidence: with the same toolchain and profile, an
  equivalent `--vm-stats` rendering written inline in that function aborts with SIGABRT (134) while the
  same rendering behind one call into `vm::stats::print` exits 101; giving `run_driver` one more
  argument (and threading it through its callers) made every guest panic abort, caught or not, while
  the same feature shaped as a process-global flag read at the execution seam kept the canaries green
  (`tests/run.sh case panic_exit`, `threads_panic`). The abort is a silent `abort()` after the panic
  hook, i.e. the unwinder failing in phase 2, and it disappears under
  `CARGO_PROFILE_RELEASE_CODEGEN_UNITS=256` but not under `=1`, which points at codegen-unit packing
  and inlining rather than at any particular statement. The current shape is held by the comment on
  that branch; a real close needs the miscompiled construct identified (LLVM 22 / nightly-2026-07-02),
  not another layout coincidence.
- **E42** `UNSCHEDULED`: a within-run tier upgrade has no signal. The interpreter stops counting a
  function the moment its first entry is published, so a function that becomes hot during a run keeps
  the tier the *previous* run's `cache/package-heat` gave it. The candidates are a counter beside the
  PLT slot, a cheap sampled counter in the fast path, and the interpreter's existing call counter;
  whichever lands has its cost measured against the D16 ledger before the tier thresholds mean
  anything. Analysis in [jit-code-cache-design.md](designs/jit-code-cache-design.md) §6.
- **E44** `UNSCHEDULED`: three dep-sharing numbers are still owed. Binding-walk cost on the warm path
  is unmeasured, so whether it erodes the L2 gate is unknown (the counter-move is caching bound bodies
  in the L2 entry). The store grows only from cold sessions, because a session that loaded any layer
  publishes nothing — growing it in place needs the placement rule to be independent of the load set,
  of which the stable home rule is the first half, and the store's coverage per program is the number
  to watch. And std-residue fragments that name no unit land in the first unit whose closure covers
  them, which costs unit-manifest size rather than sharing; a base-owned home is D9's adaptive-base
  direction. Analysis in [dep-sharing-design.md](designs/dep-sharing-design.md) §7.

## Distribution and product

- **D2** `UNSCHEDULED`: release form and naming — miri-style first, JDK-style self-contained tarball
  later. Kit naming candidates are MDK/mirvm toolkit; MRsDK is rejected. The design prerequisite (the
  mode B `.mirvm` package and `mirvm pack`) has landed, so only the form/naming ruling remains.
- **D3** `UNSCHEDULED`: per-function lazy loading works, and the package is loaded from one immutable
  copied snapshot (`src/pack/read.rs`), so snapshot ownership and the no-verify-then-swap requirement
  are met. What remains is archive-direct verification: verifier and executor walking the same
  bounds-checked immutable bytes, with a function body restored from the already-verified
  representation on first use; until then loading decodes every function into a temporary `FuncBody`
  for verification. Do not start D4 before this closes.
- **D4** `UNSCHEDULED`: public format freeze. `fmt_ver` is 4 and merely separates generations
  (`src/pack/format.rs`); close D3's archive verification contract first, then review compatibility
  windows, capability bits, migration tooling and corruption/signature policy. The version reading 4 is
  not a freeze.
- **D6** `UNSCHEDULED`: the residual cross-project sharing work. Machine-wide sharing of lowered
  products is delivered — `cache/frags`, `cache/frozen` and `cache/units`, with units keyed by a
  path-independent `unit_key` and a stack `[std base, unit…]` serving two programs — including
  multi-layer stack merging. What remains is tainted-layer merging: `Purity::Tainted` instances still
  land in the per-program delta (`src/lower/purity.rs`), and full cross-project sharing of the L1 rlib
  target dir is still not delivered. Schedule on real need.
- **D7** `UNSCHEDULED`: frontend-phase cost has no lever owner beyond the rustc thread pool. eco ~143ms
  and ripgrep ~730–800ms are still unowned. The `-Zthreads` parallel frontend (axis A) is implemented
  and default-on (`src/options.rs`, injected at the compiler-session boundary), so the "not restarted
  since V3" framing is obsolete; V5's own parallel lowering (axes B/C, researched in
  [history/parallel-frontend-research.md](history/parallel-frontend-research.md)) is still unbuilt.
  Awaiting a scheduling ruling.
- **D8** `UNSCHEDULED`: the real-project correctness probes still have no timing gate. Only `load`,
  `rayon-gate` and `fib32` carry a ceiling in `tests/manifest`, plus the `resources` metering case; no
  real-project probe has one. Add ceilings on real-workload demand.
- **D9** `UNSCHEDULED`: two remaining S4 base-image directions — adaptive base (scheme B of the S4
  brief) and AOT machine code in the base. The std base image itself is delivered (`src/store/mod.rs`
  `base`, `src/image/base.rs`). The third direction no longer conflicts with "machine code never enters
  the cache": the L3 cache persists machine code machine-locally
  ([jit-code-cache-design.md](designs/jit-code-cache-design.md)). It conflicts with "mode B distributes
  bytecode, never JIT output", which is the check to run before scheduling.
- **D10** `UNSCHEDULED`: the M3 product surface — daemon, agent API, resource governance, a real
  sandbox, and virtualization hooks (fake FS, path redirection, accounting that distinguishes guest
  `open` from interpreter-internal cache reads).
- **D11** `UNSCHEDULED`: REPL/Notebook plus safe typed embedding bindings (M7+). The remaining public
  trust surface is exactly E22: build typed bindings for concrete exports rather than dressing the raw
  ABI as safe. A persistent heap would make a REPL natural.
- **D12** `UNSCHEDULED`: the `-Cincremental` script path — not a current lever, restarts on the "large
  user crate edit-rerun" trigger. Cargo's own incremental mode is deliberately not enabled for the
  scripts mirvm builds (`src/cargoless/schedule/args.rs`).
- **D13** `UNSCHEDULED`: address-model P5 increment (domain expansion and reclamation), keeping the
  current fixed-base-address spline engineering (`src/os_arch/linux_x86_64/addrspace.rs`). Capability
  is recorded for M7+.
- **D14** `UNSCHEDULED`: whether a native rlib store with the original compile-key layout is still
  wanted at all. Machine-unique build products are already delivered on both sides: native rlibs come
  from the shared cargo target dir keyed by Cargo's fingerprints, and lowered products are shared
  content-addressed with a finer unit than the full compile key — one canonical body in `cache/frags`,
  4 KiB chunks in `cache/frozen`, one manifest per crate unit in `cache/units` — with the self-managed
  build plan, extern injection and the publish-once/read-only-hit lock ruling in service. Only the
  literal `~/.mirvm/store/<compile-key-hash>/` layout has no delivered home, so what is open is the
  ruling, not construction.
- **D15** `UNSCHEDULED`: dropping the default path's hard Cargo dependency. Own resolution and build
  scheduling, workspace resolver 2/3, Git dependencies, config layering, alternate sparse/Git
  registries, credential providers, registry/directory source replacement, and path/Git/registry
  `[patch]` and `[replace]` are done. Still open: Git source replacement, patches targeting a Git URL,
  `paths`, and `HOST_RUSTFLAGS` (`src/cargoless/rustflags.rs`). Cargo is never removed — it stays the
  explicit user fallback, the behavior judge and the continuous differential path, while cargoless
  stays the default.
- **D16** `UNSCHEDULED`: the unified performance campaign. The mainline is the remaining HostSyscall
  direct hot path, then the trace interpreter loop, then the stateless inline-asm raw syscall sites
  (fork generations are closed), with perf capture in parallel, then the data ruling. Performance
  contracts must not be frozen on current fixed-double-page/generic-helper numbers. Known RED: `fib32`
  against its 80ms gate, which must not be relaxed to close the account (E40 records the measurement).
  Full-process syscall/duration still needs an independent kernel raw-syscall stream; the old
  `MIRVM_SYSCALL_TRACE` is only a debt baseline.
- **D17** `UNSCHEDULED`: `mirvm test` as a product command. Cargo test/bench/doctest parity is
  implemented — bench, root proc-macro, resolver 1, complex member globs, workspace lints and full
  package specs, with single-package contracts 34/34 and workspace contracts 31/31 and the self leg
  proving zero Cargo through PATH sentinels and execve auditing. The command's argument surface is
  implemented; what remains open is the ruling that promotes it from a contract to a supported product
  command.
- **D18** `UNSCHEDULED`: the env/GC management surface — uv-style environments: the global
  content-addressed store plus an environment as a lock-materialized reference set, with
  `~/.mirvm/envs/` registered as roots.
  GC is registered roots plus mark-sweep; raw reference counting is rejected because a crash mid-persist
  leaves permanent inconsistency. Purge unroots an environment and reclaims what is unreachable from
  the root set. Missing packages auto-fetch by default (logged explicitly), and `--offline`/`--locked`
  must fail loudly with the fetch instruction. Review together with D14.
- **D19** `UNSCHEDULED`: standalone `mirvm doc`/HTML generation. The rustdoc/doctest front end is
  delivered — default and `--doc` selection, argument conflicts, root-library and dev dependencies,
  build.rs cfg/env, source line numbers, `no_run`, `ignore`, `compile_fail`/error codes,
  `should_panic(expected)`, filters, status text and exit codes, agreed against fixed Cargo/rustdoc on
  three tracks. rustdoc keeps extracting and judging code blocks; independent HTML documentation
  generation is not claimed.

## Corpus-driven product debt

- **C6** `UNSCHEDULED`: intrinsic queue residual — `pclmulqdq.256/.512`, `vaes`, and the remaining
  gather forms. Add on real workload demand through the existing four-contact-point method; untriggered
  forms keep a loud Trap. `png_round` is the live trap on record. The avx512 vpmadd52 family has
  landed.
- **C8** `UNSCHEDULED`: Rust-side ctor / `.init_array` (linkme family) has never entered the corpus.
  Project Rust-side ctor/linkme on demand; nothing is pre-funded. C archive constructors (DT_INIT and
  `.init_array`) are allowed by partition, while bare `.init`/`.fini` stay refused.

## Maintenance and infrastructure

- **G1** `ACCEPTED`: GitHub and remote work are fully paused (local `main` is ahead of `origin/main`).
  Pending on resume: re-review sources/revs/locks/licenses, continuous remote gating, restored triage.
- **G2** `ACCEPTED`: corpus and real-project evidence are Git-ignored and not a continuous gate. The
  deferred harness set is suite inventory, object GC/retention, cross-host cache, NFS/object-store
  durability and a remote gate. CheckIDs are a bounded closure, extended when Cargo fingerprints or a
  full sysroot Merkle becomes the divergence source. One symptom to know before reading a run: the
  `cargo-diff` rows whose materialized project directory is absent report FAIL rather than SKIP, because
  they carry a `stem=` but no `needs=`, so they redden on a checkout-free host instead of announcing
  that the evidence is missing. On a fresh home that is `ecosystem`, `ffi_zlib`, `ripgrep_regex` and
  `warning_return`.
- **G3** `UNSCHEDULED`: jieba_cut and opencc are fully green but are `manual`-tier only, so no automatic
  gate runs them; opencc needs a `/tmp/opencc-local` prefix.
- **G5** `UNSCHEDULED`: two evaluation triggers plus the purity-ledger re-review. Cross-project sharing
  is answered yes by [dep-sharing-design.md](designs/dep-sharing-design.md) and implemented as per-crate
  units plus the fragment store, so the single-project-key simplification is closed. Still open:
  clap-derive-style tainted macros → project-local tainted images, and the ripgrep/tokei purity-ledger
  re-review.
- **G6** `ACCEPTED`: zxcvbn upstream exact-tie nondeterminism, logged to prevent misdiagnosis —
  `scoring.rs` picks from HashMap iteration order on u64::MAX-saturated ties, so even native
  self-comparison is unstable. The fixture is out of the saturation region and it is not mirvm debt.
- **G7** `UNSCHEDULED`: real-workload decidability. The `native-diff` mode now gives continuous
  byte-exact native-vs-mirvm differentials (54 cases, 48 in `fast`), but the interpreter/JIT/native
  triple on a fixed pinned crate set is still not one; frontmatter dependencies use `--locked` only
  when `MIRVM_CARGO_LOCKED` is set, and the cargoless default resolves fresh. Before release
  acceptance: pick a small fixed representative crate set, pin dependency locks, and make the
  three-way differential continuously reproducible without turning the corpus into a universal gate.
- **G8** `UNSCHEDULED`: milestone labels remain in user-visible text — `src/cli/mod.rs` USAGE
  (`mode B slice 2`); error strings in `src/native/artifact/archive.rs` (`M5.1`, `P1 entry`),
  `src/lower/linker/entries.rs` (`M4.4`), `src/vm/native_instance/wire.rs` (`P1 recipe`) and
  `src/cargoless/driver.rs` (`(P5 boundary)`); panic strings in `src/lower/linker/{mod,alloc}.rs`
  (`A2 closure violation`) and `src/vm/thunks.rs` (`P1 closure`); plus the A2 re-lower line in
  `src/lower/purity.rs`, the `M4.x` phase buckets in the `--vm-stats` rendering (`src/vm/stats.rs`),
  and the `P5` verdict label in `src/cargoless/audit.rs`. Wording-only.
- **G9** `UNSCHEDULED`: compiler-required deletion candidates — module-level `#![allow(dead_code)]` in
  `src/cargoless/lockfile.rs` and `src/cargoless/manifest/mod.rs` may hide real dead code;
  `src/cargoless/resolver_config.rs` is entirely `#![cfg(test)]`, has no consumer and duplicates
  resolver policy from `config.rs`; the duplicated `(lo,hi)` out-store in `src/vm/jit/helpers.rs` and
  the unread `trap_if` `_msg` in `src/vm/jit/translate/place.rs`; the stale `#[allow(dead_code)]` on
  `pending_rebuild_recipe` in `src/telemetry/capture_session/rebuild.rs`; a duplicated `#[cfg(test)]`
  in `src/vm/ctx/thread_ctx.rs`. Each was left because reading without changing cannot prove it.
- **G10** `UNSCHEDULED`: two TSan harness blind spots. (a) JIT is outside the net:
  `tests/data/fixtures/tsan/Cargo.toml` omits cranelift and `src/vm/jit/**` is cfg'd out behind
  `feature = "cranelift"`, so JIT worker slot/`trace_enter` publication and the trace domain's
  pinned-register path are uninstrumented; covering them costs a cranelift dependency and slows the
  build. (b) Fork-child capture rebuild is structurally untestable — TSan refuses to create a thread
  after a multithreaded fork (exit 66), and the only thread the engine creates in a child is the
  `rebuild_session_from_recipe` writer. Both are documented in `tests/data/fixtures/tsan/README.md`.
- **G11** `UNSCHEDULED`: a mode's `bad()` does not fail its case unless the mode's last statement is
  fail-sensitive. `repo-quality`, `prepare`, `vmcall` and `framework-self-test` now guard on their fail
  counter, but several modes still end on a bare `ok` or an `if`/`fi` — `deps-image`, `frag-collect`,
  `jit-cache`, `metering`, `unit-share`, `telemetry`, `pack`, `diagnostics`, `cargoless-git` and
  `cargoless-sources` among them — so a non-final `bad` in them is reported and discarded.
  `case_summary` (`tests/lib/harness.sh`) is still not any mode's verdict.
- **G12** `ACCEPTED`: the network-dependent cases need the dev container's HTTP proxy, and mirvm's own
  registry client does not read Cargo's config, so an unexported proxy makes them fail in a way that
  looks like a product defect. Run the suite under `the proxy helper` (`docs/environment.md`); the proxy must
  not be exported globally. Without it, `telemetry` reports seven FAILs — "parent produced no capture
  file", "fork child produced no capture file", the generation checks and the three trace-domain
  checks — all downstream of one `HTTP fetch failed https://index.crates.io/config.json: Network is
  unreachable`, and `pair` fails the same way. With it, `telemetry` passes all fourteen. This is runner
  environment, not debt: recorded so the next person does not read the cascade as a capture regression.

## Refusal boundaries (reopening needs hard evidence)

- **R1** `REFUSED`: synchronous fault signals (SEGV/BUS/FPE/ILL/TRAP) refuse guest handlers — host and
  guest faults are indistinguishable, interpreter depth is not reentrant, and returning from a handler
  would re-execute the faulting instruction forever. Crash-time exit semantics are already faithful
  (same signal as native, same SIGABRT on stack overflow), and fault-site ownership with the
  productized `MIRVM_SEGV_DUMP` is delivered (`src/os_arch/linux_x86_64/signal.rs`). The closeable face
  is the guest-ized crash line.
- **R2** `REFUSED`: the vfork/clone/clone3/setjmp/longjmp family, `pthread_exit`, `pthread_atfork` and
  multithreaded fork. Fork-alone in a single thread is allowed behind the `/proc/self/task` guard; the
  rest needs frame-model-level engineering or non-local control flow through interpreter frames.
  Reopening the multithreaded fork/vfork/atfork face can only go to native parity, where "best effort,
  may break" is native's own wording.
- **R3** `REFUSED`: eleven guest-visible unwinder context/state symbols
  (`_Unwind_Set/GetGR/SetIP/Resume/ForcedUnwind/LSDA…`). They would see host interpreter frames, not
  guest frames. Closure needs a guest frame/IP/LSDA translation layer plus differential probes;
  `Backtrace`/`GetIP`/`FindEnclosingFunction`/`GetCFA` are already honored through shadow frames.
- **R4** `REFUSED`: general nested DSTs and other metadata forms. Freeze an evaluation of a general DST
  layout expression first; slice/str static formulas and direct dyn tail-vtable runtime alignment are
  already supported.
- **R5** `REFUSED`: cold 128-bit forms (`Transmute pair→aggregate`; `InvalidEnumConstruction` with a
  Zst/Pair source). 16-byte enum tags narrow to the low 64 bits rather than being refused. No real
  workload has hit the residual; add on demand.
- **R6** `REFUSED`: the residual static-archive surface — non-PIC/thin archives, cross-archive
  dependencies and ordering, duplicate exports, RTLD_DEFAULT collisions, export-symbols, non-Linux-ELF
  and bare `.init`/`.fini`. `.init_array` is allowed and RTLD_DEFAULT same-name collisions already
  prefer the archive. Multi-archive link plans must be scheduled before any relaxation.
- **R7** `REFUSED`: weak memory-order specialization. Mapping host atomics already stays inside the RAM
  nondeterminism envelope; revisit on real workload demand.
- **R8** `REFUSED`: the asm refusal surface — `att_syntax`, `label` (asm goto), `may_unwind`,
  `naked_asm`, non-x86_64, and cleanup-unwind actions. `const`/`sym` operands and >8B xmm/vector value
  operands are handled (`src/lower/func/asm.rs`), and `noreturn` is supported. asm goto needs
  native-compiling the whole host function with control flow crossing the boundary, and Cranelift
  supports no inline asm at all.
- **R9** `REFUSED`: `type_id`/`type_name`/`offset_of`/`field_offset` Trap. Add when a real program hits
  them.
- **R10** `REFUSED`: an intrinsic with no lowering is a loud refusal, never a silent fallback — a name
  the engine has no builtin for fails lowering with `Error::unsupported`
  (`src/lower/func/term.rs`), and a recognized form whose semantics are deliberately not implemented
  lowers to `Stmt::Trap` (R9's identity/offset family). Add a form when a real program hits one;
  ASan/fuzz-class guarantees stay unscheduled without hard evidence.
- **R11** `REFUSED`: `intrinsics::abort` yields SIGABRT where native yields SIGILL. Authorized
  difference with a workaround; align if a differential ever compares them.
- **R12** `REFUSED`: TSan cannot run guest TSD destructor scenarios — TSan thread state is destructed
  before the TSD phase, so under TSan the Ctx leaks permanently and tests avoid the scenario.
- **R13** `REFUSED`: the virtual address model, including the linear-memory compromise. The FFI axis has
  fundamental obstacles; the real address model stays (P1/P2 fixed, P3 frozen, P4 a non-issue,
  P5 → D13).
- **R14** `REFUSED`: tokei parallel JSON report ordering is unstable — upstream behavior, not mirvm
  debt. JSON cannot serve as an oracle until deterministic ordering exists; the corpus runs tokei only
  in its default (non-JSON) mode.
- **R16** `REFUSED`: residual `global_asm` `sym` refusals — a `sym` fn whose signature cannot be derived
  (aggregate/Rust ABI/varargs), a `sym` static pointing at a guest static (still hit by the
  mangled-static audit), and in dependency crates a `sym` pointing at the dep's own guest fn needing the
  entry budget in bin-link context. Real forms such as pulp take zero operands and are unblocked.
- **R17** `REFUSED`: residual FFI by-value marshalling boundaries — by-value unions, by-value SIMD
  vectors, vararg trailing aggregate positions, aggregates with align>8, and by-value multi-variant
  enums (all a loud `Err`), plus packed/align(N) unnatural-layout aggregates, which
  `validate_agg_natural` turns from silent miscalls into a loud freeze. `{i128}`/f128/long-double/
  `_Complex` keep their existing scalar boundaries. Full padding expression is scheduled only on real
  workload demand.
- **R19** `REFUSED`: `#![no_main]` / `#[start]` entry forms — any entry type other than
  `EntryFnType::Main` is refused loudly with exit 1 plus diagnostics. Reopening needs a real workload.
- **R20** `REFUSED`: libffi foreign/callback supports only the C/System ABI; other ABIs are rejected
  during lowering rather than squashed into plain C. Reopening needs a real adapter and a native
  differential for the target ABI.
- **R21** `REFUSED`: the async signal support surface. Covered: traditional process- and thread-directed
  handlers on Linux/ELF/x86_64 with no advanced guest flags; `SI_TKILL` from `pthread_kill` or a real
  libc `raise` entering a stable per-installation cell for the target pthread; glibc's
  global-last-round TSD teardown ordering; `_exit(70)` for a stub reinstalled after its owner closed;
  and `EngineFault(70)` from `HostRaise`. Still refused: synchronous-fault guest handlers, realtime
  signals (needs per-event queueing and `siginfo` retention), `SA_SIGINFO` (needs a three-argument guest
  ABI), `SA_ONSTACK` (needs alternate-stack lifetime), and `SA_NODEFER`/`SA_RESETHAND` (would change the
  mask/registration state machine). Process-directed external signals are promised only at the owner
  Engine's next ordinary safe point.

## Reopen triggers

Each item is a condition and what it reopens.

- Stackful coroutines/continuations, or a proven win from a separate VM stack → the flat-frame model
  re-evaluation.
- D16 chooses inline allocation/guest-TLS fast paths → vmctx R cache layer re-measurement.
- A real workload proves a guest activation stays in the host for a long time and the timeline must be
  started/stopped while it runs → OSR / rebuildable code-domain migration; per-block polling is not an
  acceptable workaround.
- A long-lived activation already in the trace domain demands a concrete maximum event-visibility
  latency that page-full/natural-return publishing cannot meet → active-page publish policy, compared
  per-K watermark against a deadline under real load; no per-event low-latency switch may be added first.
- A v0 file must first be read by a second-generation producer/consumer, or a stable format is about to
  be promised → log schema compatibility, extensions, dictionary and migration tooling.
- A real diagnostic requires observing libc internals, opaque archives, or whole-process syscall
  duration → an independent kernel raw-syscall stream with unambiguous correlation.
- P2 can only show interpreter host hotspots and cannot attribute guest logical positions → logical
  sampling at safe points first; enter signal sampling only if measured bias is unacceptable.
- The first long capture imposes a disk bound or power-loss recovery requirement → rotation, byte cap,
  final fsync, or a persistent black box.
- Dynamic capture sessions, or trace code/producer descriptors, can grow unboundedly with process
  lifetime → full epoch reclamation; until then only bounded tombstones.
- T12 proves stable scanning, `pwritev`, or scheduling is the main bottleneck → challenge ready-page
  MPSC, staging/io_uring/mmap/compression, or scheduling parameters respectively.
- A real interpreter load proves frame-local storage is the end-to-end bottleneck and a native-stack
  scheme still wins after charging zeroing, stack probing, unwinding and checked costs → alloca
  frame-local storage candidate.
- The pinned rustc changes the summary structure or ships a formal diagnostic protocol → the runner
  diagnostic hook moves to a guard/formal interface (→T13).
- The pinned toolchain is upgraded → re-measure the load phase and the emit-pruning assumptions behind
  the base image.
- A "single run, huge delta, cannot pre-lower" workload shape is measured (REPL/macro-expansion style)
  → lazy lowering is re-evaluated.
- The fixed stdarch helper count grows significantly, or an instruction has no stdarch/CLIF expression
  → extend the asm-stub vector operand channel.
- The "large user crate edit-rerun" scenario appears → `-Cincremental` script path (→D12).
- A real MMIO/device-register workload appears → wide volatile chunking's "no atomicity promised"
  boundary.
- Suite inventory/set identity sees real demand → deferred harness set (→G2).
