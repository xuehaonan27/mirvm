# C1: FFI By-Value Aggregate Marshalling (Outbound and Inbound)

> Status: Implemented (2026-07-18) · Scope: by-value aggregate parameters and returns across the `extern "C"`/`extern "system"` boundary, both directions and every ABI shape, byte-identical to native. Completion is maintained in [../current-status.md](../current-status.md).

## 1. Contract
- C1. Both directions must match native byte-for-byte: outbound on the `CallForeign` path and the `CallIndirect` native_sig path; inbound through the M4.4 thunk factory and the P1 entry stub trampoline.
- C2. Every ABI shape must hold: single-field, multi-field, nested, array; the ≤16B register class and the >16B sret class. Evidence: the `c_tree_sitter` parse path funnels into a by-value `TSInput` with an embedded `read` callback, and `ts_node_*` pass and return `TSNode` (32B) / `TSPoint` (8B) by value.
- C3. Agreement with native is byte-exact, never approximate.
- C4. The closing basis is libffi `ffi_type_struct` full aggregate capability (eightbyte splitting, sret and register pairs are built in, not self-proving) + exact rustc layout field expansion + the engine's existing aggregate convention (`ParamAbi::Indirect` by address + prologue memcpy; the `RetAbi::Indirect` sret channel), both already live guest↔guest.

## 2. Model
```rust
FfiKind::Agg(FfiAgg)                       // new variant (serde; an IR layout change invalidates every cache)
FfiAgg  { size: u32, align: u32,
          fields: Vec<FfiField> }         // declaration order; padding gaps are implied by `off`
FfiField{ off: u32, leaf: FfiLeaf }
FfiLeaf { Scalar(FfiKind) | Agg(FfiAgg) }  // recursive nesting (an array = one repeated field per element)
```

- Lowering flattener: rustc `layout_of` → BackendRepr split three ways: `Scalar` → leaf (today's path); `ScalarPair` → 2-field Agg; `Memory` and the rest → field-by-field recursion (Adt single-variant/tuple/array; union/SimdVector/unsized/ZST-of-sig = loud Err). `ffi_kind_of` opens its output and `freeze_c_fnptr_sig` admits it; a guest fn with an aggregate signature now enters the P1 executable entry candidate set, and the TS `read` callback is of this family.

**One global convention removes every direction mismatch: both sides of the FFI boundary hand aggregates over by real address (bytes).**

- Outbound: the ir actual is `Operand::AddrOf(place)` (the existing shape, func.rs:2851) → evaluation yields the aggregate byte address → the libffi avalue points directly at that memory (`Arg::new(&[u8])` takes the data address for `?Sized`, already proven); return: `RetDest::Indirect(dst)` is forced at the call site for every by-value aggregate return (Pair and single-field same-size class unify on this one path), the libffi result buffer is allocated in a 16-byte-aligned bucket, and the call is followed by a memcpy of `agg.size` bytes to `dst`.
- Inbound: the libffi closure `avalue[i]` is always the aggregate byte address (the <16B and sret classes have the same shape); marshalling maps by callee `ParamAbi`: Indirect → by address (prologue memcpy), Scalar/Pair → read fields in `FfiAgg` declaration order (callee slot order = layout order; rustc layout is the same source, so the identity holds); return: `RetAbi::Indirect` makes the thunk treat the `result` pointer as a hidden first parameter slot (the existing sret convention), while Pair/small class returns (lo,hi) and repacks into struct bytes by `FfiAgg` field offset (masking the sub-width little-endian semantics, isomorphic to the outbound side).

JIT is not involved: `CallForeign`/`CallIndirect` are not admitted, so this falls back to the interpreter; the indirect-call item E1 is separate.

Slices. **A (outbound + data model)**: ir `FfiKind/Agg` + flattener and freeze opening + `ffi::call/call_addr` parameter-buffer dispatch by kind + aggregate return buffer and dst memcpy + `CallForeign`/native_sig return admitting Indirect + call-site RetDest forcing; the regression check is that every existing all-scalar case path stays byte-for-byte unchanged. **B (inbound)**: thunks `marshal_args` aggregate semantics + new `interp::call_guest_ffi` (expands av by callee `ParamAbi`) + trampoline/entry_trampoline return in two classes (sret direct / `FfiAgg` repack). **C (acceptance)**: synthetic matrix probe + `c_tree_sitter` green + full gate (see §4).

## 3. Boundaries
Rejected with a loud Err (locked red): union by value (SysV classification needs extra union rules; the TS family has no such shape); SIMD vector by value (`BackendRepr::SimdVector`, another axis, re-established separately); variadic trailing-position aggregate (a fixed aggregate is OK, a trailing aggregate is Err); an aggregate embedding a fn-ptr member passed as raw native bytes (callable once the P1 entry is executable; non-derivable entries share the P1 document boundary, where being called from native is UB anyway) — `TSInput.read` goes through the P1 entry and resolves naturally; i128/f128/long double/_Complex keep the existing scalar boundary unchanged.

Diagnostics: freeze on union/SIMD/unsized emits a loud Err reusing the non-scalar by-value-aggregate wording plus the named boundary class (locked red); an inbound marshal whose callee `ParamAbi` and `FfiAgg` field count disagree (theoretically impossible because the layout is same-source) `engine_abort`s with the symbol name and never silently downgrades; a libffi struct type construction failure (the align/size contract) surfaces as an Err upward, the precedent that preserves `dlerror`.

Out of scope: JIT admission for `CallForeign`/`CallIndirect` (see §2); the indirect-call item E1 stands alone.

## 4. Verification
Three-dimension byte equality is the requirement: mirvm default / native `cargo run` / `MIRVM_JIT_THRESHOLD=1` must agree byte-for-byte. Every cell below is exercised as a parameter and as a return, in both directions.

| ABI shape | 1 eightbyte | 2 eightbytes | 3+ eightbytes |
|---|---|---|---|
| single field | covered | covered | covered |
| multi field | covered | covered | covered |
| nested | covered | covered | covered |

- Synthetic matrix probe `tests/scripts/ffi_agg_probe.rs`: the matrix above; cc compiles a small .so at runtime through `Command` + `dlopen`; the comparison is two-dimension isomorphic as specified here. `tests/scripts/c_tree_sitter.rs`, registered as `tree_sitter` in `tests/suites/corpus/cases.manifest`, is green as-is in three dimensions; the B-dimension 15-line oracle is fixed.
- Commands: `./tests/run.sh suite corpus.run tree_sitter`; full gate `./tests/run.sh gate`.

## 5. Open items
- Residuals: the rejected forms in §3 stay open, carried by the debt registry under R17 (FFI by-value marshalling residual boundaries after C1), and each form is extensible through the same helper.
- Landing this design closes open-issues C1; the `c_tree_sitter` expected-red pattern (the by-value-aggregate non-scalar diagnostic, exit 70) XPASSes to green.
- Non-blocking relations and reopen: C2 (symbol in rlib) is orthogonal, E8 backtrace symbolization is unrelated, R16's SymFn bypass shares the P1 entry budget with this slice; a real workload that hits a rejected form re-establishes that form, and nothing is pre-committed.
