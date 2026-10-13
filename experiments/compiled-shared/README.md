# Compiled shared frame-entry experiment

This experiment uses production `SharedBody` objects and the existing Cranelift service to evaluate
one machine-code body shared by concrete generic instances. `apply.py` patches a disposable source
copy; it adds no product option or cache format to the ordinary checkout. The contract and release
gates belong to [runtime-generics-design.md](../../docs/designs/runtime-generics-design.md).

## Boundary

The concrete interpreter prologue retains ownership of the activation: argument and return ABI,
aligned frame allocation, dictionary and opaque-local pointers, shadow frame and exception guard.
The compiled entry takes `(frame_pointer, entry_block)` and returns the block whose non-branch
terminator must run. It compiles ordinary statements, branches and loops. Calls, return-value
writeback and cleanup use the existing interpreter boundary and the instance's bound call adapter.
Cleanup entered after an exception remains interpreted.

The transformation rewrites local accesses to the external frame and uses ordinary verified IR.
Index operands are staged in temporary scalar slots before their statement. This conservative
representation deliberately materializes all guest locals in memory; it does not demonstrate the
performance of a future shared-entry compiler that keeps temporaries in registers across blocks.
Dictionary loads remain dynamic. No type-specific frame size, field offset or callee is baked into
the shared code. Structural template hashes group instructions, including across archive chunks;
instance descriptors and original guest FuncIds stay distinct.

Each Engine owns its mapping and appended internal code entries. The original guest PLT continues
to use its ABI-correct c2i trampoline; the experiment never installs a frame entry into a guest ABI
slot. Shared entry counts nominate compilation across instances, and existing backedge requests
map to the same internal entry. Ordinary non-shared functions retain the existing JIT policy. A
compiled entry is selected once at normal activation entry, so a loop already running in the
interpreter does not transfer when compilation finishes. There is no OSR or later concrete tier.

Preparation currently decodes all shared descriptors, groups them and translates each distinct
template before guest entry. Ordinary function archives remain lazy. Whole-process measurements
include this setup, the unchanged interpreter prologue, region transitions and worker shutdown.
This intentionally exposes the overhead that a lazy product integration would need to remove.

The admitted experiment is plain execution with persistent JIT caching disabled. It refuses trace,
machine-code reload and simultaneous work-budget enforcement. Synchronous threshold-one execution
is a correctness control, not the timing policy. Existing guest cleanup, recursion, concurrent
entries, native callbacks and single-threaded fork are exercised by the generic contract. That
contract does not establish a general lifecycle, signal or trace gate for product integration.
Use the region counters, phase timings and total compile ledger for this comparison. The existing
generic own-body sampler surrounds mixed execution in this patch, so its estimate is not an
interpreter-only measurement and is not used to claim specialization payback.

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

`run.py` requires a fresh results directory. It compares the complete generic contract with native
under synchronous shared compilation, checks bound-image reuse and runs fork separately. Diagnostic
counters must show more entered concrete instances than compiled templates, region exits around
calls and no on-demand concrete instruction generation during the normal contract run.

The timing matrix compares interpretation, direct instance specialization and shared regions on the
same executable and bound image. Five alternating rounds cover sparse instances, 64 repeatedly used
type/const instances, two hot instances, zero-iteration tiny bodies and native-call-heavy wrappers.
The first stdout line follows a completed batch; process time includes all remaining work and
teardown. Native output is checked for every run. Ledgers are separate diagnostic captures, and OS
page caches are retained. Raw output, phase timings, binary/input hashes and the protocol remain in
the results directory.

`controls.py` separates nomination policy from compilation reuse: three alternating pairs force
both compiled forms from the first call, including all compilation in process time. It then builds
native serde/flate references and compares the default policies on prepared bound images over five
rounds. Flate uses the corpus program with the input limit reduced to 2 KiB. The first flate output
describes its generated input, so it is not a completed compression result. Dependency caches must
be populated for this offline run. Measurements and conclusions belong to
[current-status.md](../../docs/current-status.md#runtime-generics-prototype).
