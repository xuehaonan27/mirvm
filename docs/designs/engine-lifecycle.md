# Engine Lifecycle

> Status: Decided RFC · Scope: an Engine's holds, its phase machine, the two teardown contexts it can
> be ended by, and how engine-owned per-thread state survives a non-local exit. Guest threads'
> mapping to OS threads is [concurrency-arch.md](concurrency-arch.md)'s; how an entry reaches
> per-thread execution state is [vmctx-passing.md](vmctx-passing.md)'s; what a panic or a foreign
> exception may cross is [c-unwind-contract.md](c-unwind-contract.md)'s.

## 1. Contract

1. **A guest thread is the guest's.** A thread the guest created belongs to the guest, and the way to
   wait for one is the guest's own `pthread_join`. Engine teardown is never a join over the guest's
   threads.
2. **Two teardown contexts, never one.** A guest process exit follows native exit: the guest's exit
   handlers run, the process ends, and nothing waits. An embedding close reclaims the Engine while
   the host process continues, and only reclamation requires quiescence. The two share no code path
   that waits.
3. **One owner for the phase machine.** `EngineControl` owns every phase transition, and the phase
   word is private to it. A subsystem that needs a transition asks for it by name.
4. **One count, three named holds.** Every reason an Engine must stay alive until something finishes
   is one of three holds with its own contract, and every hold is acquired through its own name
   rather than through the raw count.
5. **A non-local exit is recoverable, not fatal.** The host stack can be crossed without running Rust
   destructors, so every engine-owned per-thread stack must be restorable from a checkpoint its own
   frame recorded.
6. **Reclamation never races guest code.** An Engine's memory, images and compiled code are released
   only when no thread is inside guest code, and that is the entire reason reclamation waits.

## 2. Holds

An Engine must not be finalized while any of these is outstanding.

- **`ExecutionLease`** — guest code is running now on this thread. Held across one guest entry:
  `run_main`/`run_export`, a thunk that re-enters the VM, a signal handler's short execution. This is
  the hold that makes every guest frame safe, so it spans the whole entry and not one step of it.
- **`DeferredHold`** — native code has accepted a guest callback and has not yet invoked or revoked
  it. It covers the gap between acceptance and the callback's own lease, during which no guest code
  runs but the callback must still be deliverable.
- **`FinalizerPermit`** — the finalizer is between quiescence checks. It is the only hold that exists
  inside Closing, and it blocks the seal, not close.

Executions and deferred holds share one count. The reason is the `Running -> Closing` transition: a
registration that read Running and then incremented must not be able to land after close scanned,
and one atomic is what makes that impossible. The permit is a count of one taken only from a fully
idle Engine, which is what makes the signal-disposition teardown a quiescence point rather than a
race.

Holds are acquired by name — `ExecutionLease::for_thunk`, `ExecutionLease::for_registered_callback`,
`DeferredHold::acquire`, `FinalizerPermit::acquire` — and each name carries its own admission rule.
A boolean parameter that lets a caller opt into entering a Closing Engine is not an interface: the
rule belongs to the kind of entry, and the kind is what the entry point is called.

## 3. Phases

```text
Running ──close──> Closing ──seal──> Finalizing ──finish──> Closed
```

- **Running** — every entry is admitted.
- **Closing** — no new unregistered entry is admitted. Registered callbacks and deferred holds still
  are, because close must not strand a callback native code already owns. Close work begins here:
  TSD registrations are marked closing and the current thread's deferred callbacks drain.
- **Finalizing** — the seal. Reached only from a fully idle Engine, with no live signal registration
  and no pending signal work, and it admits nothing at all. Signal dispositions have run and the
  deferred registry is proven empty.
- **Closed** — `Shared` is released, the engine registry row is gone and the waiters are woken.

`Finalizing` is the point of no return, and it is deliberately hard to reach: every phase between
Running and it exists so that work which was admitted while the Engine was still Running can finish
under its own lease. The seal is taken by the finalizer as part of signal teardown, because signal
dispositions and pending deliveries are the last thing that can start new work.

## 4. The two teardown contexts

### 4.1 Process exit

A guest `main` returning ends the process, and it is the route mirvm owns: `run_main` runs the
guest's `atexit` callbacks and reports the status, and the Engine's own exit-time work follows. A
guest that calls `exit` itself reaches the platform's `exit` as a passthrough, so the process ends
inside that call rather than here.

Nothing waits, and nothing is reclaimed. A guest thread the guest created and did not join dies with
the process, exactly as under native, because that is what the guest asked for: native `exit` does
not join the process's threads either, and an address space about to be destroyed needs no
reclamation. Making the exit path wait on quiescence would invent a step native does not have, and it
would convert a legal native program — one that leaves a worker pool parked — into a hang.

Guest exit handlers are part of this context, not of reclamation: they are the guest's own exit
semantics and they run under an ordinary activation, in guest context. So is the image finalizer list
and the `__cxa_atexit` registrations an image's initializers made, which native runs at exit too.

### 4.2 Embedding close

An embedder closing an Engine is asking for its resources back while the host process continues.
`Shared`, the loaded images, the mappings and the JIT's code must actually be released, and a later
Engine may reuse those addresses. Reclamation therefore requires that no thread is inside guest code.

`close()` requests and returns. `wait_closed()` waits for quiescence, and it is the embedder's
contract that this wait can be long: if the guest left a thread inside guest code, the host is asking
to release memory a live guest frame is using, and the host owns that thread. A bounded wait is not
available, because a timeout cannot make concurrent reclamation safe — it would replace a hang with a
use-after-free.

## 5. Non-local exits

The host stack can be crossed without Rust destructors running: a foreign `longjmp` (a Lua error
raise out of a callback, a wasm trap), a C++ exception, or a Rust panic escaping through a
`c-unwind` hole. The frames crossed are not unwound; they are abandoned.

mirvm keeps per-thread stacks that its own frames push onto, so the rule is one rule:

> Every engine-owned per-thread stack records a checkpoint at the frame that pushes onto it and
> truncates or restores to that checkpoint on exit. No such stack aborts because it is deeper than
> the frame that is returning.

A frame that is returning cannot be wrong about the state before its own push, and entries above it
belong to frames that no longer exist — either they popped their own entry, or they were crossed. An
abort stays reserved for state that is inconsistent while every frame is still live, such as a count
that underflowed.

The stacks that carry a checkpoint are the activation stack (truncate to the length recorded at
entry), the nested main-run states (truncate to the index the run pushed), the interpreted shadow
frames and their depth (restore the values recorded at entry), the callback lease registry (below),
and the per-thread signal mask (restore through its own guard). A stack that a guest callback can be
entered and abandoned across and that carries no checkpoint is a defect, not a variation.

The callback lease registry has no frame of its own to truncate at, because the callback's frame is
the abandoned one. Its checkpoint is the guest-to-native call site in `call_addr`: once that native
call has returned, no callback frame it entered can still be live, so any entry left on the registry
belongs to a crossed frame and is released there. A callback entered on a thread with no guest-to-
native frame has no such point, and its entry is released by that thread's own exit.

The in-flight fault tokens are the one stack that keeps an ordering check rather than a checkpoint.
An entry above a returning frame can still be live there: a native catch may suspend an unwind and
re-enter the Engine, so the token a boundary consumes not being the innermost is a real defect rather
than the signature of a crossed frame. A foreign non-local exit crossing a *live* fault is not a case
the two can share, because the fault is mirvm's own and propagates by host unwinding.

## 6. Mirvm's own threads

Mirvm starts threads the guest cannot see, and the fork guard measures the guest by subtracting them
from the platform's thread count. The set it must subtract is "threads the guest could not have
created", and a thread is excluded by registering as a service thread unless it is already excluded
by being inside the baseline:

- a service thread alive when the guest's fork baseline is pinned is already inside the baseline, so
  subtracting it again would be wrong; the JIT compile worker is this case;
- any other engine-owned thread registers, so the subtraction stays exact; the telemetry capture
  writer and the close worker are this case, and the close worker needs it because an embedder can
  close an Engine while guest threads are still executing.

A new engine-owned thread has to be classified into one of the two before it is spawned.

## 7. Deliberate refusals

- A bounded wait on `wait_closed` — a timeout cannot make reclamation safe.
- Waiting on guest threads at process exit — native does not, and the guest owns its own joins.
- Reclaiming the Engine on the process-exit path — there is nothing to reclaim, and doing it would
  require the wait that §4.1 refuses.
- A separate count per hold — the `Running -> Closing` race is closed by one atomic, and splitting it
  would reopen it.
- Aborting on a skipped frame — the C-level semantics that skip it are ones the guest asked for.

## 8. Verification

- `make smoke` and the corpus: `threads_exit_parked` is the witness that §4.1 holds — a guest that
  returns from `main` with a worker parked in guest code must exit with native's status and output,
  and `atexit` and `signal` cover the exit handlers that still have to run.
- The tsan run's `guest-threads` and `engine-close-race` cases: the embedding contract of §4.2,
  including a close that waits for a guest thread still inside guest code.
- The library suite's engine-lifecycle cases: hold admission per kind, phase transitions, the seal,
  and a callback abandoned by a foreign `longjmp` leaving no lease and no activation behind.
- `c-unwind` cases cover the panic-through-FFI half of §5.

## 9. Open items

1. **A guest thread that exits inside an active Engine call.** Aborting there is not obviously
   native-faithful — native `pthread_exit` also abandons its outer frames — but the guest's own TSD
   and stack accounting would be the ones abandoned with it.
2. **Diagnosing a long §4.2 wait.** A close that is waiting while a guest thread sits inside guest
   code is legitimate and indistinguishable, from the Engine's side, from one that will never finish.
   A diagnostic that names the outstanding hold kinds is the honest answer; a deadline is not.
3. **An activation boundary reading a mask a crossed handler left set.** The mask itself is
   restored by the outermost handler's own guard, so it is never wrong for long; but an activation
   that returns before that guard does reads a nonzero mask and defers work that was already
   unblocked.
4. **`atexit` ordering across the two registries.** Native runs `atexit` and `__cxa_atexit`
   registrations in one LIFO order; the engine runs its guest callbacks and its native ones as two
   lists. Only a program that registers both and observes their relative order can tell.
5. **A guest `exit` skipping the engine's exit-time work.** `exit` is a passthrough, so a linked C
   library's `__cxa_atexit` destructors — which the engine holds in its own registry — do not run on
   that route, where native would run them. A guest `main` that returns does not have this gap.
