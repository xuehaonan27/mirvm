# Concurrency Architecture

> Status: Decided RFC · Scope: the M4 born-concurrent engine — how guest threads map to OS threads, why the execution phase must never hold a `tcx`, and how every piece of engine state is synchronized.
>
> Landed: the tcx-free execution phase, the three-way state split, 1:1 true threads, host atomics, the TSan gate and the independent `os::` layer (2026-07-18/19, E21 closed). Open when this RFC closed: mode B, the hand-rolled TLAB (v1 = `mimalloc` crate backend, E14) and checked mode (E23). [current-status.md](../current-status.md) is the authority on the present state; do not read every future shape here as current directory structure.

## 1. Contract

These rules bind every engine change. Ledger ids are given in parentheses where the original RFC cited them.

1. **Born concurrent (C1).** VM tier maps N guest threads to N real OS threads, each running `interp_frame` on its own native stack plus compiled code, with no GIL. tier-0 (GIL) is a transition only. Passing the TSan gate is the hard gate before removing the GIL.
2. **Concurrent memory model (C2).** Every piece of engine state belongs to exactly one cell of §2.3 and is synchronized per that cell. The engine exposes the concurrent memory model; it does not arbitrate guest synchronization.
3. **Rust dividends (C3).** No GC, so no concurrent GC and no safepoints. The C++20 memory model is ready-made and maps atomics directly to hardware. Safe Rust's type system rules out data races, so mirvm protects only the implementation's own state; a guest race is the guest's problem.
4. **Guest UB stance (C4).** An unsafe guest race must behave as native. The engine must not lock on the guest's behalf.
5. **The three moves (C8).** The engine keeps `tcx` out of the execution path by three measures: `tcx`-derived metadata is frozen into `BytecodeBody` during lowering; lowering is confined to the single-threaded load phase and JIT compilation runs on a background service thread; the Rust Heap gets a thread-local allocator.
6. **Model A (C11).** Guest activations live on the per-thread native stack. This document is the concurrency half and `frame-abi-bytecode.md` is the frame/bytecode half; the two hold together or not at all.
7. **Cranelift and packaging (C12).** The JIT is Cranelift over tcx-free bytecode that stays close to MIR; `.mirvm` is a multi-target artifact.
8. **Isolation (C13).** Guest and engine share one real address space, so structural isolation is mandatory (`L1`) and checked mode is the design reserve (§3.2).
9. **Never hold `tcx` in the execution phase.** A `tcx` in the execution phase is the tier-0 disease recurring. `tcx`'s only user is mode A's load phase. `TyCtxt` is `!Sync` (arena, interner are not thread-safe) and `InterpCx` queries `tcx` on every step (`layout`/`instance_mir`/…), which is why tier-0 could only run one host thread.
10. **Guest atomics must become real host atomics.** The interpreter must issue host atomic instructions for `atomic_*` ops; simulating them with ordinary reads/writes under real threads is a data race in the engine itself. Compiled code emits atomic instructions and fences through Cranelift.
11. **`Sync` engine, lock-free execution path.** No global lock in VM tier; threads meet only in the explicit-sync cell. New state that fits neither "private" nor "read-only" must be explicitly synchronized and preferably insert-once/published.

## 2. Model

### 2.1 Two run modes

Both modes share one runtime (execution engine, memory, threads, JIT) and differ only in the first half: how source or an artifact becomes bytecode + frozen metadata. The lifecycle therefore splits into two phases.

| Phase | Threads | Produces / does |
|---|---|---|
| Load | single, before any guest thread spawns | the complete bytecode set + frozen metadata |
| Execution | N real OS threads | runs bytecode + compiled code; never holds a `tcx` |

**Mode A — run from source** (`mirvm run x.rs`, dev inner loop):

```
rustc_private frontend (parse / macros / typeck / borrowck / MIR) → tcx
  → mono collector (all reachable monomorphized instances, as codegen does)   [uses tcx]
  → lower MIR → bytecode + freeze layout / offsets / vtable / drop / call targets [uses tcx] → complete bytecode set → (tcx may be dropped) → spawn threads, execute
```

`tcx` exists but is confined to the load phase (single-threaded, before spawn); it may be dropped after loading (daemon mode may retain it for incremental re-lowering after a code change). Startup cost = rustc frontend (seconds) — acceptable for dev, and far below a full compile because codegen/LLVM/link are skipped.

**Mode B — run `.mirvm`** (`mirvm run app.mirvm`, distribution):

```
mirvmc (offline): frontend + monomorphization + Stable MIR extraction → multi-target .mirvm
runtime: pick matching target segment → deserialize Stable MIR → lower Stable-MIR → bytecode + freeze   [no tcx]
  → complete bytecode set → spawn threads, execute
```

There is no `tcx` and no rustc at all. Stable MIR is self-contained (pre-monomorphized bodies + layout + symbols), so lowering needs no `tcx`. Startup = deserialize the matching segment.

**`mirvmc` vs mode A.** `mirvmc` is the first half of mode A's load phase (frontend + monomorphization + Stable MIR extraction) that serializes instead of executing. One frontend/mono implementation serves both: run → execute (mode A), or serialize → distribute (mode B).

| | Mode A: from source | Mode B: `.mirvm` |
|---|---|---|
| Frontend / tcx | yes, confined to load phase | none |
| Monomorphization | load phase, eager (uses `tcx`) | offline (`mirvmc` already did it) |
| Lowering input | internal MIR (queries `tcx`) | serialized Stable MIR (self-contained) |
| Startup cost | rustc frontend (seconds) | deserialization (fast) |
| Execution-phase concurrency | tcx-free, same as B | tcx-free |
| Use | dev inner loop | distribution (consumer needs no toolchain) |
| Analogy | `java Main.java` (source launcher: compile then run) / CPython running `.py` | `java -jar app.jar` / running `.pyc` |

### 2.2 Why execution is identical, and why it must never hold a `tcx`

Both modes converge before any guest thread spawns: the same complete bytecode set, the same frozen metadata, the same runtime from spawn onward. `mirvm run x.rs` and `mirvm run app.mirvm` must be the same program, same entry, same behavior, swappable when a program grows — the JVM JEP 330 property. From-source runs the whole rustc frontend before executing, so it does not share JEP 458's "error delayed to execution time" drawback: every error surfaces before execution.

Rust monomorphization is static. The mono collector walks reachability at compile time, and `dyn`/function pointers use fixed vtables/pointers, so no new monomorphization and no reflective run-time instantiation occurs. All reachable bytecode can therefore be lowered before execution. `tcx` is a load-phase tool, and the parallel execution phase never touches it; that is the foundation of a `Sync` engine. The JIT compile service is background and lazy, but it translates hot bytecode — already tcx-free — to native with Cranelift, so it needs no `tcx` either. `tcx`'s only user is mode A's load phase.

**Eager vs lazy lowering.** Eager lowering (lower everything before execution) is the default and the recommendation: lowering is cheap (resolving offsets and calls, not JIT compilation) and in mode A the rustc frontend already dominates the cost. Lazy lowering (lower a function on first call, for faster startup) is harmless in mode B (no `tcx`, only a concurrent cache), but in mode A it would drag `tcx` into the execution phase and require a service thread to confine it. Mode A therefore uses eager. The conclusion: `tcx` is a load-phase matter, never an execution-phase matter; only the JIT is lazy/background, and it needs no `tcx`.

**Load-phase parallelism.** In from-source mode the load phase runs through rustc's frontend: rustc frontend parallelization is still nightly-experimental (#113349, not stable); typeck/borrowck/MIR-opt parallelize, but parse/macro expansion/HIR lowering stay serial (currently `-Z threads` saves 20-30%). mirvm rides whatever parallelism rustc gives, free. In `.mirvm` mode the load phase is Stable-MIR→bytecode, tcx-free, and that parallelism is ours to control. Execution parallelism (N real threads, fully parallel) is therefore our design and is decoupled from rustc's frontend: rustc's frontend parallelism affects only from-source load-phase speed.

### 2.3 The three-way state split

Every piece of engine state must fall into exactly one cell, with the sync strategy of that cell. Landed names: `Shared` for published read-only, `Ctx` for per-thread private.

| Cell | Contents | Sync |
|---|---|---|
| **Per-thread private** | native stack (guest frames) / slaved operand area / TLS instance / errno / unwind payload stack / **Rust Heap thread arena** | none (naturally private) |
| **Published read-only** | frozen `BytecodeBody` / layout tables / vtable / constant pool / resolved instance metadata / loaded `.mirvm` program | release barrier (release publish → acquire read), then lock-free reads; never modified |
| **Explicit sync** | instance→bytecode cache (lazy lowering) / instance→compiled code cache (JIT) / thread registry / Rust Heap global block pool | lock / concurrent structure / atomic publish (write-rare, read-many) |

Design guideline: every M4 data structure must pass this table. Classify every new engine state first. Anything that fits neither "private" nor "read-only" must be explicitly synchronized and preferably made insert-once/published (write once, read many) so locks stay off hot paths.

Guest state is not in this table. Guest `static`s, heap objects and atomics live in the Rust Heap at real addresses and are synchronized by guest code (a safe `static` is `Sync`-constrained or behind a `Mutex`; an unsafe race is the guest's under C4). The engine provides memory (real addresses) and executes atomic instructions; it never adds a lock for the guest.

| Region | Contents | Sharing |
|---|---|---|
| `VmShared` | frozen bytecode / layout tables / vtable / constant pool (read-only); instance→bytecode and instance→compiled caches, thread registry, Rust Heap global block pool (explicit sync) | all OS threads |
| Per thread | native stack (guest frames) + operand area + TLS + errno + unwind payload + Rust Heap arena | one OS thread |

Each guest thread is one OS thread running `interp_frame` (Model A: frames on its native stack) or compiled code. VM tier has no global lock; threads meet only in the explicit-sync cell.

### 2.4 What makes the engine Sync

**Metadata frozen at lowering.** Lowering MIR/Stable-MIR → bytecode resolves all tcx-derived data into `BytecodeBody` (`frame-abi-bytecode.md` §5): layout, field offsets, discriminant encoding, vtable layout, drop glue instances, call targets. Execution reads only `BytecodeBody` and never touches `tcx`. The execution path is therefore thread-safe (no `!Sync` value) and fast (no per-op query, ≈ HotSpot's resolved constant pool). `BytecodeBody` is published read-only, so all threads share it lock-free.

**Lowering in the load phase; JIT as a background service.** Bytecode lowering is eager and happens before guest threads spawn. Mode A uses `tcx` there (single-threaded); mode B has none (Stable MIR is self-contained). The execution phase receives the complete bytecode set and never touches `tcx`. JIT compilation is a background service thread, isomorphic to HotSpot's compiler threads: hot bytecode → native via Cranelift, no `tcx`. An execution thread that meets an uncompiled hot function requests compilation or keeps interpreting; the finished body is atomically published into the compiled-code cache (explicit-sync cell). That same background service is M5's background JIT. Invariant, stronger than the earlier draft: `tcx` appears only in mode A's single-threaded load phase; the parallel execution phase and the background JIT service never touch it. Violation is the tier-0 disease recurring. (An earlier draft had a service thread confining `tcx` inside the execution phase; eager lowering replaces that. Only lazy lowering would need to return to confinement, which is why mode A chooses eager, §2.2.)

**Thread-local Rust Heap allocator.** Decision (2026-07-05): go directly to a thread-local allocator; do not take the "system malloc v0" fallback. Borrow the JVM TLAB concept — a thread-local lock-free fast path — but use mimalloc/snmalloc structure, not a pure bump pointer. A pure TLAB bump holds only with a GC (bulk reclamation, never an individual free); mirvm has individual free (every `Box`/`Vec` drop deallocs), so it needs a size-class free list plus a remote-free queue.

- Fast path is lock-free: each guest thread allocates from its own thread heap (size-class free list / new-page bump). `__rust_alloc`/`alloc_zeroed` land here; no lock, no libc round-trip.
- Cross-thread free (common when an `Arc`/`Box` is sent across threads and dropped on another): a remote-free queue (mimalloc/snmalloc style) returns the block to the owning thread heap. Not a hot path.
- Large objects: above a threshold they go straight to the global block pool / `mmap`, bypassing the thread heap.
- Refill / global block pool: a thread heap short of pages asks the global block pool (explicit sync, low frequency); the backend is large `mmap` blocks.
- Real addresses, isolation and alignment: blocks are sliced to guest alignment; real addresses are natural; guest memory and engine metadata use separate pools.
- Implementation choice: hand-roll, or use `mimalloc` / `snmalloc-rs` as the Rust Heap backend (they are exactly this, battle-tested, snmalloc especially good at cross-thread free) and add only a thin real-address/isolation wrapper. Do not reinvent the wheel.

| JVM TLAB experience | Borrow? | Note |
|---|---|---|
| thread-local lock-free fast path | yes, core | modern concurrent malloc does the same |
| refill-waste heuristic (tail too large → large objects go to the shared area; a mostly-full buffer is not retired) | yes | avoids wasting the tail; the waste limit adapts |
| adaptive sizing (by thread allocation rate and target refill count) | idea only | JVM recomputes per GC epoch; with no GC, trigger on period/refill count instead |
| large objects bypass the fast path | yes | already covered |
| filler/dummy to keep the heap parseable | no | no GC, not needed |
| pre-zeroing (`ZeroTLAB`) | no | Rust `alloc` returns uninitialized and only `alloc_zeroed` zeroes — cheaper than JVM |
| generational/promotion/Eden/safepoint retire | no | no GC |
| individual free | special case | JVM has no such problem; mirvm must handle it, hence the size-class free list (mimalloc), not a pure bump |

**True OS threads.** Creation intercepts only `pthread_create`, inserting a trampoline (C8); `Thread.id` is a real `pthread_t`, so `join`/`futex`/`into_pthread_t` go through real libc (`frame-abi-bytecode.md` §8). A new thread is a real OS thread and allocates its private state (arena/operand area/TLS). The thread registry (explicit-sync cell) registers live guest threads for id allocation, shutdown and diagnostics; create/destroy take the lock. Attach (the JNI `AttachCurrentThread` analogy): when a C library-created thread calls back into the guest, the thunk finds the OS thread absent from the registry, attaches it (allocates per-thread state, registers it), and then runs `interp_frame`; detach cleans up. Returning from the main thread is process exit (native semantics); detached threads die with the process.

**Atomics: the engine does not intervene.** Guest atomic ops carry guest semantics; the engine only executes hardware atomic instructions. The interpreter implements `atomic_*` ops with host atomic instructions (host `AtomicU*` / inline atomics) on real addresses; compiled code emits atomic instructions and fences via Cranelift. Both operate on real addresses, so they are naturally correct across real OS threads (= native behavior). Rust's C++20 memory model is ready-made and maps directly to hardware (C3). The engine does not synchronize guest atomic accesses — that is guest-level synchronization — it only guarantees that an atomic op becomes a real atomic instruction.

### 2.5 Interface with `frame-abi-bytecode.md` (co-hold, C11)

- Per-thread native stack holding guest frames (Model A) + per-thread slaved operand area = the per-thread-private cell.
- `BytecodeBody` (frozen metadata) = the published-read-only cell; lowering (§2.4) produces it.
- `JITBackend`/Cranelift compiled-code cache = the explicit-sync cell; the JIT service thread (§2.4) produces it.
- Unwind (`frame-abi-bytecode.md` §7 candidate A, Cranelift landing pad) proceeds independently on each per-thread native stack, with no cross-thread sync.
- Trampoline/thunk (`frame-abi-bytecode.md` §8) = create/attach (§2.4). Together the two documents are the complete foundation of the M4 engine: frames/bytecode/JIT (frame-abi) plus concurrency/state/lifecycle (this document).

## 3. Boundaries

### 3.1 Deliberate refusals

| Not done | Reason |
|---|---|
| `L2` MPK/PKU | too arch-specific (x86-only); demoted to an optional accelerator for `L3` |
| `L4` process sandbox | out of scope (user decision, 2026-07-05: no sandbox) |
| Pure TLAB bump | invalid with individual free (unbounded growth) |
| `filler`/dummy heap parseability | no GC, not needed |
| TLAB pre-zeroing (`ZeroTLAB`) | not needed; Rust `alloc` returns uninitialized and only `alloc_zeroed` zeroes |
| Generational/promotion/Eden/safepoint retire | no GC |
| Adding locks for the guest | guest synchronization is the guest's responsibility (C4) |
| Cooperative scheduling as an end state | GIL/cooperative scheduling is a stepping stone; `into_pthread_t` proved cooperative scheduling runs wrong |
| Alloca migration away from the slaved operand area | revoked 2026-08-12; the interpreter formally keeps slaved; alloca reopens only on real performance evidence |
| Guard-page / implicit-trap check elimination as the main checked-mode mechanism | guest memory is scattered (heap arena + native stack + statics), not a bounded region — the Wasm Memory64 problem; guard pages only help 32-bit-offset guests |
| Reference-level validation (full Miri) as the default checked mode | too slow; checked mode is a spectrum, lite → full Miri |
| Lazy lowering in mode A | would drag `tcx` into the execution phase; mode A uses eager (§2.2) |

### 3.2 Isolation and the checked-mode design reserve (C4/C13)

Threat: in real-address mode guest and VM share one address space, so guest unsafe UB (wild pointer/UAF/out-of-bounds), FFI/C-library defects and inline asm can write into VM-owned memory (metadata, interpreter, bytecode, other threads' stacks) → crash or silent corruption. Guest safe code provably cannot (Rust type system, C3); only UB or native defects can, so this is not a threat for the vast majority of code.

Fundamental tension: real addresses (chosen for zero-marshalling FFI and native fidelity) are incompatible with Wasm-style cheap memory enclosure. Wasm bounds-checks every access into one linear memory; a Rust guest uses real pointers = real addresses. Pick one; there is no free lunch.

Layered defense (user decision, 2026-07-05: cut `L2`/`L4`, focus on `L1`+`L3`):

| Layer | Blocks | Status |
|---|---|---|
| `L0` type system (free) | safe guest code cannot reach VM memory | natural, covers the vast majority |
| `L1` structural isolation | VM memory vs guest memory in separate pools, in a known address region + guard page | do it; also reduces the `L3` region check to one range comparison |
| `L3` checked mode (opt-in, untrusted/LLM) | region check before every raw dereference → a wild write is caught before corruption | design reserve, below |
| ~~`L2` MPK/PKU~~ | — | rejected: too arch-specific (x86-only); optional `L3` accelerator |
| ~~`L4` process sandbox~~ | — | rejected: out of scope |

Checked mode (the reserve; the Rust type system makes it far cheaper than Wasm):

- The JIT can insert checks: mirvm lowers MIR/bytecode → CLIF and Cranelift compiles only the CLIF it is given, so checked mode inserts region checks (compare + branch before load/store) at lowering; fast mode inserts none (= native codegen). Machine code never escapes our control.
- Only raw dereferences are checked: a safe reference access (`*r`, `r: &T`) is provably valid absent UB and is not checked; only raw pointer dereferences (unsafe) can go wild and are checked (MIR distinguishes by pointer type). Good code is overwhelmingly safe references, so check points are few — Wasm checks everything, mirvm checks only raw dereferences, and total overhead is pressed down by that.
- The check is a region check (`addr ∈ guest memory region`): compare + branch, predicted-taken, only at raw dereferences, almost always passing (only a true wild pointer fails). `L1` reduces it to a single range comparison.
- Borrowed from the JVM ([implicit null check](https://shipilev.net/jvm/anatomy-quarks/25-implicit-null-checks/), [uncommon trap](https://shipilev.net/jvm/anatomy-quarks/29-uncommon-traps/)): static check elimination (a raw pointer proven to come from a known allocation with a bounded offset drops the check; the same idea as BCE); deopt/speculation + profile-driven (M5). Implicit trap (guard page) is harder for us — guest memory is scattered, not a bounded region, exactly the [Wasm Memory64](https://github.com/WebAssembly/memory64/issues/3) problem — so explicit region checks are the main mechanism.
- Wasm boundary: guard-page elimination is near-zero cost, but only for 32-bit-offset guests; mirvm's real 64-bit pointers are worse than Memory64 and guard pages are unusable. The Rust safe/unsafe distinction keeps check points few, and overhead is pressed there rather than with guard pages.
- Honest boundary: checking only raw dereferences misses the case where unsafe code launders a wild address into a `&T` and then dereferences it (that needs reference-level validation = full Miri, slow); wild writes almost always go through raw pointers, so the cost/benefit is good. Checked mode is a spectrum: lite (raw dereference, cheap, catches most) → full Miri (everything, slow).
- Model-A interaction: the slaved operand area keeps guest locals in a known region, separate from VM native stack frames, so the region check is cheap; alloca would interleave with VM state and need per-frame tracking. The alloca-must-migrate promise was revoked on 2026-08-12; the interpreter formally keeps slaved, and alloca reopens only on real performance evidence. The decoupling requirement stands: frame-local storage and fast/checked mode must not be coupled; they meet only at the `GuestMemory::contains(addr) -> bool` predicate.
- Profile: checked mode is opt-in (fast mode has no checks ≈ native speed; checked serves untrusted/LLM). The interpreter/JIT speed delta from checks is an explicit profile and optimization target (pressed by BCE/deopt).
- Key asymmetry: compiled code touches only guest memory, so checks go into its CLIF; the interpreter performs guest accesses, so checks go into its raw-deref handling. Both tiers can check; only the insertion point differs.
- By use: trusted dev/own project = fast mode + `L1` (guest UB is the guest's own bug, = native); untrusted/LLM/agent (P0) = checked mode (`L3`) + `L1`. Bottom line: real addresses and Wasm-style cheap enclosure are incompatible, no free lunch, but the Rust type system makes checked mode far cheaper than Wasm (only raw dereferences are checked). Same source as "OS-level isolation handles safety": that protects the host, this protects the VM from the guest.

### 3.3 tier-0 → VM tier (transition)

| | tier-0 (transition) | VM tier (target) |
|---|---|---|
| Threads | real pthread + trampoline (real `pthread_t`, `into_pthread_t` holds) | same |
| Execution | GIL over real threads: interp execution serialized by a global lock, released before blocking (futex/join/FFI/lowering) | no GIL, true parallel |
| `tcx` | single-threaded access under the GIL (safe) | never touched (§2.4) |
| Structure | isomorphic to VM tier (drop the GIL and it is parallel) | — |

The GIL is a stepping stone, not an end state: it lets `into_pthread_t` and friends hold in tier-0 too, with the same structure as VM tier. Cooperative scheduling is emulation to throw away (user judgement: `into_pthread_t` proved it runs wrong). Removing the GIL requires the three-way state split (§2.3) in place, `tcx` out of the execution path (§2.4), and a concurrent Rust Heap (§2.4).

## 4. Verification

- `./tests/run.sh suite runtime.tsan` — the TSan verdict (`tests/suites/runtime/tsan.sh`): `src/vm` is compiled source-for-source under `-Zsanitizer=thread` with `TSAN_OPTIONS=halt_on_error=1`, and every id in the suite's `EXPECTED` list must print `PASS`: `mixed-stack-fib`, `atomic-cross-tier`, `blocking-io-liveness`, `mixed-stack-unwind`, `engine-atomics-thunk-cache`, `capture-session-lifecycle`, `engine-close-race`, `guest-threads`, `signal-delivery`, `fork-guard`. One case: `cd tsan && MIRVM_BUILD_ID=0000000000000000 RUSTFLAGS="-Zsanitizer=thread" cargo +nightly-2026-07-02 run -Zbuild-std --target x86_64-unknown-linux-gnu --release -- <case-id>`. TSan targets only the engine's own state (`VmShared`, caches, registry, arena/block pool, the publish protocol); guest unsafe races are explicitly not covered (C4), and cases keep guest memory race-free by construction so any warning is an engine bug. The channel depends on the engine core staying free of `rustc_private` (the `tests/tsan/` harness shares `src/vm`).
- `./tests/run.sh suite runtime.semantics` — the threads section: the five `tests/scripts/threads_{spawn,channel,sync,time,panic}.rs` differentials against native (stdout + exit code + normalized stderr); `tests/scripts/c_blocking_io.rs` must print `got: [104, 105]` and `tests/scripts/c_net_echo_threaded.rs` must print `echo = "echo"` (a blocking syscall blocks only itself); `tests/scripts/c_rayon.rs` must print `par_sort ok = true` inside the 20 s hard gate; `tests/scripts/recursion_deep.rs` under `MIRVM_STACK_SIZE=1m MIRVM_JIT_THRESHOLD=1 MIRVM_JIT_SYNC=1` must exit 70 with the JIT stack-overflow diagnosis; this section also runs `runtime.tsan` unless `SKIP_TSAN=1`.
- Spike 4 acceptance (2026-07-07): 8 real host threads in parallel mixed execution (i2c/c2i concurrent) + cross-tier same-address atomics + blocking syscall liveness (corpus §2.1 scenario closed) + concurrent mixed-stack unwind, with TSan full instrumentation and zero race warnings. The Spike 4 workload is atomic counter, mpsc producer/consumer, Mutex 8×N contention, Arc shared sum and scoped threads. Output must also agree with the single-thread/tier-0 differential (byte-for-byte on deterministic loads; timing-sensitive loads rely on invariants). This is the concretization of "the engine passes TSan" in ledger §5.3 and C1's hard gate before removing the GIL.
- `./tests/run.sh fast` (daily), `./tests/run.sh smoke` (adds small real-world/corpus loads) and `./tests/run.sh gate` (full gate) are the repository entries; `runtime.tsan` is the suite that proves this document.

## 5. Open items

Unimplemented when this RFC closed: mode B, the hand-rolled TLAB (v1 = `mimalloc` crate backend, E14) and checked mode (E23).

1. **Publish protocol.** Insert-once and tear-free publication for the instance→bytecode and instance→compiled-code caches. Concurrent `HashMap` (dashmap-style) + atomic publish vs `RwLock` (correctness first). Start with `RwLock`; profile before optimizing.
2. **TLAB allocator details.** Arena chunk size and size classes, the concrete remote-free queue, the large-object threshold, and arena reclamation at thread exit. Copy mimalloc/jemalloc structure.
3. **Thunk attach.** Cost and lifetime of per-thread state allocation (detach timing) when a C-created thread first calls back.
4. **Isolation strength.** The concrete layout of the Rust Heap vs engine metadata pools; whether to add guard pages / a separate `mmap` region.
5. **GIL release points.** Where tier-0 releases the lock (`futex_wait`/`join`/blocking FFI) to avoid deadlock from holding the lock while blocking.
6. **Mode A eager lowering cost.** If lowering all bytecode makes startup slow on large programs, re-evaluate lazy + tcx-confine (§2.2/§2.4), but prefer eager to keep the execution phase tcx-free.

Reopen triggers: alloca reopens only on real performance evidence (2026-08-12 revocation); checked mode (`L3`) activates for untrusted/LLM/agent workloads (P0); mode A lazy lowering reopens only with a proven eager-lowering startup cost; the hand-rolled-vs-crate TLAB choice is E14.
