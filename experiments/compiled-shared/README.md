# Compiled shared frame-entry experiment

This experiment uses production `SharedBody` objects and the existing Cranelift service to evaluate
machine code shared by concrete generic instances. `apply.py` patches a disposable source copy;
it adds no product option or cache format to the ordinary checkout. The contract and release gates
belong to [runtime-generics-design.md](../../docs/designs/runtime-generics-design.md).

## Boundary

The concrete interpreter prologue owns argument and return ABI, aligned frame allocation, dictionary
and opaque-local pointers, shadow frame and exception guard. The private compiled entry takes
`(frame_pointer, entry_block, activation_pointer)` and returns the block whose terminator the
interpreter must execute. The activation descriptor borrows the concrete body and cleanup-edge cell
for the duration of that entry, including callbacks and unwinding. Cleanup entered after an
exception remains interpreted.

The transformation rewrites local accesses to the external frame and uses verified ordinary IR.
Index operands are staged in temporary scalar slots. Guest locals remain in memory; this does not
demonstrate a compiler that retains temporaries in registers across blocks. Dictionary loads remain
dynamic. No concrete layout or guest callee is baked into shared code.

Each Engine scans borrowed archive tags and template addresses to reserve internal ABI headers.
It admits only groups with at least two descriptors; singletons keep direct specialization. This
scan neither decodes instance descriptors nor hashes/translates bytecode. Template identity is the
owned or archived object's address, scoped to its immutable owner; structurally equal templates in
different archive copies can occupy separate groups. At the first compile request the worker
decodes one representative, translates and verifies its template, and builds the actual guarded
entry. Unused groups incur no translation. There is no additional lazy cell or lock to settle at
fork. The private placeholders cannot be reached through guest function addresses or guest calls.

The original guest PLT retains the concrete ABI. Shared entry counts and interpreted backedges
nominate internal entries; ordinary functions retain the existing JIT policy. Entry selection is
once per normal activation, so code publication never transfers an already running loop. There is
no OSR. The three candidate modes isolate separate decisions:

| Experiment options | Behavior |
|---|---|
| `MIRVM_SHARED_REGIONS=1` | Lazy shared regions; calls return through the interpreter loop. |
| Above plus `MIRVM_SHARED_REGION_CALLS=1` | Shared code calls the existing concrete adapter implementation directly. |
| Above plus `MIRVM_SHARED_REGION_SPECIALIZE=1` | Hot instances can publish concrete ABI entries for subsequent calls. |

The call bridge extracts the interpreter's direct-call arm into one function used by both paths.
It preserves argument evaluation, sret, scalar/pair writeback, cleanup edges, terminate and main
panic boundaries. Its returned continuation is authoritative; compiled code takes the template's
usual successor when it agrees and otherwise dispatches the returned block. The outer raw catch
still owns cleanup. This removes repeated packed-region returns and re-entries; it does not remove
all adapter cost or the concrete interpreter prologue.

Concrete nomination uses existing per-instance call and interpreted-backedge thresholds, separately
from aggregate template heat. Publication changes the original ABI slot, leaving active shared
frames and internal code valid. Shared compiled loops do not yet collect own-work or backedge
credit. Threshold-one verification can request both forms immediately; it is not the timing policy
or proof of specialization payback. The interpreter work sampler excludes shared compiled entries.

The admitted experiment is plain execution with persistent JIT caching disabled. It refuses trace,
machine-code reload and simultaneous work-budget enforcement. The generic contract covers mixed
layouts, cleanup, recursion, concurrent entries, native callbacks and single-threaded fork. It does
not establish the general lifecycle, signal or trace gate for product integration. Whole-process
time includes preparation, compilation and worker shutdown. Diagnostic ledgers also report lazy
translations, bridges, concrete publications and actual shared code size.

## Reproduce

Use a Linux x86_64 host, the repository's pinned toolchain and a writable experiment directory on
the intended development filesystem. Keep source, Cargo outputs, temporary files and results under
that directory. `MIRVM_HOME` used below must already provide the managed MIR sysroot.

```sh
experiment_dir=/path/to/experiment
mkdir -p "$experiment_dir/source" "$experiment_dir/tmp"
tar -cf - src Cargo.toml Cargo.lock build.rs rust-toolchain.toml experiments/compiled-shared \
    tests/data/programs/{generic_templates,c_serde_json,c_flate2}.rs | \
    tar -xf - -C "$experiment_dir/source"
python3 experiments/compiled-shared/apply.py "$experiment_dir/source"
export TMPDIR="$experiment_dir/tmp"
export CARGO_TARGET_DIR="$experiment_dir/target"
cargo build --manifest-path "$experiment_dir/source/Cargo.toml" --release --locked --bin mirvm
python3 "$experiment_dir/source/experiments/compiled-shared/run.py" \
    "$experiment_dir/target/release/mirvm" "$(rustup which rustc)" \
    /path/to/managed-home "$experiment_dir/results"
python3 "$experiment_dir/source/experiments/compiled-shared/controls.py" \
    "$experiment_dir/target/release/mirvm" "$experiment_dir/results" \
    "$experiment_dir/controls"
```

`run.py` checks native agreement and bound-image reuse for synchronous shared calls, including fork.
Its timing matrix compares interpretation, direct specialization and lazy regions over five
alternating rounds. `controls.py` adds forced-compilation controls and serde/flate corpus inputs.
Dependency caches must be populated for its offline native build. Flate uses a 2 KiB input.

For a comparison against the eager frame-entry prototype, retain that executable and its `run.py`
and `controls.py` result directories, then build this candidate and run:

```sh
python3 "$experiment_dir/source/experiments/compiled-shared/compare.py" \
    "$experiment_dir/target/release/mirvm" /path/to/eager-reference "$(rustup which rustc)" \
    /path/to/reference-results /path/to/reference-controls "$experiment_dir/comparison"
```

`compare.py` checks native agreement for shared calls and concrete promotion, each with separate
normal/fork invocations. It compares direct specialization, eager regions, lazy regions, direct call
bridges and subsequent concrete promotion over five alternating rounds on seven workloads. The
four candidate modes share an executable and bound image. The eager reference has its own prepared
build generation with byte-identical input; both generations are prepared before timing. Every
measured run must hit its image and match native output. A failed preparation assertion invalidates
the run. Ledgers are separate captures and OS page caches are retained.

The synthetic first stdout line follows a completed batch. Flate's first line describes generated
input before compression completes. Raw stdout, phase timings, binary/input hashes and the protocol
remain in the results directory. Results and conclusions belong to
[current-status.md](../../docs/current-status.md#runtime-generics-prototype).
