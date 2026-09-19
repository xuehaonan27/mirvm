# Mode B: `.mirvm` package format v4, pack/run, the MC machine-code section, and multi-Engine

> Status: **Implemented; repeatable instantiation and the real embedding loop closed 2026-08-13**. Parent case: [distribution-design.md](distribution-design.md) D9b (a package is the L2 engine-IR cache made portable: version header / checksums / relocation section). New inputs: git history (the machine-code section is the clean home for the third kind of content) and §7.23 (C4 slice ① data source — the dep global_asm manifest `.so` now enters the `required_native_libs` chain). **A package must be honestly self-contained**: apart from genuinely foreign FFI libraries (the glibc class), it reads no `$HOME/.mirvm` and no rustc/cargo traces at run time.
>
> **Format-stability statement (user ruling, 2026-07-23)**: this format is **not frozen now** — it may change with later mode B work (slice ③ machine-code section, D15, the C12 review). `fmt_ver` only separates generations; no cross-version compatibility is promised. External freezing is D4's separate review, which will fix it and state the migration rules.
>
> v2/v3/v4 corrections still in force: all self-produced library bytes are embedded in NATIVELIBS (including the `.so` materialized from a static archive) and materialize at run time by content hash — STAMPS/envs are provenance only; the loader uses a read-only mmap, MODULE holds only non-function metadata, and FUNCS holds a per-function index plus independent bodies that become resident lazily on access, with full E20 semantic validation done as one temporary per-function decode at first load; a fixed address survives only as a logical address in the artifact (`LinkAddr`), never as a run address. `Package::load` copies and validates an immutable byte snapshot once; each `unsafe Package::instantiate` independently maps frozen/TLS, materializes per-instance P1 closures, and patches bytecode, static pointers, entries, GOT and the native bridge through `LoadMap` and `FrozenReloc`. One `Package` can create several Engines concurrently; rewriting or deleting the source file after load does not affect a loaded object.

## 1. Contract

The loader is refuse-loud and **never silently rebuilds** — a package is a distribution artifact, not a cache. Every violation below is a rejection:

- **C1** `fmt_ver` or `build_id` mismatch must be rejected, naming the toolchain that must repack.
- **C2** A `whole_hash` or section hash mismatch must be rejected as corruption.
- **C3** A build target that does not match the current platform must be rejected. STAMPS/envs never take part in the run-time permission check (format is deserialized only).
- **C4** NATIVELIBS and MODULE paths must cross-validate in exact order; `role` must be legal; the embedded `bytes` must match their `fnv128`. MC must additionally cross-validate against the `role=global_asm` entries.
- **C5** The section count must fit the remaining section table; every offset/len conversion must be checked, and a section must never exceed bounds, overlap, or reuse a tag. Every section hash is checked, unknown tags included.
- **C6** FUNCS count, table length, per-function range and overlap must be checked. Per-function hashes are recomputed at load and at every on-demand decode; corrupt data must not reach execution.
- **C7** A v4 package that still declares `requires_fixed_base` must be rejected; every run instance must map dynamically.
- **C8** In the strict address table, every guest fn address outside the frozen range must have exactly one P1 entry recipe with the same FuncId. A missing recipe, a duplicate, or an overlap with a frozen address must be rejected during safe load.

## 2. Model

### 2.1 Goals and non-goals

Goals: a single-file package format v4; `mirvm pack` (cargo project/script → `.mirvm`); `mirvm run x.mirvm` (validate, load, execute); and three-dimensional acceptance — pack+run must be byte-identical to a direct `mirvm run` on three loads (eco / faer / wasmtime), while refusal probes (stale, missing library, base conflict, version mismatch) must be loud.

Non-goals (registered separately, not pre-spent): C12 cross-mirvm-version bytecode compatibility (build_id must still match exactly); multi-target fat artifacts (the section table has already reserved a multi-MODULE slot); in-process loading of a static-archive `.so` without a file (today it is materialized from the package and then dlopen'd); L3 JIT machine-code caching (the D5 forbidden surface, unrelated to packages).

### 2.2 Package container (v4)

```
offset 0   magic        8B   "MIRVMAR\0"
           fmt_ver      u32  = 4
           build_id     u32 length + UTF-8 (MIRVM_BUILD_ID, exact match)
           section_cnt  u32
sections   tag u32 | off u64 | len u64 | fnv1a-128 (section content hash)
tail       whole_hash   u128 fnv1a (whole file except this field)
```

Unknown tag → skip (forward compatibility); missing required tag → reject.

| tag | required | content |
|---|---|---|
| META | yes | postcard: `{args, envs, base_key, target_triple, whether BASE is included}` (= L2 Header metadata) |
| STAMPS | yes | postcard: `Vec<(path,size,mtime_ns)>`; a build-provenance record only, never a run-time permission input |
| BASE | reserved | The current writer produces none; the loader rejects the BASE/delta shape. A reserved tag must never be treated as supported. |
| MODULE | yes | postcard module metadata: exports, frozen snapshot, `link_fn_addrs`, `FrozenReloc`, TLS, the asm/GOT/P1 entry recipe, and so on; no run addresses and no function bodies |
| NATIVELIBS | yes | postcard: `Vec<{path, role, fnv128, bytes}>`; all self-produced `.so` bytes are inside the package, and the old path only serves cross-validation with MODULE and diagnostics |
| RELOC | yes | postcard: `{requires_fixed_base: bool, entry: Box<str>}` (fixed-base requirement plus entry symbol; argv is forwarded by run) |
| MC | optional | the complete ELF bytes of global_asm/dep_asm, parsed, relocated and symbol-registered in-process by mcload |
| FUNCS | yes | `count u32`; fixed index entries `offset u64 | len u64 | fnv1a-128`; then the functions' postcard `FuncBody`s stored contiguously |

### 2.3 pack flow (`mirvm pack <target> [-o out.mirvm]`)

Shared with `mirvm run` up to the Module; only the fork changes (execute → write to disk):

1. **Cargo project**: a full build through cargo_shim (deps as usual: metadata-only plus the C4 manifest), with a **forced full cold lowering** at lower time (deps-image and base layering bypassed, so the single module is self-contained; packing is a build action and can pay a seconds-long cold lowering). No BASE is written; MODULE holds the full module metadata and FUNCS stores every function body independently.
2. **Script / single file**: the single-file variant of 1 (frontmatter dependencies take the same cargo path).
3. First run full E20 validation on the in-memory Module. Then collect META/STAMPS/NATIVELIBS (expanded from `module.required_native_libs`, reading each self-produced library's bytes, computing `fnv128` on the spot, and classifying global_asm vs static_archive) and RELOC; global_asm enters MC by default as well, and every function is encoded into FUNCS. Publish atomically last, via a temporary name plus rename.

### 2.4 run flow (`mirvm run x.mirvm [-- args]`)

1. Magic sniff (first 8 bytes) → a non-package takes the existing path; read once and take a process-owned immutable byte snapshot.
2. Validate whole_hash and section hashes against that snapshot; later lazy decoding never reads the source inode again.
3. META: check build_id / fmt_ver / target. STAMPS/envs are only deserialized to check their format.
4. Restore non-function metadata from MODULE and parse the FUNCS fixed index. To preserve E20, decode every function temporarily and complete semantic validation before any MC/native materialization, then release the temporary objects; the execution-time FuncTable still binds the owned snapshot, and a function is decoded and made resident only on first access. Every instantiation recovers its dynamic frozen state independently, establishes all P1 `LinkAddr → closure` mappings first, and then applies frozen pointer relocations; each Engine gets its own `.so` image of a self-produced archive, and MC loads per instance too. GOT, the TLS template, Static, AddrImm and entry all resolve through the same LoadMap.
5. `RELOC.entry` starts (the main startup chain; argv is forwarded; `--vm-call` semantics do not travel with the package).

Function access records the real order of this run. On Module release the order is written atomically to `$MIRVM_HOME/package-heat/<hash>.order`, keyed by the FUNCS section content hash; the next load prefetches in that order in the background from one `mirvm-decode` worker. Demand tasks always precede predicted ones, and a predicted function that is suddenly demanded moves to the demand queue. The wait bound is the item currently decoding plus that demand itself: a wrong prediction costs speed only and never changes semantics.

### 2.5 MC section

- Content: one `{fnv128, bytes}` per global_asm/dep_asm ELF, cross-validated with the NATIVELIBS `role=1` entries; identical content is stored once.
- Load contract: mcload parses the ELF in-process, maps/relocates it, and registers symbols and eh_frame; genuinely foreign FFI libraries (the glibc class) are still resolved through the system ABI.
- Calls to guest fns from global_asm and from a C2 archive no longer bake a fixed P1 address. In plain terms P1 is "the guest function entry handed to native code": machine code jumps only to a hidden RIP-relative 8-byte slot, which instantiation fills with this Engine's closure. A self-produced archive pins its internal bindings with `-Bsymbolic` and is copied into a unique image per Engine; old images and closures are never reclaimed or reused, so stale pointers after close cannot land in a new Engine through address reuse.
- With MC absent or `MIRVM_PACK_NO_MC=1`, the same embedded bytes are materialized automatically to `$MIRVM_HOME/package-native/<hash>.so` and dlopen'd; that is only a loading strategy, not a dependency on a preexisting cache.

### 2.6 Embedding surface

The public embedding surface is `Package::load(path) -> Result<Package, String>` and `unsafe Package::instantiate() -> Result<Engine, String>`. `load` is safe container/bytecode validation plus an owned snapshot; the `unsafe` on `instantiate` means the caller must still trust the package's native libraries, foreign ABI declarations and host symbol contracts. One `Package` supports concurrent, repeated instantiation. A hand-built Module can only go through `unsafe Engine::from_module_unchecked`; untyped export calls live in `vm::engine::raw::run_export_raw`, also `unsafe`, returning two machine words. Ordinary callers never see `Shared`, so this is not a completed safe typed export API.

### 2.7 Engine lifecycle

Public states and transitions:

| state | entry | what it accepts / refuses |
|---|---|---|
| Running | `instantiate` returns | normal entries; leases may be acquired |
| Closing | `close`, atomically | refuses new normal entries; the original call chain and registered deferred callbacks may finish |
| Finalizing (internal) | all normal calls and deferred callbacks have exited | final cleanup; no guest execution |
| Closed | final cleanup done, observed by `wait_closed` | stale closures report `EngineClosed`; addresses stay reserved |

Rules:

1. Every public call, native callback and startup/destructor process first takes an execution lease — the count credential meaning "this call is still using the Engine".
2. `DeferredHold` covers both the window between a pthread having accepted a callback and its actual start/revocation, and the period in which pthread thread-specific-data destructors may still run. `close` synchronously clears the current thread's destructor values and waits for other surviving threads to exit or delete the key; it never releases `Shared` early while a callback can still happen.
3. Signals use a process-level disposition owner chain, a per-Engine inbox, and a stable per-pthread cell. Every guest handler installation gets a fixed 22-byte RX stub; the kernel frame performs only fixed TLS reads and an atomic registration and never enters guest. Process-directed events go to the owner inbox; `pthread_kill`/real libc `raise` `SI_TKILL` goes to the target pthread's cell established by the current handler installation. `close` first deactivates this Engine's registration (the registration object of one handler installation), removes the owner from the chain non-LIFO, restores the next surviving layer or the native baseline per the current kernel disposition, then waits for in-flight frames, the owner inbox and already-received target-thread cells; a target event can only be closed out by the target pthread at a safepoint or at exit. Pthread exit alternates between closing out managed TSD and its own cell along glibc's global-final-round raw key cursor, and only then blocks catchable signals, rechecks and closes the inbox. While the current thread still has its own target events, `wait_closed` returns `ActiveOnCurrentThread` instead of sleeping on itself. Queries and `oldact` always return the guest handler address and never leak the kernel stub.
4. Each host thread's `Ctx` is the execution context that thread uses when entering the Engine. The Engine registers `CtxSlot` by weak reference; once all leases have exited the finalizer clears each slot, so a host thread that never exits does not keep holding guest TLS, the 1 GiB virtual frame area or the whole `Shared` — only an empty slot remains.
5. Native images go through "relocate → fill P1/GOT/bridge slots → constructors" in that order. Only an instance whose constructors all completed runs its destructors once, in reverse order, at close. A controlled MIRVM exception in a constructor is classified as a `Result` failure and starts the shutdown; destructors are a non-unwinding teardown boundary. A destructor may legitimately call back into this Engine during Closing or create pthread work with an explicit completion event, after which one more wait round runs before Finalizing. But any MIRVM, foreign or host Rust exception that escapes a destructor must be pinned to the diagnostic `native finalizer unwound during Engine teardown` and then `abort`; it must not propagate and leave the Engine stuck in Closing.
6. `Engine::close` only initiates shutdown; `wait_closed` waits for the final cleanup. If the current host thread is still inside that Engine's call chain, the wait explicitly returns `ActiveOnCurrentThread`, letting the outer call exit and wait again rather than deadlocking.

Closing is not the immediate unmapping of every executable address. Native code may hold function pointers for a long time, and general FFI has no "revoke every copy" protocol, so these addresses are deliberately retained until process end: published plain/P1 libffi closures, JIT machine code and the `.eh_frame` the system unwinder uses, committed MC images, and self-produced dynamic library images. A stale closure after close keeps only a small Engine identity tombstone, not Module/Shared; C-unwind entries reliably report `EngineClosed` and plain C entries terminate per their non-unwinding ABI. A new Engine never reuses old P1 addresses, avoiding the ABA case where a stale pointer first goes invalid and then happens to point at a new object. An instantiation failure before constructors are allowed to run reclaims not-yet-published closures, MC images and dlopen handles; once construction has begun addresses may already have escaped, so even a failed construction only runs the close protocol and never reclaims published code.

### 2.8 CLI surface: both commands use the same `Package`/Engine loading path.

```
mirvm pack <proj-dir|script.rs> [-o <out.mirvm>]   # default <name>.mirvm
mirvm run <x.mirvm> [-- <guest args>]
```

## 3. Boundaries

**Not safe.** The `unsafe instantiate` admission: `Package::load` is safe (container/bytecode validation into an owned snapshot), but the native/FFI contract of a trusted package is the `unsafe Package::instantiate` caller's responsibility, and only a structured `run_main` exists — no safe host bindings are generated from exported Rust types.

- If an arbitrary third-party library keeps a callback indefinitely without ever giving a completion or revocation event, the engine cannot know when that address may be released. Today this is solved with process-lifetime closures plus a close tombstone, so an unknown horizon does not become a permanent `wait_closed` stall.
- The missing general revocation protocol for foreign-library callbacks makes the retained-address list a permanent process-level cost, not a leak to be fixed later.
- Before constructors run, an instantiation failure is still reclaimable; after that, escaped addresses are never reclaimed even if construction fails.

**Not frozen.**

- C12 cross-version bytecode: `build_id` must match exactly today. **External format freezing** awaits the D4 trigger (mode B has been raised; the freeze review comes after slice ③). `fmt_ver` only separates generations and promises no cross-version compatibility.
- The v4 package does not occupy the artifact's fixed base at run time; every Engine uses its own anonymous mapping.
- A multi-target fat artifact is still only a reserved section-table tag (the `MODULE@<triple>` shape), to be judged after D4.

**Not supported.**

- Signals currently support a traditional process-directed handler and an `SI_TKILL` thread-directed handler, with no guest advanced `sigaction` flags. A process event waits for the owner Engine's next normal safepoint; a thread event waits for the target pthread's next safepoint or exit closeout. `close` may wait but cannot close out on another thread's behalf. Fixed stubs, registrations and thread cells are retained until process end; when an old stub is reinstalled after its owner has closed, bare kernel delivery does `_exit(70)` and a `raise` through the MIRVM bridge reports `EngineFault(70)`. Synchronous faults, realtime, and `SA_SIGINFO/SA_ONSTACK/SA_NODEFER/SA_RESETHAND` are still rejected loudly, and a process-directed external event also makes no native-handler-level latency promise.
- proc-macro/build.rs really runs only during **pack** (the D9 §5 hard boundary, unchanged); no in-process loading of a static-archive `.so` without a file (the package materializes and dlopens it), and no L3 JIT machine-code cache (D5 forbidden surface).

## 4. Verification

- **Three-dimensional byte equality**: eco (large cargo project) / c_faer_lu (dep global_asm + pulp LD_ST) / c_wasmtime_wat (large dependency closure + fiber sym skip branch) — a direct `mirvm run` versus `pack + run` (cold / hot / package runs) must agree byte for byte.
- **Refusal probes**: fmt_ver, whole/section hash, duplicate tag, truncated section table, offset overflow, section overlap, FUNCS table truncation/out-of-bounds/overlap/per-body hash, and NATIVELIBS/MC cross-validation failures must all return errors; a P1 address with a missing recipe, a duplicate, or an overlap with frozen must also return an error and must not panic.
- **Self-containment acid test**: pack with MC disabled, move the original native/global-asm caches away, switch to a fresh `MIRVM_HOME` — the package must still materialize by content hash and run byte-identically.
- **Hot-order contract**: the first run produces exactly one non-empty `.order`; a second run reusing the same `MIRVM_HOME` produces unchanged output, proving the prediction channel does not change results.
- **Real embedding contract**: after load, rewrite and delete the source package; build A/B Engines concurrently from the same object; static/TLS isolation, distinct fn-ptr addresses, and global_asm and the C2 bridge each returning to their own Engine. After closing A, the old C-unwind pointer reliably returns A's `EngineClosed`, B keeps running, and creating C does not reuse A/B addresses.
- **Signal embedding contract**: queries/`oldact` keep the guest address; after A/B override the same signal, closing them in non-LIFO order still restores the correct previous layer and native action; a process signal enters only A's inbox while owner A is inactive and B is active; `pthread_kill` runs only at the target pthread's safepoint, a registration replacement does not cross generations, and traditional signals of the same generation coalesce per kernel semantics; a blocked `raise` can be consumed by `sigwaitinfo` with a real `SI_TKILL`. During close/JIT locks a frame is only registered and close waits for the target thread to close out itself, while an unblocked `raise` from guest or from a self-produced native archive completes its nested handler before returning. Thread-exit/close races and stale stub state 70 failures are locked in by real-kernel regressions, and **gate5 full set** + cargo test + diff dual state (SYNC) stay green.

### 4.1 Implementation locations

- `src/pack.rs` — container read/write, checked parsing, self-produced library embedding and content-addressed materialization;
- `src/vm/engine/mcload.rs`, `native_instance.rs`, `signal.rs` — in-process MC ELF loading; per-Engine native image isolation, P1 hidden-slot refill and the self-produced archive's three-symbol owner slot; process-level disposition owner chain, fixed registration stubs, per-Engine inbox, synchronous `raise` and close-time deactivate/wait/drain;
- `src/vm/engine/ctx/` / `deferred.rs` — close state, execution leases, pthread deferred callbacks and long-lived host thread `CtxSlot` cleanup;
- `src/cli/` — the pack subcommand and run's magic dispatch.

## 5. Open items

- **D3 archive direct verification**: still needs a later unstable format. The executed function bodies become an offset-based read-only archive representation, with every length, offset and enum payload bounds-checked first, E20 walking the same bytes through a borrowed view, and `FuncBody` restored from that already-verified representation on first use; the "dual representation + equal hashes" shortcut is not adopted, because a side-by-side summary cannot prove another postcard representation's executing bytes safe. Reopen trigger: the format-freeze review.
- **D4 external format freeze**: fix the format and state the migration rules; the review comes after slice ③. Reopen trigger: the D4 review.
- **C12 cross-version bytecode**: today `build_id` must match exactly. Reopen trigger: the D4 freeze, or the first need to run a package built by another mirvm version.
- **Multi-target fat artifact**: the section-table tag is reserved in the `MODULE@<triple>` shape; judge after D4.
- **D15 cargoless**: pack now uses cargoless's own dependency driver by default, while `MIRVM_DEPS=cargo` explicitly retains the Cargo fallback; the package format and the run path do not change because of it.
