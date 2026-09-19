# vmctx passing: how compiled code (and the callback boundary) reaches VM execution state

> Status: Decided RFC · Scope: the mechanism by which compiled frames and native-to-guest entry points (escaped function pointers, callbacks, signals) reach the per-thread VM execution state; options T/R, the rejected alternatives, and the final verdict.

## 1. Contract

1. **Boundary = lookup by current thread (TLS) plus lazy attach.** Every entry that native code or the kernel can invoke on an arbitrary thread — escaped guest function pointers, native-to-guest callbacks, signals — must look up the *current* thread's execution state in its prologue (TLS read) and lazily attach when the read is null. This is forced, not preferred (§2.2, §2.3). Attach logic belongs to `os::thread` (P7).
2. **Compiled-to-compiled calls never touch TLS.** They use the internal fast convention on `f_fast`; the plain-C-signature boundary entry `f_boundary` is a separate entry per compiled guest function (§2.5).
3. **Final verdict: T** (T3/M5.5 verdict, 2026-07-21; extended 2026-08-10). T is thread-local "current Engine" identity. Each Engine holds its own `Arc<Shared>`; the host thread's execution-state table stores `Ctx` keyed by Engine id; a boundary entry activates the matching entry and a nested return restores the outer. The old process-level `SHARED` and the JIT single worker were split into per-Engine state.
4. **The T/R choice was not a performance judgment.** R (reserved register) makes an already-selected ctx read faster, but it cannot say which Engine the thread has currently entered; multi-instance identity must first be established by T. R therefore remains a performance candidate for the point where the E6 alloc/TLS fast path enters compiled code, and cannot replace T.
5. `fast` signature = pure guest signature (CalleeAbi, all forms, T1-a). The single ctx seam is `get_ctx()` = SHARED static + boundary TLS attach. No register rent is prepaid for any hypothetical load.
6. **Compiled code today touches per-thread execution state at zero sites**, with the three admission tables (stmt/rvalue/terminator) exhaustive: cell ① (GC/safepoint/TLAB bump/stack check/linear memory) does not exist structurally by architectural commitment; cell ② (FFI/c2i/catch/panic bookkeeping) is semantically helper-shaped, so ctx acquisition (SHARED + TLS attach, the boundary mechanism forced under M4.4, unchanged) amortizes inside the helper's heavyweight cost; cell ③ (alloc/TLS inlining) has not entered.
7. **T-to-R is an ABI-compatible single switch** (shape verified by Spike 5): (a) `enable_pinned_reg` ISA flag; (b) lower the `get_ctx()` TLS load to `get_pinned_reg`; (c) boundary entry save/set/restore. Flip granularity = the whole JIT code cache recompiled under the new regime. That is free: the code cache is in-process volatile, mode B distributes bytecode not machine code, and there is no cross-process compatibility surface. Honesty clause: layering is **never** same-process per-function mixing — in T code r15 is an ordinary callee-saved temporary, R code assumes r15 = ctx, so cross-regime calls need wrapping.
8. **Signal supersession (2026-08-13).** Signal no longer goes through this document's TLS attach/thunk boundary. The kernel frame only atomically registers; process-directed events go to the owner Engine and `SI_TKILL` thread-directed events go to the target pthread; both establish a new activation at an ordinary safepoint. Passages below that list signal alongside ordinary callbacks are the 2026-07-07 argument, not the current signal implementation.

## 2. Model

### 2.1 Frames, state, pressure

Three frame kinds share one native stack (Model A, verified by Spike 2) with one execution state per thread:

```
interp frame --i2c--> compiled frame --FFI--> native frame
(host code,           (JIT output)            (libc / C)
 explicit *mut Ctx)        |
                           v   how does it reach ctx? = this document
Ctx (vmctx, one per guest thread; points at the shared read-only program)
  TLAB alloc pointer | operand area | dispatch/kinds | panic state
```

- interp frame: host code, explicit `*mut Ctx` parameter (settled by Spike 2) — no problem.
- native frame: by definition does not know ctx exists — the source of the escape/callback problem.
- compiled frame: needs a mechanism to obtain ctx.

**mirvm is under far less pressure than Wasm.** mirvm uses real addresses (C2): guest memory access compiles to bare load/store, statics/vtables are frozen real-address constants, compiled-to-compiled is a direct native call — none of these go through ctx. Wasmtime reaches vmctx on every linear-memory access to obtain the memory base; mirvm does not. Compiled code genuinely touching ctx:

| scenario | frequency |
|---|---|
| TLAB allocation (bump pointer lives in the per-thread state) | hot (every Box/Vec growth) |
| c2i: calling a callee that is still interpreted | cold to common during warmup; disappears once JIT-linked |
| panic/unwind bookkeeping, guest stack bound check | low / entry level |
| checked mode `GuestMemory::contains` (C13) | checked mode only |

Corollary: most pure computation functions need no ctx at all. Threading vmctx through every function (the Wasm default) taxes everyone for the few users; this weakens the explicit-parameter option from the start.

### 2.2 Decisive constraint: FFI correctness, not performance

Mechanism performance differences are second-order (a few instructions). Two first-order criteria hold:

1. **Escaped guest function pointers must be plain-C callable.** `into_pthread_t`, qsort comparators, signal handlers, C library callbacks: mirvm's FFI design (DESIGN §7, C2) requires an escaped guest function pointer to be callable by native with an ordinary C signature, zero marshalling, zero wrapping. Wasmtime can afford an explicit vmctx first parameter because Wasm has no bare function pointer escaping to native under the C ABI (everything goes through the Wasmtime API). mirvm is the opposite; this is the root reason it diverges.
2. **ctx is per-thread** (concurrency-arch.md). Under M4, `Ctx` = one execution state per guest thread (operand area and TLAB are thread-private), pointing at the shared read-only program. A scheme that sews ctx into a thunk captures the *creating* thread's ctx; a callback on another thread then uses the wrong thread's operand area/TLAB = data race/corruption. Principle error, not slowness.

### 2.3 Signal handlers pin the boundary down

A signal handler runs on whatever thread receives the signal (POSIX). When a guest-registered handler is delivered:

- capture thunk (ctx sewn in) -> most likely the wrong thread -> wrong;
- reserved register -> on interrupt the register holds an arbitrary value of the interrupted code -> wrong;
- the only correct approach: look up the execution state by current thread at entry = TLS read (lazy attach if empty).

So the boundary mechanism has no choice. Any entry native/kernel can invoke on an arbitrary thread (escaped pointer, callback, signal) must do a TLS lookup in its prologue. The only remaining degree of freedom is how compiled code passes ctx internally.

### 2.4 Candidate mechanisms

**(a) P — explicit vmctx first parameter** (Wasmtime / JNI-JNIEnv / lua_State style). Compiled signature = guest signature + one hidden first parameter. Pros: ctx is among the fastest to obtain internally (already in the first-parameter register), no global state; Spike 2 used it to verify the adapter model. Fatal flaw for mirvm: the escaped-pointer signature mismatches.

```
native expects:  cmp(a: *T, b: *T) -> i32      <- plain C
compiled has:    cmp(ctx, a, b)     -> i32      <- one hidden first parameter
remedy = capture thunk (libffi closure):
  qsort --call thunk(a,b)--> rdi <- captured ctx --jmp--> cmp(ctx,a,b)
```

- **Every** escaped pointer then needs a thunk (qsort/pthread/signal all wrapped), which breaks "real pointer passed directly".
- The thunk captures the creating thread's ctx, so cross-thread callback/signal is a principle error (§2.2, §2.3) unless the thunk also does a TLS lookup — and then (a) degrades to "boundary relies on TLS" and the explicit parameter has internal meaning only.

Precedents: Wasmtime (vmctx), LuaJIT/Lua C API (`lua_State*`), JNI C API (`JNIEnv*` is an explicit ctx parameter, but JNI is a dedicated API boundary, not a plain-C escape).

**(b) T — thread-local (TLS).**

```
guest thread start (or foreign-thread attach) sets once: TLS[CUR_CTX] = &this thread's Ctx
compiled code needing ctx: ctx = TLS[CUR_CTX]     <- one fs-segment addressed load
qsort --call cmp(a,b)--> compiled_cmp: ctx = TLS[CUR_CTX]   (same thread => current thread's state)
signal (arbitrary thread) -> handler: ctx = TLS[CUR_CTX]    (automatically the receiving thread's ctx)
```

Escape and callback need zero handling; the signature is already plain C and the lookup naturally selects the right thread. The foreign-thread problem is solved by lazy attach (JNI `AttachCurrentThread` style):

```
a C library's own worker thread (never seen by mirvm) calls back into guest:
  entry: ctx = TLS[CUR_CTX] = NULL  ->  lazy attach
  create this thread's execution state (operand area/TLAB) -> set TLS -> continue
```

JVM (`AttachCurrentThread`) and Go (cgo callback's `needm`) have the exact same mechanism. No scheme escapes attach — (a)'s thunk on a foreign thread also has to create execution state; TLS merely turns "whether to attach" into a natural null check. The 1:1 real-thread model makes attach cheap: any OS thread that attaches an execution state can run guest.

JIT codegen note (the truth about cost). The cost of a TLS read depends entirely on the TLS model:

- ctx lives in the mirvm host itself (a `#[thread_local]` static in the main program or its library) -> initial/local-exec -> one `mov rax, fs:[tpoff]`, about an ordinary L1 load. `tpoff` is constant after process load and the host hands that constant to codegen at JIT time; it does not go through Cranelift `tls_value`'s general-dynamic path (the expensive `__tls_get_addr` call).
- Fallback: compiled code does `call mirvm_ctx()` (a 3-instruction host helper) — one extra call per use site, irrelevant at "once at entry" frequency.
- Fine print: if the mirvm engine is later embedded as a dlopen plugin (M7+ embedding API scenario), the host's own TLS falls into the dynamic model; revisit then.
- Frequency can be reduced to at most once per activation: ctx is a loop invariant inside a function body (a frame activation lives and dies on one thread), so the JIT loads once at entry and carries it in a register for the whole body. That degrades (b) to (a) inside the function; see §2.5.

**(c) R — reserved register** (HotSpot r15 / Go g style).

```
JIT code pins r15 as ctx for the whole body (regalloc may not allocate it):
  compiled_f: ctx = r15, zero instructions to read   <- fastest
  call native (SysV: r15 is callee-saved)
  native frame: push r15 ... freely reuse r15 as a temp ... pop r15; ret   => compiled->native->return is fine
  but native calls back into guest midway:
  compiled_cb reads r15 = native's temp value = garbage
```

Why HotSpot survives: the JNI boundary is not plain C. Java-to-native passes `JNIEnv*` explicitly, and a native callback into Java must go through the JNIEnv function table; the callback path recovers the thread pointer from `JNIEnv*` and rebuilds r15, so it never relies on the register surviving. That is, HotSpot = internal (c) + boundary (a) (a `JNIEnv*` smuggling channel). mirvm's FFI target is plain C with no smuggling channel (§2.2), so (c)'s boundary hole cannot self-heal for us and must be paired with TLS boundary recovery.

As an internal mechanism (c) is still feasible: Cranelift supports pinned registers (`enable_pinned_reg`, on x86-64 that is r15) and SpiderMonkey uses it. Cost: one callee-saved register commandeered for the whole body plus arch-specific mental burden.

Precedents: HotSpot (r15 = JavaThread*), Go (g register + cgo `needm`), Erlang BEAM (process pointer register). Common trait: all pair it with a boundary reconstruction mechanism; none bare-relies on the register across FFI.

### 2.5 Hybrid verdict: TLS boundary + one fast internal convention

§2.3 pins the boundary and §2.1 shows internal use sites are sparse. Together they give HotSpot's multi-entry idea (isomorphic to verified-entry / c2i-adapter): every compiled guest function has two entries.

```
escaped to native ->  f_boundary: C-ABI boundary entry, plain C signature
                                   ctx = TLS[CUR_CTX] (lazy attach if null, §2.4)
                                   jmp f_fast(ctx, args...)        <- tail call
internal call     ->  f_fast(ctx, args...): fast entry, internal convention
                                   body; compiled->compiled calls another f_fast directly,
                                   ctx continues in registers

compiled->compiled : f_fast all the way, zero TLS
native->guest      : one TLS read (+ first-time attach)
signal handler     : same boundary entry, naturally the right thread
```

Origin, option matrix, and precedents:

| dimension | (a) explicit param | (b) TLS | (c) pinned register | hybrid (TLS boundary + fast internal) |
|---|---|---|---|---|
| obtaining ctx inside compiled code | register | one fs-load (may load once at entry and carry) | zero instructions | internal = register |
| escaped fn ptr signature | ✗ mismatch -> thunks everywhere | ✓ plain C | ✓ plain C | ✓ (`f_boundary`) |
| native->guest callback | ✗ must thunk | ✓ direct | ✗ r15 is garbage | ✓ |
| signal (arbitrary thread) | ✗ capture scheme wrong in principle | ✓ naturally right | ✗ | ✓ |
| foreign thread | must attach too | TLS null check -> lazy attach | must attach too | same as (b) |
| per-thread ctx (§2.2) | thunk captures wrong thread | naturally per-thread | — | naturally |
| precedents | Wasmtime / lua_State / JNIEnv | V8 `Isolate::GetCurrent` / CoreCLR | HotSpot r15 / Go g / BEAM | HotSpot multi-entry (verified/adapter entry) |

No production VM bare-relies on a single mechanism across the FFI boundary — HotSpot = register + `JNIEnv*` smuggling, Go = register + `needm`, V8 = register cache + TLS. Because mirvm's plain-C FFI has no smuggling channel, the boundary leaves only TLS, which converges the design space.

Thunk scope narrowing (a dividend):

| what escapes | (a) pure explicit param | hybrid (boundary TLS) |
|---|---|---|
| **compiled-state** guest fn | needs a capture thunk (and is wrong cross-thread) | **zero thunk** — `f_boundary` is itself plain-C native code |
| **interpreted-state** guest fn | needs a thunk | still needs a thunk (it has no machine address; that is the thunk's actual job), and the thunk prologue shares the same TLS/attach logic as `f_boundary` |

So the thunk narrows from "every escaped pointer needs one" back to its actual job: giving interpreted functions a machine address. The then-current corpus §2.3 signal-thunk (overturned by the 2026-08-13 signal supersession) and the pthread start_routine thunk were once grouped on this path; after warmup (the function is JITed) the escaped pointer can be given the `f_boundary` address directly and the thunk disappears, matching frame-abi-bytecode.md §8's existing "JIT tier thunk disappears" judgment.

### 2.6 Last internal degree of freedom (historical comparison; superseded by the M5 D5 layered conclusion)

M5 D5 (2026-07-11) rewrote the choice as a T skeleton plus an R-compatible cache layer: T and R share the pure guest fast signature and the TLS boundary; only `get_ctx()` lowering, the pinned-reg switch, and the boundary save/set/restore differ. Whether to enable R is retested against the load only once allocation or guest TLS inlining enters compiled code.

| | internal = explicit vmctx param (Wasmtime style, P) | internal = pinned r15 (HotSpot style, R) |
|---|---|---|
| obtain ctx | first-parameter register | zero instructions |
| call site | one extra parameter loaded per call | none |
| register pressure | occupies a **parameter** register (regalloc may reuse) | commandeers a callee-saved register for the whole body |
| leaf function needing no ctx | still taxed by threading | zero tax |
| Cranelift support | trivial (it is a parameter) | `enable_pinned_reg` |
| arch dependence | none | high (pick a register per arch) |

Both are correct (the boundary is already covered by TLS); the difference is pure performance/engineering and was to be decided with data once Spike/M4 had a real Cranelift pipeline.

**Spike 5 initial data (2026-07-07).** Both variants were implemented on real Cranelift and fully correct (`enable_pinned_reg`/`get/set_pinned_reg` work out of the box; R's `f_boundary` follows the §2.5 shape: save -> set -> call fast -> restore, host callee-saved semantics preserved, reentry idempotent). fib(30) direct-call microbenchmark: **R (pinned r15) 5.47ms vs P (explicit param) 5.90ms — R ~8% faster**; vcode confirms P's threading tax (per-frame ctx into a callee-saved register plus reload at every call site) and R's zero ctx movement on internal direct calls. Initial judgment: R loses nowhere to P and the multi-entry structure is empirically verified. The final verdict was still left to M4 real load (unrepresentative microbenchmark, do not over-read).

## 3. Boundaries

What each rejected option makes impossible, and every refusal with its reason:

1. **Reject (a)/P as the boundary mechanism.** An explicit hidden first parameter makes a plain-C escaped pointer impossible: native expects `cmp(a: *T, b: *T) -> i32` while compiled code has `cmp(ctx, a, b) -> i32`, so every escaped pointer (qsort, pthread, signal) needs a capture thunk, and "real pointer passed directly" is lost. A capture thunk also captures the creating thread's ctx, so cross-thread callback/signal is impossible to get right without adding a TLS lookup — which reduces the explicit parameter to internal-only meaning. Since 2026-07-21 P is not a production candidate; it is retained only as the skeleton/Spike choice (Spike 2, Spike 3), where the handwritten adapter was cleanest.
2. **Reject (c)/R as the boundary mechanism.** r15 survives compiled -> native -> return, but not a native callback or a signal: the callee is entitled to reuse it as a temporary, and mirvm's plain-C FFI has no `JNIEnv*`-like smuggling channel to rebuild it. HotSpot only survives because its JNI boundary is not plain C. R may therefore never own the boundary; it is legal only as an internal mechanism behind a TLS boundary prologue.
3. **Reject R as a substitute for T.** R addresses ctx-read speed, not Engine identity; it cannot say which Engine the current thread has entered. Multi-instance identity must be established by T first. R is gated on reopen trigger 1 (§5).
4. **Reject capture thunks as the boundary answer.** They capture the creating thread's ctx, so any cross-thread callback or signal uses the wrong thread's operand area/TLAB: a data race/corruption and a principle error, not a slowdown. Thunks stay legal only for their actual job, giving an interpreted function a machine address.
5. **Reject any single global ctx.** `Ctx` is per-thread; `SHARED` being a process-level singleton is a real blocker for daemon / mode B / libraryized multi-instance uses (reopen trigger 2).
6. **Reject assuming the calling thread is known.** Foreign threads must attach; no scheme escapes attach, only how it is detected differs (TLS makes it a null check).
7. **Reject same-process per-function mixing of T and R.** In T code r15 is an ordinary callee-saved temporary; R code assumes r15 = ctx; cross-regime calls need wrapping.

## 4. Verification

- `MIRVM_JIT_STATS=1` helper-frequency statistics: 12-bucket atomic counters plus atexit dump, slice 1 `05021d2`. All timings below are JIT-on wall clock.
- fib(32): 61–80ms (≤80ms hard gate); all buckets empty — direct proof of zero ctx sites in a numeric kernel.
- rayon (corpus entry): cold 506ms / hot 189ms; `tls_ref=1392`, `c2i=225467`, `alloc=0` — under a real parallel load cell ③ is still zero (allocation has not entered compiled code).
- unwind_probe (30000 iterations, panic-dense path): `alloc=118529`, `tls_ref=268212`, `c2i=3.59M`, `call_terminate=2.97M` — the extreme form of cell ② density; these calls are themselves helpers, so the T/R difference does not apply to them.
- Spike 5 (real Cranelift): both variants fully correct; fib(30) R 5.47ms vs P 5.90ms (~8%); `enable_pinned_reg`/`get/set_pinned_reg`; CFI half verified — after JIT frame eh_frame registration a host panic crosses the JIT frame correctly, including R's pinned entry.
- Unit regressions: dual-Engine independent ctx, nested activation restore, Engine destruction deregistration.
- corpus per-crate distribution: git history (T3 slice 2 full batch run). T3 mount-point review ruled both mount points unchanged: CallIndirect inline cache and LSDA storage promoted to a JIT data object; no current load distinguishes their value, so they stay in the E7 optimization pool and are re-evaluated with the trigger retest.

## 5. Open items

Reopen triggers (double gate, ruled parallel by the user on 2026-07-21; the first to arrive decides):

1. **E6 entry** — when allocation fast-path inlining or guest TLS fast-path inlining into compiled code is proposed, retest T vs R with that load. Control baseline = §4 data plus Spike 5's ~8%.
2. **Multi-Engine embedding proposal** — `SHARED` is a process-level singleton and is a real blocker in daemon / mode B / libraryized multi-instance scenarios; then vmctx (explicit parameter or TLS mirror) becomes unavoidable, unrelated to performance, and the final verdict is reopened directly.

Unimplemented and open:

- Cell ③ (alloc/TLS inlining) has not entered compiled code.
- attach lifecycle semantics: when a foreign thread's execution state is reclaimed (JNI requires an explicit Detach, Go pools m's; interaction with thread exit hooks and TLS destructors).
- interaction of the `f_boundary` entry with unwind info; Spike 5 verified the CFI half only.
- E22 follow-up boundary: callback revocation, TSD reclamation on long-lived host threads, a stable embedding API.
- historical: the "to be decided at M4" items are marked historical; the internal convention is T, and R is gated on trigger 1. The 2026-07-07 decision scene is preserved only as argument, with open items unchanged.

Related: concurrency-arch.md (per-thread execution state), frame-abi-bytecode.md §10.7 (open-question mount) and §8 (thunk reentry), modeb-mirvmar-design.md (mode B multi-instance).
