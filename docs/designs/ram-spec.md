# RAM-SPEC — Rust Abstract Machine Specification (mirvm's semantic contract)

> Status: Contract · Scope: the Rust abstract machine (RAM) that mirvm targets — correctness contract, the four degrees of definition, boundaries, as-if freedom, UB stance, declared deviations.

## 1. Contract

> **For any program P with defined behavior under RAM, mirvm's observable behavior when executing P conforms to the set of behaviors RAM permits P to produce.**

Three points, none of them optional:

1. **Only observable behavior is constrained** (as-if, §2.3): I/O, syscall effects, volatile accesses, process exit code, panic messages. Everything internal (allocation placement, execution tier, scheduling) is free.
2. **Only defined behavior is promised**: mirvm promises nothing for UB programs (§1.1 UB level, §2.4 stance).
3. **Conformance is to a behavior set, not to a single value**: RAM constrains many things only to a **set** (unspecified / non-deterministic, §1.1). Producing any member of that set is conformant; mirvm **need not** be byte-identical to native (examples: address values, HashMap iteration order, repr(Rust) layout, thread scheduling).

**Rule**: mirvm must never accept producing, for a legal program, a result outside the well-defined behavior set.

### 1.1 The four degrees of definition (strict core)

RAM partitions program behavior into four levels; mirvm's obligation differs per level:

| Level | What RAM says | mirvm obligation | Example |
|---|---|---|---|
| **well-defined** | uniquely determined behavior | **must** produce that behavior | `2+2==4`; len+1 after `Vec::push` |
| **unspecified** | an allowed **set**; the implementation picks one | producing **any** member is conformant (**not necessarily the same as native**) | repr(Rust) field order, HashMap iteration order, the concrete address value of `&x as usize`, uninitialized padding bytes |
| **non-deterministic** | **multiple executions** are allowed | producing **any legal execution** is enough | thread scheduling interleavings, weak-memory visibility, `thread_rng`, `HashMap` random seed |
| **UB** | **no definition** | **no constraint** (assumed not to happen; not detected, §2.4) | data race, out-of-bounds, use-after-free, reading uninitialized memory, aliasing violation |

**Corollary (critical for differential testing, §4)**: only **well-defined observable output** can be compared byte-for-byte against native. unspecified / non-deterministic output can only be compared by **invariant** (e.g. "the sum is 25", not "the order is …") or by **normalization** (e.g. thread names). UB programs are **never** paired; both sides may do anything.

### 1.2 Versioning and consistency

- mirvm **locks** the rustc version (D9, currently nightly-2026-07-02), so this RAM-SPEC corresponds to exactly **one** rustc version.
- Bytecode and distribution artifacts are versioned (C12): a `.mirvm` is like a classfile with a version number, and the runtime must either match it or convert it.
- Consistency statement: mirvm vX is consistent, under the §1 contract, with the RAM defined by rustc vY; deviations follow the registration rules in §5.

## 2. Model

Rust has no official formal specification, but a **de facto abstract machine** exists: rustc's MIR operational semantics plus the opsem team's memory model (borrowed from C++20) plus the provenance model plus rustc's layout algorithm. This document pins that factual RAM down in **contract form**, citing authoritative sources and stating mirvm's boundaries, UB stance, freedoms and deviations. It is **not** an official formal specification (none exists) and **not** a from-scratch operational semantics (that would be the opsem team's decade-long project plus Miri's code). The implementation documents (§2.5) say HOW; this document says WHAT.

### 2.1 One machine, three implementations

| Implementation | Stance | Use |
|---|---|---|
| **native codegen** (rustc+LLVM/cranelift) | production execution | compile to machine code and run |
| **Miri** | *checking* implementation (prefer slowness over missing UB; full provenance/aliasing checking) | UB detection |
| **mirvm** | *running / standard* implementation (assumes legality, aims for speed, checks off) | fast execution / de facto standard |

**Core corollary**: all three implement the same RAM, so **"mirvm output == native output" follows necessarily from shared provenance, it is not a coincidence** — this is the theoretical basis for why differential pairing against native is valid (§4). mirvm and Miri differ not in semantics but in **quality orientation** (detection vs execution); UB detection is an optional QoI for mirvm, not its identity.

### 2.2 Composition: five parts plus UB

| Part | RAM definition | mirvm implementation |
|---|---|---|
| **Storage** | allocation is distinct, aligned, sized, live/dead, and returns a distinct, aligned, non-null address; every byte carries an **initialization state** (init/uninit) and pointer-sized bytes may carry **provenance**; **pointer = address + provenance** (int→ptr, ptr→int, exposed provenance, Strict Provenance); the **aliasing model** (Tree Borrows; the opsem team is still settling it) **defines UB** when violated, and legal programs never violate it | **real addresses** (an allocation's base address is the host's real address, §3); **no per-allocation metadata is tracked** (init mask, provenance and bounds are checker overlays that a fast machine does not need); aliasing is not enforced (§2.4, §3) |
| **Values and layout** | how a type is **realized as bytes**: size / align / field offsets / discriminant encoding / niche optimization, fixed by the **rustc layout algorithm** and **target-specific** (pointer width and alignment follow the platform); repr(C) follows the C ABI, repr(Rust) layout is **unspecified** (§1.1); value shapes are scalar, scalar pair (e.g. `&[T]`, a fat pointer), aggregate | **reuses rustc layout** (target==host / frozen into the bytecode, C8/C12) — bit-identical to native |
| **Computation** | **MIR operational semantics**: place (including projection), rvalue, statement, terminator; function calls, argument passing, returns; **unwinding** (a panic unwinds frames and runs Drop) and **Drop** (including drop glue and drop order); **const eval** is a compile-time subset of the **same machine** (const evaluation runs RAM at compile time) | interprets MIR/bytecode (tier-0/M4); **unwinding is self-implemented** (the VM owns its stack frames; under Model A it uses the native stack plus Cranelift landing pads, frame-abi-bytecode.md §7) |
| **Concurrency** | **memory model derived from C++20**: atomic operations plus orderings (SeqCst/Acquire/Release/AcqRel/Relaxed), happens-before, synchronizes-with; **a data race is UB**; threads follow `std::thread` semantics (spawn/join/lifetime); **TLS** (thread_local) | **real 1:1 OS threads** (C8); guest atomics → **host atomic instructions** (real addresses, i.e. native-codegen behavior; weak memory ordering recovers naturally, C2/C3); the engine does not interpose in guest synchronization (concurrency-arch.md §4) |
| **Observable behavior** | **I/O, syscall effects, volatile accesses, process exit code, panic output** — what the as-if rule must preserve | **true OS passthrough** (read/write/epoll/… against real kernel fds); a panic becomes exit code 101, etc. |
| **UB** | program states RAM leaves **undefined**; a conformant implementation is **unconstrained** on UB, a *checking* implementation (Miri) reports it, and a *standard* implementation (mirvm fast) **assumes it does not happen** | **assume legality, do not detect** (P3); guest UB (race / out-of-bounds / UAF) under real addresses is **host UB**, consistent with native (C4) |

### 2.3 As-if freedom

As long as the observable-behavior contract of §1 holds, mirvm is **free** in the following respects (and already uses that freedom in its design):

- **Execution tier**: interpreter / bytecode VM / JIT (C11/C12) — different implementations of the same RAM.
- **Managed heap allocator**: arena/TLAB, real addresses — RAM only requires allocations to be distinct, aligned and non-null; where the memory comes from is free.
- **Thread implementation**: real OS threads / (tier-0) GIL over real threads — as long as both implement the §2.2 concurrency semantics and produce legal executions.
- **Scheduling**: any schedule that produces a **legal execution** (§1.1 non-deterministic) is conformant.
- **Concrete values of unspecified items**: addresses, repr(Rust) layout, HashMap order — pick any (§1.1).

**Not free**: well-defined observable behavior, which must be preserved. The test is always: **did observable behavior change? If not, it is free.**

### 2.4 UB stance

- **Not detecting UB is a design choice, not a deviation.** mirvm **assumes programs are legal and does not detect UB** (P3). A deviation would be a discrepancy from RAM on a legal program; this is a **quality-orientation choice** that leaves UB detection to Miri. On **legal programs** mirvm conforms to RAM fully.
- **When RAM is undecided, mirvm is naturally neutral.** The factual RAM is still undecided in places (the opsem team is still debating, e.g. the exact aliasing rules of Tree Borrows vs Stacked Borrows). Because mirvm does not detect UB, it is **naturally neutral** about those details: they differ only when **deciding** UB, and mirvm does not decide UB, so **whatever opsem eventually settles, mirvm keeps running legal programs unchanged**. This is a side benefit of turning checking off.
- **Guest UB = host UB.** Under real addresses, guest unsafe UB (data race / out-of-bounds / UAF) is host UB inside the mirvm process, consistent with native behavior (C4). Consequently guest UB, FFI defects and inline asm can break through the VM's own memory (a shared address space) and crash. **Safe guest code provably cannot** (C3); only UB or native defects can trigger it. Protection designs compared L0 type system / L1 structural isolation / L2 MPK / L3 checked / L4 process containment; the later scope decision dropped in-project L2/L4 product work, keeping L1 plus optional L3 as the long-term direction, and **checked mode is not implemented today**. There is no free lunch (real addresses vs Wasm-style cheap enclosure); see concurrency-arch.md §6 and ledger C13.

### 2.5 Implementation pointers

| Layer | Document |
|---|---|
| **Semantics (WHAT)** | this RAM-SPEC |
| Memory implementation (HOW) | DESIGN.md §4 |
| Concurrency implementation (HOW) | docs/designs/concurrency-arch.md |
| Frame / bytecode / JIT / async (HOW) | docs/designs/frame-abi-bytecode.md, git history |
| Boundary / os (HOW) | DESIGN.md §7, P7 os:: |

## 3. Boundaries

**FFI is the boundary of the abstract machine**, which gives a principled definition of what is inside and outside the semantics:

- **Inside** (interpreted/compiled Rust): implements RAM semantics.
- **Outside** (native code: libc, C libraries, raw machine code): **RAM does not model its interior**; mirvm only **hands over control** (FFI out) or **receives control** (thunk in). Native memory allocation (the Native Heap) and native internal behavior are **outside RAM**.
- **inline asm**: an **opaque machine-code effect** inside RAM — not part of RAM computation; mirvm can only model its effect or intercept it at function level (§3.1, C10).
- **Cross-boundary exceptions**: plain `extern "C"` must never unwind; a Rust panic escaping that boundary terminates, and a foreign exception unwinding back into Rust is UB. `extern "C-unwind"` explicitly allows the system unwinder to traverse; mirvm **must** run cleanups along the way and preserve the exception object. Rust `catch_unwind` does not guarantee catching a foreign exception; the pinned toolchain currently terminates when one arrives. See [c-unwind-contract.md](c-unwind-contract.md).

Meaning: **"can run under mirvm ≈ can be compiled by rustc and run natively"**; at the boundary both implementations equally "hand over to native", so consistency at the boundary is guaranteed by both sides calling real native code.

### 3.1 Rejected paths and residual boundaries

- **Unimplemented paths must trap explicitly** and must never manufacture observable behavior outside the legal set through a success return value.
- **Stack overflow depth** stays unspecified; Model A promises only approximate native behavior, not frame-for-frame identity.
- **Volatile**: an independent IR plus an `alignment=1` opaque `MaybeUninit` byte carrier, to avoid host UB from low alignment/padding.
- **Backtrace and the unwinder context API**: before guest frame/IP mapping exists, these must be explicitly rejected and must never return the host interpreter stack.
- **Guest signal handlers**: on 2026-07-12 changed from silent success to explicitly unsupported. On 2026-08-13 the async signal handler was refactored from the M5.2 signal-frame AS-trampoline direct execution into a fixed atomic registration stub plus dispatch at ordinary safe points: process-directed events enter the owner Engine inbox, and `SI_TKILL` thread-directed events enter the target pthread's stable slot established per registration generation. M5.2 D8e implements a guest shadow-frame backtrace, and wide volatile now uses snapshot chunking.
- **Residual boundaries** (registered in [open-issues.md](../open-issues.md) R1/R21/R3): guest handlers for synchronous fault signals; realtime and advanced `sigaction` flags; safe-point latency for process-directed external events; the unwinder context family. These are **implementation gaps, not deviations RAM permits**.

## 4. Verification

- **Differential pairing against native codegen** is valid only for **well-defined observable output**; unspecified / non-deterministic output is compared by invariant or normalization; UB programs are never paired (§1.1). This is the whole theory behind the method.
- **Miri**: a checking implementation of the same RAM → usable as mirvm's **second oracle** (especially for finding mirvm's own bugs); mirvm also borrows its shim/intrinsic **code** (not its mental model, P1).
- **rustc const-eval**: compile time is the same machine → const evaluation and runtime evaluation must agree (§2.2 Computation).
- **Historical tier-0 (InterpCx)** was used for bootstrap and deleted 2026-07-09; M4's differential oracle today is same-source native compiling and executing, and old tier-0 is no longer a runnable oracle. Current gaps and residual boundaries are tracked in [current-status.md](../current-status.md) and [open-issues.md](../open-issues.md), updated together with code and regressions.

**Authoritative sources for the factual RAM:**

- **MIR operational semantics**: [rustc-dev-guide: MIR](https://rustc-dev-guide.rust-lang.org/mir/index.html), rustc `rustc_const_eval::interpret` (the interpretation core shared by Miri and mirvm).
- **Memory model / aliasing model / provenance**: [opsem team / unsafe-code-guidelines](https://github.com/rust-lang/unsafe-code-guidelines), Tree Borrows, Strict Provenance.
- **Layout**: the `rustc_abi` layout algorithm (target-specific).
- **Concurrent memory model**: C++20 (borrowed by Rust).
- **Executable checking reference**: [Miri](https://github.com/rust-lang/miri).
- **Abstract machine / as-if concept**: the C++ abstract machine (the origin of the core idea).

## 5. Open items

Deviation registration rules:

- A choice that falls inside the legal unspecified/non-deterministic set may be registered as an **implementation choice**.
- An item where well-defined behavior is not yet covered is an **implementation gap** and must not be softened into a "deviation".
- Unimplemented paths must trap explicitly (§3.1); they must never return success in a way that produces out-of-set observable behavior.
- The deviations listed by older versions of this section (weak memory ordering, deterministic scheduling, main-thread name) belonged to the deleted InterpCx tier-0 and no longer describe M4.
- Current gaps are listed centrally in [current-status.md](../current-status.md) and kept in sync with code and regressions.

Open items and reopen triggers:

- **rustc version bump**: RAM evolves with Rust (new features, opsem decisions), and mirvm locks the rustc version (D9, currently nightly-2026-07-02); a bump requires re-reviewing this document.
- **opsem settles an undecided rule** (Tree Borrows vs Stacked Borrows): mirvm is neutral because it does not detect UB, so legal programs keep running unchanged; the resolution still triggers a review of §2.4.
- **Residual boundaries** in [open-issues.md](../open-issues.md) R1/R21/R3 remain implementation gaps, not RAM-permitted deviations, and reopen when their mechanisms land.
- **checked mode** is not implemented; L1 structural isolation plus optional L3 checked mode remain the long-term direction (ledger C13, concurrency-arch.md §6).
