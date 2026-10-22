# On-stack replacement

Status: Implementation design. This document owns transfer of an active interpreted invocation into
compiled execution. Ordinary entry ABI belongs to [frame-abi-bytecode.md](frame-abi-bytecode.md),
exceptions to [c-unwind-contract.md](c-unwind-contract.md), and admission/heat to
[jit-code-cache-design.md](jit-code-cache-design.md).

## 1. Transfer without moving guest storage

The interpreter retains ownership of the invocation's operand-region frame, depth and shadow-frame
guard. At a normal backward edge, after signal polling and before executing the destination block,
it may call a published OSR continuation. That continuation borrows the original frame and executes
the remainder of the function. The suspended interpreter returns the continuation's result without
executing another guest instruction. The original guard releases storage exactly once.

This is a transfer within one invocation: the function prologue and completed side effects are not
replayed. It is not a jump into the ordinary compiled entry, whose parameters and local storage
describe a new invocation.

All guest slots remain in the borrowed frame in the initial implementation. The continuation does
not eagerly read uninitialized or dead slots, copy the frame, relocate address-taken locals or
reconstruct pointers. Loads/stores retain their declared widths and alignment. Later promotion to
SSA must supply an entry-specific initialization/liveness proof and preserve addressable storage.

## 2. Entry and publication

One continuation per function/domain accepts the frame address and destination basic-block number.
Only backward-edge destinations are valid entries. Its return convention is the function's existing
zero/one/two-word or indirect return convention. A packed boundary uses the ordinary context/recorder
pinning machinery to enter it from host Rust.

OSR requests are deduplicated separately from ordinary entry requests and use the existing worker.
Compilation is requested only for an invocation still interpreting after sufficient loop activity.
Normal execution does not wait for publication; sparse polls continue interpreting until an entry is
available. Unavailable, rejected, stopped or failed compilation leaves the invocation interpreted.

OSR publication is separate from ordinary fast/packed slots. Instructions, callee trampolines, symbol
ranges and unwind registrations are complete before a Release publication; transfer uses Acquire.
The initial OSR entries live for the same code lifetime as other JIT entries, but are not persisted.
Their distinct profiler roles must not introduce a second guest backtrace frame alongside the
interpreter shadow frame that still owns this invocation.

## 3. Generics and code domains

An exact generic instance may use its specialized body because specialization preserves the shared
activation's ABI, frame layout and block numbering. Existing dictionary and local-pointer slots stay
at their original addresses. Transfer does not re-run argument installation or rebuild the frame.

Plain and trace continuations have separate publication slots. A trace continuation is entered only
through the recorder-pinning boundary of the active trace domain. OSR does not start/stop capture or
migrate an activation between domains.

## 4. Unwind and ownership

Transfer occurs with no pending interpreter cleanup edge and never starts inside a cleanup chain.
Compiled cleanup pads own exceptions raised after transfer, including guest Drop and foreign
exceptions; EngineFault keeps its existing bypass rules. The suspended interpreter must not repeat
cleanup already performed by the continuation. Its outer guard still restores region, depth and
shadow state on both return and unwind.

Keep ordinary stack-headroom checks, return/backedge signal safepoints and registered unwind metadata.
A continuation must not be entered without enough host-stack headroom. Function-pointer identity
continues to refer to the ordinary guest entry, never to an OSR continuation.

## 5. Acceptance

- Prove transfer during one long invocation, with a side effect before the loop executed exactly
  once, and compare the result with native and interpretation.
- Cover multiple/nested loop entries, narrow scalars, pairs, indirect aggregates, alignment and
  pointers to live locals across transfer; include exact generic instances.
- Exercise normal return, panic cleanup, EngineFault propagation and caller-frame restoration.
- Cover both code domains, publication failure and concurrent request deduplication; identify OSR
  code in CPU profiles.
- Measure full workloads and short controls against the same build with OSR disabled. Report
  additional compilation/code cost as well as saved interpretation. A successful transfer alone
  does not establish a latency improvement.

Persistent OSR code, entry-specific scalar promotion, speculative deoptimization and code-domain
migration require separate extensions; none is implied by the initial transfer contract.
