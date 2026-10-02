# mirvm design contract

> Status: Contract · Scope: the project's long-term mental model and design contract — the machine
> mirvm implements, the shape of the VM that implements it, the principles a decision is measured
> against, and which sibling document owns each topic. It is not a progress table: what is
> implemented, what is missing and what comes next live in [current-status.md](../current-status.md),
> and open debt in [open-issues.md](../open-issues.md).

## 0. Thesis

**mirvm is a factual standard implementation of the Rust Abstract Machine (RAM), built the way
JVM-class systems software is built.**

- It **implements an abstract machine**; it is not "Rust with an interpreter bolted on". Correctness is
  defined as fidelity to RAM.
- It is RAM's **running** reference. Miri is RAM's *checking* reference — it prefers slowness over
  missing UB — while mirvm assumes legality and aims for speed. Two quality orientations, one machine.
- It is designed from the **VM author's point of view**: managed heap, tiered execution engine, real OS
  threads, loading and linking, JIT. Where a choice comes up, the question is what JVM-class systems
  software would do, not what Miri does.

The normative statement of the machine, its four degrees of definition, its UB stance and every
declared deviation are [ram-spec.md](ram-spec.md). This document states the model the implementation is
built around, and links rather than repeats what a sibling document owns.

## 1. The machine

Rust has no official specification but a de facto abstract machine: rustc's MIR operational semantics,
the opsem team's memory model (borrowed from C++20), its provenance model and rustc's layout algorithm.
RAM is storage, values and layout, computation, concurrency and observable behaviour, with UB as the
state it leaves undefined.

Two consequences frame everything below; [ram-spec.md](ram-spec.md) states both normatively.

- **The correctness contract.** For any program with defined behaviour under RAM, mirvm's observable
  behaviour conforms to RAM. Native codegen is another implementation of the same machine, so "mirvm's
  output equals native's" follows from shared provenance rather than coinciding — which is why
  differential pairing against native is a valid measure of correctness.
- **As-if is the licence.** RAM constrains observable behaviour only, so every internal choice is free:
  execution tier, allocator, scheduling, and the concrete values of unspecified items. The test for any
  optimization is whether observable behaviour changed.

## 2. VM architecture

The JVM is scaffolding for the VM author's view; each of its parts maps onto one of mirvm's.

| JVM part | mirvm counterpart | What it is |
|---|---|---|
| class loading + bytecode verification | **rustc front end** (parse/macros/typeck/borrowck/MIR) + **eager monomorphization** | RAM's loader and verifier — a decade of engineering, reused rather than rebuilt. "Loading" means obtaining one instance's MIR. |
| bytecode | **MIR → mirvm's own typed bytecode** | The load phase freezes everything RAM computation needs; the execution phase never touches `tcx`. |
| managed heap (GC) | **Rust Heap** — managed, never moved, reclaimed by `Drop` rather than a GC | RAM storage, realized; [ram-spec.md](ram-spec.md) §2.2. |
| execution engine (interpreter → C1 → C2) | **typed-bytecode interpreter + method-level Cranelift JIT** | RAM computation, executed; [frame-abi-bytecode.md](frame-abi-bytecode.md). |
| threads (1:1) | **1:1 real OS threads** | [concurrency-arch.md](concurrency-arch.md). |
| JNI | **FFI with a soft boundary** — real addresses, no marshalling | RAM's edge; [ffi-boundary.md](ffi-boundary.md). |
| intrinsics / native methods | **Rust intrinsics + a VM built-in runtime** (allocation, threads, unwind, signals) | RAM's built-in operations, implemented by the VM itself. |

The difference from the JVM is the boundary, and it drives every concrete design. The JVM serves
languages that run *on* it, and its boundary to native code is hard — a GC heap, object headers and
handles that native code cannot be handed — which is why it needs JNI-style marshalling. mirvm is a VM
for a native language: one memory model on both sides, real addresses, no GC, so the boundary is soft.
What it needs in place of JNI is an explicit model of where the abstract machine ends, plus complete
built-in virtualization of the operations RAM defines.

### 2.1 Why the rustc front end is reused

Four walls make a hand-written RAM loader impractical. The front end (trait solver, inference, macro
hygiene) is a decade of engineering. proc-macros are compile-time native code, so a purely interpreted
model cannot run them. Generic monomorphization crosses crates, so MIR that depends on it must be
executable. And std itself rests on unsafe code, intrinsics and syscalls.

Reusing the front end buys more than saved work: **layout and ABI are bit-identical to native**,
because they come from rustc's own layout queries. That is the correctness foundation under real
addresses and FFI. The self-built part is deliberately concentrated in the **execution engine** — the
part worth owning.

## 3. What the machine buys

A correct, fast, embeddable RAM implementation makes these uses consequences rather than features:

1. **Script execution for agents and LLM tooling** — fast start on one file, process-level sandboxing
   and resource limits, and rustc's structured JSON diagnostics for free. Frontmatter follows cargo
   script ([RFC 3424](https://rust-lang.github.io/rfcs/3424-cargo-script.html)).
2. **Project inner-loop acceleration** — `mirvm run .` on a full Cargo project skips codegen and linking,
   and dependency MIR is built once into a content-addressed store, so a rerun pays the load phase
   rather than a rebuild; the measured split is in [distribution-design.md](distribution-design.md)
   §2.6.
3. **REPL/notebook** (unscheduled) — a persistent heap makes state persistence natural and cross-cell
   borrowing a non-problem.
4. **Embedding** — the `Package`/`Engine` lifecycle exists, but the trust boundary is deliberately not a
   safe typed API: `Package::load` is safe while instantiation and untyped export calls are `unsafe`,
   because no system can prove a package's native and FFI declarations
   ([modeb-mirvmar-design.md](modeb-mirvmar-design.md) §2.6).

## 4. The four pillars

Each pillar below is a summary; the document it links owns the details.

### 4.1 Memory: storage, realized

RAM storage is realized at **real addresses**, in one host address space, and split into three parts by
who manages them: implementation-private VM metadata (provenance tables, caches, the thread and layout
tables), the **Rust Heap** that `__rust_alloc` fills and `Drop` reclaims, and the **Native Heap** that
`libc::malloc` and C libraries own. A guest pointer is therefore a host pointer, so memory access is a
plain host read or write for interpreted and native code alike, and either kind of code can touch
either heap. The `AllocId` of the old Miri-derived tier-0 was the key of a checker metadata overlay —
initialization mask, provenance, bounds — which a fast machine that assumes legality does not carry; it
is gone from the engine.

Managed, but never moved, is the Rust-specific correction to the JVM model: managed, because RAM asks
only for distinct, aligned, non-null allocations, so a per-thread arena or TLAB with a lock-free fast
path is allowed; not moving, because pointer-to-integer casts, provenance and pointers handed to native
code all depend on stable addresses. Isolation is structural and honest about its limit: VM metadata
and guest memory live in separate pools, yet real addresses, native FFI and inline asm still mean guest
UB can corrupt VM state, which makes checked mode a reserve rather than a claim. The normative
statement of all of this is [ram-spec.md](ram-spec.md) §2.2–§2.3 and §3; the isolation ladder is
[concurrency-arch.md](concurrency-arch.md) §3.2; the allocator's fast path,
[concurrency-arch.md](concurrency-arch.md) §2.4.

### 4.2 Threads: real OS threads

One guest thread is one OS thread with a real `pthread_t`. `std` already wraps pthread, and the guest
interpreter walks into `pthread_create` itself, so there is nothing to emulate and nothing to wrap
again: `join`, `futex`, mutexes and scheduling stay with the real libc and kernel, and a guest race is a
guest problem rather than something the engine arbitrates.

The one piece of machinery is the **reverse direction**. Natively compiled Rust has machine code for a
thread start routine, so the OS can jump into it; interpreted code has only MIR, whose function pointer
is a placeholder. When a pointer to an interpreted function escapes into native code — a pthread start
routine, a C callback, an escaped function pointer — mirvm materializes it as a real thunk that takes an
execution lease, attaches the current thread's `Ctx` and re-enters the VM. A rejected cooperative tier-0
could not provide a real `pthread_t` and was therefore judged unfit as a product implementation. The
state split that makes this safe, and the engine's own synchronization, belong to
[concurrency-arch.md](concurrency-arch.md); frames and transitions to
[frame-abi-bytecode.md](frame-abi-bytecode.md); how a callback reaches VM state to
[vmctx-passing.md](vmctx-passing.md).

### 4.3 Execution: tiered, one machine

The semantic body is an interpreter over frozen typed bytecode; a method-level Cranelift JIT is the
optimizing tier and is on by default; local machine-code stubs cover the cases the interpreter must
call into native code. Tiering is legal precisely because RAM constrains observable behaviour only, so
all tiers must stay observably equivalent — which is what the interpreter/JIT/native three-way
differential checks.

The bytecode is the frozen interface between the load phase and the execution phase, which is what keeps
`tcx` (and therefore `!Sync` front-end state) out of execution and lets every guest thread run
concurrently. Frame kinds, the calling convention, the four interp↔compiled transitions and unwind are
owned by [frame-abi-bytecode.md](frame-abi-bytecode.md); the `.mirvm` package, which is that same
frozen form made portable, by [modeb-mirvmar-design.md](modeb-mirvmar-design.md); what each tier has
actually reached by [current-status.md](../current-status.md).

### 4.4 Boundary: FFI is the edge of the machine

**FFI is the boundary of the abstract machine.** Inside it mirvm implements RAM; outside it, RAM models
nothing and mirvm only hands control over or receives it back. Every foreign call is classified as
handler-served (a RAM builtin such as an intrinsic, the allocator, unwind, threads or signals), pure
passthrough to the system libc, or inline asm, which is not a call and has no boundary at all. Guest
code is always interpreted or compiled by mirvm, so a foreign call it makes is always visible;
"passthrough" says the handler forwards to the real OS, not that the call is unobserved.

Security is not this boundary's job. A process-level OS sandbox (seccomp-bpf, namespaces) catches every
syscall regardless of which layer issued it, and mirvm-level hooks exist only for virtualization an OS
sandbox cannot express. The classification, the interposition points, the sandbox doctrine and what
native code may do to memory mirvm handed it are owned by [ffi-boundary.md](ffi-boundary.md); each
concrete foreign contract by [c1-ffi-agg-design.md](c1-ffi-agg-design.md),
[c2-rlib-symbols-design.md](c2-rlib-symbols-design.md) and
[c-unwind-contract.md](c-unwind-contract.md).

## 5. Design principles

- **P0 — implement RAM; buy freedom with as-if.** Correctness is fidelity to RAM; every internal
  mechanism (allocator, tier, schedule) is free as long as observable behaviour is preserved.
- **P1 — the VM author's view.** Answer a design question by asking what JVM-class systems software
  would do, not what Miri does. Miri is a checking reference and a code reference for shims and
  intrinsics, never the mental model.
- **P2 — reuse the front end, self-build the engine.** Never rebuild the trait solver, the type system
  or the front end (RAM's loader); build the execution engine (RAM's executor) ourselves.
- **P3 — no borrow checker, no UB detection.** Those are the front end's and Miri's jobs. mirvm assumes
  legal programs and aims for speed.
- **P4 — the boundary is RAM's boundary; never interpose broadly on native code.** Inside it (the
  guest's foreign-call boundary) mirvm implements semantics; outside it (native code, raw function
  pointer calls) mirvm does not see and does not try to intercept, because broad interposition is an
  engineering disaster. Real addresses make the boundary soft and cheap.
- **P5 — do not emulate; use the real OS.** Use real OS primitives (threads, `futex`, files, clocks)
  and do the minimum at the foreign-call boundary, such as a trampoline at `pthread_create`. Emulating a
  mechanism is an anti-pattern: slower and often wrong for legal programs, as `into_pthread_t` proved.
  True parallelism is the default implementation; cooperative scheduling is a rejected emulation, not a
  fallback.
- **P6 — Unix first, one claimed baseline.** Unwinding and FFI are Unix-first, and Linux/ELF/x86_64 is
  the verified baseline and the only platform that may be claimed; Windows is out of scope. A second
  platform is a directory plus an arm in the axis ladder — never a second code path through the core.
- **P7 — OS interaction goes through `src/os` primitives.** Everything that touches the real OS or a
  system library goes through the primitives layer, with `src/os_arch/<os>_<arch>/` for knowledge that
  needs one kernel and one CPU at once. Guest-level semantic rulings stay in the engine. The layout and
  the gate that hold this are `CLAUDE.md`'s *Constraints to Preserve*.

Engineering conventions that follow from the same discipline — the pinned nightly and its monthly bump,
dependency rlibs carrying MIR so the store is content-addressed, the engine as a library behind a thin
CLI, and the borrowing of Miri's shim code with attribution — are owned by
[distribution-design.md](distribution-design.md) where they concern loading, caching and release, and
by [d15-cargoless-design.md](d15-cargoless-design.md) where they concern resolution and compilation.

## 6. What mirvm is not

- **Not Miri.** Miri is RAM's *checking* implementation (prefer slowness to missing UB); mirvm is its
  *running* implementation. One machine, different quality orientation, and UB detection is an optional
  quality property of mirvm rather than its identity.
- **Not evcxr.** evcxr is a compiler shell, driving rustc and linking per cell; mirvm has its own
  execution engine.
- **Not a rustc front-end rebuild.** The trait solver and type system are RAM's loader and are reused.

## 7. Risks and mitigations

| Risk | Mitigation |
|---|---|
| nightly API drift | one pinned dated toolchain with a monthly bump; rustc interaction confined to a few modules; follow Miri's sync commits as a migration guide |
| shim workload (the largest risk) | implement on demand; the boundary model of [ffi-boundary.md](ffi-boundary.md) removes pointless shims (real resources pass through); borrow Miri's shim code |
| test false positives and misreported semantics | a green must compare output or check an invariant; both legs failing is never a PASS; an expected red pins its exact code and diagnostic |
| silent stubs | unimplemented observable semantics must trap or be really implemented — never a success return that fakes support |
| interpreter performance ceiling | the method-level JIT is the answer to the ceiling, and the hard gate is the interpreter/JIT/native three-way differential; the JIT's own gaps (no OSR, no deopt, no production tiering) are listed in [current-status.md](../current-status.md) |
| FFI callbacks from C into Rust | ordinary callbacks go through a thunk plus TLS attach; signals use a fixed atomic registration stub and safe-point dispatch, never libffi or guest code in the signal frame |
| platform semantics | unwind, signal and ABI behaviour is verified per platform and is never extrapolated from the Linux baseline; each platform is a directory plus a ladder arm (P6) |
| lifecycle and embedding | close/wait, execution leases and deferred pthread callbacks are the model of [engine-lifecycle.md](engine-lifecycle.md); `CtxSlot` reclamation and per-instance constructors/destructors exist; retaining process-lifetime code addresses is the explicit boundary for an arbitrary native pointer. Only a safe typed export API or a per-API revocation contract could shrink it — container validation cannot prove an arbitrary FFI ABI |

## 8. Prior art

- [Miri](https://github.com/rust-lang/miri) — RAM's checking reference; the **code** reference for
  `InterpCx`/`Machine`/shims/native libraries, not the mental model.
- [C++ abstract machine and the as-if rule](https://en.cppreference.com/w/cpp/language/as_if) — the
  origin of this project's spine.
- [Rust opsem / unsafe-code-guidelines](https://github.com/rust-lang/unsafe-code-guidelines) — where the
  factual RAM's memory and aliasing models come from.
- [rustc_codegen_cranelift](https://github.com/rust-lang/rustc_codegen_cranelift) — the JIT reference;
  see also its [June 2025 unwinding report](https://bjorn3.github.io/2025/06/30/progress-report-june-2025.html).
- [cargo script RFC 3424](https://rust-lang.github.io/rfcs/3424-cargo-script.html) ·
  [stabilization PR](https://github.com/rust-lang/cargo/pull/16569).
- [rustc-dev-guide: `rustc_private` and the driver](https://rustc-dev-guide.rust-lang.org/rustc-driver/intro.html).
- [evcxr](https://github.com/evcxr/evcxr) — the counter-example: a compiler shell, with per-cell
  latency and state that cannot be embedded.

## 9. Document index

| Document | Status | Owns |
|---|---|---|
| [ram-spec.md](ram-spec.md) | Contract | the Rust abstract machine: correctness contract, degrees of definition, storage/computation/concurrency semantics, as-if freedom, UB stance, declared deviations |
| [ffi-boundary.md](ffi-boundary.md) | Contract | the abstract machine's edge: the foreign-call classification, interposition points, the sandbox doctrine, native access to guest memory |
| [concurrency-arch.md](concurrency-arch.md) | Decided RFC | guest threads → OS threads, the state split, why execution never holds a `tcx`, isolation and the checked-mode reserve |
| [engine-lifecycle.md](engine-lifecycle.md) | Decided RFC | an Engine's holds, its phase machine, process exit versus embedding close, and how engine-owned per-thread state survives a non-local exit |
| [frame-abi-bytecode.md](frame-abi-bytecode.md) | Implemented | frame kinds, guest local storage, the calling convention and the four interp↔compiled transitions, the bytecode format, JIT backend and distribution |
| [vmctx-passing.md](vmctx-passing.md) | Decided RFC | how compiled frames, escaped function pointers, callbacks and signals reach per-thread VM execution state |
| [c-unwind-contract.md](c-unwind-contract.md) | Implemented | Rust panic and C++ exceptions across foreign ABIs, and which flows each boundary may carry |
| [c1-ffi-agg-design.md](c1-ffi-agg-design.md) | Implemented | by-value aggregate parameters and returns across `extern "C"`/`extern "system"` |
| [c2-rlib-symbols-design.md](c2-rlib-symbols-design.md) | Decided RFC | closing the `.a` → `.so` gap when a symbol's definition lives in a Rust rlib |
| [modeb-mirvmar-design.md](modeb-mirvmar-design.md) | Implemented | the `.mirvm` container, `pack`/`run`, the machine-code section, multi-Engine instantiation and the embedding surface |
| [distribution-design.md](distribution-design.md) | Decided RFC | load ingestion, cache layering, toolchain bundling and release form |
| [dep-sharing-design.md](dep-sharing-design.md) | Decided RFC | fine-grained reuse of lowered dependency products: per-crate image units, the content-addressed fragment store, the home rule, store collection |
| [jit-code-cache-design.md](jit-code-cache-design.md) | Decided RFC | the persistent JIT machine-code cache: the translator's relocation choke point, relocatable per-function artifacts, load-time linking, adaptive optimization tiers |
| [d15-cargoless-design.md](d15-cargoless-design.md) | Implemented | resolving and scheduling without cargo: manifests, versions, topology, build scripts, proc-macros |
| [mirvm-test-cargoless-contract.md](mirvm-test-cargoless-contract.md) | Contract | `mirvm test` for single packages and workspaces with no Cargo process at run time |

When this index and a document disagree, the document owns its topic; when a document and the code
disagree about what is implemented, [current-status.md](../current-status.md) and the code win.
