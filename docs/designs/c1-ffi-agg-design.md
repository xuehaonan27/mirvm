# C1: FFI By-Value Aggregate Marshalling

> Status: Implemented · Scope: by-value aggregate parameters and returns across the
> `extern "C"`/`extern "system"` boundary, in both directions and every ABI shape, byte-identical to
> native.

## 1. Contract

- Both directions must match native byte-for-byte: outbound on the `CallForeign` path and the
  `CallIndirect` native-signature path, inbound through the thunk factory and the P1 entry stub
  trampoline.
- Every ABI shape must hold — single-field, multi-field, nested and array, in both the ≤16B register
  class and the >16B sret class. The `c_tree_sitter` parse path is the evidence: it funnels into a
  by-value `TSInput` with an embedded `read` callback, and `ts_node_*` pass and return `TSNode` (32B)
  and `TSPoint` (8B) by value.
- Agreement with native is byte-exact, never approximate.
- The closing basis is libffi's `ffi_type_struct` full aggregate capability (eightbyte splitting, sret
  and register pairs are built in), exact rustc layout field expansion, and the engine's existing
  aggregate convention — `ParamAbi::Indirect` by address with a prologue memcpy, and the
  `RetAbi::Indirect` sret channel — both already live guest-to-guest.

## 2. Model

One global convention removes every direction mismatch: **both sides of the FFI boundary hand
aggregates over by real address.**

```rust
FfiKind::Agg(FfiAgg)                       // new variant; an IR layout change invalidates every cache
FfiAgg  { size: u32, align: u32, fields: Vec<FfiField> }
FfiField{ off: u32, leaf: FfiLeaf }        // declaration order; padding gaps are implied by `off`
FfiLeaf { Scalar(FfiKind) | Agg(FfiAgg) }  // recursive; an array is one repeated field per element
```

Lowering flattens rustc `layout_of`: `Scalar` becomes a leaf, `ScalarPair` a two-field aggregate, and
everything else recurses field by field (struct, tuple, array). Unions, SIMD vectors, unsized types
and ZSTs of significant size are a loud error. A guest function with an aggregate signature now enters
the P1 executable-entry candidate set, which is how the TS `read` callback resolves.

- **Outbound.** The IR actual is `Operand::AddrOf(place)`, so evaluation yields the aggregate's byte
  address and the libffi avalue points straight at that memory. For every by-value aggregate return
  the call site forces `RetDest::Indirect(dst)`, the libffi result buffer is allocated
  16-byte-aligned, and the call is followed by a memcpy of `agg.size` bytes.
- **Inbound.** The libffi closure's `avalue[i]` is always the aggregate byte address, in both the
  register and sret classes. Marshalling maps by the callee's `ParamAbi`: `Indirect` by address
  through the prologue memcpy, `Scalar`/`Pair` by reading fields in `FfiAgg` declaration order (callee
  slot order equals layout order, and rustc layout is the same source, so the identity holds). A
  `RetAbi::Indirect` return makes the thunk treat the result pointer as a hidden first parameter,
  while a pair or small-class return repacks `(lo, hi)` into struct bytes by field offset.

The JIT is not involved: `CallForeign` and `CallIndirect` are not admitted, so this falls back to the
interpreter.

Work split as landed: **A** — the IR variant and flattener, freeze opening, `ffi::call`/`call_addr`
parameter-buffer dispatch by kind, the aggregate return buffer and destination memcpy, and call-site
`RetDest` forcing, with every existing all-scalar path byte-for-byte unchanged. **B** — thunk
`marshal_args` aggregate semantics, `interp::call_guest_ffi` expanding avalues by callee `ParamAbi`,
and trampoline returns in the two classes. **C** — acceptance: the synthetic matrix probe,
`c_tree_sitter` green, and the full gate.

## 3. Boundaries

Rejected with a loud error: union by value (SysV classification needs extra union rules); SIMD vector
by value (a separate axis); a variadic trailing-position aggregate (a fixed aggregate is fine); an
aggregate embedding a raw fn-pointer member that is not a derivable P1 entry; and
`i128`/`f128`/long double/`_Complex`, which keep the existing scalar boundary.

Diagnostics: freezing on a union, SIMD vector or unsized type emits the non-scalar
by-value-aggregate error plus the named boundary class; an inbound marshal whose callee `ParamAbi` and
`FfiAgg` field count disagree aborts with the symbol name and never silently downgrades; a libffi
struct-type construction failure surfaces upward as an error.

Out of scope: JIT admission for `CallForeign`/`CallIndirect`, and the separate indirect-call item.

## 4. Verification

Three-dimension byte equality (mirvm default, native `cargo run`, `MIRVM_JIT_THRESHOLD=1`) is the
requirement, and every shape is exercised as both a parameter and a return, in both directions.

- `tests/scripts/ffi_agg_probe.rs` builds the synthetic matrix — single-field, multi-field, nested and
  array, one to three eightbytes each — and compiles a small `.so` at runtime through `Command` and
  `dlopen`.
- `make suite S=corpus.run ARGS=tree_sitter`: `tests/scripts/c_tree_sitter.rs` is green in three
  dimensions with a fixed 15-line inbound oracle.
- `make gate` closes the full defense line.

## 5. Open items

- The forms rejected in §3 stay open under the debt registry's FFI by-value marshalling entry, and each
  is extensible through the same helper.
- Non-blocking relations: the rlib-symbol work is orthogonal, backtrace symbolization is unrelated,
  and the `SymFn` bypass shares the P1 entry budget with this slice. A real workload that hits a
  rejected form re-establishes that form; nothing is pre-committed.
