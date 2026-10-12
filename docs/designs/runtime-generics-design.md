# Shared generic execution and specialization

> Status: Production template export/binding is opt-in; shared execution remains an isolated prototype.
> Owns generic execution products, promotion and integration gates. Frontend selection and semantic
> validation remain in [frontend-boundary.md](frontend-boundary.md#41-runtime-generics); measurements
> belong to [current-status.md](../current-status.md#runtime-generics-prototype). D7 and T13 in
> [open-issues.md](../open-issues.md) own unfinished work. The production runtime still uses concrete
> bytecode.

## 1. Decision and boundary

Separate a generic function's semantic validation, its concrete instance description, and its
instruction stream. Keep rustc's required rejection and instance discovery before guest entry.
Export owned templates and bindings; release the compiler session before Engine runs. Cold
instances share instructions, while profitable hot instances obtain concrete bytecode for the
existing JIT. Unsupported functions retain ordinary eager lowering in the product integration.

The production lowerer admits fixed scalar/pair frames, slice metadata and pair moves, fixed-layout
field projections, scalar const parameters, integer operations and opaque-pointee offset/copy loops
under `MIRVM_GENERIC_TEMPLATES=1` (unset disables it). Direct calls bind concrete guest functions,
including resolved trait methods and Clone/Drop shims, through the existing linker worklist. One compiler
session caches both eligible templates and rejected definitions. Only ordinary Item instances are
eligible; the session's definition identity and semantic profile fix the cache key. Binding resolves
type sizes, const values and callee identities without instantiating a whole concrete MIR body.
Compiler-owned call recipes remain separate from the owned template and resolve completely before
the linker allocates any FuncId. Equal-layout types can therefore bind different witnesses. Unsupported
frames/projections, indirect or foreign calls, cleanup and caller-location ABIs retain eager lowering. Collection still performs
required instance validation before export. The owned template carries ordinary IR dictionary reads
and a positional schema; immediate specialization removes that hidden argument before the existing
verifier, archive and runtime see the body. This stage neither persists templates nor shares runtime
instructions. Its flag changes lowering strategy, not artifact semantics; an existing concrete image
can satisfy either setting without entering the exporter.

The execution prototype supports this architecture, not a whole-program speedup claim. Its real
MIR exporter handles an opaque-pointee copy loop, and its witness/unwind experiments use native
Rust callbacks. Arbitrary trait calls, layout-dependent locals, guest exceptions and persistent
generic images remain integration gates. Production rollout must be measured on the existing
serde, flate and generic-matrix workloads with prepared dependencies and std.

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
Generic storage must retain that body and its descriptor for the activation's full lifetime. The
research hook allows publication only between serial calls; it is not the production implementation.

Separate metadata lookup from obtaining executable instructions. Frame/ABI/callee-header queries,
archive verification and image merging must not accidentally specialize a body. Preserve the
compiled caller's stack-headroom assumptions: a published callee form either fits the stable
descriptor's checked frame contract or goes through a boundary that checks its own requirements.
The current small-call fast path cannot silently inherit a different frame requirement.

Only one producer may publish an instance's verified concrete form. Recursive calls can continue
through its shared form while specialization is pending; unsupported optimization leaves that
verified form usable. Compile failures retain the current JIT failure policy. Per-Engine lifetime,
close, native reentry, unwind ownership, plain/trace separation and fork must cover both forms.
A child must not inherit an initialization slot owned by a vanished thread.

Exact type/const specialization does not require speculative deoptimization. It also cannot move a
currently executing loop into newly published code. OSR is a separate transfer contract under T13:
live-value maps, operand and shadow frames, cleanup state and code-domain identity must agree at a
safe point. An entry-only policy cannot claim OSR's single-invocation benefit.

## 4. Promotion policy

Count work as well as entries, and price the full transition: concrete emission, validation,
machine-code generation, publication and queue delay. The prototype's measured concrete emission
is much cheaper than machine-code generation; optimizing only the former misses the main payback
cost. Time spent in native witnesses or outside the body is not time a specialized loop can remove.

Use shared interpretation as the guaranteed fallback. A compiled shared template is a candidate
intermediate tier when many instances execute briefly; it can amortize one compilation across
instances. It is not automatically enabled: a short run can finish before even that cost is repaid.
Compare it against direct instance specialization rather than assuming three tiers always win.
Sparse instance use, a hot repeated loop, tiny calls, native-call-heavy work and one long invocation
are separate policy acceptance cases. Initial thresholds are an experiment, not a product default.

## 5. Integration roadmap and gates

The next changes are ordered so that each produces a usable, reviewable boundary:

1. **Production template export and binding.** Extend slice operations to indexing and checked paths,
   then layout-dependent locals/projections and Clone/Drop results requiring dynamic frames. Keep
   eager fallback and compile-time diagnostic parity, including an unentered invalid instance.
   Acceptance: one template serves multiple actual Rust types; exported runtime state owns no
   compiler references; all unsupported shapes execute through the current lowerer.
2. **Per-Engine generic function entries.** Add explicit template/descriptor storage to FuncTable
   and separate headers from instructions. Retain the pinned body and descriptor across the entire
   interpreted activation and cleanup chain when enabling concurrent promotion. Preserve function
   pointers and concrete ABI adapters, recursion, frame checks, native callbacks, close and fork. Acceptance: mixed
   shared/concrete/JIT execution, concurrent first publication, guest unwind, signals and plain/trace
   differentials agree with eager execution; no global research override remains.
3. **Reusable generic products.** Extend the existing function archive, verifier and shared-unit
   manifests with template and descriptor references. Reuse canonical fragments and relocations;
   avoid per-instance adapter-pointer literals defeating deduplication. Store verified concrete IR
   and machine code under identities that include the template and exact bindings. Acceptance:
   installed std, project preparation, edited-source misses, unchanged hits and cache purging obey
   their existing ownership/diagnostic contracts without eager specialization on load.
4. **Measured tier policy.** Add bounded heat accounting and asynchronous promotion through the
   existing compile service. Compare shared-template JIT, direct specialization and staying shared
   using measured compile cost and removable execution work. Acceptance: cold complete time and
   first useful result improve on representative inputs without hiding late work; short/native-call
   workloads retain their cheaper path, and warm throughput and memory/code size remain acceptable.
5. **OSR and coverage expansion.** Implement T13's activation-transfer contract independently of
   generic coverage, then attach specialization to the same mechanism. Extend the generic exporter
   only from corpus evidence, with associated types, niches/enums, closures, indirect calls and
   recursive drop/cleanup handled explicitly. Acceptance includes a single hot invocation and a
   real generic dependency workload, not just synthetic repeated calls.

Do not select default enablement from the execution microbenchmarks. The release decision needs
unmodified reference/candidate whole-process measurements, native output agreement, the relevant
repository correctness checks, and demonstrated reuse of a bound image after preparation. OSR can
proceed alongside the export/storage work once the activation ownership contract is fixed.
