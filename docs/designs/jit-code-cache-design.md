# L3: The Persistent JIT Machine-Code Cache

> Status: Decided RFC · Scope: how JIT-compiled guest machine code becomes a machine-local,
> persistent, relocatable artifact — the translator's relocation choke point, the artifact and its
> typed relocation vocabulary, the `(fragment id × jit-key)` keying, load-time linking and
> publication, adaptive optimization tiers, and collection. Parent:
> [distribution-design.md](distribution-design.md) (cache layering; its former "L3 forbidden" rule is
> replaced by this contract). Siblings: [dep-sharing-design.md](dep-sharing-design.md) (fragment ids
> are this cache's semantic keys), [frame-abi-bytecode.md](frame-abi-bytecode.md) (the backend,
> calling convention and unwind model whose *output* is persisted here),
> [vmctx-passing.md](vmctx-passing.md) (the regime dimension of the key),
> [modeb-mirvmar-design.md](modeb-mirvmar-design.md) (unchanged: packages distribute bytecode, never
> JIT output).

## 1. Contract

- **The artifact is a pure function of `(canonical fragment bytes, jit-key)`.** One cache entry per
  compiled function: machine code, a typed relocation table, and unwind material. Everything
  program-, process- or placement-specific is a relocation, never a baked value, so the entry is
  valid in any program that binds the same fragment. Cranelift codegen is deterministic; a
  double-build byte gate enforces it.
- **The choke point rule.** The translator may not emit a raw absolute value. Every address-bearing
  constant goes through one recording API that emits the value *and* its relocation entry, so the
  relocation table is complete by construction. This rule exists because a missed relocation is a
  silent wrong value — the one failure class the store discipline never tolerates — and auditing
  emission sites cannot prove absence.
- **Key and generation.** Entry key = fragment id (from
  [dep-sharing-design.md](dep-sharing-design.md) §3.3) × jit-key. The jit-key names every codegen
  input that is not the fragment: the ISA feature set actually enabled (host CPU detection included),
  the vmctx regime (T or R — one namespace per regime, which is what keeps the
  [vmctx-passing.md](vmctx-passing.md) §1.7 flip free), and the codegen options, of which the
  optimization level is one dimension (§2.6). `build_id` is the generation, as in every store family:
  helper ABI, translator behaviour and TLS offsets are build-stable, so no finer versioning is
  needed and no cross-build reuse is promised.
- **Load is linking, through the existing publication protocol.** The loader maps code into a
  self-managed arena, patches relocations, flips W→X, synthesizes and batch-registers `.eh_frame` at
  the final addresses, then publishes entry addresses with the same Release store the compile worker
  uses today (fast first, then packed). Published code addresses live to process end; the tombstone
  discipline is unchanged.
- **Any doubt is a miss.** A failed hash check, an unresolvable relocation, an unknown relocation
  kind or a jit-key mismatch falls back to compiling — self-healing, never a trap and never a
  degraded run.
- **Not cached:** trace-domain bodies (they assume a pinned recorder register and a live session),
  libffi closures, c2i trampolines and entry stubs (per-process by nature, microseconds to rebuild).
- **Machine-local only.** The cache never enters a `.mirvm`; mode B keeps distributing bytecode.
- **Two-state cross-check.** `MIRVM_NO_JIT_CACHE=1` bypasses the cache entirely, mirroring the
  deps-image discipline, so cached and freshly compiled states stay differentially comparable.

## 2. Model

### 2.1 What is already cache-shaped

Four existing properties make the artifact model natural rather than invasive:

- **Calls are already a PLT.** Compiled-to-compiled calls go through `slots_fast[callee]` memory
  indirection with an atomic Release/Acquire publication protocol (`src/vm/jit/state`), so machine
  code never bakes a callee's code address, and a loaded entry plugs into dispatch exactly like a
  freshly compiled one.
- **The input is the frozen bytecode.** The translator consumes `ir::FuncBody` with no `tcx`, under
  a bit-identical-to-interpreter contract, so the semantic key of a compile really is the body — and
  the canonical fragment is that body with context factored out.
- **Unwind info is synthesized, not scraped.** `create_unwind_info` → gimli `FrameTable` → one
  `.eh_frame` section registered per batch (`compiler/symbols.rs`). The loader reuses the same path
  at the load address; nothing about CFI is inherently placement-bound in the pipeline.
- **In-process loading has a precedent.** mcload already parses, maps, relocates and
  `.eh_frame`-registers ELF objects inside the process for the MC section; per-blob content hashes
  (`FuncBlob.expected_hash`) and the heat-order prefetch worker exist in the package path.

### 2.2 The relocation debt (why L3 was forbidden)

What today's translator bakes as immediates, and what each becomes:

| baked today | where | becomes |
|---|---|---|
| `*const ir::Stmt / Rvalue / Builtin / ForeignSig`, `sym.as_ptr()`, trap-reason pointers — pointers into this process's decoded body | `translate/{stmt,rvalue,term}.rs` | `BodyRef` relocation (§2.3) |
| `resolve_link_addr(..)` results — frozen-region runtime addresses | `translate/place.rs` | `FrozenAddr` relocation bound through the LoadMap |
| callee/self `FuncId` immediates passed to helpers | `translate/term.rs` | `FuncIdImm` relocation bound per program |
| the slot-table base behind call indirection | call sequences | `SlotBase` relocation |
| `mirvm_*` helper addresses resolved by the `JITModule` at finalize | `compiler/imports.rs` | `Helper(ordinal)` relocation against the closed whitelist |
| `.eh_frame` pc ranges and CIE personality, registered at publish addresses | `compiler/symbols.rs` | synthesized at load from stored unwind material |

### 2.3 The choke point and the relocation vocabulary

One translator-side API replaces every raw `iconst`-of-address; each call records a typed site:

```text
Helper(ordinal)        -> the imports.rs whitelist, resolved in-process at load
SlotBase(domain)       -> this Engine's slots/slots_fast base
FuncIdImm(binding #i)  -> the fragment binding table -> this program's absolute FuncId
FrozenAddr(binding #i) -> the fragment binding table -> LoadMap-translated real address
BodyRef(block, item)   -> the address of that statement/terminator interior (sigs, symbol and
                          reason strings included) inside this process's resident decoded body
```

`BodyRef` implies the decoded, bound `FuncBody` must be resident before its compiled entry is
published; that is already the factual order (slow-path helpers and c2i need the body), and the
loader enforces it by demanding the body through the lazy table first. The choke point has value
independent of persistence: it is the same enumeration a T→R regime flip, arena compaction or any
future code motion needs.

The recorded site carries the *identity*, not the address, and the offset comes from the backend
rather than from a parallel bookkeeping pass: each site is emitted as a named `global_value`, so
cranelift records a relocation for it exactly where the immediate lands, the symbol's value is
supplied through the module's lookup hook at compile time, and the compiler reads the finalized
code's relocation list back into the entry's table. That is also why no site may `iconst` an
address: a constant inside an instruction is invisible to the relocation table, and there is no
second source of truth to keep in step with the encoding. Calls to the helper whitelist already
work this way — they are symbol references today — so the choke point is what brings the baked
absolutes to the same footing.

### 2.4 Artifact and store form

Entries are stored in the same pack mechanism as fragments (append-only packs plus index, atomic
publish), in a `cache/jit/` family. One entry: header (jit-key material for exact comparison),
machine-code bytes, relocation table, unwind material — the Cranelift `UnwindInfo` CFA program plus
the LSDA body with its code-relative landing-pad map; CIE and FDE are synthesized at load, so no
absolute survives in the stored form. Entry integrity is BLAKE3-verified on read, like every blob.

### 2.5 Load-time linking and publication

Compile-or-load sits where compilation sits today: the tier-up request path. On a hit the worker maps
the entry into the self-managed code arena (mcload/StubArena precedent — the `JITModule` is not
involved, and cranelift-jit's no-per-function-release memory model stays irrelevant to loaded code),
patches every relocation, protects RX, synthesizes and registers the batch `.eh_frame`, then
publishes fast/packed entries in the existing order. A batch is the natural unit: heat-order
prefetch (the package-heat precedent) can pre-link the learned hot set in the background at startup,
turning warmup into one link wave instead of a compile wave.

### 2.6 Adaptive optimization tiers

The optimization level is a jit-key dimension, not a global constant, and the policy is
deliberately adaptive rather than fixed (the tiering boundary of
[frame-abi-bytecode.md](frame-abi-bytecode.md) §3 stays open): the heat ledger — the existing
call-counter and jit-stats machinery plus the persisted heat order — decides per function which tier
to request, and the cache makes a higher tier a one-time machine-wide cost instead of a per-run
cost. The cache may hold several tiers of one fragment; dispatch publishes the best available.
Thresholds and hysteresis are tuned from the D16 measurement ledger, not designed up front.

### 2.7 Collection

Mark-sweep piggybacks on the fragment pass: a jit entry is live iff its fragment is live and its
jit-key matches the current build's regime/ISA namespace. An optional size budget evicts coldest
entries by the heat ledger. `cache purge --jit` takes the family whole.

## 3. Boundaries

- No cross-`build_id` reuse; no cross-machine or package distribution of machine code.
- Linux/x86_64 first, per P6: the write→execute flip and code-arena mapping go through `src/os`;
  the macOS `MAP_JIT` discipline is that pair's item, never a core-path special case.
- Trace domain, closures, trampolines and stubs are never cached (§1).
- No OSR, no deoptimization, no speculative entries: the cache persists exactly what the compile
  path can already produce; tiering policy stays outside this contract.
- The interpreter remains the semantic reference; a cache hit changes where machine code comes
  from, never what may run without verification.

## 4. Verification

- **Determinism gate**: compiling one fragment twice (fresh sessions, threads 1 and 8) yields
  byte-identical artifacts — the base-image determinism gate, applied to codegen. The `jit-cache`
  case's last step is it: one workload compiled cold in two homes, one with `MIRVM_THREADS=1` and one
  with `MIRVM_THREADS=8`, must leave stores that hold the same keys with the same bytes (measured on
  a 634-entry probe: none differing).
- **Cold/warm differential**: one workload with `MIRVM_JIT_THRESHOLD=1` run compile-cold and
  cache-warm must produce byte-identical guest output through the existing three-way gate; this is
  the "stale semantics from a hit" catcher.
- **Two-state cross-check**: `MIRVM_NO_JIT_CACHE=1` against the default, same output bytes.
- **Unwind proof at loaded addresses**: the `lsda_probe` pipeline extended to a relocated entry — a
  panic must cross a *loaded* frame, run cleanup and be caught, on both CIEs.
- **Linker honesty**: a corrupted entry, an unknown relocation kind and a missing fragment each
  produce a miss plus recompile, observably (stats counter), never an abort or a silent value.

## 5. Construction order

1. **Choke point refactor** — route every translator absolute through the recording API; delete raw
   pointer `iconst`s. Independent value: T→R flip and code motion become mechanical. Gate: existing
   JIT differentials unchanged.
2. **In-process reload proof** — serialize a compiled function, drop it, re-link it into the same
   process, run the differential. This proves the relocation vocabulary complete with no
   cross-process variables yet. Implemented: `vm::jit::artifact` captures each defined symbol's code
   with its relocation list in canonical form — a reference of the fragment by the ordinal the
   canonical walk gives it (`Func`/`Tls`/`Asm`/`Link`), an interior of the resident body, the entry's
   own function id, a name of the import whitelist or of another symbol of the entry, or an offset
   inside the symbol — encodes that through one version byte and postcard, maps it into a fresh
   executable region, resolves every relocation against live state and republishes the linked entries.
   The entry holds no program's numbering or addresses, so one body's artifact is byte-identical in two
   programs that number its functions and place its frozen data differently; a unit test compiles one
   body twice that way and compares the bytes, which is the precondition the store keys on.
   `MIRVM_JIT_RELOAD` runs a whole session that way, so every existing JIT differential also runs
   against linked code, and `fib32-reload` is its standing gate. What a reload publishes is the linked
   region, so it registers the CFA programs the artifact carries *at the addresses the link placed them
   at* (`Compiler::linked_frames`), the same way the store path does: without that, an unwind out of a
   reloaded frame found no FDE and ended as `_URC_END_OF_STACK`, which a panic raised inside a reloaded
   body reproduces immediately. The kinds applied are the x86_64 ones:
   the call encoding, veneers and write/execute discipline of the macOS pair are that pair's item
   (§6), and a kind this engine does not apply is a miss, never a guess.
3. **The store family** — packs, keys, generation, purge; cross-process warm runs; the §4 gates.
   Implemented: `store::jit` holds one entry per `(fragment, jit-key)` in packs whose index is keyed by
   that pair and whose records carry their own BLAKE3, so a lookup is a binary search and every read is
   verified; the family is in the register, so `mirvm cache status` accounts for it and
   `cache purge --jit` takes it whole, with liveness scored against the fragments the manifests name.
   The jit-key is the build id, the triple, the codegen options and the ISA-dependent options — host
   CPU detection included — plus the code domain, and the entry stores the material itself, so a read
   compares it field by field before anything is linked. The compile worker looks the store up before
   it compiles, links a hit, registers the stored CFA programs as FDEs at the loaded addresses and
   publishes the same two entries a compile would; a miss compiles, stages the entry and publishes the
   batch as one pack when it is worth a file, and `MIRVM_NO_JIT_CACHE=1` bypasses the whole family.
   `fib32-jit-cache` is the standing case: cold stores, warm reuses at least one entry, the bypassed
   run compiles, a corrupted pack is refused rather than used, and all four agree on stdout. The
   counters behind that last leg (`cache_hits`/`cache_misses`/`cache_refused`, dumped with the helper
   buckets) are the linker-honesty observable of §4, and the loaded-address unwind proof is the
   `a_linked_entry_unwinds_through_a_loaded_frame` probe: the same cleanup-pad chain the LSDA probe
   already runs, but through frames the *link* placed and FDEs synthesized from the stored CFA
   programs.
   Every absolute an entry carries is a site of the fragment, the `ForeignSig` of an *indirect* native
   call included: it was the one value still baked raw (the compiling process's heap address), so an
   entry written by one process segfaulted the next one that called through it. Two fences now hold
   the property: `an_indirect_native_signature_is_recorded_and_replayed` asserts the captured code
   carries no such address and that the replay recovers this process's signature from the body, and the
   case's last step is the §4 determinism gate: one workload compiled cold in two homes, one with one
   frontend thread and one with eight, whose stores must hold the same keys with the same bytes.

4. **Startup pre-linking by heat order**, then **adaptive tiers** on the D16 measurement ledger.
   Pre-linking is implemented: one run records the order the compile worker was asked for functions in
   (`cache/package-heat`, filed under the same key as the IR entry whose ids it names), and the next run
   hands that order to the worker, which links what the store holds for it *before* it serves a request —
   so the first call into a hot function finds a published entry instead of enqueueing a compile. The
   wave goes through the ordinary load path, so a stale or unreadable entry is a miss exactly as it is
   there, and `fib32-jit-cache` requires the warm run to prelink and count it.
   Tiers are built as the mechanism §2.6 asks for: one Engine runs one compiler per tier (each with its
   own ISA, module and jit-key, both publishing into the same slots), the pre-link wave prefers a stored
   optimized entry for a hot function and falls back to the baseline one, and the *request* a function
   raises while it is interpreted asks for the tier the heat ledger gives it (`JitState::tier_for`).
   `fib32-jit-cache` asserts the split: a cold run is all baseline, a warm one links from the store and
   compiles the functions the heat order names at the optimized tier.
   The policy has no within-run upgrade, and measuring says why: a request is raised once while a
   function is interpreted, and publishing the baseline entry — pre-linked or compiled — means its later
   calls go through the PLT slot, where no counter observes them. So the ledger's unit of evidence is one
   run, and the second threshold this file first carried was unreachable rather than merely conservative.
   A within-run signal needs the compiled-call path (the PLT slot load, or a cheap sampled counter) and
   is open in §6.

Step 1 needs canonical fragment ids only as *names* (the encoding pass of
[dep-sharing-design.md](dep-sharing-design.md) §6 step 2), not the fragment store; the two designs
share that keystone and are otherwise parallel.

## 6. Open items

- The compile-time/code-size ledger (D16) is in place and has priced both questions:
  `MIRVM_JIT_LEDGER=1` prints one row per function this process built or linked — the kind, the
  canonical body's bytes, the machine code's bytes and the microseconds the call took — and a
  per-kind summary at exit. Measured on two generated programs, one of 200 small bodies (13 B to
  3 388 B) and one of graded large ones (678 B to 27 720 B), both with `MIRVM_JIT_THRESHOLD=1` so a
  first call builds each function: linking is cheaper than compiling at **every** measured size, by
  9.6× at the median (32.5 µs against 312 µs) and by 2.5× to 31× per size band. The small-function
  floor this item hypothesized does not exist — the cheapest link measured (14 µs for a 27 B body)
  beats the cheapest compile measured (82 µs for a 23 B body) — so the pre-link wave links the whole
  order and no size gate is set. That also fixes the scope: a warm start pays ~62 µs per hot
  function (184 links = 11.5 ms, against 89 ms to compile the same bodies), which is a price worth
  paying for every function in the ledger.
- A within-run tier upgrade needs a signal the compiled-call path can carry: the interpreter stops
  counting a function the moment its first entry is published, so today a function that becomes hot
  during a run keeps the tier the *previous* run's ledger gave it. The candidates are the PLT slot load
  (a counter beside the slot), a cheap sampled counter in the fast path, and the call-counter machinery
  the interpreter already has. Whichever lands needs its cost measured against the D16 ledger before the
  tier thresholds mean anything.
- Whether `.eh_frame` batches should merge across load waves or stay one section per wave. Measuring
  says the question is not only about bytes: the compile worker registers each batch with
  `__register_frame` while guest threads may already be unwinding, and libgcc mutates that list under a
  lock it does not take on the lookup path. A panic that races a registration can therefore end as
  `_URC_END_OF_STACK` instead of a caught exception. Registration is serialized with the other
  registrations (one process lock) and always happens before the entries are published, which is what
  the design asks for; closing the rest means either merging every batch into one section per Engine —
  re-registered under a lock the unwinder also observes, which libgcc offers only through its private
  `struct object` — or registering from a point the guest cannot be unwinding at. The repository gate
  runs its tests on one thread for the same reason, and the item closes when the JIT can register
  while guest threads unwind without that window.
- The adaptive-tier statistic is chosen: the persisted **heat order**, not a counter. A function the
  previous run's order names is asked for at the optimized tier the first time this session compiles
  it, and everything else starts at the baseline one, so the decision is made once per function per
  session from evidence the store already keeps (`cache/package-heat`). No hysteresis is needed while
  the decision is a single membership test per session; it becomes necessary only with a within-run
  signal, which is the item above.
- macOS pair items: `MAP_JIT`, `pthread_jit_write_protect_np`, the code-arena placement rules, and the
  `Arm64Call` encoding — a `bl` patches one instruction field, in range or through a veneer, and which
  of the two is decided when the entry lands, so the pair owns both the decision and the space a
  veneer needs.
