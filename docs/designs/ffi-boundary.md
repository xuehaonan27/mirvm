# The FFI boundary (the edge of the abstract machine)

> Status: Contract · Scope: how a guest's foreign call is classified — served by a VM handler, passed
> through to the system libc, or not a call at all — where mirvm may interpose, where security lives,
> and what native code may do to memory mirvm handed it. Exception flows are normative in
> [c-unwind-contract.md](c-unwind-contract.md); by-value aggregates in
> [c1-ffi-agg-design.md](c1-ffi-agg-design.md); the archive closure gap in
> [c2-rlib-symbols-design.md](c2-rlib-symbols-design.md); signals in
> [modeb-mirvmar-design.md](modeb-mirvmar-design.md).

## 1. Contract

1. **FFI is the boundary of the abstract machine.** Inside it mirvm implements RAM semantics; outside
   it RAM models nothing and mirvm only hands control over (a foreign call) or receives it back (a
   thunk). Native allocation and the behaviour inside a native library are outside RAM.
2. **A guest foreign call is always visible; native code's own calls are not.** Guest code is always
   interpreted or compiled by mirvm, so every call it makes crosses a boundary mirvm stands on.
   "Passthrough" means the handler forwards the work to the real OS — from there on a library's own
   `malloc`, `memcpy` and syscalls are machine code reaching the real libc: invisible, outside RAM, and
   deliberately not interposed on.
3. **Every foreign call is classified once**, as handler-served (a RAM builtin), passthrough, or inline
   asm.
4. **No broad interposition on native code.** mirvm interposes only at the guest's foreign-call boundary
   and inside the native archives it produces itself, and only at entries with a stated lifecycle
   contract.
5. **Security belongs to a process-level OS sandbox, not to this layer.** seccomp-bpf and namespaces
   around the whole process catch every syscall no matter which layer issued it, so mirvm does not
   re-invent security filtering. mirvm-level hooks exist for virtualization an OS sandbox cannot
   express.
6. **Unimplemented semantics trap.** A foreign call mirvm can neither serve nor pass through fails
   loudly; it never returns success in a way that fakes support.

## 2. Model

### 2.1 Handler-served: RAM builtins

- **Intrinsics** are part of RAM computation and natively understood by the VM — the analogue of a JVM
  bytecode instruction.
- **Allocation.** The motive for intercepting allocation is ownership plus metadata.
  `__rust_alloc`/`alloc_zeroed` is the Rust allocator (compiler-synthesized, no MIR), so it is
  intercepted at the MIR foreign-call boundary and lands in the managed Rust Heap, where the interpreter
  owns whatever bookkeeping it needs to dereference a guest pointer. `libc::malloc`/`calloc`/`free` and
  their relatives go straight to the real libc and land in the Native Heap: a guest `malloc` pointer
  must survive being handed to C `free`, so those bytes must not come from the Rust Heap. "Allocation
  has one entry point" therefore means the managed heap's entry point; native allocation is a second,
  real-libc path rather than an exception inside the first ([ram-spec.md](ram-spec.md) §2.2).
- **Unwind** reuses the host panic machinery and unwinder. Which exception may cross which boundary,
  and what cleanup runs on the way, is [c-unwind-contract.md](c-unwind-contract.md).
- **Threads** are real OS threads — not an emulation and not a pthread wrapper — and the interpreter's
  thread start routine is materialized as a thunk when a pointer to it escapes
  ([concurrency-arch.md](concurrency-arch.md)).
- **Signals** cannot go through a libffi closure: every guest registration materializes a fixed RX stub
  whose kernel frame does only TLS reads and an atomic registration, and the guest handler runs at a
  safe point in a fresh activation ([modeb-mirvmar-design.md](modeb-mirvmar-design.md) §2.7).

### 2.2 Passthrough: real resources

Calls that touch a real resource and involve no interpreted entity — file descriptors, clocks,
randomness, the math library — pass through to the system libc. Library loading and symbol resolution go
through the platform primitives (`os::dll`: `dlopen`/`dlsym`/`dlerror`, with `RTLD_DEFAULT` named rather
than passed through), and the syscall family has a single varargs entry (`os::process::syscall`). A
guest's direct foreign call takes the generic channel: `dlsym` plus libffi under a frozen signature,
with resolution ordered link-time binding (the archive's hidden-symbol fallback table) → `RTLD_DEFAULT`
→ each open handle, and variadic calls using libffi's variadic CIF with the trailing argument classes
frozen at the call site. `target == host` is what makes this cheap: the ABI is bit-identical, so there
is nothing to translate. Loading discipline belongs to
[distribution-design.md](distribution-design.md); the archive case to
[c2-rlib-symbols-design.md](c2-rlib-symbols-design.md).

### 2.3 Inline asm: not a call

Inline asm is opaque machine code in the middle of a function, with no symbol and no call boundary, so
it can never be proxied at one place. There are two options and no third: simulate the specific
template, or intercept the enclosing function by name. The VM's encoder must know the difference — a
foreign call has a boundary and inline asm does not — which is why an unknown template is a trap rather
than a silently mis-encoded call.

### 2.4 Native code writing guest memory

A C library that mirvm has handed a buffer is real machine code: its internal `malloc`/`memcpy` reach
the real libc and are not intercepted, by design. The boundary is that one FFI call, and all mirvm owns
is the buffer it passed; real addresses make the library write straight into guest memory, and the
interpreter sees those writes because they are ordinary host writes. Miri has the same limit from the
other side — native code that allocates its own memory and returns a pointer for interpreted code to
dereference cannot work, since the checker has no metadata for it. Compute-shaped C libraries do not do
this, and are painless.

### 2.5 Where mirvm may interpose

Interposition happens at the guest's foreign-call boundary and inside mirvm's own native archive bridge,
and only at entries with a stated lifecycle contract. Past those entries mirvm does not pretend to see
a third-party library's internal state:

- A guest call to `pthread_create` (from std or a bare `extern "C"`) registers a one-shot deferred hold
  and then calls the real libc; the hold is cleared when the thread entry starts or creation fails.
- A function pointer obtained from guest `dlopen` and called directly goes through FFI; a callback that
  points at interpreted code goes through a thunk.
- A pthread call from inside an archive mirvm produced is redirected through a hidden slot onto the same
  lifecycle bridge; the thread is still a real native thread.
- A guest or own-archive call to `signal`/`sigaction`/`raise` enters a process-level signal registry
  that records the owning Engine. mirvm's archive wraps exactly those three known symbols and does not
  extend that to arbitrary third-party dynamic libraries.
- A third-party library that keeps a callback without a completion or revocation event leaves mirvm
  unable to know when the address may be released. The closure is then retained to process end, and
  after `close` only a stable close identity remains — an honest boundary, not a solved problem.

## 3. Boundaries

- **Not a sandbox.** Real addresses, native FFI and inline asm together mean guest UB can corrupt VM
  state; isolation is structural, and checked mode is a reserve rather than a claim
  ([concurrency-arch.md](concurrency-arch.md) §3.2).
- **Not the security layer.** seccomp-bpf and namespaces own security. mirvm-level hooks exist for
  virtualization an OS sandbox cannot express: a fake filesystem, path redirection, resource metering,
  and telling a guest `open` apart from the VM reading its own cache.
- **Not a generic interposition framework.** Broadly intercepting native code is an engineering
  disaster (P4 in [README.md](README.md)); the boundary is cheap precisely because real addresses make
  that interposition unnecessary.
- **Not tracking native allocation metadata.** The Native Heap is libc's, and mirvm keeps no
  per-allocation table for it ([ram-spec.md](ram-spec.md) §2.2).
- **Not free of gaps.** Synchronous fault signals, realtime signals and the advanced `sigaction` flags
  are refused loudly rather than accepted silently, and the residual boundaries are registered in
  [open-issues.md](../open-issues.md) (R1, R21).

## 4. Verification

- The **differential corpus** is the verdict for foreign behaviour: `ffi_libc` and `ffi_agg_probe`
  (native-diff), `ffi_zlib` (cargo-diff), the `c-unwind` contract case, and the `vmcall` cases for the
  reverse direction, where a native caller enters the VM through an exported entry.
- The **absence of a second interposition site** is what `repo-quality`'s `platform boundary` gate
  checks: no `libc` constant or protocol function, no `asm!` site, no host `cfg` and no
  `std::os::unix` outside the three axis trees, so a new raw foreign touch point cannot appear
  silently.
- What each of these has actually reached is in [current-status.md](../current-status.md).

## 5. Open items

- **A single generic passthrough channel** is the intended end state; today the entries are enumerated
  where they are served. That is polish, not a hole.
- **Callback revocation** has no universal mechanism; process-lifetime retention plus a close tombstone
  is the accepted boundary ([modeb-mirvmar-design.md](modeb-mirvmar-design.md) §3).
- **Signal-boundary residuals** — synchronous faults, realtime signals, advanced flags, safe-point
  latency for process-directed events — are registered in [open-issues.md](../open-issues.md) R1/R21 and
  reopen when their mechanisms land.
