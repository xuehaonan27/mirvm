# Runtime generics experiment

This is a runnable execution and bounded frontend prototype, not a product option. It installs
research hooks into a disposable MIRVM source copy; the ordinary checkout and its caches are not
modified. The engineering contract and remaining integration gates belong to
[runtime-generics-design.md](../../docs/designs/runtime-generics-design.md). Measurements belong to
[current-status.md](../../docs/current-status.md#runtime-generics-prototype).

The runtime uses ordinary MIRVM bytecode throughout. Cold instance adapters supply an immutable
dictionary to one shared instruction stream. Between complete calls, promotion substitutes its
bindings, verifies the concrete body, and asks the existing JIT worker to compile the same FuncId.
The runner waits for actual machine-code publication. A `shared-jit` control compiles the template
without specializing any instance. Automatic JIT call/backedge requests and the persistent JIT cache
are disabled so they cannot silently change a control's execution tier.

`frontend.rs` exports the generic MIR of `copy_elements` directly, without cloning/instantiating a
body per type. Its admitted operations are fixed-width pointer/usize/isize/bool locals, scalar
use/casts/add/unsigned comparison, branches, `arith_offset`, and `copy_nonoverlapping`. Rustc mono
collection still validates the reachable program. Rustc computes five concrete layout/const
descriptors; the callback retains only owned records, and writes the artifact after compiler
diagnostics and teardown. `semantic.py` checks those descriptors against native Rust and loads the
artifact in fresh executor processes. An invalid instantiation in an unentered branch and an
unsupported MIR shape must both fail without producing a new artifact.

The execution controls also use hand-authored slice/array and Clone/Drop templates. Clone and Drop
are real native Rust witness implementations called with `C-unwind`; the unwind differential
checks destruction and subsequent reentry after a native Rust panic. It does not prove guest panic
ownership, trait resolution, enum layouts or arbitrary Rust generic export. The array control
passes zero as the runtime length, so only the const descriptor can supply its actual length.

The promotion hook is intentionally limited to one Engine, one calling thread, and publication
between calls before compilation. Its process-global override is unsuitable for production:
concurrent activations must pin their instruction stream, and per-Engine state, caches, native
callbacks, fork and trace domains need the design's integration work. An exported JSON file is a
trusted experiment input, not a validated public artifact format. No source profile/cache identity
or compatibility promise is attached to it.

## Reproduce

Use a Linux x86_64 host, the repository's pinned toolchain, and its usual build dependencies. From
the repository root:

```sh
experiment_dir=$(mktemp -d)
tar -cf - src Cargo.toml Cargo.lock build.rs rust-toolchain.toml |
    tar -xf - -C "$experiment_dir"
python3 experiments/runtime-generics/apply.py "$experiment_dir"
cargo build --manifest-path "$experiment_dir/Cargo.toml" --release --bin generic_prototype
python3 experiments/runtime-generics/run.py \
    "$experiment_dir/target/release/generic_prototype" target/generic-prototype/results
python3 experiments/runtime-generics/semantic.py \
    "$experiment_dir/target/release/generic_prototype" \
    experiments/runtime-generics/input.rs target/generic-prototype/semantic
python3 experiments/runtime-generics/report.py target/generic-prototype
```

Use a fresh results directory for each independent run. The scripts write raw stdout/stderr, RSS,
binary identity, protocol and JSON measurements. `run.py` alternates six modes across three timing
repetitions after a bounded correctness matrix. Native mode is an execution-only oracle: it also
constructs an unused Engine, so its whole-process time is not a native startup baseline. Export
phase timers follow mono validation and can reuse its warmed rustc queries.

Preparation scaling uses many synthetic nominal entries with equal descriptors. It measures
adapter construction versus concrete IR construction, not thousands of distinct Rust types or
the cost of semantic analysis. Fragment accounting exposes existing deduplication; adapter pointer
immediates in this transient harness are not a production relocation/storage strategy. Audit
serialization is timed separately and excluded from `first_result_ns`; whole-process time includes
audit work, native oracles, buffer setup and teardown. OS caches are not flushed.
