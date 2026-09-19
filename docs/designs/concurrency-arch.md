# Concurrency Architecture

> Status: Decided RFC · Scope: how guest threads map to OS threads, why the execution phase must
> never hold a `tcx`, and how engine state is synchronized. Frame and bytecode questions live in
> [frame-abi-bytecode.md](frame-abi-bytecode.md); the two hold together or not at all.

## 1. Contract

These rules bind every engine change.

1. **Born concurrent.** The VM tier maps N guest threads to N real OS threads, each running
   `interp_frame` on its own native stack plus compiled code, with no GIL. Passing the TSan gate is
   the hard gate for any change to this path.
2. **Concurrent memory model.** Every piece of engine state belongs to exactly one cell of §2.3 and
   is synchronized per that cell. The engine exposes the model; it does not arbitrate guest
   synchronization.
3. **Rust dividends.** No GC, so no concurrent GC and no safepoints. The C++20 memory model maps
   atomics directly to hardware, and safe Rust rules out data races, so mirvm protects only its own
   state — a guest race is the guest's problem.
4. **Guest UB stance.** An unsafe guest race must behave as native. The engine must never lock on the
   guest's behalf.
5. **The three moves.** `tcx` stays out of the execution path because tcx-derived metadata is frozen
   into `BytecodeBody` during lowering, lowering is confined to the single-threaded load phase with
   JIT compilation on a background service thread, and the Rust Heap gets a thread-local allocator.
6. **Model A.** Guest activations live on the per-thread native stack.
7. **Cranelift and packaging.** The JIT is Cranelift over tcx-free bytecode that stays close to MIR;
   `.mirvm` is a multi-target artifact.
8. **Isolation.** Guest and engine share one real address space, so structural isolation is
   mandatory and checked mode is the design reserve (§3.2).
9. **Never hold `tcx` in the execution phase.** `TyCtxt` is `!Sync` (arena and interner are not
   thread-safe) and `InterpCx` queries it on every step, which is exactly why the old interpreter
   could only run one host thread. `tcx`'s only user is mode A's load phase.
10. **Guest atomics become real host atomics.** The interpreter issues host atomic instructions for
    `atomic_*` ops; simulating them with ordinary reads and writes under real threads is a data race
    in the engine itself. Compiled code emits atomics and fences through Cranelift.
11. **`Sync` engine, lock-free execution path.** No global lock in the VM tier; threads meet only in
    the explicit-sync cell. New state that is neither private nor read-only must be explicitly
    synchronized and preferably insert-once/published.

## 2. Model

### 2.1 Two run modes

Both modes share one runtime (execution engine, memory, threads, JIT) and differ only in how source
or an artifact becomes bytecode plus frozen metadata. The lifecycle therefore has two phases:

- **Load** — single-threaded, before any guest thread spawns; produces the complete bytecode set and
  frozen metadata.
- **Execution** — N real OS threads run bytecode and compiled code, and never hold a `tcx`.

**Mode A, run from source** (`mirvm run x.rs`, the dev inner loop):

```text
rustc_private frontend (parse / macros / typeck / borrowck / MIR) -> tcx
  -> mono collector (every reachable monomorphized instance, as codegen does)        [uses tcx]
  -> lower MIR -> bytecode + freeze layout / offsets / vtable / drop / call targets  [uses tcx]
  -> complete bytecode set -> (tcx may be dropped) -> spawn threads, execute
```

Startup costs the rustc frontend (seconds) — acceptable for development and far below a full compile,
because codegen, LLVM and linking are skipped.

**Mode B, run a `.mirvm`** (`mirvm run app.mirvm`, distribution): `mirvmc` does frontend +
monomorphization + Stable MIR extraction offline into a multi-target artifact; the runtime picks the
matching segment, deserializes Stable MIR, lowers it (no `tcx`), and executes. Startup is a
deserialization. `mirvmc` is mode A's load phase with serialization instead of execution, so one
frontend and mono implementation serves both.

The two modes are swappable when a program grows, in the same way JEP 330 lets `java Main.java` and
`java -jar app.jar` mean the same thing. From-source runs the whole frontend before executing, so it
does not share JEP 458's "error delayed to execution time" drawback.

| | Mode A: from source | Mode B: `.mirvm` |
|---|---|---|
| Frontend / `tcx` | yes, confined to the load phase | none |
| Monomorphization | load phase, eager | offline, in `mirvmc` |
| Lowering input | internal MIR (queries `tcx`) | serialized Stable MIR (self-contained) |
| Startup cost | rustc frontend (seconds) | deserialization (fast) |
| Analogy | `java Main.java` / CPython running `.py` | `java -jar app.jar` / running `.pyc` |

### 2.2 Why execution is identical and never holds a `tcx`

Both modes converge before any guest thread spawns: the same complete bytecode set, the same frozen
metadata, the same runtime. Rust monomorphization is static — the collector walks reachability at
compile time and `dyn`/fn pointers use fixed vtables — so no instantiation happens at run time and
all reachable bytecode can be lowered up front. The background JIT service translates hot bytecode
that is already tcx-free, so it needs no `tcx` either.

Lowering is eager in mode A. Lazy lowering would drag `tcx` into the execution phase and need a
service thread to confine it; only the JIT is lazy and background. Mode B may lower lazily at no
`tcx` cost, and re-evaluates that only if eager lowering is measurably slow to start.

Load-phase parallelism comes free from rustc (typeck/borrowck/MIR-opt parallelize while parse, macro
expansion and HIR lowering stay serial), and in mode B the tcx-free load phase is ours to control.
Execution parallelism is decoupled from it: rustc's frontend parallelism affects only from-source
load-phase speed.

### 2.3 The three-way state split

Every piece of engine state falls into exactly one cell, with that cell's sync strategy. Landed
names: `Shared` for published read-only, `Ctx` for per-thread private.

- **Per-thread private** (`Ctx`) — native stack (guest frames), slaved operand area, TLS instance,
  errno, unwind payload stack, Rust Heap thread arena. Sync: none, it is naturally private.
- **Published read-only** (`Shared`) — frozen `BytecodeBody`, layout tables, vtable, constant pool,
  resolved instance metadata, loaded `.mirvm` program. Sync: release publish, then lock-free reads;
  never modified.
- **Explicit sync** — instance→bytecode cache (lazy lowering), instance→compiled-code cache (JIT),
  thread registry, Rust Heap global block pool. Sync: lock, concurrent structure, or atomic publish;
  write-rare and read-many, so locks stay off hot paths.

Classify every new engine state first. Anything that fits neither private nor read-only must be
explicitly synchronized and preferably made insert-once/published.

Guest state is not in this table. Guest `static`s, heap objects and atomics live in the Rust Heap at
real addresses and are synchronized by guest code; the engine provides the memory and executes the
atomic instructions, and never adds a lock for the guest.

Each guest thread is one OS thread running `interp_frame` or compiled code.

### 2.4 What makes the engine Sync

- **Metadata frozen at lowering.** Lowering resolves all tcx-derived data into `BytecodeBody` —
  layout, field offsets, discriminant encoding, vtable layout, drop glue instances, call targets.
  Execution reads only `BytecodeBody` and never touches `tcx`, which makes the path thread-safe (no
  `!Sync` value) and fast (no per-op query, roughly HotSpot's resolved constant pool).
- **Lowering in the load phase, JIT as a background service.** The execution phase receives a
  complete bytecode set. The JIT service thread is isomorphic to HotSpot's compiler threads: hot
  bytecode to native through Cranelift, no `tcx`; an execution thread that meets an uncompiled
  function requests compilation or keeps interpreting, and the finished body is atomically published
  into the compiled-code cache.
- **Thread-local Rust Heap allocator.** Go directly to a thread-local allocator rather than a system
  malloc fallback. Borrow the JVM TLAB's lock-free fast path and its refill-waste and large-object
  bypass rules, but keep mimalloc/snmalloc structure instead of a pure bump pointer, because mirvm
  frees individual objects (every `Box`/`Vec` drop), which a GC-only bump design cannot express.
  - fast path: each guest thread allocates from its own thread heap (size-class free list, new-page
    bump); `__rust_alloc`/`alloc_zeroed` land here with no lock and no libc round trip;
  - cross-thread free: a remote-free queue returns the block to the owning thread heap (not a hot
    path);
  - large objects: above a threshold they go straight to the global block pool or `mmap`;
  - refill: a thread heap short of pages asks the global block pool (explicit sync, low frequency);
  - real addresses: blocks are sliced to guest alignment, and guest memory and engine metadata use
    separate pools.
  Rejected from the JVM comparison: filler/dummy heap markers, TLAB pre-zeroing (Rust `alloc`
  returns uninitialized and only `alloc_zeroed` zeroes) and generational/promotion retire — all of
  them exist for a GC that mirvm does not have.
  Implementation may hand-roll or reuse `mimalloc`/`snmalloc-rs` with a thin real-address and
  isolation wrapper.
- **True OS threads.** Creation intercepts `pthread_create` and inserts a trampoline; `Thread.id` is
  a real `pthread_t`, so `join`, `futex` and `into_pthread_t` go through real libc. A new thread is a
  real OS thread allocating its private state. The thread registry (explicit-sync cell) holds live
  guest threads for id allocation, shutdown and diagnostics. When a C-created thread first calls back
  into the guest, the thunk attaches it — allocating per-thread state and registering it — then runs
  `interp_frame`; detach cleans up. Returning from the main thread is process exit.
- **Atomics: the engine does not intervene.** Both tiers execute hardware atomic instructions on real
  addresses, so they are correct across real threads by construction. The engine does not synchronize
  guest atomic accesses; it only guarantees that an atomic op becomes a real atomic instruction.

### 2.5 Interface with `frame-abi-bytecode.md`

- The per-thread native stack holding guest frames plus the slaved operand area is the
  per-thread-private cell.
- `BytecodeBody` is the published-read-only cell, produced by lowering.
- `JITBackend`/Cranelift compiled code is the explicit-sync cell, produced by the JIT service thread.
- Unwind proceeds independently on each per-thread native stack, with no cross-thread sync.
- Trampolines and thunks are create/attach.

## 3. Boundaries

### 3.1 Deliberate refusals

- `L2` MPK/PKU — too arch-specific (x86-only); demoted to an optional `L3` accelerator.
- `L4` process sandbox — out of scope.
- Pure TLAB bump — invalid with individual free (unbounded growth).
- `filler`/dummy heap parseability, TLAB pre-zeroing, generational/promotion/Eden/safepoint retire —
  all require a GC.
- Adding locks for the guest — guest synchronization is the guest's responsibility.
- Cooperative scheduling as an end state — `into_pthread_t` proved it runs wrong.
- Alloca migration away from the slaved operand area — the interpreter formally keeps slaved; alloca
  reopens only on real performance evidence.
- Guard-page/implicit-trap elimination as the main checked-mode mechanism — guest memory is scattered
  (heap arena, native stack, statics), not a bounded region, so guard pages only help 32-bit-offset
  guests.
- Reference-level validation (full Miri) as the default checked mode — too slow; checked mode is a
  spectrum from lite to full Miri.
- Lazy lowering in mode A — it would drag `tcx` into the execution phase.

### 3.2 Isolation and the checked-mode reserve

In real-address mode guest and VM share one address space, so guest unsafe UB, FFI/library defects
and inline asm can write into VM-owned memory and corrupt it silently. Guest safe code provably
cannot, so this concerns UB and native defects only. Real addresses (chosen for zero-marshalling FFI
and native fidelity) are incompatible with Wasm-style cheap memory enclosure: Wasm bounds-checks
every access into one linear memory, while a Rust guest uses real pointers. Pick one.

- `L0` type system (free): safe guest code cannot reach VM memory; covers the vast majority.
- `L1` structural isolation: VM memory and guest memory in separate pools in a known address region
  with a guard page. Also reduces the `L3` check to one range comparison.
- `L3` checked mode (opt-in, for untrusted and LLM workloads): a region check before every raw
  dereference, so a wild write is caught before corruption.

How `L3` works, and why it is far cheaper than Wasm:

- Checks are inserted at lowering: mirvm lowers MIR/bytecode to CLIF and Cranelift compiles only what
  it is given, so fast mode inserts nothing and checked mode compares and branches before load/store.
- Only raw pointer dereferences are checked. A safe reference access is provably valid absent UB, and
  MIR distinguishes the two by pointer type, so check points are few because good code is
  overwhelmingly safe references.
- The check is a single region comparison, predicted-taken, almost always passing; `L1` makes it one
  range check.
- Static check elimination and deopt/profile-driven elimination are borrowed from the JVM
  ([implicit null check](https://shipilev.net/jvm/anatomy-quarks/25-implicit-null-checks/),
  [uncommon trap](https://shipilev.net/jvm/anatomy-quarks/29-uncommon-traps/)). Implicit trapping
  through guard pages is not available to us — the [Wasm Memory64](https://github.com/WebAssembly/memory64/issues/3)
  problem — so explicit region checks are the mechanism.
- Honest boundary: checking only raw dereferences misses unsafe code that launders a wild address into
  a `&T`, which would need reference-level validation. Wild writes almost always go through raw
  pointers, so the cost/benefit is good.
- The interpreter's slaved operand area keeps guest locals in a known region, separate from VM native
  stack frames, which makes the region check cheap. Frame-local storage and fast/checked mode stay
  decoupled: they meet only at `GuestMemory::contains(addr) -> bool`.
- Trusted own-project work runs fast mode plus `L1` (guest UB is the guest's own bug, as native);
  untrusted and agent workloads run checked mode plus `L1`.

## 4. Verification

- `make case C=tsan` is the verdict for this document: `src/vm` is compiled source-for-source
  under `-Zsanitizer=thread` with `TSAN_OPTIONS=halt_on_error=1`, and every id in the suite's
  `EXPECTED` list must print `PASS` — `mixed-stack-fib`, `atomic-cross-tier`, `blocking-io-liveness`,
  `mixed-stack-unwind`, `engine-atomics-thunk-cache`, `capture-session-lifecycle`,
  `engine-close-race`, `guest-threads`, `signal-delivery`, `fork-guard`. One case runs outside the
  bundled suite with `cd tests/tsan && MIRVM_BUILD_ID=0000000000000000 RUSTFLAGS="-Zsanitizer=thread"
  cargo +nightly-2026-07-02 run -Zbuild-std --target x86_64-unknown-linux-gnu --release -- <case-id>`.
  TSan covers only the engine's own state (`VmShared`, caches, registry, arena and block pool, the
  publish protocol); guest races are explicitly out of contract, and cases keep guest memory race-free
  so any warning is an engine bug.
- `make mode M=vmcall  # the exported-entry cases`, threads section: the five `data/programs/threads_{spawn,channel,sync,time,panic}.rs`
  differentials against native (stdout, exit code, normalized stderr); `c_blocking_io.rs` prints
  `got: [104, 105]` and `c_net_echo_threaded.rs` prints `echo = "echo"`, proving a blocking syscall
  blocks only itself; `c_rayon.rs` prints `par_sort ok = true` inside the 20s hard gate; and
  `recursion_deep.rs` under `MIRVM_STACK_SIZE=1m MIRVM_JIT_THRESHOLD=1 MIRVM_JIT_SYNC=1` exits 70 with
  the JIT stack-overflow diagnosis. The section also runs `runtime.tsan` unless `SKIP_TSAN=1`.
- `make test` is the daily entry, `make smoke` adds small real-world workloads and `make gate` is the
  full gate.

## 5. Open items

1. **Publish protocol.** Insert-once, tear-free publication for the instance→bytecode and
   instance→compiled-code caches: concurrent map plus atomic publish versus `RwLock`. Start with
   `RwLock`, profile before optimizing.
2. **TLAB allocator details.** Arena chunk size, size classes, the concrete remote-free queue, the
   large-object threshold, and arena reclamation at thread exit.
3. **Thunk attach.** Cost and lifetime of per-thread state allocation, including detach timing.
4. **Isolation strength.** The concrete layout of the Rust Heap versus engine metadata pools, and
   whether to add guard pages or a separate `mmap` region.
5. **Mode A eager lowering cost.** If lowering all bytecode makes startup slow on large programs,
   re-evaluate lazy lowering with tcx confinement, preferring eager to keep execution tcx-free.
