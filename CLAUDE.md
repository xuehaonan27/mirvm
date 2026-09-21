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

Write documentation accurate and precise. 
Do not pile up sentences or charts. 
Modify wrong or out-dated statements instead of appending things.

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

### 5. Do not repeat tests
Do not run tests again and again during developing. Run once after a batch of
modifications.

## Core Concepts
[TBD].

## Build and Validate

[TBD].

## Configuration and Conventions

- Use Rust 2024, existing style, and tracing initialized only in binaries:
  `info` for lifecycle, `debug` for internals, `warn` for recoverable issues,
  `error` for unrecoverable failures. Update schemas/config/docs with contract changes.
- Write code, comments, doc comments, user-visible messages, `README.md` and
  everything under `docs/` in English. `DESIGN.md` is still Chinese; convert it
  only when asked.
- A code comment describes the code in front of it: say what it does and why the
  non-obvious parts are that way (invariants, preconditions, hazards, units,
  ownership). No milestone/slice/design-document citations, no "was X, now Y"
  narration, no restating the identifier. Keep `TODO`/`FIXME`/`NOTE`/`SAFETY` for
  open work and hazards.
- Use Conventional Commit prefixes (`feat:`, `fix:`, `refactor:`, `ci:`, `chore:`).
- **Never pile up records.** A TODO document lists open items only: the moment an
  item is finished, delete its entry in the same change — no "closed" rows, no
  moved-out notes, no dates of past work, no per-item history. A status document
  describes the present, not the road taken. History lives in git, nowhere else.

## Code Map

[TBD].

## Constraints to Preserve

- An axis holds only what varies along it. `src/arch/` is the CPU (instruction encoding and
  execution, registers and feature facts, the ELF machine identity, the assembly vocabulary);
  `src/os/` is the platform outside mirvm, the C library and the kernel together (pthread, `dlopen`,
  `errno`; mappings, `/proc`, process and signal primitives);
  `src/os_arch/<os>_<arch>/` is the two at once (signal frames and restorers, raw syscall sequences,
  the fixed-address layout, kernel TLS). An object-file byte layout is none of them — it does not vary
  with either axis — so it belongs to the layer that produces and parses those objects
  (`src/native/{elf,ar}.rs`), with `e_machine` in `arch` and the loader's half in `os`. Each axis
  declares its surface in its own `mod.rs` and dispatches through one `#[cfg]` ladder, so a call
  site names one path on every target. A new architecture or platform is a
  directory plus an arm in that ladder; the axis refusals name what a pair must implement.
  `repo-quality`'s `platform boundary` gate enforces the boundary.
- A frozen surface stays byte-identical unless its own test changes: guest stdout/stderr, rustc
  diagnostics, cargo-mirror lines, and the first line of `cache status`. The lowering cache key is
  the FNV of the final assembly text, so a whitespace change in emitted asm silently invalidates
  every entry.

## Details by Topic

[TBD].

Keep this guide concise; put implementation details in the linked docs or code.