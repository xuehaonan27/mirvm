# mirvm Open Issues Register

The only register of unresolved work: construction debt, corpus-driven product debt, engine and
architecture debt, distribution and product direction, refusal boundaries, maintenance work, and
condition-triggered reopens. Every entry here is open; an item is deleted from this file in the same
change that closes it. What is currently true lives in [current-status.md](current-status.md).

Status words:

- `APPROVED`: the blueprint is accepted, only construction remains.
- `UNSCHEDULED`: diagnosis and a repair path exist, the work is not approved yet.
- `WORKAROUND`: a legitimate workaround is in service.
- `ACCEPTED`: a known, knowingly accepted limitation.
- `REFUSED`: a finalized boundary; reopening needs hard evidence.

Design references: [ram-spec.md](designs/ram-spec.md), [concurrency-arch.md](designs/concurrency-arch.md),
[modeb-mirvmar-design.md](designs/modeb-mirvmar-design.md),
[d15-cargoless-design.md](designs/d15-cargoless-design.md), [c-unwind-contract.md](designs/c-unwind-contract.md),
[frame-abi-bytecode.md](designs/frame-abi-bytecode.md),
[distribution-design.md](designs/distribution-design.md),
[mirvm-test-cargoless-contract.md](designs/mirvm-test-cargoless-contract.md).

## T. Approved, awaiting construction

- **T7** `APPROVED`: fork-child capture generation — the child must build its own generation, files,
  page pool, writer, producer and errno pointer at an ordinary boundary. Closes with a regression
  proving parent generation 0 and child generation 1 each carry their own pid and records.
- **T8** `APPROVED`: HostSyscall direct hot path. Three pieces remain — dropping the per-record cold
  sequence update on the healthy pair (needs a separate ruling, since `next_sequence` is also the
  producer-end ledger's `attempted` and deriving it in-page changes the v0 file/sequence contract);
  code-domain selection at the outermost activation entry, which needs one Engine materializing both
  plain and trace code plus both interpreter loops; and the in-page inline 64B Enter + 24B Exit writes.
- **T9** `APPROVED`: the 1B stateless raw syscall site and the double-materialized inline-asm raw
  sites. Depends on T8; closes with differential agreement on RFLAGS, GPRs, red zone, stack, full
  vector state and raw return semantics.
- **T11** `APPROVED`: Linux perf capture — a profile command and a thin script, first version
  user-space/IP-only/inherit. Permissions, lost samples and missing maps must fail loudly or be marked
  incomplete; rerun fib and the D16 real workloads; fork registry/map reset closes with T7.
- **T12** `APPROVED`: adaptive page pool and writer parameters. Under one memory budget, measure
  4/16/64 KiB pages, 24/32B Exit, return gap, drop, guest cycles, RSS and writer CPU, then implement
  4→64 KiB auto-scaling and rule on batch/checksum. No pre-filled numbers.

- **T13** `APPROVED`: one output grammar and one error vocabulary. `src/diag` is the vocabulary
  (component, severity, `codes!` register, two sinks, `src/diag/table.rs` as the one report renderer,
  `MIRVM_OUTPUT=text|json`), `src/error.rs` is the failure root that composes module enums and turns
  one into the process status, `src/sysroot.rs` and `src/options.rs` are typed, the CLI and entry
  layer speak the grammar with named exit codes, and `cache status`, `cache purge`, `deps audit` and
  `--vm-stats` are data structures with text and versioned JSON renderings. The `mirvm_log!` macro
  plus the `log` and `anyhow` dependencies are gone. Remaining: the `Result<_, String>` tail per module
  tree (vm, lower, cargoless, pack, telemetry, native, cargo_shim, image) and the raw print sites that
  go with it, rustc `--error-format=json`, and the repo-quality gates (no `Result<_, String>`, no bare
  exit code, no raw print, unique codes, no duplicated prose, diag purity). The `#![allow(dead_code)]`
  in `src/diag/mod.rs` is deleted by the last print conversion.

## C. Corpus-driven product debt

- **C6** `UNSCHEDULED`: M5.x intrinsic queue residual — `pclmulqdq.256/.512`, `vaes`, the remaining
  gather forms, the avx512.pmadd family. Add on real workload demand through the existing
  four-contact-point method; untriggered forms such as AES keep a loud Trap.
- **C8** `UNSCHEDULED`: Rust-side ctor / `.init_array` (linkme family) has never entered the corpus.
  Project Rust-side ctor/linkme on demand; nothing is pre-funded. C archive constructors (DT_INIT) are
  allowed by partition, while bare `.init`/`.fini` stay refused.

## E. Engine and architecture debt

- **E14** `WORKAROUND`: allocation goes through the mimalloc crate instead of a hand-rolled TLAB, so
  chunk, size-class and remote-free-queue details are absent. Related to E6.
- **E15** `WORKAROUND`: `--vm-stats` cannot see indirect fn-pointer out-edges, so its debt reading is
  permanently "at least this much". Closes with an out-edge discovery mechanism beyond incremental hits.
- **E16** `UNSCHEDULED`: io_uring pass-through is unproven; the optional tokio-uring path appears only
  in old design text and has no corpus comparison.
- **E17** `UNSCHEDULED`: two L2 cache gaps — sessions carrying warnings or errors are refused admission
  and diagnostics are never replayed, so warning programs get no cache hits; and entries have no
  eviction, only manual `mirvm cache purge`.
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
- **E26** `ACCEPTED`: Linux/ELF/x86_64 only — depends on pthread, dlopen, GNU linking and x86 asm
  wrappers. macOS is deferred and per-platform unwinding is unverified.
- **E32** `ACCEPTED`: inline-asm setjmp/longjmp captured-frame memory reuse hazard. The capture point
  sits in the asm-stub wrapper frame, and the interpreter frame's synthetic protocol can collide when
  that host stack memory is reused between capture and restore (confirmed by a v2 spike inside
  `Channel::send`). Real workloads, including the full wasmtime trap surface, do not hit it, and it
  disappears for a function with true native frame identity; the interpreter-frame path keeps the
  limitation.
- **E33** `UNSCHEDULED`: the unsafe trust-boundary audit is not prioritized — roughly 475 unsafe blocks
  with SAFETY comments on only 5. Closes by auditing five boundaries (FFI, global `Shared`, ELF
  parsing, fixed-address mapping, thunks, unwinding) with a minimal proof or test per real invariant,
  not by mechanical commenting. ASan/fuzz-class guarantees are unscheduled without hard evidence.
- **E34** `ACCEPTED`: `jit/translate.rs` is a single ~2,939-line file with three large matches and is a
  maintenance hotspot. Evaluate the benefit/disturbance ratio of splitting it by family before the next
  large change.
- **E35** `WORKAROUND`: the pinned toolchain's release-build miscompile of the interpreter's cleanup
  chain is more sensitive than the `[profile.release] debug = 2, strip = "debuginfo"` workaround in
  `Cargo.toml` implies: a guest panic stays at the native-matching 101 only for some code shapes of
  `run_vm_engine`. Evidence: with the same toolchain and profile, an equivalent `--vm-stats` rendering
  written inline in that function aborts with SIGABRT (134) while the same rendering behind one call
  into `vm::stats::print` exits 101; `tests/run.sh case panic_exit` and `threads_panic` are the fast-tier
  canaries. The current shape is held by the comment on that branch; a real close needs the miscompiled
  construct identified (LLVM 22 / nightly-2026-07-02), not another layout coincidence.
- **E36** `OPEN`: `jit_builtin_probe` hangs — the `native-diff` mode is RED at 53 passed, 1 failed, with
  `mirvm=124` against `native=0`, and the hang is deterministic across runs. Not a regression of the
  platform refactor: it reproduces identically on `c82a065` with that refactor reverted. It also has no
  `xfail=` marker in `tests/manifest`, so unlike a known-RED it fails the mode instead of being counted
  as expected. It went unnoticed because per-commit verification names a handful of modes rather than
  the whole `fast` tier, and this is the first `native-diff` sweep in the platform refactor.
  Needs a stack or a bisect from the JIT builtin path; a `timeout` kill leaves no diagnostic.
- **E37** `OPEN`: the `deps-image` case is RED — "L2 rerun did not hit (no cache-load)", with the
  second run reporting `cache-store` and no `cache-load` in its `mirvm-timing` line, deterministically.
  The mode is `gate` tier and carries no `xfail=`, so it fails. Found by the ELF-format sweep and not
  caused by it: the same failure reproduces on `c82a065`, `77b8e7d` and the working tree. The open
  question is whether the store half is writing a key the load half never asks for, or whether the two
  bins in the fixture genuinely should not share an image; the mode's own message names the symptom and
  not the key.

## D. Distribution and product

- **D2** `UNSCHEDULED`: release form and naming — miri-style first, JDK-style self-contained tarball
  later. Kit naming candidates are MDK/mirvm toolkit; MRsDK is rejected.
- **D3** `UNSCHEDULED`: per-function lazy loading works, but the archive is not directly verified and
  the whole package is copied; postcard is sequential, so E20 still forces per-function temporary
  decoding at load. Closes with an offset-based read-only archive that lets verifier and executor walk
  the same bounds-checked immutable bytes, stating snapshot ownership and adding no verify-then-swap
  window on a mutable inode. Do not start D4 before this closes.
- **D4** `UNSCHEDULED`: public format freeze. Format v4 is unstable; close D3's archive
  verification/ownership contract first, then review compatibility windows, capability bits, migration
  tooling and corruption/signature policy. The version reading 4 is not a freeze.
- **D5** `UNSCHEDULED`: L3 JIT machine-code cache — the largest single dev-loop lever, since re-burning
  hot functions every process is pure waste. The MC section and in-process ELF loader already prove JIT
  output can be serialized and reloaded. Candidate under D16.
- **D6** `UNSCHEDULED`: the full S3′c form, cross-project sharing — path-independent content-hash keys
  (~50ms each) plus tainted-layer and multi-layer image merging. Only same-workspace cross-bin smoke is
  delivered; schedule on real need.
- **D7** `UNSCHEDULED`: frontend-phase cost has no lever owner. eco ~143ms and ripgrep ~730–800ms are
  unowned, and V5 `-Zthreads` parallel lowering has not been restarted since V3 was dropped. An
  upstream-progress survey and change-surface assessment exist (axis A `-Zthreads` injection, axis B
  parallel prefetch, axis C full form, with a bump-pin checklist); awaiting a scheduling ruling.
- **D8** `UNSCHEDULED`: eight correctness cases have no benchmark — ripgrep_gzip, parallel_nomatch,
  mmap_binary, parallel_match, multiline_replace, tokei_sort_code, streaming_json, rust_files.
- **D9** `UNSCHEDULED`: three S4 base-image directions — base-imaging, adaptive base, and AOT machine
  code in the base. The third conflicts with "machine code never enters the cache" and must be checked
  before scheduling.
- **D10** `UNSCHEDULED`: the M3 product surface — daemon, agent API, resource governance, a real
  sandbox, and virtualization hooks (fake FS, path redirection, accounting that distinguishes guest
  `open` from interpreter-internal cache reads).
- **D11** `UNSCHEDULED`: REPL/Notebook plus safe typed embedding bindings (M7+). The remaining public
  trust surface is exactly E22: build typed bindings for concrete exports rather than dressing the raw
  ABI as safe. A persistent heap would make a REPL natural.
- **D12** `UNSCHEDULED`: the `-Cincremental` script path — not a current lever, restarts on the "large
  user crate edit-rerun" trigger. The `finalize_session_directory` pitfall is on record.
- **D13** `UNSCHEDULED`: address-model P5 increment (domain expansion and reclamation), keeping the
  current fixed-base-address spline engineering. Capability is recorded for M7+.
- **D14** `UNSCHEDULED`: a native content-addressed dependency store. The dedup unit is the full compile
  key (crate version × features × dependency closure × cfg/flags × toolchain), so X@V has one
  machine-unique build product across scripts and projects. Near-term piece delivered: a shared cargo
  target dir with a fingerprint-as-content-addressed compile key. End state is
  `~/.mirvm/store/<compile-key-hash>/` with a self-managed build plan and extern injection, designed
  together with `.mirvm` local resolution. Concurrency is ruled: publish once, then read-only hits with
  no resident big lock; coarse cleanup granularity is acceptable.
- **D15** `UNSCHEDULED`: dropping the default path's hard Cargo dependency. Own resolution and build
  scheduling, workspace resolver 2/3, Git dependencies, config layering, alternate sparse/Git
  registries, credential providers, source replacement, and path/Git/registry `[patch]` and `[replace]`
  are done. Still open: Git source replacement, patches targeting Git URLs, and remaining config
  surface such as `paths`/HOST_RUSTFLAGS. Cargo is never removed — it stays the explicit user fallback,
  the behavior judge and the continuous differential path, while cargoless stays the default.
- **D16** `UNSCHEDULED`: the unified performance campaign. The mainline is L2 fork → L3 direct hot path
  → L4 raw site, with P2 profiling in parallel, then the data ruling. Performance contracts must not be
  frozen on current fixed-double-page/generic-helper numbers. Known RED: hot-cache `fib(32)` at ~97ms
  against an 80ms threshold, which must not be relaxed to close the account. Full-process
  syscall/duration still needs an independent kernel raw-syscall stream; the old `MIRVM_SYSCALL_TRACE`
  is only a debt baseline.
- **D17** `UNSCHEDULED`: `mirvm test` as a product command. Cargo test/bench/doctest parity is
  implemented — bench, root proc-macro, resolver 1, complex member globs, workspace lints and full
  package specs, with single-package contracts 34/34 and workspace contracts 31/31 and the self leg
  proving zero Cargo through PATH sentinels and execve auditing. What remains open is the command's own
  surface and scheduling.
- **D18** `UNSCHEDULED`: the env/GC management surface — uv-style environments: a global store (D14)
  plus an environment as a lock-materialized reference set, with `~/.mirvm/envs/` registered as roots.
  GC is registered roots plus mark-sweep; raw reference counting is rejected because a crash mid-persist
  leaves permanent inconsistency. Purge unroots an environment and reclaims what is unreachable from
  the root set. Missing packages auto-fetch by default (logged explicitly), and `--offline`/`--locked`
  must fail loudly with the fetch instruction. Review together with the D14 native-store end state.
- **D19** `UNSCHEDULED`: the rustdoc/doctest frontend. The `mirvm test` scope is implemented — default
  and `--doc` selection, argument conflicts, root-library and dev dependencies, build.rs cfg/env, source
  line numbers, `no_run`, `ignore`, `compile_fail`/error codes, `should_panic(expected)`, filters,
  status text and exit codes, agreed against fixed Cargo/rustdoc on three tracks. rustdoc keeps
  extracting and judging code blocks; standalone HTML documentation generation is not claimed.

## R. Refusal boundaries (reopening needs hard evidence)

- **R1** `REFUSED`: synchronous fault signals (SEGV/BUS/FPE/ILL/TRAP) refuse guest handlers — host and
  guest faults are indistinguishable, interpreter depth is not reentrant, and returning from a handler
  would re-execute the faulting instruction forever. Crash-time exit semantics are already faithful
  (same signal as native, same SIGABRT on stack overflow). The closeable face is crash diagnostics:
  fault-site ownership, a guest-ized crash line, then termination with the same signal, i.e. the T4
  generalization plus a productized `MIRVM_SEGV_DUMP`.
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
- **R5** `REFUSED`: cold 128-bit forms Trap (`Transmute pair→aggregate`, tag width >8B
  `InvalidEnumConstruction`). No real workload has hit them; add on demand.
- **R6** `REFUSED`: the residual static-archive surface — non-PIC/thin archives, cross-archive
  dependencies and ordering, duplicate exports, RTLD_DEFAULT collisions, export-symbols, non-Linux-ELF
  and bare `.init`/`.fini`. `.init_array` is allowed and RTLD_DEFAULT same-name collisions already
  prefer the archive. Multi-archive link plans must be scheduled before any relaxation.
- **R7** `REFUSED`: weak memory-order specialization. Mapping host atomics already stays inside the RAM
  nondeterminism envelope; revisit on real workload demand.
- **R8** `REFUSED`: the asm refusal surface — `att_syntax`, `sym`, `label` (asm goto), `may_unwind`,
  non-x86_64, and asm-stub xmm/vector value operands with no slot expansion (bypassed by the stdarch
  helper route, no triggering workload). `noreturn` was upgraded to hard debt. asm goto needs
  native-compiling the whole host function with control flow crossing the boundary, and Cranelift
  supports no inline asm at all.
- **R9** `REFUSED`: `type_id`/`type_name`/`offset_of`/`field_offset` Trap. Add when a real program hits
  them.
- **R10** `REFUSED`: five intrinsics — `va_arg`, `carryless_mul`, `autodiff`, `rustc_peek`, SVE. The
  first two keep a Trap for lack of real use cases; the last three have no ecosystem meaning.
- **R11** `REFUSED`: `intrinsics::abort` yields SIGABRT where native yields SIGILL. Authorized
  difference with a workaround; align if a differential ever compares them.
- **R12** `REFUSED`: TSan cannot run guest TSD destructor scenarios — TSan thread state is destructed
  before the TSD phase, so under TSan the Ctx leaks permanently and tests avoid the scenario.
- **R13** `REFUSED`: the virtual address model, including the linear-memory compromise. The FFI axis has
  fundamental obstacles; the real address model stays (P1/P2 fixed, P3 frozen, P4 a non-issue,
  P5 → D13).
- **R14** `REFUSED`: tokei parallel JSON report ordering is unstable — upstream behavior, not mirvm
  debt. JSON cannot serve as an oracle until deterministic ordering exists; work around it with a stable
  compact aggregate.
- **R15** `REFUSED`: `ClosureFnPointer` and other track_caller-era adjustments. `ReifyFnPointer` only
  follows rustc's `resolve_for_fn_ptr`; nothing can be inferred beyond that.
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

## G. Maintenance and infrastructure

- **G1** `ACCEPTED`: GitHub and remote work are fully paused. Pending on resume: land the corpus
  manifest, re-review sources/revs/locks/licenses, continuous remote gating, restored triage.
- **G2** `ACCEPTED`: corpus and real-project evidence are Git-ignored and not a continuous gate. The
  deferred harness set is suite inventory, object GC/retention, cross-host cache, NFS/object-store
  durability and a remote gate. CheckIDs are a bounded closure, extended when Cargo fingerprints or a
  full sysroot Merkle becomes the divergence source. One symptom to know before reading a run: the
  `cargo-diff` rows whose materialized project directory is absent report FAIL rather than SKIP,
  because they carry a `stem=` but no `needs=`, so they redden on a checkout-free host instead of
  announcing that the evidence is missing. On the dev container that is `ecosystem`, `ffi_zlib`,
  `ripgrep_regex` and `warning_return`.
- **G3** `UNSCHEDULED`: jieba_cut and opencc are fully green but not wired into an automatic gate —
  jieba runs in 77–89s, close to the timeout, and opencc needs a `/tmp/opencc-local` prefix.
- **G5** `UNSCHEDULED`: three post-A2 evaluation triggers plus the purity-ledger re-review:
  clap-derive-style tainted macros → project-local tainted images; no real need for cross-project
  sharing → simplify back to a single-project key; ripgrep/tokei purity-ledger re-review.
- **G6** `ACCEPTED`: zxcvbn upstream exact-tie nondeterminism, logged to prevent misdiagnosis —
  `scoring.rs` picks from HashMap iteration order on u64::MAX-saturated ties, so even native
  self-comparison is unstable. It is now out of the saturation region and is not mirvm debt.
- **G7** `UNSCHEDULED`: real-workload decidability. The corpus's byte-exact three-way differential runs
  only when a driver is created, and the continuous gate is exit-code/oracle level; frontmatter
  dependencies use `--locked` only when `MIRVM_CARGO_LOCKED` is set, so an unset clean runner can
  re-resolve. Before release acceptance: pick a small fixed representative crate set, pin dependency
  locks, and make the interpreter/JIT/native differential continuously reproducible without turning the
  corpus into a universal gate.
- **G8** `UNSCHEDULED`: milestone labels remain in user-visible text — `src/cli.rs` `USAGE` (`mode B
  slice 2`, `M5.3-M5.5`, `D15 ... P4 default flip`, `M4 precursor spikes`, a git-history spike path),
  two error strings in `src/native/archive.rs` containing `M5.1`, panic strings in
  `src/lower/linker/{mod,entries}.rs` containing `A2` and `M4.4`, and one error string in
  `src/cargoless/driver.rs` saying `(P5 boundary)`. Wording-only.
- **G9** `UNSCHEDULED`: compiler-required deletion candidates — module-level `#![allow(dead_code)]` in
  `src/cargoless/{lockfile,manifest}.rs` may hide real dead code; `src/cargoless/resolver_config.rs` is
  entirely `#![cfg(test)]`, has no consumer and duplicates resolver policy from `config.rs`; the
  duplicated `(lo,hi)` out-store and the unread `trap_if` `_msg` in `src/vm/jit/helpers.rs`; a
  single-use `addr_of_local` in `jit/translate.rs`; a stale `#[allow(dead_code)]` in
  `src/telemetry/capture/session.rs` and a duplicated `#[cfg(test)] #[cfg(test)]` in `thread_ctx.rs`.
  Each was left because reading without changing cannot prove it.
- **G10** `UNSCHEDULED`: two TSan harness blind spots. (a) JIT is outside the net: `tests/data/fixtures/tsan/Cargo.toml`
  omits cranelift and `src/vm/jit/**` is cfg'd out behind `feature = "cranelift"`, so JIT worker
  slot/`trace_enter` publication and the trace domain's pinned-register path are uninstrumented;
  covering them costs a cranelift dependency and slows the build. (b) Fork-child capture rebuild is
  structurally untestable — TSan refuses to create a thread after a multithreaded fork (exit 66), and
  the only thread the engine creates in a child is the `rebuild_session_from_recipe` writer. Both are
  documented in `tests/data/fixtures/tsan/README.md`.
- **G11** `UNSCHEDULED`: a mode's `bad()` does not fail its case, so some checks are reported and
  then discarded. `tests/run.sh` runs a mode inside a subshell and judges it by that subshell's exit
  status, and `harness.sh`'s `case_summary` — the only thing that turns the `fail` counter into a
  non-zero status — is called by no mode: `framework-self-test.sh` ends with `[ "$fail" -eq 0 ]`,
  every other `mode_run` ends on an `if`/`fi` and therefore returns 0. A check that is not the last
  statement of its mode, and does not `return 1` itself, cannot fail the suite. The visible instance
  is `cargo-runner default output drifted` in the `diagnostics` mode, which fails on every run —
  the compiler's diagnostics reach physical stderr on the cold first `run` (1116 bytes) and not on
  the cache-warm `capture` (503) — while the case still reports PASS, on `f2279f1` as well as on the
  platform-refactor tree. Wiring `case_summary` into each mode surfaces every such silent failure at
  once, which is why it is recorded rather than changed here.
- **G12** `ACCEPTED`: the network-dependent cases need the dev container's HTTP proxy in the
  environment, and mirvm's own registry client does not read Cargo's config, so an unexported proxy
  makes them fail in a way that looks like a product defect. `~/.cargo/config.toml` carries the proxy
  for Cargo; exporting `HTTPS_PROXY`/`HTTP_PROXY`/`ALL_PROXY` (and the lowercase spellings) before
  `tests/run.sh` is what makes them turn green. Without it, `telemetry` reports seven FAILs —
  "parent produced no capture file", "fork child produced no capture file", the generation checks and
  the three trace-domain checks — all downstream of one
  `HTTP fetch failed https://index.crates.io/config.json: Network is unreachable`, and `pair` fails
  the same way. With it, `telemetry` passes all fifteen. This is runner environment, not debt:
  recorded so the next person does not read the cascade as a capture regression.

## F. Reopen triggers

Each item is a condition and what it reopens.

- Stackful coroutines/continuations, or a proven win from a separate VM stack → frame model B
  re-evaluation.
- D16 chooses inline allocation/guest-TLS fast paths, or multi-Engine embedding is scheduled (two gates,
  first to land rules) → vmctx R cache layer re-measurement.
- A real workload proves a guest activation stays in the host for a long time and the timeline must be
  started/stopped while it runs → OSR / rebuildable code-domain migration; per-block polling is not an
  acceptable workaround.
- A long-lived activation already in the trace domain demands a concrete maximum event-visibility
  latency that page-full/natural-return publishing cannot meet → active-page publish policy, compared
  per-K watermark against a deadline under real load; no per-event low-latency switch may be added first.
- A v0 file must first be read by a second-generation producer/consumer, or a stable format is about to
  be promised → log schema compatibility, extensions, dictionary and migration tooling.
- After L4, a real diagnostic requires observing libc internals, opaque archives, or whole-process
  syscall duration → an independent kernel raw-syscall stream with unambiguous correlation.
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
  diagnostic hook moves to a guard/formal interface (→E22).
- The pinned toolchain is upgraded → D9e emit-pruning re-measurement.
- A "single run, huge delta, cannot pre-lower" workload shape is measured (REPL/macro-expansion style)
  → lazy-lowering candidate A restarts.
- The fixed stdarch helper count grows significantly, or an instruction has no stdarch/CLIF expression
  → D7b general asm-stub vector ABI.
- Cross-project S3′c sharing becomes a hard requirement → plan B (chain + build barrier) revives.
- The "large user crate edit-rerun" scenario appears → `-Cincremental` script path (→D12).
- A real MMIO/device-register workload appears → wide volatile chunking's "no atomicity promised"
  boundary.
- Suite inventory/set identity sees real demand → deferred harness set (→G2).
