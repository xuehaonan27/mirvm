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

The transformation uses verified ordinary IR and retains the interpreter's frame at execution
boundaries. The memory control rewrites each local access to that external frame. With
`MIRVM_SHARED_REGION_SSA=1`, scalar accesses within compiled regions use SSA unless their byte ranges
escape, overlap or have inconsistent widths. Selection reuses the JIT's address-taken analysis;
opaque storage and addressable locals retain memory semantics. Pointer and index values can use
SSA without moving their pointees. Cranelift decides physical register allocation and spilling.
Dictionary loads remain dynamic; no concrete layout or guest callee is baked into shared code.

SSA values are loaded on entry and written back before returning to the interpreter. A direct scalar
call synchronizes the slots its adapter reads or writes, including partial-width overlaps; the normal
successor reloads the affected return slots. A cleanup edge flushes every promoted local before the
call, because cleanup resumes interpreted. Opaque/indirect destinations retain complete
synchronization. Any unexpected adapter continuation also flushes before entering the frame-based
dispatcher. Addressable storage is excluded from promotion, so arbitrary callee/callback memory
writes cannot alias a retained SSA value. Analysis and synchronization code are produced only when
the worker translates a demanded template.

Each Engine scans borrowed archive tags and stored template content IDs to reserve internal ABI headers.
It admits only groups with at least two descriptors; singletons keep direct specialization. This
scan neither decodes instance descriptors nor hashes/translates bytecode. Equal templates in separate
archive copies use one group, while each activation retains its own dictionary, ABI and call bindings.
`MIRVM_SHARED_REGION_LOCAL_IDENTITY=1` provides an address-based grouping control in the same binary.
At the first compile request the worker decodes one representative, translates and verifies its
template, and builds the actual guarded entry. Unused groups incur no translation. There is no additional lazy cell or lock to settle at
fork. The private placeholders cannot be reached through guest function addresses or guest calls.

The original guest PLT retains the concrete ABI. Shared entry counts and interpreted backedges
nominate internal entries; ordinary functions retain the existing JIT policy. Entry selection is
once per normal activation, so code publication never transfers an already running loop. There is
no OSR. The candidate modes isolate separate decisions:

| Experiment options | Behavior |
|---|---|
| `MIRVM_SHARED_REGIONS=1` | Lazy shared regions; calls return through the interpreter loop. |
| Above plus `MIRVM_SHARED_REGION_CALLS=1` | Shared code calls the existing concrete adapter implementation directly. |
| Above plus `MIRVM_SHARED_REGION_SPECIALIZE=1` | Hot instances can publish concrete ABI entries for subsequent calls. |
| All three plus `MIRVM_SHARED_REGION_PROFILE=1` | Sample interpreted/shared own-body work while retaining fixed admission. |
| All three plus `MIRVM_SHARED_REGION_POLICY=1` | Gate shared/concrete compilation using executed diversity and work credit. |
| Any shared mode plus `MIRVM_SHARED_REGION_LOCAL_IDENTITY=1` | Group by template address to measure the effect of cross-archive identity. |
| Any shared mode plus `MIRVM_SHARED_REGION_SSA=1` | Promote nonaliasing scalars across blocks with frame synchronization at adapter/interpreter boundaries. |

The call bridge extracts the interpreter's direct-call arm into one function used by both paths.
It preserves argument evaluation, sret, scalar/pair writeback, cleanup edges, terminate and main
panic boundaries. Its returned continuation is authoritative; compiled code takes the template's
usual successor when it agrees and otherwise dispatches the returned block. The outer raw catch
still owns cleanup. This removes repeated packed-region returns and re-entries; it does not remove
all adapter cost or the concrete interpreter prologue.

Fixed concrete nomination uses existing per-instance call and interpreted-backedge thresholds,
separately from aggregate template heat. Publication changes the original ABI slot, leaving active
shared frames and internal code valid. Synchronous threshold-one verification bypasses policy
admission and can request both forms immediately; it is not evidence of specialization payback.

The experimental policy records actual entries, independently of how many descriptors are linked.
A group needs two executed instances before shared nomination. A hot individual instance can
nominate concrete code directly when it dominates the group and no shared request is pending.
After shared publication, concrete admission draws on that instance's shared execution credit,
rather than its earlier interpretation. Existing entry/backedge thresholds nominate candidates;
sparse retries and per-instance/group pending bits bound deferred requests. Active shared loops do
not request concrete promotion through backedges.

Both sampling modes count the first and every 64th entry separately for interpretation and shared
machine code. The clock starts after frame setup and entry selection. Direct guest calls pause it
around the callee, including native waits and callbacks; argument and return adaptation remain
inside the sample. Interpretation stops sampling after 4,096 instruction/terminator units and pauses
for signal draining. Shared execution stops after 4,096 call bridges or return; a pure compiled loop
has no intermediate sampling poll. Compiled signal delivery is not separately excluded. Weighted
wall time can include scheduling delays and sampler overhead; it is neither exact CPU time nor an
estimate of removable work. Concrete execution is not sampled.

The worker calibrates cost per static IR unit from successful compilations. Admission requires both
instance/group credit and unspent Engine credit; an empty calibration permits the first transition.
Accepted generic transitions spend their actual worker time, including underestimated cost and
failed compilations. Unrelated compilations calibrate cost without spending generic credit. Queue
wait and rejected admission checks are not charged. This budgets compiler work; it does not predict
remaining lifetime or compare the speed of shared and concrete code. Tiny and call-only bodies may
still receive an unprofitable intermediate tier.

The admitted experiment is plain execution with persistent JIT caching disabled. It refuses trace,
machine-code reload and simultaneous `MIRVM_GENERIC_PAYBACK` enforcement. The generic contract covers mixed
layouts, cleanup, recursion, concurrent entries, native callbacks and single-threaded fork. It does
not establish the general lifecycle, signal or trace gate for product integration. Whole-process
time brackets process creation through observed exit, including preparation, compilation and worker
shutdown. Result-file setup, closing and persistence are outside the clock; setup and spawn latency
are reported separately. Diagnostic ledgers also report lazy
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

To evaluate the work policy, retain a fixed-admission executable with lazy groups, call bridges and
concrete promotion, then use the same prepared input directories:

```sh
python3 "$experiment_dir/source/experiments/compiled-shared/policy_compare.py" \
    "$experiment_dir/target/release/mirvm" /path/to/fixed-reference "$(rustup which rustc)" \
    /path/to/reference-results /path/to/reference-controls "$experiment_dir/policy-comparison"
```

`policy_compare.py` checks synchronous bypass and asynchronous policy execution, each with separate
normal/fork invocations. It compares the retained reference with candidate direct, fixed,
sampling-only and policy modes over five alternating rounds on nine workloads. The candidate modes
share an executable/image; the reference has its own prepared generation. `policy_workload.rs`
adds two probes: two linked compute instances with only one executed per run, and generic wrappers
around native sleeps. Separate ledger assertions check executed-instance accounting and exclusion
of callee waits. All valid timing samples, including outliers, are retained. These are fresh-process
bound-image hits with retained OS caches, not cold frontend builds or cold filesystem measurements.

To compare memory-backed shared code with scalar SSA, using the same prepared input directories:

```sh
python3 "$experiment_dir/source/experiments/compiled-shared/ssa_compare.py" \
    "$experiment_dir/target/release/mirvm" "$(rustup which rustc)" \
    /path/to/reference-results /path/to/reference-controls "$experiment_dir/ssa-comparison"
```

`ssa_compare.py` holds address grouping fixed to isolate scalar promotion. It checks synchronous
SSA and asynchronous SSA policy execution, each with separate normal/fork invocations. Nine workloads compare direct specialization, shared memory, shared SSA,
policy with memory and policy with SSA in five alternating rounds. A further three paired rounds
force compilation on many/hot/tiny inputs, separating shared code from policy decisions that might
bypass it. All 243 samples must hit their bound images and agree with native output. Ledgers report
promoted scalar slots, actual shared code bytes, translations and call bridges. No build, lint or
profile work should overlap timing. CPU profiles are separate from this matrix.

To isolate cross-archive template identity in the same binary:

```sh
python3 "$experiment_dir/source/experiments/compiled-shared/identity_compare.py" \
    "$experiment_dir/target/release/mirvm" "$(rustup which rustc)" \
    /path/to/reference-results /path/to/reference-controls "$experiment_dir/identity-comparison"
```

`identity_compare.py` checks image/package execution with memory and SSA, each in normal and fork
invocations. Its 100-sample matrix compares address/content grouping under fixed nomination and
work policy over many/hot/tiny, serde and flate inputs. It holds scalar promotion off during timing.
Separate ledgers report admitted instances, groups, actual compilations, code bytes and preparation
cost. Content grouping may admit former singletons or change executed diversity; fewer groups do
not necessarily mean fewer compilations or better latency. The focused
`shared_regions::tests::cross_archive_identity_preserves_bindings` test can run with synchronous
threshold-one compilation in all four memory/SSA and address/content configurations. Boolean flags
are enabled by presence; omit the local-identity or SSA variable for its disabled control.
