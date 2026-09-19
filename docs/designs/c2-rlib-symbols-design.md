# C2: Native-Archive Closure When Symbols Live in the rlib

> Status: Decided RFC · Scope: closing the `.a` -> `.so` closure gap for function symbols whose definitions live in a Rust rlib, via link-time P1 trampoline injection.

## 1. Contract

The gap is closable in full; the decided route is **P1 stub trampolines injected at link time**.

1. In native semantics, an rlib `#[export_name]`/`#[no_mangle]` fn is a cell of the final link's symbol set. The mirvm dual is that Rust fn's **P1 executable entry** (`4202317`): a derivable signature means stub code address + libffi closure trampoline back into the interpreter.
2. The closure decision set must be widened from "archive self-closure" to "archive + the crate graph's rlib-exported fn set". An undefined symbol found in the rlib export set must materialize a P1 entry and inject a same-name hidden trampoline; `-z defs` closure then still holds.
3. **Rejected route 1 — runtime lazy resolution.** Under RTLD_NOW discipline an undefined `.so` symbol dies at `dlopen`; RTLD_LAZY violates the engine's required-libraries discipline, and a P1 stub is absent from the process dynamic symbol table, so there is nothing to resolve against.
4. **Rejected route 2 — treat the rlib as a native object to link.** A metadata-only rlib (S2 `-Zno-codegen`) carries no machine code at all — the same root cause as dep global_asm (C4).
5. The closure contract covers every form of ".a -> .so closure gap: symbol in rlib" for **function** symbols; data symbols (an rlib static referenced from C) must be loudly rejected (`to be opened separately`), as must signatures the rescue chain cannot derive.
6. The injection path must fire only on the failure path: when the first link succeeds, all archive behavior is byte-for-byte unchanged.

Concrete evidence for this contract: wasmtime v46 `libwasmtime-helpers.a`, whose trampolines call `resolve_vmctx_memory_ptr_46_0_1` and peers (`#[export_name]`, versioned-export-macros, defined in wasmtime's own rlib); and the bzip2-sys vendored BZ_NO_STDIO assertion stub `bz_internal_error` (`#[no_mangle]` `extern "C" fn(c_int)`, defined in a Rust rlib). Minimal reproducer `/tmp/mre` (build.rs cc trampoline `c_side_trampoline` -> `#[export_name]` `rust_side_def`) currently rejects loudly with "cannot be safely converted to a shared library"; ld stderr = `undefined reference to `rust_side_def'`.

## 2. Model

### 2.1 Ordering and consumer surface

- Conversion point: `lower/mod.rs:2044` (before draining the worklist) — the same budget window as global_asm C7. The entry budget `fn_entry_addr(inst)` is immediately usable there (P1 placement order equals startup order reproduction; addresses are stable across processes).
- Authoritative name -> Instance table: `Linker::exported_defs()` (keyed by `tcx.symbol_name`; covers local definitions plus dependency-crate non-generic exports collected via `dependency_formats(Executable)` — exactly the final link's symbol set, corroborated by the rustc_middle queries.rs:2358 documentation). Both sides canonicalize keys through `canonical_link_name`, stripping `\x01` (the aws-lc prefix family precedent).
- Budget: `linker.fn_entry_addr(inst)`. Dep-crate fns land automatically in the image domain (the `alloc_entry_stub` image_side branch); delta-domain functions land in the local domain. Idempotent.
- Derivable signature is a precondition: for `-> !` (the bz assertion stub) the layout is ZST, so `FfiKind::Void` is derivable. Non-derivable signatures keep today's loud rejection — the pre-existing boundary of the real gap; C1 already eliminated one family of aggregation gaps.

### 2.2 Implementation plan — the rescue chain in `src/native_archive.rs::materialize_for_target_in`

1. The first chain runs as before (`cc -z defs --whole-archive …`). On failure, enter rlib injection evaluation — **never parse any tool text output**. Instead, extend the repository's existing `src/elfsym.rs` ELF64 symbol-table parser with an ar member walk, and at binary level enumerate the `SHN_UNDEF` global/weak symbols of every archive member (skipping LOCAL and the ar symbol-table members).
2. Canonicalize the enumeration through `canonical_link_name` and intersect it with `exported_defs()`:
   - hit and it is a Fn => budget `linker.fn_entry_addr(inst)`, yielding `(name, stub_addr)`;
   - hit and it is a Static => Err "data symbol in rlib (C2 boundary, to be opened separately)";
   - empty intersection => take the original error path directly (loud rejection, same text as today).
3. Emit the trampoline `.s` in the same form as global_asm C7, with **`.hidden <name>`** — bound only inside the `.so`, never polluting the process global namespace (under native, an rlib definition is a link-time static binding anyway): `.globl <name>; .hidden <name>; .type <name>,@function; <name>: movabs rax, <addr>; jmp rax`.
4. Compile the trampoline object with cc and fold it into the **relink** (`archive.a tramp.o -l…`). If the relink still fails (unresolved symbols outside the intersection) => original error path (a diagnostic byte-for-byte identical to today's).
5. Cache key: the injection path folds the **sorted (name,addr) pairs** into the `content_hash` component (the first-chain success path's key is untouched, so behavior stays compatible). The `.so` is a module-specific artifact — P1 code addresses are stable across processes, so repeated runs of the same module always hit, and different modules naturally get different keys.
6. Signature and call surface: `materialize_static_libraries(tcx)` becomes `(tcx, &mut Linker)` (single call site, `lower/mod.rs:2045`; the global_asm C7 change is the precedent).

Non-ad-hoc basis for the plan, recorded as conclusions: (1) the closure set "rlib exported fn" is rustc's `exported_non_generic_symbols`, the local authority for the native final link's symbol set, not a per-instance allowlist; (2) the trampoline is the link-time materialization of a P1 executable entry (the native static-binding dual, not a wasmtime special case), and hidden visibility is the general guard against global interposition; (3) the two-stage chain on failure is a deliberate compatibility choice, not a bypass design; (4) `movabs+jmp` is the same-family form already accepted under C7 (the unified shape after GAS Intel `call ABS` was confirmed).

## 3. Boundaries

- The closure decision set is ar-member `SHN_UNDEF` intersected with the crate graph's rlib export set, enumerated statically at binary level with the `src/elfsym.rs`-sourced parser and no dependence on tool text. Unresolved symbols outside the intersection (cross-archive duplicate names, a third-party library missing a component) still fail the relink => original error path; closure discipline is never relaxed.
- The `-z defs` discipline does not loosen: injection only admits "rlib exported fn" into the closure decision; an unmatched NAME is still loudly rejected.
- The trampoline uses `.hidden` to avoid RTLD_GLOBAL global interposition. Two archives in the same crate graph referencing the same guest fn each inject the same stub address (the budget is idempotent), so hidden visibility produces no global collision.
- L2/cache: the injection path's key contains the (name,addr) pairs; cold and hot behavior agree, backed by the existing P1 startup-phase rebuild (a trampoline stores only a code address; the entry body is reproduced in the startup phase).
- Not opened: rlib **data** symbols (a static referenced from C; ABS `.set` semantics for data references need separate proof) — loudly rejected, to be opened on demand.
- Non-derivable signatures are not covered by the rescue chain and keep today's loud rejection.

## 4. Verification

The acceptance matrix is closure acceptance; every case must be byte-exact on all three dimensions — mirvm default, native `cargo run`, and `MIRVM_JIT_THRESHOLD=1`.

| # | Case | Requirement | Evidence |
|---|---|---|---|
| 1 | **MRE promoted** | Rebuild `/tmp/mre` as a corpus probe (`c_rlib_sym_probe`: a helper.a trampoline plus an `#[export_name]` Rust definition writing back an observation value) and pass it on all three dimensions. | corpus probe driver + `tests/suites/corpus/cases.manifest` registration |
| 2 | **bzip2 C backend green again** | `c_bzip2_pure` currently uses the 0.6 pure-Rust backend (the detour-period decision); add or switch back a driver that uses the **vendored C backend**, verifying both the symbol rescue of the BZ_NO_STDIO assertion stub through rlib injection and normal compress/decompress. The workload's normal path never triggers the assertion callback, so this case must not be used to claim the callback panic has executed; the callback is an ordinary `extern "C"`, and a real escaping panic must terminate. | corpus driver + three-dimension byte equality |
| 3 | **c_wasmtime_wat face change** | With layer 1 eliminated, the driver's red pattern must move from `cannot be safely converted to a shared library`/101 to `inline asm noreturn`/70 (layer 2 C3 entry; execute the driver header's stated wiring strategy verbatim). | corpus driver + red pattern |
| 4 | **Zero-change regression** | Archives whose first link succeeds (rusqlite/aws_lc/libgit2/…) must stay byte-for-byte identical; run the full gate5 plus `cargo test` plus diff. | gate5, `cargo test`, byte diff |

## 5. Open Items

- rlib **data** symbols (a static referenced from C): ABS `.set` semantics for data references are unproven. Reopen trigger: a real workload where C references an rlib static.
- Non-derivable signatures: signatures the rescue chain cannot derive keep today's loud rejection. Reopen trigger: a real workload whose gap needs such a signature.
- Corpus ids: this design cites `c_rlib_sym_probe`, `c_bzip2_pure`, `c_wasmtime_wat`; `tests/suites/corpus/cases.manifest` currently lists `bzip2_pure`, `bzip2_csys`, `wasmtime_wat`. Reopen trigger: registering the promoted/split drivers, when the manifest ids must be reconciled with these names.
- Status vs. tracker: `docs/open-issues.md` carries a "C1/C2/C3/C5/C7 closed (2026-07-18)" note while this document remains a decided plan. Reopen trigger: reconciling that closure claim with the acceptance matrix above.
