# Frontend boundary and preparation cost

> Status: Research and proposed direction. Owns the choice of semantic frontend, the compiler/runtime
> handoff, and the experiments needed to change that boundary. No replacement frontend is selected.
> [Current measurements](../current-status.md#frontend-research) own timing results;
> [D7](../open-issues.md) owns unfinished implementation work.

## 1. Finding

The largest measured preparation cost is obtaining and materializing a closed executable program,
especially the std startup/panic closure. Parsing the user's source and reaching `after_analysis`
account for much less time in the measured small programs. This does **not** mean rustc becomes free
after analysis: MIRVM lowering triggers substantial metadata, layout, instance-resolution and
constant-evaluation queries inside rustc.

The next experiments should preserve rustc's semantic services while changing when their products
are produced, reused and loaded. A dedicated frontend remains an option if a measured bottleneck or
a separately chosen language contract justifies its cost. Source interpretation alone does not
remove semantic analysis or the need to implement the supported Rust library and ABI behavior.

## 2. Actual pipeline

```text
source / Cargo inputs
  -> resolve and compile missing dependency metadata
  -> validate input identity; try bound execution image
       hit -> restore instance -> Engine
       miss -> load available base/dependency layers; try program delta
            -> rustc parse/expand/resolve/typeck/borrowck
            -> after_analysis callback
            -> construct a missing canonical base and load compatible dependency layers
            -> collect mono seeds; MIRVM closes over guest calls/constants/vtables
            -> instantiate MIR, resolve layout/ABI, freeze bytecode/data/native recipes
            -> publish reusable products and finish compiler diagnostics
            -> destroy compiler session -> instantiate/run Engine
```

Relevant ownership boundaries:

| Owner | Current responsibility |
|---|---|
| `src/cli/driver.rs` | Early bound-image lookup, layer preparation, rustc callback, program publication, compiler finalization, Engine entry |
| `src/cli/base_image.rs`, `src/image/base.rs` | Canonical std construction in the user session, isolated preparation, persistent base validation |
| `src/cli/cargo.rs`, `src/cargoless/schedule/args.rs` | Target dependency metadata with MIR, host build scripts/proc-macros, required mono diagnostics and assembly extraction |
| `src/lower/collect.rs`, `src/lower/mod.rs` | Native codegen's mono seeds plus MIRVM's guest worklist closure, runtime roots, frozen data and image placement |
| `src/lower/func/mod.rs`, `src/lower/frame.rs` | Clone/instantiate/normalize each MIR body, derive layout and calling convention, emit owned bytecode |
| `src/image/`, `src/pack/`, `src/depinfo.rs` | Persist and validate compiler-independent products; several files also contain compiler-side extraction/writing |
| `src/vm/` | Execute owned bytecode after the compiler has finished |

The root callback returns `Compilation::Stop` before native code generation. Target dependencies
already use `-Zalways-encode-mir` and `-Zno-codegen`; host build scripts and proc-macros need executable
host artifacts. Replacing LLVM code generation is therefore not the main root-session opportunity.
The explicit dependency mono pass also preserves compile-time errors which simply stopping early
could suppress (`src/cli/cargo.rs::DepCallbacks`).

The driver ledger's `frontend` runs from driver entry through analysis, including pre-session cache
loading. On a miss, `base-build` follows analysis and precedes program `lower`; both lowering phases
include the rustc queries they demand. These fields describe work phases, not clean module boundaries.

Canonical base construction, identity and immutable ownership are specified by
[startup-cache-design.md](startup-cache-design.md#std-base-loading). Loading, merging and
bound-image publication preserve demand decoding. The user session builds a missing base and reuses
its mapping and archive while lowering the program delta.

## 3. Interpreting the evidence

The isolated-builder reference uses a release binary for cache-state comparisons and an external
callback probe linked to the same release library for compiler phases. The probe stops after analysis, mono collection, or existing MIRVM lowering; it separates
three timing runs per mode from a self-profile capture. `perf` captures both lowering probes and a
complete cold fib process, including its base child. Instrumented captures explain attribution;
they are not substituted for ordinary wall-time samples.

The empty program has ten native mono items but its MIRVM base has thousands of functions. This
difference does not prove that those functions can be deleted. Native codegen can call non-generic
dependency code already compiled into libraries; a metadata-only guest sysroot instead requires
MIRVM to supply that execution closure. Panic cleanup, address-taken functions and vtables extend
the roots beyond directly executed calls. The existing `--vm-call` selection happens at Engine
entry and does not narrow the frontend's roots.

Self-profile separates MIRVM's outer lowering activity from nested rustc queries. Its self time
includes uninstrumented MIRVM operations, allocations and other library work; it is not a precise
breakdown of bytecode emission alone. Query **self** times can be compared without counting nested
queries twice. Inclusive layout/instance-resolution times overlap and must not be summed.

The base-bypass experiment is an attribution control, not a candidate patch's benchmark: it disables
bound execution caching and the samples record no successful program-cache publication. A proposed
single-session route must include publication, a subsequent hit and cross-program reuse before a
speedup is accepted. Prebuilding a base moves work into release production; this is the intended
installed-product boundary, while reconstruction cost remains relevant to development and custom
profiles. The distribution and project-installation contract is owned by
[distribution-design.md](distribution-design.md#27-installed-execution-products). Measure first run
with installed products separately from an empty-cache reconstruction.

The shipped telemetry v0 records guest syscall events with `CLOCK_NONE`. It cannot attribute host
compiler time. The phase ledger, rustc self-profile and native CPU stacks are used for that scope;
a future phase trace should distinguish base loading/building, analysis, lowering, publication and
compiler teardown without changing guest event semantics.

## 4. Alternatives

| Direction | Work it can avoid | Principal cost or limitation | Assessment |
|---|---|---|---|
| Prebuilt base plus cheaper/lazier base consumption | Std lowering on first program; repeated owned-IR reconstruction | Compatible build/profile variants, installed-artifact lifetime, immutable ownership and address identity | Immutable consumption implemented; release delivery pending |
| Single-session miss path with reusable std products | Separate empty-main compiler session, intermediate base write/read and duplicated query work | Deterministic std ownership, custom allocator/profile differences, publication and cross-program reuse | Implemented; compare complete cache states |
| Runtime executable/library separated from compiler dependency | Loading rustc/LLVM on cache/package hits | Compiler-worker startup and artifact transfer on misses; extraction of mixed modules | First runtime-boundary experiment |
| Persistent generic semantic IR, specialized by MIRVM | Some repeated rustc metadata decoding and semantic-query work | New generic type/substitution/layout/const/vtable machinery and versioned dependencies | Larger research project; require evidence from edited real crates |
| Custom rustc backend/plugin | A different artifact-emission integration point | Most frontend and semantic-query work remains; driver dependency may still load LLVM | Useful integration choice, not a demonstrated latency fix |
| Independent frontend or source interpreter | Rustc implementation costs which the replacement actually avoids | Rust semantics, diagnostics, macros, const evaluation, type layout, ABI and std compatibility | Feasible scope must be stated; no current full-compatibility speedup demonstrated |

A compiler worker can initially be a short-lived subprocess, keeping compiler finalization and guest
execution separated. A persistent worker is a distinct experiment: it adds invalidation, retained
memory and session/global-state lifecycle questions. Keeping one live `TyCtxt` during arbitrary guest
execution would reopen the current lifecycle contract and is not implied by demand loading.

MIR remains a useful semantic extraction point: types and control flow are explicit, and desugaring,
borrow checking and drop handling are available. Its pretty-printed text is not a serialization
contract. A generic semantic artifact would need owned identities and types, constant graphs,
substitutions, drop/vtable information, layout/ABI rules and dependency versions; serializing raw
`TyCtxt` references or parsing MIR text does not supply those contracts.

### 4.1 Runtime generics

Runtime generics are feasible, but concrete type information and specialized function bodies are
different products. A shared body can avoid cloning code for every type argument while still needing
that type's size, alignment, layout, drop behavior and trait implementations. Rust's source semantics
do not require MIRVM to use native rustc's eager code-generation strategy.

Target dependency rlibs already retain generic MIR. The current root session instantiates each
required body in `lower::func::lower_instance`, freezes its frame and calling convention in
`lower::frame`, and closes over callees before execution. Archive demand decoding only delays
decoding an already-lowered concrete body. The existing JIT likewise consumes concrete bytecode;
it has no generic type resolver.

| Candidate | Deferred or shared work | Main cost |
|---|---|---|
| First-call specialization | Lower a concrete generic instance only when its entry is called, then cache it | First-call stalls, access to semantic services, stable lazy entries and concurrent publication |
| Shared generic bytecode | Emit one parameterized body and supply concrete type/trait dictionaries at calls | Parameterized bytecode, instance descriptors and adaptation to concrete call boundaries |
| Shared body plus hot specialization | Execute cold instances through the shared body; specialize hot instances for the existing concrete JIT path | Both execution forms, adaptation and a measured specialization policy |

With first-call specialization, repeated calls can use the resolved entry without rerunning type
resolution. The savings come from bodies never demanded by that run, or from moving work beyond
its first useful result. If most bodies execute, compilation service overhead can increase total
time. Starting a compiler session per function is unsuitable; retaining `TyCtxt` across arbitrary
guest execution would change the current lifecycle contract. An isolated compiler service can keep
compiler state out of Engine, but must pin a coherent input snapshot and solve transfer, failure,
reentrancy and lifetime rules. An owned generic artifact avoids runtime rustc queries only if it
also supplies the semantic machinery those queries currently provide.

A shared body could receive a hidden dictionary containing type layouts, value operations and
resolved trait methods. Parameters of unknown layout can be passed by address with an explicit
return destination; this does not inherently require boxing each value or changing Rust's memory
layout. The generic frame still needs dynamically computed sizes/offsets. Associated types, const
parameters, enum niches, drop glue and indirect calls require owned representations and operations,
not just `size_of(T)` and a method pointer. Equal size/alignment alone cannot establish that two
instances may share specialized code. Preserve concrete ABI adapters at native and function-pointer
boundaries; Rust generic bounds need not be representable as source-level `dyn Trait` objects.

Type/trait descriptors may be computed at installation or first use and cached; hot specialization
can fold descriptor values into constants. This is an architectural inference from established
runtime techniques: Swift documents type metadata and value/protocol witness tables, while CoreCLR
documents canonical shared code with per-instantiation dictionaries. CoreCLR's documented sharing
of reference-type instantiations does not directly cover Rust's arbitrary value layouts. These are
feasibility references, not evidence of a MIRVM speedup.

Semantic rejection stays before program entry. `cli::cargo::DepCallbacks` explicitly runs mono
collection to preserve required const-evaluation errors even with `-Zno-codegen`; root collection
has related obligations. Deferring a body must not hide an invalid instantiation in an unexecuted
branch or report it after guest side effects. A candidate may retain eager validation and concrete
signatures while deferring bytecode emission, but then cannot claim all monomorphization/query cost
as saved. Lazy body publication also needs stable function entries, immutable verified products,
recursive/concurrent initialization rules and fork/unwind behavior; existing fixed function tables
and frozen domains do not acquire those properties from a `OnceLock` alone.

The [runtime-generics census](../current-status.md#runtime-generics-evaluation) finds unentered
concrete bodies and concentrated executed work with an installed std base. It supports a bounded
prototype, not a production speedup claim. Existing fragment deduplication already removes some
storage duplication, and direct MIR substitution is only part of the measured lowering work.
Runtime entry coverage is a cost estimate, never a correctness root-pruning rule.

The selected direction and integration gates are owned by
[runtime-generics-design.md](runtime-generics-design.md). A bounded experiment exports an actual
generic MIR loop to an owned template, supplies concrete layout/const descriptors, and executes
shared or specialized instructions through the existing interpreter/JIT after compiler teardown.
Its separate witness experiment covers native Clone/Drop and C-unwind. The
[prototype measurements](../current-status.md#runtime-generics-prototype) establish execution
feasibility and show that short/native-call-heavy work can lose when promoted. They do not establish
a serde/flate cold-start improvement or general guest trait/unwind coverage. Full frontend savings
still require production eligibility/binding, eager fallback and reuse of validated installed
products. OSR is tracked separately under T13.

## 5. Proposed handoff

Retain rustc as the initial semantic frontend. Treat a validated, owned executable artifact as the
runtime boundary; make frontend implementation replaceable behind it. The existing bytecode,
archive, relocation and input-manifest contracts are the starting point, not a second parallel
format. The runtime must be able to load a valid artifact without loading compiler libraries.

The compiler side owns analysis, generic instantiation, layout/ABI extraction, constants, diagnostics
and input-dependency collection. The runtime side owns artifact validation, native materialization,
per-Engine state, execution and JIT. Diagnostics must complete before guest entry, and unsuccessful
or diagnostic-bearing compilations must keep the current publication rules.

This boundary is partly present but not a Cargo/link boundary. `src/lib.rs` links rustc crates;
`depinfo.rs` combines input collection with compiler-independent validation; `pack/write.rs` consumes
`TyCtxt`; native archive handling mixes compiler queries with loading; runtime code reaches assembly
helpers under `lower`. The existing TSan fixture compiles shared engine sources without rustc-private
dependencies, but stubs product leaves and excludes JIT: it supports feasibility, not a complete
standalone-runtime proof.

A compiler worker's output cannot retain its process's helper pointers, native handles or `tcx`
values. Use the existing owned identities, relocation recipes and per-instance restoration rules.
Preserve managed/external input validation, coherent snapshot publication, frozen address domains,
guest function-pointer identity and native constructor ownership. Root-set pruning would also need
an explicit artifact identity: current `--vm-call` is an execution choice, not cache-key material.

## 6. Bounded experiments and acceptance

1. **Runtime isolation:** build a minimal real artifact runner with no `librustc_driver`/LLVM dynamic
   dependency. Prove this through its dependency list and mappings; measure package and bound-image
   hits. Measure compiler-worker misses too, including startup, diagnostics and transfer cost.
2. **Reachability and reuse:** record why functions enter the closure (mono seed, runtime root,
   direct call, function pointer, constant or vtable). Use those edges to assess smaller/lazy
   artifacts. Hot-path execution coverage alone cannot justify dropping cold panic/drop paths.
3. **Generic frontend decision:** expand query profiles to edited macro/trait/generic-heavy crates,
   including host proc-macros and build scripts. Consider a new semantic IR or independent frontend
   only against this workload and an explicit compatibility contract.

All candidates retain target/profile semantics, native and Rust ABI behavior, const-evaluation and
borrow-check diagnostics, source/build-script/proc-macro dependency tracking, indirect-call targets,
drop/unwind/signal behavior, and content-based invalidation. Use focused relevant oracles after a
batch of changes; unmodified research probes do not require a full repository test run. Report cold
and warm cost, output equality, memory and artifact size separately, without relaxing existing gates.

## 7. Primary references

- [rustc compiler overview](https://rustc-dev-guide.rust-lang.org/overview.html): semantic stages and
  the demand-driven query architecture.
- [Monomorphization](https://rustc-dev-guide.rust-lang.org/backend/monomorph.html): native mono seeds
  and the treatment of dependency code; this explains why seed count differs from guest closure size.
- [MIR](https://rustc-dev-guide.rust-lang.org/mir/index.html): typed control flow and the unstable
  human-facing text representation.
- [Compiler profiling](https://rustc-dev-guide.rust-lang.org/profiling.html) and
  [measureme](https://github.com/rust-lang/measureme): query self-profile, summaries and timelines.
- [Always encode MIR](https://doc.rust-lang.org/beta/unstable-book/compiler-flags/always-encode-mir.html):
  the dependency metadata facility already used by MIRVM.
- [rustc_codegen_cranelift](https://github.com/rust-lang/rustc_codegen_cranelift): a backend replacement
  within rustc, distinct from replacing its semantic frontend.
- [Swift type metadata](https://github.com/swiftlang/swift/blob/main/docs/ABI/TypeMetadata.rst):
  generic type metadata and value/protocol witness information; not a proposed Rust layout contract.
- [CoreCLR shared generics](https://github.com/dotnet/runtime/blob/main/docs/design/coreclr/botr/shared-generics.md):
  canonical bodies, per-instantiation dictionaries and the limits of reference-type code sharing.
- [HotSpot tiered compilation](https://docs.oracle.com/en/java/javase/25/vm/java-hotspot-virtual-machine-performance-enhancements.html):
  staged execution and profiling; Rust generic templates and concrete layouts need their own handoff.

These references explain architecture and tools. Measurements and callback ordering were checked
against the repository's pinned `nightly-2026-07-02`, not inferred from current upstream defaults.
