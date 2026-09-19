# vmctx passing: how compiled code and the callback boundary reach VM execution state

> Status: Decided RFC · Scope: how compiled frames and native-to-guest entry points — escaped function
> pointers, callbacks, signals — reach the per-thread VM execution state, and why the alternatives
> were rejected.

## 1. Contract

1. **Boundary = lookup by current thread (TLS) plus lazy attach.** Every entry that native code or the
   kernel can invoke on an arbitrary thread must look up the *current* thread's execution state in its
   prologue and lazily attach when the read is null. This is forced, not preferred. Attach logic
   belongs to the thread layer.
2. **Compiled-to-compiled calls never touch TLS.** They use the internal fast convention on `f_fast`;
   the plain-C-signature boundary entry `f_boundary` is a separate entry per compiled guest function.
3. **Thread-local "current Engine" identity (T) is the verdict.** Each Engine holds its own
   `Arc<Shared>`, the host thread's execution-state table stores `Ctx` keyed by Engine id, a boundary
   entry activates the matching entry, and a nested return restores the outer one. The old
   process-level `SHARED` and the single JIT worker were split into per-Engine state.
4. **The T/R choice was not a performance judgment.** A reserved register (R) makes an
   already-selected context read faster, but it cannot say which Engine the thread has entered;
   multi-instance identity must be established by T first. R remains a performance candidate for the
   point where the allocation or guest-TLS fast path enters compiled code, and cannot replace T.
5. **The fast signature is the pure guest signature** (all ABI forms). The single context seam is
   `get_ctx()`: the shared static plus the boundary TLS attach. No register rent is prepaid for a
   hypothetical load.
6. **Compiled code today touches per-thread execution state at zero sites.** The three admission
   tables (statement, rvalue, terminator) are exhaustive: the GC/safepoint/TLAB/stack-check/linear-
   memory cell does not exist structurally by architectural commitment; the FFI/c2i/catch/panic cell is
   helper-shaped, so context acquisition amortizes inside each helper's heavyweight cost; and the
   allocation/guest-TLS inlining cell has not entered.
7. **T-to-R is an ABI-compatible single switch**: enable the pinned-register ISA flag, lower the
   `get_ctx()` TLS load to a pinned-register read, and save/set/restore at the boundary entry. The flip
   granularity is the whole JIT code cache recompiled under the new regime, which is free because the
   cache is in-process volatile and mode B distributes bytecode, not machine code. Layering is never
   per-function mixing within a process: in T code the pinned register is an ordinary callee-saved
   temporary, while R code assumes it holds the context, so cross-regime calls need wrapping.
8. **Signal no longer goes through this boundary.** The kernel frame only atomically registers;
   process-directed events go to the owner Engine and `SI_TKILL` thread-directed events go to the
   target pthread, and both establish a new activation at an ordinary safepoint. Any passage below
   that lists signals alongside ordinary callbacks is the earlier argument, not the current
   implementation.

## 2. Model

### 2.1 Frames, state, pressure

Three frame kinds share one native stack with one execution state per thread:

```text
interp frame --i2c--> compiled frame --FFI--> native frame
(host code,           (JIT output)            (libc / C)
 explicit *mut Ctx)        |
                           v   how does it reach the context? = this document
Ctx (vmctx, one per guest thread; points at the shared read-only program)
  TLAB alloc pointer | operand area | dispatch/kinds | panic state
```

An interpreted frame is host code with an explicit context parameter, so it has no problem. A native
frame by definition does not know the context exists, which is the source of the escape and callback
problem. A compiled frame needs a mechanism to obtain it.

mirvm is under far less pressure than Wasm here: real addresses mean guest memory access compiles to
bare load and store, statics and vtables are frozen real-address constants, and compiled-to-compiled
is a direct native call — none of which go through the context. Wasmtime reaches vmctx on every
linear-memory access to obtain the memory base; mirvm does not. Compiled code genuinely touching the
context is limited to:

- TLAB allocation — hot, every `Box`/`Vec` growth;
- c2i into a still-interpreted callee — cold to common during warmup, disappearing once JIT-linked;
- panic and unwind bookkeeping, and the guest stack bound check — low frequency, entry level;
- checked-mode `GuestMemory::contains` — checked mode only.

So most pure computation functions need no context at all. Threading vmctx through every function, as
the Wasm default does, taxes everyone for the few users.

### 2.2 Decisive constraint: FFI correctness, not performance

Mechanism performance differences are second-order; two first-order criteria decide the design.

1. **Escaped guest function pointers must be plain-C callable.** `into_pthread_t`, qsort comparators,
   signal handlers and C library callbacks all require an escaped guest function pointer to be callable
   by native code with an ordinary C signature, zero marshalling and zero wrapping. Wasmtime can afford
   an explicit vmctx first parameter because Wasm has no bare function pointer escaping to native under
   the C ABI — everything goes through the Wasmtime API. mirvm is the opposite, and that is the root
   reason it diverges.
2. **The context is per-thread.** A `Ctx` is one execution state per guest thread, holding a private
   operand area and TLAB, and pointing at the shared read-only program. A scheme that sews the context
   into a thunk captures the *creating* thread's context, so a callback on another thread would use the
   wrong operand area and TLAB — a data race and a principle error, not slowness.

### 2.3 Signal handlers pin the boundary down

A signal handler runs on whatever thread receives the signal. For a guest-registered handler, a
capture thunk would most likely run on the wrong thread, and a reserved register would hold an
arbitrary value of the interrupted code. The only correct approach is to look up the execution state by
current thread at entry, with a lazy attach when the read is empty. Any entry native code or the kernel
can invoke on an arbitrary thread must therefore do a TLS lookup in its prologue; the remaining degree
of freedom is only how compiled code passes the context internally.

### 2.4 Candidate mechanisms

**(a) Explicit vmctx first parameter** (Wasmtime, `lua_State*`, JNI's `JNIEnv*`). The compiled
signature gains one hidden first parameter, which is the fastest possible internal access and needs no
global state. Its fatal flaw for mirvm is that the escaped-pointer signature no longer matches: native
expects `cmp(a: *T, b: *T) -> i32` while compiled code has `cmp(ctx, a, b) -> i32`. Every escaped
pointer would then need a capture thunk, breaking "a real pointer passed directly", and the thunk would
capture the creating thread's context — so a cross-thread callback still needs a TLS lookup, which
reduces the explicit parameter to internal meaning only. JNI is not a counterexample because it is a
dedicated API boundary rather than a plain-C escape.

**(b) Thread-local state.** A guest thread start or foreign-thread attach sets the thread's context
once, and compiled code needing it does one segment-addressed load. Escapes and callbacks need zero
handling: the signature is already plain C and the lookup naturally selects the current thread, so a
signal handler automatically sees the receiving thread's context. The foreign-thread case is handled by
lazy attach, exactly as the JVM's `AttachCurrentThread` and Go's cgo `needm` do:

```text
a C library worker thread calls back into guest:
  prologue: TLS context is null -> create this thread's execution state -> set TLS -> continue
```

The cost of the load depends on the TLS model. When the context lives in the mirvm host itself, it is
initial-exec or local-exec, about an ordinary L1 load whose offset the host hands to codegen at JIT
time; it does not take Cranelift's general-dynamic `__tls_get_addr` path. As a fallback, compiled code
can call a three-instruction host helper, which is irrelevant at once-per-entry frequency. If the
engine is later embedded as a dlopen plugin, the host's TLS falls into the dynamic model and this must
be revisited. Frequency can be reduced to at most once per activation, because a frame activation lives
and dies on one thread, so the JIT can load once at entry and carry it in a register for the whole body
— which degrades (b) to (a) inside a function.

**(c) Reserved register** (HotSpot's r15, Go's g). The JIT pins one register as the context, so
reading it costs nothing, and a compiled-to-native-to-compiled round trip is fine because the register
is callee-saved. The hole is a native callback mid-way: the callee is entitled to reuse that register
as a temporary, so reading it then yields garbage. HotSpot survives only because its JNI boundary is
not plain C — `JNIEnv*` is passed explicitly and a native callback rebuilds r15 from it. mirvm's FFI
target is plain C with no such smuggling channel, so the boundary hole cannot self-heal and R must be
paired with TLS boundary recovery. As an internal mechanism R remains feasible, since Cranelift
supports pinned registers. No production VM bare-relies on one mechanism across the FFI boundary:
HotSpot pairs a register with `JNIEnv*`, Go with `needm`, V8 a register cache with TLS.

### 2.5 Verdict: TLS boundary plus one fast internal convention

The boundary is pinned by §2.3 and internal use sites are sparse, which together give HotSpot's
multi-entry idea: every compiled guest function has two entries.

```text
escaped to native ->  f_boundary: plain C signature, context from TLS (lazy attach if null),
                                   then tail-call f_fast(ctx, args...)
internal call     ->  f_fast(ctx, args...): fast entry; compiled-to-compiled calls another
                                   f_fast directly with the context in registers
```

So compiled-to-compiled never touches TLS, a native-to-guest entry pays one TLS read plus a first-time
attach, and a signal handler uses the same boundary entry and is naturally on the right thread. The
consequences that decide the design:

- An escaped compiled-state guest function needs **no thunk at all**, because `f_boundary` is itself
  plain-C native code.
- An escaped interpreted-state function still needs a thunk, but that is the thunk's actual job: giving
  an interpreted function a machine address. Its prologue shares the same TLS and attach logic as
  `f_boundary`.
- Once a function is JIT-compiled, its escaped pointer can be the `f_boundary` address directly and the
  thunk disappears.
- A foreign thread must attach in every scheme; TLS merely turns "whether to attach" into a null check,
  and the 1:1 real-thread model makes attaching cheap.

### 2.6 Recorded measurement

Both T and R were implemented on real Cranelift and were fully correct, including the boundary save,
set and restore shape and idempotent re-entry. On a `fib(30)` direct-call microbenchmark, R was about
8% faster than the explicit-parameter variant (5.47ms against 5.90ms), with the generated code
confirming P's per-call threading tax and R's zero context movement on internal direct calls. The
microbenchmark is unrepresentative, so the final internal choice is left to real load: R is gated on
the reopen triggers in §5.

## 3. Boundaries

- **Reject the explicit parameter as the boundary mechanism.** It makes a plain-C escaped pointer
  impossible, forces a capture thunk onto every escaped pointer, and then captures the creating
  thread's context, so cross-thread callbacks and signals still need a TLS lookup. It survives only as
  an internal option and as the cleanest handwritten adapter skeleton.
- **Reject the reserved register as the boundary mechanism.** It survives compiled-to-native-to-
  compiled but not a native callback or a signal, and mirvm has no smuggling channel to rebuild it. R
  may therefore never own the boundary, only work behind a TLS boundary prologue.
- **Reject R as a substitute for T.** R addresses context-read speed, not Engine identity; it cannot
  say which Engine the current thread has entered, and multi-instance identity must come from T first.
- **Reject capture thunks as the boundary answer.** They capture the creating thread's context, so any
  cross-thread callback or signal uses the wrong operand area and TLAB. Thunks stay legal only for
  giving an interpreted function a machine address.
- **Reject any single global context.** `Ctx` is per-thread, and a process-level singleton blocks
  daemon, mode B and libraryized multi-instance uses.
- **Reject assuming the calling thread is known.** Foreign threads must attach; only the detection
  method differs.
- **Reject per-function mixing of T and R within a process**, because cross-regime calls need
  wrapping.

## 4. Verification

- `MIRVM_JIT_STATS=1` gives 12-bucket atomic helper-frequency counters dumped at exit. All timings are
  JIT-on wall clock.
- fib(32): 61–80ms against the 80ms hard gate, with all buckets empty — direct proof of zero context
  sites in a numeric kernel.
- rayon (corpus entry): cold 506ms, hot 189ms, with `tls_ref=1392`, `c2i=225467`, `alloc=0`. Under a
  real parallel load the allocation cell is still zero, because allocation has not entered compiled
  code.
- unwind_probe (30000 iterations on a panic-dense path): `alloc=118529`, `tls_ref=268212`,
  `c2i=3.59M`, `call_terminate=2.97M` — the extreme form of helper-cell density, where the calls are
  themselves helpers so the T/R difference does not apply.
- Spike with real Cranelift: both variants correct, including a host panic crossing a JIT frame after
  `.eh_frame` registration.
- Unit regressions: two Engines with independent contexts, nested activation restore, and context
  deregistration on Engine destruction.

## 5. Open items

Reopen triggers, either of which decides:

1. **Allocation or guest-TLS fast-path inlining into compiled code** — retest T against R with that
   load, against the §4 data and the ~8% microbenchmark.
2. **A multi-Engine embedding proposal** — a process-level `SHARED` blocks daemon, mode B and
   libraryized multi-instance scenarios, at which point a vmctx parameter or TLS mirror becomes
   unavoidable for reasons unrelated to performance, and the verdict is reopened.

Unimplemented:

- The allocation and guest-TLS inlining cell has not entered compiled code.
- Attach lifecycle: when a foreign thread's execution state is reclaimed, given that the JVM requires
  an explicit detach and Go pools its machine threads, including interaction with thread-exit hooks and
  TLS destructors.
- How the `f_boundary` entry interacts with unwind info; only the CFI half was verified.
- Callback revocation, TSD reclamation on long-lived host threads, and a stable embedding API.

Related: [concurrency-arch.md](concurrency-arch.md) for the per-thread execution state,
[frame-abi-bytecode.md](frame-abi-bytecode.md) for thunk reentry, and
[modeb-mirvmar-design.md](modeb-mirvmar-design.md) for mode B multi-instance.
