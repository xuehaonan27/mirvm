# MIRVM Agent Guide

`AGENTS.md` links to this file (`CLAUDE.md`); edit this shared source.
`mirvm` is a Rust project building a Rust interpreter with JIT.

## Behavioral Guidelines

Behavioral guidelines to reduce common LLM coding mistakes, combined with the
project-specific instructions below.

**Tradeoff:** These guidelines bias toward caution over speed. For trivial tasks,
use judgment.

### 1. Think Before Coding

**Don't assume. Don't hide confusion. Surface tradeoffs.**

Before implementing:

- Read the affected module and its tests. Use the code map and linked design
  docs to locate ownership boundaries.
- State your assumptions explicitly. If uncertain, ask.
- If multiple interpretations exist, present them; don't pick silently.
- If a simpler approach exists, say so. Push back when warranted.
- If something is unclear, stop. Name what's confusing. Ask.

### 2. Simplicity First

**Minimum code that solves the problem. Nothing speculative.**

- No features beyond what was asked.
- No abstractions for single-use code.
- No "flexibility" or "configurability" that wasn't requested.
- No error handling for impossible scenarios.
- If you write 200 lines and it could be 50, rewrite it.

Ask yourself: "Would a senior engineer say this is overcomplicated?" If yes,
simplify.

### 3. Surgical Changes

**Touch only what you must. Clean up only your own mess.**

When editing existing code:

- Don't "improve" adjacent code, comments, or formatting.
- Don't refactor things that aren't broken.
- Match existing style, even if you'd do it differently.
- If you notice unrelated dead code, mention it; don't delete it.
- Preserve unrelated work already present in the checkout.

When your changes create orphans:

- Remove imports, variables, and functions that YOUR changes made unused.
- Don't remove pre-existing dead code unless asked.

The test: Every changed line should trace directly to the user's request.

### 4. Goal-Driven Execution

**Define success criteria. Loop until verified.**

Transform tasks into verifiable goals:

- "Add validation" -> "Write tests for invalid inputs, then make them pass."
- "Fix the bug" -> "Write a test that reproduces it, then make it pass."
- "Refactor X" -> "Ensure tests pass before and after."

For multi-step tasks, state a brief plan:

```text
1. [Step] -> verify: [check]
2. [Step] -> verify: [check]
3. [Step] -> verify: [check]
```

Strong success criteria let you loop independently. Weak criteria ("make it
work") require constant clarification.

Run the narrowest relevant checks during development, then the applicable
repository checks below. Report what ran and any checks blocked by missing
hardware, permissions, dependencies, or credentials.

**These guidelines are working if:** fewer unnecessary changes in diffs, fewer
rewrites due to overcomplication, and clarifying questions come before
implementation rather than after mistakes.

## Core Concepts

- **What it is**: not a compiler for a new language. It implements a Rust *abstract machine*:
  `rustc` is embedded as a library so language semantics stay upstream-exact, and mirvm supplies
  the memory model, threads, FFI boundary and execution engine. `DESIGN.md` is the contract,
  `docs/designs/ram-spec.md` the semantic spec.
- **Lowering** (`src/lower/`): the `after_analysis` callback walks rustc MIR and emits mirvm's own
  typed bytecode IR (`src/vm/engine/ir.rs`), resolved through a `Linker` worklist into a `Module`
  (funcs, frozen statics/const pool, TLS, exports, inline-asm sites).
- **Execution** (`src/vm/engine/`): interpreter by default, plus a method-level Cranelift JIT that
  publishes translated functions on a call-count threshold. Both must agree byte-for-byte with
  native output; the interpreter is the reference.
- **Images**: a std base image, per-dependency images and an L2 engine-IR cache
  (`src/baseimage.rs`, `src/depsimage.rs`, `src/ircache.rs`) are key-chained so a warm run skips
  the whole rustc session.
- **Cargo**: `src/cargoless/` is mirvm's own resolver/scheduler (no cargo at run time), while
  `src/cargo_shim.rs` keeps a real-Cargo compat track that acts as the behavioural judge.

## Build and Validate

- Build with `cargo build --release --locked`. Execute and benchmark only the release binary;
  a debug build distorts every timing gate.
- The only test entry is `./tests/run.sh`: `fast` = daily commit check, `smoke` = adds small
  real-world/corpus loads, `gate` = full final gate. `./tests/run.sh list` enumerates suites;
  `tests/README.md` documents them.
- Corpus cases are registered in `tests/suites/corpus/cases.manifest` alone; a driver file with no
  registration is a loud gate failure. A new corpus driver must pass three-dimension byte-equality
  (mirvm default / native `cargo run` / `MIRVM_JIT_THRESHOLD=1`).
- Green means an observable output or invariant was checked; an expected red locks its failure
  reason. "Approved", "implemented" and "tests pass" are three different claims.
- `cargo check --all-targets` does **not** build `tsan/` (it opts out of the workspace); only
  `runtime.tsan` does. One case:
  `cd tsan && MIRVM_BUILD_ID=0000000000000000 RUSTFLAGS="-Zsanitizer=thread" cargo +nightly-2026-07-02 run -Zbuild-std --target x86_64-unknown-linux-gnu --release -- <case-id>`.
- `demo/jit_unwind_probe.rs` (30k panic+catch) takes ~2.5 min under mirvm versus ~0.3 s native and
  dominates `fast`; do not mistake it for a hang. Timing gates can flake under load — re-run on a
  quiet machine before calling it a regression.

## Configuration and Conventions

- Use Rust 2024, existing style, and tracing initialized only in binaries:
  `info` for lifecycle, `debug` for internals, `warn` for recoverable issues,
  `error` for unrecoverable failures. Update schemas/config/docs with contract changes.
- Write code, comments, doc comments and user-visible messages in English. Design
  documents under `docs/` stay Chinese.
- A code comment describes the code in front of it: say what it does and why the
  non-obvious parts are that way (invariants, preconditions, hazards, units,
  ownership). No milestone/slice/design-document citations, no "was X, now Y"
  narration, no restating the identifier. Keep `TODO`/`FIXME`/`NOTE`/`SAFETY` for
  open work and hazards.
- Use Conventional Commit prefixes (`feat:`, `fix:`, `refactor:`, `ci:`, `chore:`).

## Code Map

| Path | Owns |
|---|---|
| `src/cli/` | command dispatch, the rustc driver + callbacks, the cargo runner / `RUSTC` shim entry |
| `src/lower/` | MIR -> mirvm bytecode: `collect.rs` (mono closure), `func/` (per-instance emit), `linker/` (ids/frozen/queue), `frame.rs` (layout), `asm.rs`/`global_asm.rs`, `rebase.rs` |
| `src/vm/engine/` | bytecode IR + verifier, interpreter (`interp/`), Cranelift JIT (`jit/`), frozen arena, code arena and entry stubs, `ctx/` (thread/TLS/Engine identity), signals, backtrace, FFI, native/MC images |
| `src/vm/` | thin facade; must stay free of `rustc_private` (the TSan harness re-compiles it file by file) |
| `src/cargoless/` | manifest/lockfile/resolve/registry/git/vendor, compile scheduling (`schedule/`, `driver/`), build scripts, rustflags |
| `src/pack/` | `.mirvm` package format, instantiation and lifecycle |
| `src/telemetry/` | capture sessions, event stream, ledger |
| `src/baseimage.rs`, `src/depsimage.rs`, `src/ircache.rs` | the three cache layers and their key chains |
| `src/sysroot.rs` | self-built MIR sysroot and its stamps |
| `src/os/`, `src/arch/` | the only places allowed to touch the host platform (Linux, x86_64) |

## Constraints to Preserve

- `src/vm/` stays free of `rustc_private`: the TSan harness shares those files via `#[path]`.
  Files shared that way must not contain implicit child modules — a bare `mod child;` will not
  resolve; make children sibling files (e.g. `capture_session.rs`).
- Inline-asm stubs must register their full, zero-terminated `.eh_frame` in one `__register_frame`
  call; per-FDE registration breaks multi-level JIT unwind.
- Adding a `FuncId`-bearing field means auditing the rebase consumers: exports, fn_addrs, ids,
  entry_stub_sites, custom_alloc_shims.
- `MIRVM_SEGV_DUMP` and `MIRVM_JIT_DEBUG` are deliberate diagnostic knobs; do not remove them.
- `MIRVM_BUILD_ID` keys every cache layer, so a rebuild invalidates base/deps/L2 images by design.

## Details by Topic

**Host environment.** Development happens on the Linux x86_64 container `dev-cpu-hg`; the macOS
checkout can edit and review but cannot build (no writable cargo registry cache). The container
ships no Rust: install `nightly-2026-07-02` with the components listed in `rust-toolchain.toml`
(`rustc-dev`, `rust-src`, `llvm-tools`, `rustfmt`, `clippy`). The pinned compiler sources live at
`~/.rustup/toolchains/nightly-2026-07-02-*/lib/rustlib/rustc-src/rust/compiler` — note
`rustc-src/`, not `src/`, which holds only the library.

**Network.** Anything that fetches must run under `withproxy` (a `~/.profile` function); do not
export a proxy globally. `harness`-spawned cargo does not inherit `http_proxy`, so it needs
`[http] proxy` in `~/.cargo/config.toml`, otherwise every cold dependency costs a 30 s timeout and
goes red. mirvm's own registry does not read the cargo cache either, so dependency resolution needs
the network even with a warm cargo. Conversely the local HTTP fixture registries in
`contracts.cargoless-{sources,git}` must bypass the proxy: `tests/support/harness.sh` appends
`127.0.0.1,localhost` to `no_proxy`, and a hand-run suite must do the same or those cases go red.
A whole suite run without the proxy fails with `HTTP fetch failed ... Network is unreachable`.

**Tooling the suites need.** `rg` is the assertion tool of seven suites and is not preinstalled;
a missing `rg` is not a SKIP but a wall of false reds. `nasm` is absent, which makes a few corpus
candidates unjudgeable.

**Commits.** A commit may be auto-pushed to `origin main` within ~20 s, but it does not always
fire: check with `git ls-remote origin main` and push manually when it has not (direct GitHub
access from the macOS checkout often fails; the container's `withproxy` works). Never rewrite an
existing commit (`--amend`, `rebase`, `filter-branch`) — that forks local `main` from
`origin/main`; append a new commit instead.

Keep this guide concise; put implementation details in the linked docs or code.