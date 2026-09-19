# C and C-unwind Cross-Language Exception Contract

> Status: Implemented · Scope: how mirvm handles Rust panic and C++ exceptions on Linux/ELF/x86_64,
> and which exception flows each foreign ABI may carry.

## 1. Contract

Per-boundary rules. The observation column is what tests may lock.

- **Exception-free outbound call** (`C` or `C-unwind`) — return by the original signature; a JIT call
  with cleanup recovers the return value correctly. Observable: the return value and Drop count.
- **C++ exception through guest back to C++** (`C-unwind` end to end) — preserve the original
  exception object and type; interpreter and JIT frames run cleanup. Observable: the typed catch value
  and Drop count. Not promised: that Rust `catch_unwind` catches it.
- **Guest panic through C++ back to guest** (`C-unwind` end to end, C++ rethrows only) — preserve the
  original Rust panic payload and run cleanup. Observable: the payload, the C++ rethrow marker and the
  Drop count. Not promised: that C++ may swallow the panic and continue.
- **C++ swallows a Rust panic** (`C-unwind`) — terminate as native does. Observable: non-zero exit and
  termination reason.
- **C++ exception reaches a guest `catch_unwind`** (`C-unwind`) — terminate as the pinned rustc does,
  and the guest catch function must not run. Observable: non-zero exit and an explicit
  foreign-exception reason. Not promised: disguising a C++ exception as a guest panic.
- **Rust panic crosses a plain C callback** (`C`) — terminate at that ABI boundary. Observable:
  non-zero exit and termination reason. Not promised: byte-identical stderr.
- **C++ exception tries to cross a guest plain C wrapper** (`C` wrapper calling `C-unwind`) — terminate
  at rustc's `Terminate` edge, and the outer C++ catch must not receive it. Observable: non-zero exit
  and the outer frame not returning normally.
- **C++ exception reaches the top of the Engine** (`C-unwind` end to end) — do not consume or rewrite
  it; hand it back to the outer system unwinder. Observable: the outer C++ still catches the original
  type and value.
- **Guest panic escapes an unsafe raw export** — first decrement the panic count in guest std, drop and
  release the payload, then report `RunOutcome::GuestPanic`. Observable: the payload is dropped exactly
  once and the Engine stays callable.
- **`main` panic caught by `lang_start`** — lowering marks the call wrapping user `main` exactly, and
  that `run_main` records its catch result separately. Observable: `GuestPanic` distinct from a normal
  `Termination` return of 101. Not promised: inferring a panic from the numeric value 101.
- **Asynchronous signal to a guest handler** — the kernel frame only registers atomically; process
  events go to the owner inbox, thread events to the target pthread's stable slot, and an ordinary
  safepoint runs the handler under a fresh lease and activation. Observable: the handler belongs to the
  Engine that registered it, thread events still run on the target pthread, and the outer main catcher
  is not polluted. Not promised: entering libffi, guest code or unwind from the signal frame; prompt
  latency for process-directed external events.
- **Native fini during Engine close** — a non-unwindable teardown boundary: catch any MIRVM, foreign or
  host Rust exception, emit the fixed diagnostic `native finalizer unwound during Engine teardown`,
  then abort. Not promised: continuing to propagate `EngineFault` to the embedder, or continuing to
  close after an exception escapes.

Numbered rules:

- **C1** Lowering freezes the unwind bit from rustc `ExternAbi::C/System { unwind }` and never defaults
  it to false. Other ABIs must not masquerade as plain C and are rejected before entering libffi.
- **C2** A plain C direct foreign call keeps libffi's plain C declaration. Only `unwind=true` calls the
  same `ffi_call` symbol through a local `extern "C-unwind"` declaration.
- **C3** The callback thunk and the P1 executable entry select a plain C or C-unwind wrapper from
  `ForeignSig.unwind`; both share one argument-moving and execution body.
- **C4** MIR `UnwindAction::Terminate` outranks the callee's ABI. Direct foreign calls and native
  function pointers must both pass the terminate guard, and no exception may cross a guest plain C
  wrapper.
- **C5** JIT `try_call` is terminatory; its return value comes from the `TryCallRet` block parameter on
  the normal branch, never from the instruction's results.
- **C6** Cleanup treats every exception source alike and must not skip Drop under a C++ exception just
  to recognize guest panics only.
- **C7** Guest panic and engine fault use MIRVM-owned exception classes, and catch sites classify the
  raw exception pointer the system unwinder delivers. MIRVM does not write its own personality;
  interpreter and JIT frames keep using the existing Rust personality and LSDA for cleanup.
- **C8** Classifying an owned exception must check the exception class, the ABI cookie, an in-process
  canary, and whether the `Arc<Shared>` inside the exception points at the same object as the current
  Engine. The class alone or a numeric Engine id is not sufficient ownership evidence, because another
  Engine's guest panic must keep unwinding.
- **C9** `EngineFault` is an engine fault, not a guest panic. Every interpreter raw catch and JIT
  landing pad must inspect the pointer it actually received and skip guest cleanup only when that
  object really is an `EngineFault` — never substitute "this thread still has some unsettled fault",
  since a native catch can suspend an outer fault and then re-enter a guest panic that needs cleanup.
- **C10** The outer MIRVM exception stores only the original guest std exception pointer. On a guest
  catch that pointer goes back to the guest catch function; when uncaught, the pinned toolchain's
  `catch_unwind::cleanup` and matching drop glue run. The engine moves two opaque machine words and
  never reads std's private exception, Box or vtable layout.
- **C11** Host-thread TLS holds a nonce-carrying LIFO stack of `EngineFault` tokens that validates
  owner and consumption order only and does not decide frame cleanup. An outer fault suspended by a
  native catch may be followed by a re-entered guest panic, or by an inner fault pushed and settled
  first; the outer owner settles last, in stack order.
- **C12** A real `main` panic is consumed by guest `lang_start_internal`, so it must not be classified
  after leaving the Engine. Lowering locates the catch wrapping user `main` uniquely from the pinned
  std's MIR call graph and freezes it as `CallRole::MainPanicBoundary`; interpreter and JIT both claim
  the immediately following first-level `catch_unwind` and write it into this `run_main`'s LIFO state.
  An executable module must have exactly one such boundary whose unwind action is `Continue`, and IR
  serialization, image merging and per-function package verification all preserve and re-check it. If
  the pinned std changes shape, lowering and verification must fail loudly rather than fall back to
  function names or guessing 101.
- **C13** A native constructor is a fallible startup boundary, so a controlled MIRVM exception may be
  classified as a `Result` failure and then follow the close protocol. Native fini has entered
  non-rollback teardown, so a dedicated raw guard diagnoses guest panic, `EngineFault`, `EngineClosed`,
  foreign exception and host Rust panic uniformly and then aborts; it must not reuse the guest
  terminate guard's `EngineFault` continuation.
- **C14** The fixed 22-byte signal stub may only read fixed TLS and atomically register events: a
  process-directed event writes the owner inbox, and `SI_TKILL` writes the stable cell the target
  pthread established for this registration. It cannot attach a context, take locks, allocate, call
  libffi or guest code, or unwind. Safepoint delivery builds a brand-new activation, and a thread event
  may only be delivered on the target pthread. Activation nonces and `run_main` state must not reuse
  the interrupted execution, so a handler's catch cannot claim the outer `MainPanicBoundary`; a handler
  exception terminates at the plain C signal-callback boundary.
- **C15** The public surface is not an all-safe API. `Package::load` is a safe owned-snapshot check and
  `Package::instantiate` is `unsafe`, because bytecode verification cannot prove that an in-package
  native library, host symbols and FFI signatures agree. `run_main` is safe on an existing Engine; a
  hand-built Module and untyped raw exports are the unsafe surface, and the internal `Shared` is not
  public. Published callback, JIT, MC and native addresses may be saved by arbitrary native code and
  stay valid after close as process-lifetime code plus small owner tombstones; mirvm does not claim to
  revoke every raw pointer held by a third-party library.

## 2. Model

`extern "C"` and `extern "C-unwind"` pass machine arguments identically and differ only in exception
rules, so the two ABIs are distinguished at every boundary. A plain `C` boundary refuses exceptions: a
Rust panic escaping through it terminates the process, and a foreign exception entering Rust in the
reverse direction is undefined behaviour with no success semantics. `C-unwind` permits exceptions to
pass, so mirvm runs Rust `Drop` along the way and keeps the exception object intact, letting an outer
C++ frame catch it by its original C++ type. `std::panic::catch_unwind` only guarantees catching Rust
panic, not C++ exceptions, and the pinned toolchain terminating on a C++ exception is not a reason to
convert it into a Rust panic.

mirvm therefore must not convert arbitrary C++ exceptions into a `RunError` or a guest panic: that
would lose the C++ type, object identity and destructor responsibility, and diverge from native.
Instead, MIRVM-owned exception classes ride the existing system unwinder. Each frame's cleanup stays
with the existing Rust personality and JIT LSDA: the interpreter classifies per frame in a raw catch
of the current object, the JIT landing pad classifies the pointer the unwinder hands over, and the
frame guard only restores the operand area, shadow frames and depth. The original guest panic object
keeps belonging to guest std, which the engine neither copies nor parses, and after a guest catch or
Engine-top consumption the inner pointer goes back to the guest side for capture or release.

Exception identity is the cleanup criterion, never TLS global state. Ownership needs the class, the ABI
cookie, the canary and the `Arc<Shared>` identity test together, because an exception from another
Engine must keep unwinding and a native catch may suspend an outer fault and then re-enter a guest
panic that does need cleanup. An exception also participates in Engine lifetime: a MIRVM exception may
be suspended by a native catch with no active Engine call stack and later rethrown to its original
owner, so the exception shell holds a `DeferredHold` and Engine close waits for it to leave rather than
freeing `Shared` inside the suspended window. That hold decides object lifetime only, not frame
cleanup.

Engine execution exits are structured: `RunOutcome::Returned(value)` for a normal return,
`RunOutcome::GuestPanic` for an uncaught guest panic, and a `RunError` carrying `RunErrorKind` for a
missing entry or export, or an engine fault. A real `main` panic never leaves guest std, which is why
`MainPanicBoundary` marks the exact catch in the pinned startup chain and each `run_main` records it in
its own state stack; the CLI still maps `GuestPanic` to 101, but a normal `main` returning 101 is no
longer the same library API result. Host Rust panic and a C++ foreign exception stay distinct from
both: the former continues unchanged, the latter may cross the whole `C-unwind` Engine and be caught by
an outer C++ typed catch.

## 3. Boundaries

- A non-C/System ABI, on a direct foreign declaration or a foreign callback parameter, is rejected
  explicitly at lowering. Native does not execute such a declaration either, because calling it or
  invoking it as a callback is an ABI mismatch.
- An exception crossing plain `C` has no propagation contract: a Rust panic terminates, and a C++
  exception entering Rust in reverse is UB. A Rust panic inside a plain C callback terminates at that
  ABI boundary.
- A C++ exception or Rust panic inside a plain C wrapper — through a direct or function-pointer
  `C-unwind` call out of it — is terminated by the engine's terminate guard, because
  `UnwindAction::Terminate` outranks the callee ABI, and the outer C++ catch must not return normally.
- A C++ exception reaching a guest `catch_unwind` terminates with an explicit foreign-exception reason
  and the guest catch function must not run, matching the pinned rustc rather than disguising it.
- A C++ exception through a direct `C-unwind` call to a Rust thread root or catch runs Drop and then
  terminates, reporting a foreign exception; it is not promised that a Rust catch receives it,
  identical to native.
- C++ swallowing a Rust panic must terminate, as native does, because the rethrow is required.
- A native constructor and native fini: a controlled MIRVM exception during the fallible startup
  boundary becomes a `Result` failure and follows the close protocol, and an unwinding attempt must not
  cross fini. The dedicated raw guard catches guest panic, `EngineFault`, `EngineClosed`, foreign
  exception and host Rust panic, emits the fixed diagnostic and aborts; teardown is not rollbackable,
  so it neither propagates `EngineFault` back to the embedder nor continues closing.
- A signal frame cannot unwind through and cannot attach a context, take locks, allocate or call libffi
  or guest code; a handler exception terminates at the plain C signal-callback boundary.
- Termination diagnostics are not a stable part of the Rust ABI, so negative tests never compare native
  and mirvm stderr byte for byte. They lock a non-zero exit, an explicit termination reason, and the
  rule that a C++ handler must not swallow the exception and return normally.

The exception-class ruling: **use MIRVM-owned exception classes, but do not write an independent
personality.** The owned exception only tells the system unwinder that an object is a guest panic or
an `EngineFault` and which Engine it belongs to; how each frame runs cleanup stays with the existing
Rust personality and JIT LSDA, and the original guest panic object stays with guest std. Cleanup is
decided from the current exception object rather than thread-local state, and the nonce-carrying token
stack only guarantees that an `EngineFault` is consumed by its correct owner in LIFO order, so an
outer fault suspended by a native catch cannot pollute a subsequently re-entered guest panic or inner
fault.

## 4. Verification

```bash
make suite S=runtime.c-unwind
```

The fixture `tests/fixtures/c_unwind_contract/` first generates the native oracle with pinned Cargo,
rustc and C++, then runs the pure interpreter and the forced-sync JIT (`MIRVM_JIT=on`,
`MIRVM_JIT_SYNC=1`, `MIRVM_JIT_THRESHOLD=1`) under default cargoless. A scenario that needs published
guest machine code requires the JIT log to show `release=true` on the target function's release line
with no `release=false`, which prevents a false green where "it is called JIT but is interpreted
throughout". The thirteen items, each matching native:

- `C-unwind` exception-free return with call-site cleanup, 30,000 times: `value=42 drops=30000`.
- C++ typed exception round trip: `result=1073 caught=73 drops=1`.
- Rust panic rethrown through C++: `payload=51 caught=888 drops=1`.
- C++ typed exception out of the whole Engine: the outer C++ catches `Marker{73}`, value 73.
- C++ exception reaching a guest `catch_unwind`: it reports that it cannot catch a foreign exception,
  then terminates, and the guest catch function does not run.
- C++ swallowing a Rust panic: terminates.
- Rust panic in a plain C callback: terminates.
- A C++ throw through a direct `C-unwind` call inside a plain C wrapper: terminates, outer catch does
  not return.
- A C++ throw through a function-pointer `C-unwind` call inside a plain C wrapper: terminates.
- A Rust panic through a direct `C-unwind` call inside a plain C wrapper: explicitly terminates.
- A Rust panic through a function-pointer `C-unwind` call inside a plain C wrapper: explicitly
  terminates.
- A direct foreign call using a non-C/System ABI: rejected explicitly at lowering.
- A foreign callback parameter using a non-C/System ABI: rejected explicitly at lowering.

The fixture is part of `fast` and deliberately does not grow into a generic FFI harness.

Uncaught guest panic resource return and the real `main` result are locked separately by the standard
`runtime.semantics` unwind segment: interpreter and forced-sync JIT each run once a normal guest call
inside payload Drop followed by a second panic, and each runs once a real lowering plus
`lang_start_internal` main-panic/normal-101 contrast, which together with nine pre-existing unwind
semantics cases gives 13/13. That proves the panic count is reset on the guest side, both payloads are
destructed exactly once, the same Engine keeps executing after cleanup, and an identical OS exit code
does not erase the library API result class.

## 5. Open items

- A shape change in the pinned std must make lowering and verification fail loudly rather than fall
  back to function names or 101 guessing. The `MainPanicBoundary` contract is re-checked by IR
  serialization, image merging and per-function package verification, and those checks are what to
  extend.
- The `runtime.c-unwind` fixture stays scoped to this contract; new foreign-boundary behaviour needs
  its own cases rather than more rows here.
- A C++ exception entering Rust in reverse through a plain `C` boundary remains UB with no success
  semantics and no reopen plan.
- Explicitly not promised: prompt latency for process-directed external signal delivery, since the
  signal frame never enters libffi, guest code or unwind and only ordinary safepoints deliver handlers
  under a fresh activation; and active revocation of every raw pointer held by third-party code.
- "Default" MIRVM results never prove JIT machine code on their own, because short calls below the hot
  threshold may stay interpreted.
