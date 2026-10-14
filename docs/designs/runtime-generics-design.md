# Shared generic execution and specialization

> Status: Production shared interpretation, reusable templates and on-demand JIT specialization are opt-in.
> Owns generic execution products, promotion and integration gates. Frontend selection and semantic
> validation remain in [frontend-boundary.md](frontend-boundary.md#41-runtime-generics); measurements
> belong to [current-status.md](../current-status.md#production-shared-generic-execution). D7 and T13 in
> [open-issues.md](../open-issues.md) own unfinished work. Unsupported functions retain eager lowering.

## 1. Decision and boundary

Separate a generic function's semantic validation, its concrete instance description, and its
instruction stream. Keep rustc's required rejection and instance discovery before guest entry.
Export owned templates and bindings; release the compiler session before Engine runs. Cold
instances share instructions, while profitable hot instances obtain concrete bytecode for the
existing JIT. Unsupported functions retain ordinary eager lowering in the product integration.

The production lowerer admits scalar/pair frames, opaque by-value locals, slice metadata and pair
moves, sized struct/tuple field projections and construction, checked slice/array indexing, slice
patterns, scalar const parameters, integer operations and opaque-pointee offset/copy loops under
`MIRVM_GENERIC_TEMPLATES=1` (unset disables it). Direct calls bind concrete guest functions,
including resolved trait methods and Clone/Drop shims, through the existing linker worklist. One
compiler session caches both eligible templates and rejected definitions. Only ordinary Item
instances are eligible; reusable dependency products also retain these results across consumers.
Binding resolves layouts, const values and callee identities without instantiating a whole concrete
MIR body. Layout recipes include compound types such as `[T; N]`. Compiler-owned layout/call recipes
remain separate from the owned template and resolve completely before the linker allocates any
FuncId. Equal-layout types can therefore bind different witnesses.

An opaque local occupies a fixed pointer slot in the template. Binding appends its sized, aligned
storage to the concrete frame with checked offset arithmetic, then initializes the pointer to that
storage. Moves use byte copies that permit overlap. The parameter and return headers use the concrete
value's ZST, scalar, pair or indirect ABI; opaque call operands and destinations bind through the
same classification. Opaque storage adds no per-value heap allocation or compiler query at guest
entry. Cleanup, Resume and terminate edges retain MIR's control flow, including partial initialization when Clone unwinds.
This concrete frame contract is fixed before any interpreter/JIT caller queries its header.
Unsupported projections and aggregates, indirect or foreign calls and general
caller-location ABIs retain eager lowering.

Sized struct and tuple fields bind exact offsets from the instantiated Rust layout; field order,
padding and alignment are not inferred from sizes. Nested projections compose these offsets.
Construction writes fields from unprojected locals distinct from the destination, or supported
scalar constants, into a local destination. Aliasing projections, enum/union construction and unsized tails retain eager
lowering. Return and parameter ABIs still come from the complete concrete aggregate layout.

Bounds assertions branch to the guest panic lang item with the original source location and unwind
action. Index strides read type-size bindings through ordinary IR; address temporaries remain live
through every operand of a statement or terminator and are reused only after it completes. Slice
patterns preserve metadata while selecting from the front, from the end or a subslice. Collection
still performs required instance validation before export. The owned template carries ordinary IR
dictionary reads and a positional schema. Binding fixes the frame and guest-call adapters without
copying ordinary instructions. The interpreter initializes the dictionary pointer and opaque-local
pointers once in its prologue; neither is a guest ABI parameter. The flag changes the representation
produced on a lowering miss, not artifact semantics: a verified image can execute under either
setting without entering the exporter.

The execution prototype supports this architecture, not a whole-program speedup claim. Its real
MIR exporter handles an opaque-pointee copy loop, and its witness/unwind experiments use native
Rust callbacks. General caller-location ABIs remain outside the admitted subset.
Production rollout must be measured on the existing serde, flate and generic-matrix workloads
with prepared dependencies and std.

## 2. Owned records

| Record | Owns | Must not own |
|---|---|---|
| Template | Definition/instance-kind identity, parameterized operations, binding schema and control flow | `TyCtxt`, compiler pointers, session-local DefIds or a concrete type's accidental layout |
| Instance descriptor | Exact type/const identity, validated layouts and frame/boundary ABI, resolved callee/drop/constant bindings | A duplicate instruction stream or unresolved semantic queries at guest entry |
| Callable entry | Stable FuncId/address identity and an immutable reference to each published execution form | A new guest identity when a tier changes or process-global mutable Engine state |

Use the existing instruction and artifact vocabulary where it expresses the operation. The
prototype translates descriptor reads into ordinary memory operands and uses existing call and
cleanup edges. That proves a new universal bytecode interpreter is not required for its admitted
subset. It does not establish that every Rust layout can use those same operands efficiently.
Unknown-size values may use address-based internal operations with concrete boundary adapters;
size, alignment, niche/tag rules, projection offsets and drop behavior remain distinct bindings.
Equal size/alignment does not identify a type or prove code sharing is valid.

Templates and descriptors must be verified together: binding kinds and arity, index ranges,
control flow, frame bounds, layout arithmetic and concrete call signatures. Serialized bindings
name owned identities and relocation targets, never process callback addresses. Keep exact target,
compiler/build, semantic profile, schema, template and dependency identities in reusable products;
an incompatible template or descriptor cannot fall through to an old specialized body. Installed
std and project preparation use the ownership and invalidation rules of the existing distribution
and cache designs.

Dependency compilation exports admitted local definitions with identity generic arguments after
required instance validation. It does not need a consuming concrete type. The compiler-side store
holds one checksummed generation file per defining crate and exact target/profile. Its identity
includes the MIRVM build, stable crate identity, rustc crate hash (including upstream inputs), target
layout, CPU/features and semantic checks. Definitions use stable path hashes. Both accepted
templates and rejected export probes are reusable; concrete binding failures are instance-specific
and are not persisted as rejected definitions.

The file contains owned instruction archives and recipes locating types and call terminators in
the defining crate's MIR. Loading checks archive structure, frame/binding indexes and the admitted
instruction vocabulary, reconstructs compiler types and source spans from those recipes, and binds
the current instance before linking. No compiler allocation, session-local DefId or concrete
consumer identity is serialized. Binary-only analysis sessions without HIR hashes retain their
local definitions in memory; their dependencies still use the store. Publication merges concurrent
definition sets under a lock and removes replaced bodies. The registered cache family participates
in `cache purge --units`; removal or corruption falls back to export.

## 3. Execution and publication

```text
validated template + instance descriptor
             |
       shared execution ------> optional compiled shared template
             |
       profitable hot instance
             v
  concrete IR -> verification -> existing JIT -> published entry
```

Entry identity and concrete ABI stay fixed. An activation pins its chosen immutable body and
descriptor until return/unwind; publishing another form affects subsequent entries. The interpreter
passes its entry-selected body through block execution and cleanup instead of resolving FuncId again.
`FuncTable::execution` retains the shared body and descriptor for that entire activation.
`FuncTable::get` publishes concrete instructions in an instance's per-Engine `OnceLock` only when
a concrete consumer, including the existing JIT compile service, requests them. Shared execution
continues without waiting for this slot. A concrete form substitutes dictionary values and adds
opaque-local pointer initialization while keeping the descriptor's exact frame and ABI.

`FuncTable::header` borrows owned or archived ABI/frame metadata without initializing executable
instructions. JIT call preparation, stack-headroom relocations, FFI marshalling and signal-handler
shape checks use that boundary. Metadata lookup, archive verification, ordinary table iteration and image merging preserve shared
entries without specialization. Preserve the
compiled caller's stack-headroom assumptions: a published callee form either fits the stable
descriptor's checked frame contract or goes through a boundary that checks its own requirements.
The current small-call fast path cannot silently inherit a different frame requirement.

Only one producer may publish an instance's verified concrete form. Recursive calls can continue
through its shared form while specialization is pending; unsupported optimization leaves that
verified form usable. Compile failures retain the current JIT failure policy. Per-Engine lifetime,
close, native reentry, unwind ownership, plain/trace separation and fork must cover both forms.
Before fork, the existing preparation boundary settles archive decode and concrete publication
slots, including currently unentered shared instances. This intentionally pays their concrete
emission cost at fork so the child cannot inherit an initializer owned by a vanished thread.

A template's identity is the BLAKE3 of its versioned template fragment: the fragment encoding
version and template tag followed by its dictionary slot, binding count and instruction array.
Construction computes the identity before publishing immutable instructions. Concrete dictionary
values, frame layout, ABI, names and bound call targets belong to each instance and do not enter
this identity. Changing template semantics requires changing the fragment encoding version; normal
build and product generation checks still apply. This identity names reusable instructions, never
a guest function address or its concrete entry.

Function archives retain the 32-byte identity alongside each template object. Their enclosing
product's integrity and generation checks cover those bytes; the loader consumes trusted executable
products under the existing native-address safety contract. Borrowed validation still checks the
template's dictionary arity and accesses, admitted vocabulary, concrete local storage, call ordinals
and bound adapters before runtime decode. Grouping reads the stored identity without decoding or
rehashing instructions. Within a loaded function archive, including concatenated chunks, demand
decode publishes one template object per identity. Separate archives can retain distinct decoded
objects while a compiled group shares their content identity. Concrete publication slots remain
per instance. Shared instructions carry no external relocation: concrete adapters own FuncIds and
caller-location addresses.

Unit, dependency and program manifests store templates as separate content-addressed fragments.
A descriptor fragment hashes its template id together with the concrete binding representation;
its manifest record names both fragments for loading and collection. Rehydration decodes each
referenced template once and restores adapter relocations through the existing canonical site walk.
Republishing an Engine preserves these shared products even after individual instances specialize.
Standalone packages use the same owned representation in FUNCS; its byte layout is owned by the
[container contract](modeb-mirvmar-design.md#22-container).

Exact type/const specialization does not require speculative deoptimization. It also cannot move a
currently executing loop into newly published code. OSR is a separate transfer contract under T13:
live-value maps, operand and shadow frames, cleanup state and code-domain identity must agree at a
safe point. An entry-only policy cannot claim OSR's single-invocation benefit.

## 4. Promotion policy

Count work as well as entries, and price the full transition: concrete emission, validation,
machine-code generation, publication and queue delay. The prototype's measured concrete emission
is much cheaper than machine-code generation; optimizing only the former misses the main payback
cost. Time spent in native witnesses or outside the body is not time a specialized loop can remove.

`MIRVM_JIT_LEDGER=1` records shared-body work and separates request queue delay, concrete
specialization, compiler-worker time and final cache publication. The interpreter samples the first
entry and every 64th entry, stopping at the first block boundary after 4,096 instruction/terminator
units. Calls and signal draining pause its clock. Weighted elapsed samples are an estimate of
own-body work, not exact CPU time or a prediction of remaining lifetime.

`MIRVM_GENERIC_PAYBACK=1` opts into a compile-work budget. Existing entry/backedge triggers nominate
functions; the first shared compilation calibrates observed worker cost per static instruction.
Later requests need both per-instance work credit and sufficient unspent Engine credit. Actual
worker time charges the budget, including an underestimated transition's debt. Deferred requests
retry at sparse counter boundaries with at most one pending request per function. Under this policy,
shared entries do not prelink from previous-run heat before earning credit; synchronous verification
bypasses admission. The option is off by default. Reducing compiler work alone does not establish
better latency; all final publication work remains inside whole-process measurements.

Use shared interpretation as the guaranteed fallback. A compiled shared template is a candidate
intermediate tier when many instances execute briefly; it can amortize one compilation across
instances. It is not automatically enabled: a short run can finish before even that cost is repaid.
Compare it against direct instance specialization rather than assuming three tiers always win.
The production instance header fixes parameter/return ABI, opaque-local storage and call adapters.
Compiling only the common instruction array cannot share those concrete entry assumptions. The
[frame-entry experiment](../../experiments/compiled-shared/README.md) retains the concrete activation
and runs shared code over its frame. It lazily compiles admitted groups, invokes the existing call
adapter from compiled code, and can publish concrete entries for subsequent calls while active
shared frames remain valid. Its experimental policy distinguishes linked descriptors from executed
instance diversity and keeps interpreted/shared work credit separate. After shared publication,
concrete admission uses observed shared execution rather than earlier interpretation. A
sampling-only control retains fixed admission to expose measurement cost. These mechanisms budget
past worker cost; they do not estimate the future work a transition removes. Frame setup is outside
the current sample. Optional scalar SSA in the
frame-entry experiment retains nonaliasing values across blocks and synchronizes the interpreter
frame at call/cleanup/return boundaries. It preserves dynamic dictionaries and the concrete entry
ABI; its code-size, synchronization and compilation costs remain part of the measured tier decision.
Aggregating template heat or reducing compilation alone does not prove payback.
Sparse instance use, a hot repeated loop, tiny calls, native-call-heavy work and one long invocation
are separate policy acceptance cases. Initial thresholds are an experiment, not a product default.

## 5. Integration roadmap and gates

Remaining integration work is grouped by a complete execution or preparation boundary:

1. **Measured tier policy.** Validate the experimental work budget against latency, including
   sampling overhead, scheduling bias and deferred-admission costs. Measure frame setup and the
   removable fraction of shared execution/call adaptation before pricing concrete promotion;
   consumed work is not expected savings. Select which groups benefit from an intermediate tier,
   including bypass for cheap and call-only bodies. Compare against staying interpreted and direct
   specialization. Acceptance: cold complete time and first
   useful result improve on representative inputs without hiding late work; short/native-call
   workloads retain their cheaper path, and warm throughput and memory/code size remain acceptable.
2. **Corpus-driven coverage.** Extend remaining projection/aggregate shapes and
   range/array-subslice paths when their measured missed coverage warrants the change. Keep eager
   fallback and diagnostic parity for unsupported shapes. Associated types, niches/enums, closures,
   indirect calls and recursive drop/cleanup need explicit contracts, not layout-size equivalence.
3. **OSR (deferred).** Implement T13's activation-transfer contract independently of generic coverage,
   then attach specialization to the same mechanism. Acceptance includes one long hot invocation,
   live values and cleanup state across transfer, and plain/trace domain identity.

The shared-entry gate covers mixed shared/concrete/JIT execution, concurrent first publication,
recursion, native callbacks, unwind, signals, close and fork. These lifecycle contracts remain
required as admission and promotion change. The isolated contract probes do not close the full
product gate.

Do not select default enablement from the execution microbenchmarks. The release decision needs
unmodified reference/candidate whole-process measurements, native output agreement, the relevant
repository correctness checks, and demonstrated reuse of a bound image after preparation. OSR can
resume after the activation ownership contract is fixed; it is not part of the current preparation,
policy and coverage work.
