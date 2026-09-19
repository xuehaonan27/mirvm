# Mode B: the `.mirvm` package format, pack/run, the MC machine-code section, multi-Engine

> Status: Implemented · Scope: the `.mirvm` container v4, `mirvm pack` and `mirvm run x.mirvm`, the
> machine-code section, and repeatable multi-Engine instantiation. Parent case:
> [distribution-design.md](distribution-design.md) — a package is the L2 engine-IR cache made
> portable.
>
> A package is honestly self-contained: apart from genuinely foreign FFI libraries (the glibc class)
> it reads no `$HOME/.mirvm` and no rustc or Cargo traces at run time. The format is **not frozen**:
> it may change with later mode B work. `fmt_ver` only separates generations and promises no
> cross-version compatibility; external freezing is a separate review that will fix it and state the
> migration rules.

The following properties are normative, not history: all self-produced library bytes are embedded in
NATIVELIBS and materialize at run time by content hash, while STAMPS and envs are provenance only; the
loader uses a read-only mmap; MODULE holds only non-function metadata while FUNCS holds a per-function
index plus independent bodies that become resident lazily, with full semantic validation done as one
temporary per-function decode at first load; and a fixed address survives only as a logical address in
the artifact, never as a run address.

## 1. Contract

The loader refuses loudly and **never silently rebuilds** — a package is a distribution artifact, not a
cache. Each violation is a rejection:

- **C1** — an `fmt_ver` or `build_id` mismatch, naming the toolchain that must repack.
- **C2** — a `whole_hash` or section hash mismatch, as corruption.
- **C3** — a build target that does not match the current platform. STAMPS and envs never take part in
  the run-time permission check.
- **C4** — NATIVELIBS and MODULE paths must cross-validate in exact order; `role` must be legal; the
  embedded bytes must match their `fnv128`. MC must additionally cross-validate against the
  `role=global_asm` entries.
- **C5** — the section count must fit the remaining section table; every offset and length conversion
  must be checked; a section must never exceed bounds, overlap or reuse a tag; every section hash is
  checked, unknown tags included.
- **C6** — FUNCS count, table length, per-function range and overlap must be checked. Per-function
  hashes are recomputed at load and at every on-demand decode, and corrupt data must not reach
  execution.
- **C7** — a v4 package that still declares `requires_fixed_base` must be rejected; every run instance
  maps dynamically.
- **C8** — in the strict address table, every guest function address outside the frozen range must have
  exactly one P1 entry recipe with the same FuncId. A missing recipe, a duplicate, or an overlap with a
  frozen address must be rejected during safe load.

## 2. Model

### 2.1 Goals and non-goals

Goals: a single-file package format v4; `mirvm pack` from a Cargo project or script; `mirvm run
x.mirvm` to validate, load and execute; and three-dimensional acceptance — pack plus run must be
byte-identical to a direct `mirvm run` on three loads, while refusal probes must be loud.

Non-goals, registered separately and not pre-spent: cross-mirvm-version bytecode compatibility
(`build_id` must still match exactly); multi-target fat artifacts (the section table has reserved a
multi-MODULE slot); in-process loading of a static-archive `.so` without a file, since the package
materializes it and then dlopens it; and L3 JIT machine-code caching, which is unrelated to packages.

### 2.2 Container

```text
offset 0   magic        8B   "MIRVMAR\0"
           fmt_ver      u32  = 4
           build_id     u32 length + UTF-8 (MIRVM_BUILD_ID, exact match)
           section_cnt  u32
sections   tag u32 | off u64 | len u64 | fnv1a-128 (content hash)
tail       whole_hash   u128 fnv1a (the whole file except this field)
```

An unknown tag is skipped for forward compatibility; a missing required tag is a rejection.

- **META** (required) — postcard: arguments, envs, base key, target triple, whether BASE is included.
- **STAMPS** (required) — postcard: `Vec<(path, size, mtime_ns)>`, a build-provenance record only,
  never a run-time permission input.
- **BASE** (reserved) — the writer produces none, and the loader rejects the BASE/delta shape; a
  reserved tag is never treated as supported.
- **MODULE** (required) — postcard module metadata: exports, the frozen snapshot, `link_fn_addrs`,
  `FrozenReloc`, TLS, and the asm/GOT/P1 entry recipes; no run addresses and no function bodies.
- **NATIVELIBS** (required) — postcard `Vec<{path, role, fnv128, bytes}>`: every self-produced `.so`
  byte is inside the package, so the old path serves only cross-validation and diagnostics.
- **RELOC** (required) — postcard `{requires_fixed_base: bool, entry: Box<str>}`.
- **MC** (optional) — the complete ELF bytes of global_asm/dep_asm, parsed, relocated and
  symbol-registered in-process by mcload.
- **FUNCS** (required) — `count u32`, fixed index entries of `offset u64 | len u64 | fnv1a-128`, then
  the functions' postcard bodies stored contiguously.

### 2.3 pack flow

`mirvm pack <target> [-o out.mirvm]` shares everything with `mirvm run` up to the Module; only the
fork changes, writing to disk instead of executing.

1. For a Cargo project, a full build through the Cargo shim with a **forced full cold lowering**, so
   the single module is self-contained; deps-image and base layering are bypassed, which is acceptable
   because packing is a build action. No BASE is written: MODULE holds the full module metadata and
   FUNCS stores every function body independently.
2. A script or single file takes the single-file variant, with frontmatter dependencies on the same
   Cargo path.
3. Run full semantic validation on the in-memory Module, then collect META, STAMPS, NATIVELIBS
   (expanded from `module.required_native_libs`, reading each self-produced library's bytes and
   computing its hash on the spot), RELOC and FUNCS; global_asm enters MC by default. Publish
   atomically last, through a temporary name and a rename.

### 2.4 run flow

1. Sniff the magic; a non-package takes the existing path. Read once into a process-owned immutable
   byte snapshot.
2. Validate `whole_hash` and the section hashes against that snapshot; later lazy decoding never reads
   the source inode again.
3. Check META's `build_id`, `fmt_ver` and target; STAMPS and envs are deserialized only to check their
   format.
4. Restore non-function metadata from MODULE and parse the FUNCS index. To preserve semantic
   validation, decode every function temporarily and finish validation before any MC or native
   materialization, then release the temporary objects; the execution-time function table binds the
   owned snapshot, and a function becomes resident on first access. Every instantiation recovers its
   dynamic frozen state independently, establishes all P1 `LinkAddr → closure` mappings first, and then
   applies frozen pointer relocations. Each Engine gets its own `.so` image of a self-produced archive,
   and MC loads per instance. GOT, the TLS template, statics, address immediates and entries all
   resolve through the same load map.
5. Start at `RELOC.entry`, the main startup chain, forwarding argv.

Function access records the real order of the run. On release, that order is written atomically to
`$MIRVM_HOME/package-heat/<hash>.order`, keyed by the FUNCS content hash; the next load prefetches in
that order in the background from one decode worker. Demand tasks always precede predicted ones, and a
predicted function that is suddenly demanded moves to the demand queue. The wait bound is the item
currently decoding plus that demand, so a wrong prediction costs speed only and never changes
semantics.

### 2.5 MC section

- Content: one `{fnv128, bytes}` per global_asm/dep_asm ELF, cross-validated with the NATIVELIBS
  `role=global_asm` entries; identical content is stored once.
- Load: mcload parses the ELF in-process, maps and relocates it, and registers symbols and `.eh_frame`.
  Genuinely foreign FFI libraries are still resolved through the system ABI.
- Calls to guest functions from global_asm and from a static archive no longer bake a fixed P1 address.
  P1 is "the guest function entry handed to native code": machine code jumps to a hidden RIP-relative
  8-byte slot that instantiation fills with this Engine's closure. A self-produced archive pins its
  internal bindings with `-Bsymbolic` and is copied into a unique image per Engine; old images and
  closures are never reclaimed or reused, so a stale pointer after close cannot land in a new Engine
  through address reuse.
- With MC absent or `MIRVM_PACK_NO_MC=1`, the same embedded bytes materialize to
  `$MIRVM_HOME/package-native/<hash>.so` and are dlopen'd. That is a loading strategy, not a dependency
  on a preexisting cache.

### 2.6 Embedding surface

The public surface is `Package::load(path) -> Result<Package, String>` and
`unsafe Package::instantiate() -> Result<Engine, String>`. `load` is safe container and bytecode
validation into an owned snapshot; the `unsafe` on `instantiate` means the caller must still trust the
package's native libraries, foreign ABI declarations and host symbol contracts. One `Package` supports
concurrent, repeated instantiation. A hand-built Module can only go through
`unsafe Engine::from_module_unchecked`, and untyped export calls live in `raw::run_export_raw`,
returning two machine words. Ordinary callers never see `Shared`, so this is not a completed safe
typed export API.

### 2.7 Engine lifecycle

- **Running** — entered by `instantiate` returning; accepts normal entries and acquires leases.
- **Closing** — entered atomically by `close`; refuses new normal entries while the original call
  chain and registered deferred callbacks may finish.
- **Finalizing** (internal) — all normal calls and deferred callbacks have exited; final cleanup, no
  guest execution.
- **Closed** — final cleanup done and observed by `wait_closed`; stale closures report `EngineClosed`
  and addresses stay reserved.

Rules:

1. Every public call, native callback and startup or destructor process first takes an execution lease
   — the count credential meaning "this call is still using the Engine".
2. `DeferredHold` covers both the window between a pthread having accepted a callback and its actual
   start or revocation, and the period in which pthread thread-specific-data destructors may still
   run. `close` synchronously clears the current thread's destructor values and waits for other
   surviving threads to exit or delete the key, and never releases `Shared` while a callback can still
   happen.
3. Signals use a process-level disposition owner chain, a per-Engine inbox and a stable per-pthread
   cell. Every guest handler installation gets a fixed 22-byte RX stub; the kernel frame performs only
   fixed TLS reads and an atomic registration, and never enters guest code. Process-directed events go
   to the owner inbox, while `pthread_kill` and a real libc `raise` produce `SI_TKILL` into the target
   pthread's cell as established by the current installation. `close` first deactivates this Engine's
   registration, removes the owner from the chain non-LIFO, restores the next surviving layer or the
   native baseline, and then waits for in-flight frames, the owner inbox and already-received
   target-thread cells; a target event can only be closed out by the target pthread at a safepoint or
   at exit. Pthread exit alternates between closing out managed TSD and its own cell along glibc's
   global-final-round raw key cursor, and only then blocks catchable signals, rechecks and closes the
   inbox. While the current thread still has its own target events, `wait_closed` returns
   `ActiveOnCurrentThread` instead of sleeping on itself. Queries and `oldact` always return the guest
   handler address and never leak the kernel stub.
4. Each host thread's `Ctx` is the execution context that thread uses when entering the Engine. The
   Engine registers `CtxSlot` by weak reference, and once all leases have exited the finalizer clears
   each slot, so a host thread that never exits does not keep holding guest TLS, the virtual frame
   area or the whole `Shared` — only an empty slot remains.
5. Native images go through relocate, then fill P1/GOT/bridge slots, then constructors. Only an
   instance whose constructors all completed runs its destructors once, in reverse order, at close. A
   controlled MIRVM exception in a constructor is a `Result` failure that starts shutdown; destructors
   are a non-unwinding teardown boundary. A destructor may legitimately call back into the Engine
   during Closing, or create pthread work with an explicit completion event, after which one more wait
   round runs before Finalizing. Any MIRVM, foreign or host Rust exception that escapes a destructor
   is pinned to the `native finalizer unwound during Engine teardown` diagnostic and then aborts; it
   must not leave the Engine stuck in Closing.
6. `close` only initiates shutdown; `wait_closed` waits for final cleanup. If the current host thread
   is still inside that Engine's call chain, the wait returns `ActiveOnCurrentThread` so the outer call
   can exit and wait again rather than deadlocking.

Closing is not the immediate unmapping of every executable address. Native code may hold function
pointers for a long time and general FFI has no "revoke every copy" protocol, so these addresses are
deliberately retained until process end: published libffi closures, JIT machine code and the
`.eh_frame` the system unwinder uses, committed MC images, and self-produced dynamic library images. A
stale closure after close keeps only a small Engine identity tombstone, not Module or Shared. A new
Engine never reuses old P1 addresses, which avoids the ABA case where a stale pointer first goes
invalid and then happens to point at a new object. An instantiation failure before constructors run
reclaims not-yet-published closures, MC images and dlopen handles; once construction has begun,
addresses may already have escaped, so even a failed construction only runs the close protocol.

### 2.8 CLI surface

```text
mirvm pack <proj-dir|script.rs> [-o <out.mirvm>]   # default <name>.mirvm
mirvm run <x.mirvm> [-- <guest args>]
```

Both commands use the same `Package`/Engine loading path.

## 3. Boundaries

**Not safe.** `Package::load` is safe, but the native and FFI contract of a trusted package belongs to
the caller of `unsafe Package::instantiate`, and only a structured `run_main` exists — no safe host
bindings are generated from exported Rust types. If a third-party library keeps a callback
indefinitely without a completion or revocation event, the engine cannot know when that address may be
released; process-lifetime closures plus a close tombstone keep an unknown horizon from becoming a
permanent `wait_closed` stall, at the cost of a permanent process-level address retention. Before
constructors run, an instantiation failure is reclaimable; after that, escaped addresses never are.

**Not frozen.** Cross-version bytecode requires an exact `build_id` today, and external format freezing
awaits its own review; `fmt_ver` only separates generations. A v4 package does not occupy the
artifact's fixed base at run time — every Engine uses its own anonymous mapping. A multi-target fat
artifact is only a reserved section-table tag, to be judged after the freeze review.

**Not supported.** Signals cover a traditional process-directed handler and an `SI_TKILL`
thread-directed handler, with no advanced guest `sigaction` flags: a process event waits for the owner
Engine's next safepoint, a thread event for the target pthread's next safepoint or exit closeout, and
`close` may wait but cannot close out on another thread's behalf. Fixed stubs, registrations and thread
cells are retained until process end; when an old stub is reinstalled after its owner closed, bare
kernel delivery does `_exit(70)` and a `raise` through the MIRVM bridge reports `EngineFault(70)`.
Synchronous faults, realtime signals and
`SA_SIGINFO`/`SA_ONSTACK`/`SA_NODEFER`/`SA_RESETHAND` are rejected loudly, and a process-directed
external event makes no native-handler-level latency promise. proc-macros and build.rs really run only
during pack. There is no in-process loading of a static-archive `.so` without a file, and no L3 JIT
machine-code cache.

## 4. Verification

- **Three-dimensional byte equality** — a direct `mirvm run` versus pack plus run (cold, hot and
  package runs) over eco (large Cargo project), `c_faer_lu` (dependency global_asm plus pulp LD_ST) and
  `c_wasmtime_wat` (large dependency closure plus the fiber `sym` skip branch) must agree byte for byte.
- **Refusal probes** — `fmt_ver`, whole and section hashes, duplicate tag, truncated section table,
  offset overflow, section overlap, FUNCS truncation, out-of-bounds, overlap and per-body hash, and
  NATIVELIBS/MC cross-validation failures must all return errors; a P1 address with a missing recipe, a
  duplicate, or an overlap with frozen must error rather than panic.
- **Self-containment acid test** — pack with MC disabled, move the original native and global-asm
  caches away, switch to a fresh `MIRVM_HOME`: the package must still materialize by content hash and
  run byte-identically.
- **Hot-order contract** — a first run produces exactly one non-empty `.order`, and a second run
  reusing the same `MIRVM_HOME` produces unchanged output, proving the prediction channel does not
  change results.
- **Real embedding contract** — after load, rewrite and delete the source package; build two Engines
  concurrently from the same object; check static and TLS isolation, distinct function-pointer
  addresses, and that global_asm and the archive bridge each return to their own Engine. After closing
  one, its old C-unwind pointer reliably returns `EngineClosed`, the other keeps running, and a third
  Engine does not reuse either address.
- **Signal embedding contract** — queries and `oldact` keep the guest address; after two Engines
  override the same signal, closing them non-LIFO still restores the correct previous layer and native
  action; a process signal enters only the inactive owner's inbox; `pthread_kill` runs only at the
  target pthread's safepoint; a registration replacement does not cross generations; traditional
  signals of one generation coalesce per kernel semantics; and a blocked `raise` can be consumed by
  `sigwaitinfo` with a real `SI_TKILL`. During close and JIT locks a frame is only registered and close
  waits for the target thread to close out itself, while an unblocked `raise` from guest or from a
  self-produced native archive completes its nested handler before returning.
- **`make gate`** with the full set, `cargo test` and the SYNC differential stay green.

Implementation locations: `src/pack.rs` for the container and content-addressed materialization;
`src/vm/{mcload,native_instance,signal}.rs` for in-process MC loading, per-Engine native image
isolation, the P1 hidden slot and the disposition owner chain; `src/vm/ctx/` for close state,
execution leases, deferred callbacks and `CtxSlot` cleanup; `src/cli/` for the pack subcommand and
run's magic dispatch.

## 5. Open items

- **Archive direct verification** still needs a later unstable format: executed function bodies become
  an offset-based read-only archive representation with every length, offset and enum payload
  bounds-checked first, validation walking the same bytes through a borrowed view, and `FuncBody`
  restored from that already-verified representation on first use. The "dual representation plus equal
  hashes" shortcut is not adopted, because a side-by-side summary cannot prove another postcard
  representation's executing bytes safe. Reopens at the format-freeze review.
- **External format freeze**: fix the format and state the migration rules.
- **Cross-version bytecode**: `build_id` must match exactly today. Reopens at the freeze, or at the
  first need to run a package built by another mirvm version.
- **Multi-target fat artifact**: the section-table tag is reserved; judge it after the freeze.
- **Cargoless**: pack uses cargoless's own dependency driver by default, while `MIRVM_DEPS=cargo`
  retains the Cargo fallback; the package format and run path do not change because of it.
