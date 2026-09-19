# C2: Native-Archive Closure When Symbols Live in the rlib

> Status: Decided RFC · Scope: closing the `.a` → `.so` closure gap for function symbols whose
> definitions live in a Rust rlib, by injecting P1 trampolines at link time.

## 1. Contract

The gap is closable in full; the decided route is **P1 stub trampolines injected at link time**.

1. In native semantics an rlib `#[export_name]`/`#[no_mangle]` function is a cell of the final link's
   symbol set. Its mirvm dual is that function's **P1 executable entry**: a derivable signature means
   stub code address plus a libffi closure trampoline back into the interpreter.
2. The closure decision set widens from "archive self-closure" to "archive plus the crate graph's
   rlib-exported function set". An undefined symbol found there materializes a P1 entry and injects a
   same-name hidden trampoline, so `-z defs` closure still holds.
3. **Runtime lazy resolution is rejected.** Under RTLD_NOW discipline an undefined `.so` symbol dies
   at `dlopen`; RTLD_LAZY violates the engine's required-libraries discipline; and a P1 stub is absent
   from the process dynamic symbol table, so there is nothing to resolve against.
4. **Linking the rlib as a native object is rejected.** A metadata-only rlib carries no machine code
   at all — the same root cause as dependency `global_asm`.
5. The contract covers every ".a → .so closure gap: symbol in rlib" form for **function** symbols.
   Data symbols (an rlib static referenced from C) must be rejected loudly as "to be opened
   separately", as must signatures the rescue chain cannot derive.
6. Injection fires only on the failure path: when the first link succeeds, all archive behaviour stays
   byte-for-byte unchanged.

Evidence for the shape: wasmtime's `libwasmtime-helpers.a`, whose trampolines call
`resolve_vmctx_memory_ptr` and peers defined in wasmtime's own rlib; and the bzip2-sys vendored
BZ_NO_STDIO assertion stub `bz_internal_error`, an `#[no_mangle] extern "C" fn(c_int)` defined in a
Rust rlib. The minimal reproducer (a build.rs `cc` trampoline calling an `#[export_name]` Rust
definition) currently rejects with "cannot be safely converted to a shared library" and
`undefined reference to 'rust_side_def'` from `ld`.

## 2. Model

### 2.1 Ordering and consumer surface

- **Conversion point**: lowering, before the worklist is drained — the same budget window as
  `global_asm`. The entry budget `fn_entry_addr(inst)` is immediately usable there, and P1 placement
  order reproduces startup order, so addresses are stable across processes.
- **Authoritative name → Instance table**: the linker's exported definitions, keyed by
  `tcx.symbol_name`, covering local definitions plus dependency-crate non-generic exports collected
  through `dependency_formats(Executable)` — exactly the final link's symbol set. Both sides
  canonicalize keys through `canonical_link_name`, stripping `\x01`.
- **Budget**: `linker.fn_entry_addr(inst)`. Dependency-crate functions land automatically in the image
  domain, delta-domain functions in the local domain, and the operation is idempotent.
- **Derivable signature is a precondition.** For `-> !`, as in the bz assertion stub, the layout is a
  ZST, so `FfiKind::Void` is derivable. Non-derivable signatures keep today's loud rejection.

### 2.2 The rescue chain

1. Run the first chain as today (`cc -z defs --whole-archive …`). On failure, evaluate rlib injection
   — **never by parsing tool text output**. Extend the repository's existing ELF64 symbol-table parser
   with an ar member walk and enumerate at binary level the `SHN_UNDEF` global and weak symbols of
   every archive member, skipping LOCAL and the ar symbol-table members.
2. Canonicalize that enumeration and intersect it with the exported-definition table:
   a hit that is a function budgets `fn_entry_addr(inst)` and yields `(name, stub_addr)`; a hit that
   is a static is the loud "data symbol in rlib" error; an empty intersection takes the original error
   path with today's text.
3. Emit the trampoline assembly in the same form as `global_asm`, with `.hidden <name>` so it binds
   only inside the `.so` and never pollutes the process global namespace — under native, an rlib
   definition is a link-time static binding anyway:
   `.globl <name>; .hidden <name>; .type <name>,@function; <name>: movabs rax, <addr>; jmp rax`.
4. Compile the trampoline object with `cc` and fold it into the relink (`archive.a tramp.o -l…`). If
   the relink still fails on symbols outside the intersection, take the original error path with a
   byte-for-byte identical diagnostic.
5. Fold the sorted `(name, addr)` pairs into the cache key's content hash. The first-chain success
   path's key is untouched, so behaviour stays compatible; because P1 code addresses are stable across
   processes, repeated runs of the same module always hit and different modules naturally get
   different keys.
6. Widen the materialization entry point to take the linker as well, at its single call site.

Why this is not ad hoc: the closure set "rlib exported function" is rustc's
`exported_non_generic_symbols`, the local authority for the native final link's symbol set, not a
per-instance allowlist; the trampoline is the link-time materialization of a P1 executable entry, with
hidden visibility as the general guard against global interposition; the two-stage chain is a
deliberate compatibility choice rather than a bypass; and `movabs`+`jmp` is the same form already
accepted for `global_asm`.

## 3. Boundaries

- The closure decision set is ar-member `SHN_UNDEF` intersected with the crate graph's rlib export set,
  enumerated statically at binary level with no dependence on tool text. Unresolved symbols outside
  the intersection — cross-archive duplicate names, a third-party library missing a component — still
  fail the relink and take the original error path.
- `-z defs` is never relaxed: injection only admits "rlib exported function" into the closure
  decision, and an unmatched name is still rejected loudly.
- `.hidden` avoids RTLD_GLOBAL interposition. Two archives referencing the same guest function inject
  the same stub address because the budget is idempotent, so hidden visibility produces no collision.
- Cache: the injection path's key contains the `(name, addr)` pairs, and cold and hot behaviour agree,
  backed by the existing P1 startup-phase rebuild — a trampoline stores only a code address, and the
  entry body is reproduced in the startup phase.
- Not opened: rlib **data** symbols, whose ABS `.set` semantics for data references need separate
  proof. Loudly rejected, to be opened on demand.
- Non-derivable signatures keep today's loud rejection.

## 4. Verification

Every case must be byte-exact on all three dimensions — mirvm default, native `cargo run`, and
`MIRVM_JIT_THRESHOLD=1`.

- **MRE promoted**: rebuild the minimal reproducer as a corpus probe — a helper `.a` trampoline plus
  an `#[export_name]` Rust definition writing back an observation value — and register it in
  `tests/suites/corpus/cases.manifest`.
- **bzip2 C backend green again**: add or restore a driver using the vendored C backend, verifying both
  the symbol rescue of the BZ_NO_STDIO assertion stub through rlib injection and normal
  compress/decompress. The workload's normal path never triggers the assertion callback, so this case
  must not be used to claim the callback panic has executed; the callback is an ordinary `extern "C"`,
  and a real escaping panic must still terminate.
- **`c_wasmtime_wat` face change**: with layer 1 eliminated, the driver's red pattern must move from
  `cannot be safely converted to a shared library`/101 to `inline asm noreturn`/70, executing the
  driver header's stated wiring strategy verbatim.
- **Zero-change regression**: archives whose first link succeeds (rusqlite, aws-lc, libgit2 and peers)
  must stay byte-for-byte identical, checked by the full gate plus `cargo test` plus a byte diff.

## 5. Open items

- rlib **data** symbols: ABS `.set` semantics for data references are unproven. Reopens when a real
  workload has C reference an rlib static.
- Non-derivable signatures keep the loud rejection. Reopens when a real workload needs one.
- Corpus ids: this design cites `c_rlib_sym_probe`, `c_bzip2_pure` and `c_wasmtime_wat`, while the
  manifest currently lists `bzip2_pure`, `bzip2_csys` and `wasmtime_wat`. The names must be reconciled
  when the promoted and split drivers are registered.
