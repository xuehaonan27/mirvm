# RAM-SPEC — Rust Abstract Machine Specification (mirvm's semantic contract)

> Status: Contract · Scope: the Rust abstract machine (RAM) that mirvm targets — the correctness
> contract, the degrees of definition, boundaries, as-if freedom, UB stance and declared deviations.
> How each part is implemented lives in [DESIGN.md](../../DESIGN.md) and the sibling contracts
> ([concurrency-arch.md](concurrency-arch.md), [frame-abi-bytecode.md](frame-abi-bytecode.md),
> [c-unwind-contract.md](c-unwind-contract.md)).

## 1. Contract

> **For any program P with defined behaviour under RAM, mirvm's observable behaviour when executing P
> conforms to the set of behaviours RAM permits P to produce.**

Three points, none optional:

1. **Only observable behaviour is constrained** (as-if): I/O, syscall effects, volatile accesses,
   process exit code, panic messages. Everything internal — allocation placement, execution tier,
   scheduling — is free.
2. **Only defined behaviour is promised.** mirvm promises nothing for UB programs.
3. **Conformance is to a behaviour set, not to a single value.** Where RAM constrains only a set,
   producing any member is conformant, so mirvm need not be byte-identical to native (address values,
   HashMap iteration order, repr(Rust) layout, thread scheduling).

The rule: mirvm must never accept producing, for a legal program, a result outside the well-defined
behaviour set.

### 1.1 The four degrees of definition

- **Well-defined** — RAM determines one behaviour, and mirvm must produce it. Example: `2+2==4`, or a
  length of `len+1` after `Vec::push`.
- **Unspecified** — RAM allows a set and the implementation picks one; any member is conformant, not
  necessarily the one native picks. Examples: repr(Rust) field order, HashMap iteration order, the
  concrete value of `&x as usize`, uninitialized padding bytes.
- **Non-deterministic** — multiple executions are allowed, and producing any legal execution is
  enough. Examples: thread interleavings, weak-memory visibility, `thread_rng`, the HashMap seed.
- **UB** — no definition, so no constraint; it is assumed not to happen and is not detected. Examples:
  data race, out-of-bounds, use-after-free, reading uninitialized memory, aliasing violation.

Corollary for differential testing: only well-defined observable output can be compared byte-for-byte
against native. Unspecified and non-deterministic output is compared by invariant ("the sum is 25",
not "the order is …") or by normalization (thread names). UB programs are never paired — both sides
may do anything.

mirvm locks one rustc version, so this specification corresponds to exactly one toolchain, and bytecode
artifacts are versioned: a `.mirvm` is like a classfile with a version number, and the runtime must
either match it or convert it. mirvm vX is consistent, under the contract above, with the RAM defined
by rustc vY; deviations follow the registration rules in §5.

## 2. Model

Rust has no official formal specification, but a de facto abstract machine exists: rustc's MIR
operational semantics plus the opsem team's memory model (borrowed from C++20), its provenance model
and rustc's layout algorithm. This document pins that factual RAM down in contract form. It is not an
official specification, and not a from-scratch operational semantics — that would be the opsem team's
decade-long project plus Miri's code.

### 2.1 One machine, three implementations

- **Native codegen** (rustc + LLVM/Cranelift) — production execution.
- **Miri** — a *checking* implementation: it prefers slowness over missing UB, with full provenance and
  aliasing checking. Used for UB detection.
- **mirvm** — a *running* implementation: it assumes legality and aims for speed, with checks off.

Because all three implement the same RAM, "mirvm output equals native output" follows necessarily from
shared provenance rather than coinciding; that is why differential pairing against native is valid.
mirvm and Miri differ in quality orientation (detection versus execution), not in semantics, so UB
detection is an optional quality property of mirvm rather than its identity.

### 2.2 Composition: five parts plus UB

- **Storage.** RAM requires allocations to be distinct, aligned, sized, live or dead, returning a
  distinct aligned non-null address; every byte carries an initialization state and pointer-sized bytes
  may carry provenance, so a pointer is address plus provenance (covering int→ptr, ptr→int and
  exposed provenance); and the aliasing model defines UB when violated, which legal programs never do.
  mirvm uses **real addresses** — an allocation's base address is the host's real address — tracks no
  per-allocation metadata, since init masks, provenance and bounds are checker overlays a fast machine
  does not need, and does not enforce aliasing.
- **Values and layout.** A type is realized as bytes: size, align, field offsets, discriminant
  encoding and niche optimization, fixed by the rustc layout algorithm and target-specific. repr(C)
  follows the C ABI, repr(Rust) layout is unspecified, and value shapes are scalar, scalar pair (a fat
  pointer) or aggregate. mirvm **reuses rustc layout**, frozen into the bytecode, so it is
  bit-identical to native.
- **Computation.** MIR operational semantics: place including projection, rvalue, statement and
  terminator; calls, argument passing and returns; unwinding and Drop including drop glue and order;
  and const eval as a compile-time subset of the same machine. mirvm interprets MIR/bytecode and
  self-implements unwinding — the VM owns its stack frames, which under Model A means the native stack
  plus Cranelift landing pads.
- **Concurrency.** The memory model is derived from C++20: atomic operations and their orderings
  (SeqCst, Acquire, Release, AcqRel, Relaxed), happens-before and synchronizes-with, with a data race
  being UB; threads follow `std::thread` semantics; and TLS is `thread_local`. mirvm uses real 1:1 OS
  threads, turns guest atomics into host atomic instructions on real addresses (native-codegen
  behaviour, so weak ordering recovers naturally), and never interposes in guest synchronization.
- **Observable behaviour.** I/O, syscall effects, volatile accesses, process exit code and panic
  output are what the as-if rule must preserve. mirvm passes them through to the OS: real kernel file
  descriptors, and a panic becoming exit code 101.
- **UB.** RAM leaves these states undefined, a conformant implementation is unconstrained on them, a
  checking implementation reports them, and a standard implementation assumes they do not happen.
  mirvm assumes legality and does not detect UB; guest UB under real addresses is host UB, consistent
  with native.

### 2.3 As-if freedom

As long as the observable-behaviour contract holds, mirvm is free in these respects, and already uses
that freedom:

- **Execution tier** — interpreter, bytecode VM or JIT; they are different implementations of one RAM.
- **Heap allocator** — arena or TLAB with real addresses; RAM only requires distinct, aligned,
  non-null allocations, so the memory's origin is free.
- **Thread implementation** — real OS threads, or in a transitional design a GIL over them, as long as
  the concurrency semantics and legal executions are preserved.
- **Scheduling** — any schedule producing a legal execution.
- **Concrete values of unspecified items** — addresses, repr(Rust) layout, HashMap order.

Not free: well-defined observable behaviour. The test is always whether observable behaviour changed;
if it did not, the choice is free.

### 2.4 UB stance

- **Not detecting UB is a design choice, not a deviation.** mirvm assumes programs are legal, so on
  legal programs it conforms to RAM fully; UB detection is left to Miri.
- **Where RAM is undecided, mirvm is naturally neutral.** The factual RAM is still unsettled in places
  — the exact Tree Borrows versus Stacked Borrows aliasing rules, for instance. Those details differ
  only when *deciding* UB, and mirvm does not decide UB, so whatever opsem eventually settles, legal
  programs keep running unchanged.
- **Guest UB is host UB.** Under real addresses, a guest data race, out-of-bounds access or
  use-after-free is host UB inside the mirvm process, exactly as in native. Guest UB, FFI defects and
  inline asm can therefore break through the VM's own memory, because the address space is shared.
  Safe guest code provably cannot; only UB or native defects can. The defense ladder compares an L0
  type-system layer, L1 structural isolation, L2 MPK, L3 checked mode and L4 process containment; the
  project keeps L1 plus an optional L3 as its direction and has not implemented checked mode. There is
  no free lunch between real addresses and Wasm-style cheap enclosure.

## 3. Boundaries

**FFI is the boundary of the abstract machine**, which is what makes "inside" and "outside" precise:

- **Inside** — interpreted or compiled Rust, which implements RAM semantics.
- **Outside** — native code such as libc, C libraries and raw machine code. RAM does not model its
  interior; mirvm only hands over control (FFI out) or receives it (thunk in). Native allocation and
  native internal behaviour are outside RAM.
- **Inline asm** is an opaque machine-code effect inside RAM: mirvm can only model its effect or
  intercept it at function level.
- **Cross-boundary exceptions**: plain `extern "C"` must never unwind, a Rust panic escaping it
  terminates, and a foreign exception unwinding back into Rust is UB. `extern "C-unwind"` explicitly
  allows the system unwinder to traverse, and mirvm must run cleanups along the way and preserve the
  exception object. `catch_unwind` does not guarantee catching a foreign exception. See
  [c-unwind-contract.md](c-unwind-contract.md).

So "can run under mirvm" is approximately "can be compiled by rustc and run natively": at the boundary
both implementations hand over to real native code, which is what makes them consistent there.

### 3.1 Rejected paths and residual boundaries

- **Unimplemented paths must trap explicitly** and must never manufacture out-of-set observable
  behaviour through a success return value.
- **Stack overflow depth** stays unspecified: Model A promises approximate native behaviour, not
  frame-for-frame identity.
- **Volatile** uses an independent IR plus an `alignment=1` opaque `MaybeUninit` byte carrier, avoiding
  host UB from low alignment and padding.
- **Backtrace and the unwinder context API** must be explicitly rejected before guest frame/IP mapping
  exists, and must never return the host interpreter stack.
- **Guest signal handlers** are explicitly unsupported rather than silently successful: a fixed atomic
  registration stub dispatches at ordinary safe points, process-directed events enter the owner
  Engine's inbox, and `SI_TKILL` thread-directed events enter the target pthread's stable slot
  established per registration generation. A guest shadow-frame backtrace exists, and wide volatile
  uses snapshot chunking.
- **Residual boundaries** — registered in [open-issues.md](../open-issues.md) — are guest handlers for
  synchronous fault signals, realtime and advanced `sigaction` flags, safe-point latency for
  process-directed external events, and the unwinder context family. These are implementation gaps,
  not deviations RAM permits.

## 4. Verification

- **Differential pairing against native codegen** is valid only for well-defined observable output;
  unspecified and non-deterministic output is compared by invariant or normalization, and UB programs
  are never paired. This is the theory behind the method.
- **Miri** is a checking implementation of the same RAM, so it serves as mirvm's second oracle,
  especially for finding mirvm's own bugs; mirvm also borrows its shim and intrinsic code, not its
  mental model.
- **rustc const eval**: compile time is the same machine, so const evaluation and runtime evaluation
  must agree.
- The differential oracle today is same-source native compilation and execution. Current gaps and
  residual boundaries are tracked in [current-status.md](../current-status.md) and
  [open-issues.md](../open-issues.md), updated together with code and regressions.

Authoritative sources for the factual RAM:

- MIR operational semantics: [rustc-dev-guide: MIR](https://rustc-dev-guide.rust-lang.org/mir/index.html)
  and rustc's const-eval interpreter, the core shared by Miri and mirvm.
- Memory, aliasing and provenance: the
  [opsem team / unsafe-code-guidelines](https://github.com/rust-lang/unsafe-code-guidelines), Tree
  Borrows, Strict Provenance.
- Layout: the `rustc_abi` layout algorithm, which is target-specific.
- Concurrent memory model: C++20, borrowed by Rust.
- Executable checking reference: [Miri](https://github.com/rust-lang/miri).
- The abstract-machine and as-if concept: the C++ abstract machine.

## 5. Open items

Registration rules: a choice inside the legal unspecified or non-deterministic set may be registered
as an implementation choice; an item where well-defined behaviour is not yet covered is an
implementation gap and must not be softened into a "deviation"; unimplemented paths must trap
explicitly and never return success in a way that produces out-of-set observable behaviour. Current
gaps are listed centrally in [current-status.md](../current-status.md).

- **A rustc version bump** requires re-reviewing this document, since RAM evolves with Rust and mirvm
  locks one version.
- **opsem settling an undecided rule** (Tree Borrows versus Stacked Borrows) triggers a review of
  §2.4; legal programs keep running unchanged either way, because mirvm does not decide UB.
- **The residual boundaries** above remain implementation gaps, not RAM-permitted deviations, and
  reopen when their mechanisms land.
- **Checked mode** is unimplemented; L1 structural isolation plus optional L3 remain the long-term
  direction.
