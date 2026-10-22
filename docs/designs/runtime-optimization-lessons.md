# Runtime optimization lessons

Status: Design guidance. This document owns the lessons drawn from HotSpot and V8 and how MIRVM
selects work from them. Execution contracts belong to the linked designs; measurements belong to
[current-status.md](../current-status.md), and unfinished work to [open-issues.md](../open-issues.md).

## 1. Separate compilation cost from generated-code quality

V8's [Sparkplug](https://v8.dev/blog/sparkplug) emits machine code directly from bytecode with limited
local optimization. [Maglev](https://v8.dev/blog/maglev) uses a small SSA pipeline between baseline
execution and peak optimization. The typed-bytecode [Liftoff compiler](https://v8.dev/blog/liftoff)
is also relevant to MIRVM: it avoids constructing a general optimization IR and maintains a simple
register/stack state while generating code.

MIRVM's Cranelift `speed_and_size` and `speed` configurations do not establish two substantially
different compilation-cost tiers. Measure translation, register allocation, generation, linking and
publication separately before choosing a cheaper pipeline or another backend. A fast baseline is
justified by lower end-to-end cost on real workloads, not by its name or an earlier threshold.

## 2. Remove intermediate state across calls

HotSpot combines inlining and escape analysis to eliminate scalar-replaceable allocations and
copies ([HotSpot performance guide](https://docs.oracle.com/en/java/javase/14/vm/java-hotspot-virtual-machine-performance-enhancements.html)).
For MIRVM the analogous opportunity includes stack aggregates: after bounded inlining, a temporary
value may reduce to scalar operations instead of stores, calls, copies and reloads.

Use the existing MIR inlining and scalar SSA machinery first. Inspect remaining aggregate and helper
boundaries in generated code. Preserve observable addresses, unknown-call effects, aliasing,
volatile/atomic semantics, panic cleanup and code growth limits. Rust's known type/layout facts can
justify optimizations without introducing speculative type assumptions or deoptimization.

## 3. Observe execution after compilation and price the queue

HotSpot's [compilation policy](https://github.com/openjdk/jdk/blob/master/src/hotspot/share/compiler/compilationPolicy.hpp)
uses invocation/backedge feedback, continues profiling in selected compiled tiers, adjusts admission
under compiler load and prioritizes active work while removing stale queued tasks.

Historical heat, current activity and expected compilation payback are distinct. A published entry
must not disappear from history merely because it bypasses interpreter counters. Any additional
compiled-code sampling needs an explicit overhead budget. Charge queueing and publication delay as
well as compilation: work already spent interpreting cannot repay a transition. Entry replacement
benefits later calls; [OSR](osr-design.md) benefits the invocation already executing.

## 4. Reuse each preparation product under its own identity

V8's [JavaScript code cache](https://v8.dev/blog/code-caching-for-devs) primarily reuses parsing and
bytecode compilation. HotSpot's [AOT cache](https://docs.oracle.com/en/java/javase/24/docs/specs/man/java.html#ahead-of-time-cache)
reuses prepared runtime structures; [AOT method profiling](https://docs.oracle.com/en/java/javase/25/migrate/significant-changes-jdk-25.html)
also makes previous execution feedback available at startup. These are different products from
persisted optimizing machine code.

MIRVM keeps dependency templates, prepared launch state, execution feedback and native code separate.
An installed generic dependency cannot predict every consumer's type arguments. Reuse the work whose
inputs are known and retain the exact-instance work for binding or execution. The owners are
[distribution](distribution-design.md), [runtime generics](runtime-generics-design.md) and
[JIT persistence](jit-code-cache-design.md). A hit is useful only if validation, decoding, linking
and publication together cost less than the work avoided.

## 5. Make activation state an explicit contract

Sparkplug's interpreter-compatible frames simplify transitions and stack tooling. Maglev records
state mappings for deoptimization. MIRVM additionally exposes real Rust addresses to guest and
native code, so changing tiers must preserve the identity of live addressable storage.

OSR is an independently validated mechanism under [osr-design.md](osr-design.md), prioritized before
further admission-policy tuning. It must demonstrate a single invocation entering compiled execution
without restarting, preserve exceptions and code-domain identity, and measure transfer/polling cost.
Speculative deoptimization and migration between plain/trace activations are separate work.

## 6. Evidence required for an optimization

Use native output agreement and fresh-process controls with prepared images, empty/populated JIT
stores and preparation misses distinguished. Separate CPU samples from queue/wait time and operation
counts. Attribute both the guest and compiler threads, and keep profiling outside latency runs.
Reduced fixtures prove mechanisms; complete workloads establish payback. Preserve mixed or negative
results instead of substituting a favorable phase or cache-hit count for process latency.
