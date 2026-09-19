# mirvm

mirvm runs Rust without waiting for a full compile: it reuses rustc as its front end and executes
the result on its own abstract machine.

Goals:

1. Make Rust iteration fast — less time waiting for builds and tests.
2. A convenient runner for single-file Cargo scripts.

rustc does parsing, macros, type checking, trait solving and MIR generation. mirvm lowers the
reachable program into its own typed bytecode and runs it with a tree-walking interpreter plus a
method-level Cranelift JIT, which is on by default (`--jit off` disables it).

> Under development, not a complete Rust implementation. Current surface and verified boundaries:
> [docs/current-status.md](docs/current-status.md). Open debt:
> [docs/open-issues.md](docs/open-issues.md). Design contract: [DESIGN.md](DESIGN.md) (Chinese).

Baseline: Linux/ELF/x86_64, toolchain pinned to `nightly-2026-07-02`.

## Build and run

```bash
cargo build --release --locked

./target/release/mirvm run tests/scripts/fib.rs             # single file
./target/release/mirvm run tests/scripts/ecosystem.rs       # single file with dependencies
./target/release/mirvm run path/to/project -- arg1 arg2     # Cargo project
./target/release/mirvm test path/to/project -- --nocapture  # cargo test, without Cargo
./target/release/mirvm pack path/to/project -o app.mirvm    # self-contained package
./target/release/mirvm run app.mirvm
```

A single file declares its dependencies with cargo-script (RFC 3424) frontmatter:

```rust
#!/usr/bin/env mirvm
---
[dependencies]
serde_json = "1"
---
fn main() { /* ... */ }
```

Use the release binary: a debug build distorts every timing result. The first build and run fill a
large rustc and sysroot cache.

## Test

```bash
make test       # daily check
make smoke      # + small real workloads and runtime semantics
make gate       # full gate: strict corpus, dependency image, performance limits
make list       # suite ids, with each suite's one-line purpose
make suite S=<id> ARGS="..."
make projects   # fetch the tests/projects submodules
```

`make` is the interface and `tests/run.sh` is the implementation; nothing else is invoked directly.
All test assets live under `tests/` — see [tests/README.md](tests/README.md).

## Status of the project itself

Remote work (GitHub issues, PRDs, PRs) is paused until the maintainer resumes it. Known gaps, loud
rejection boundaries and all open debt are recorded in [docs/open-issues.md](docs/open-issues.md);
mirvm cannot yet claim to run arbitrary Rust programs. Per-topic contracts live in
[docs/designs/](docs/designs/).
