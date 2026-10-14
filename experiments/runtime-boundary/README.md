# Compiler-independent runtime boundary

This probe builds the product library with and without the `compiler` feature. Both forms use the
same package/image readers, input validation, native materialization, Engine, Cranelift JIT, guest
stack and process-exit handling. The ordinary `mirvm` CLI requires `cli`; `compiler` implies it and
remains enabled by default. The recipe runner and the optional lightweight ordinary CLI provide
separate experiments over the same runtime boundary.

`src/native/artifact/assembly.rs` owns materialization of already-generated inline/global assembly.
Compiler-side rendering stays in `lower`; the moved materialization code preserves emitted text
and content keys. Lowering's diagnostic vocabulary is available without compiler crates so loading
failures retain their existing classifications. Compiler-session collection is feature-gated while
input-manifest validation remains available to the runtime.

Native archive metadata discovery and Rust-symbol recovery live in `archive/compiler.rs`. The
common archive materializer validates bytes and links objects without rustc types; a caller can
retry a failed link with compiler-resolved symbols. Native archive and Engine lifecycle fixtures
therefore run in both builds, including the same interposition bridge and loader.

## Protocol

`runtime-runner package PATH [ARGS...]` runs a self-contained package. `runtime-runner image WORKER
RECIPE GUEST_ARGV...` reads a JSON array containing the complete compiler argument vector, including
argv[0]. It pins the selected sysroot generation and looks up the ordinary bound execution image.
On a miss it runs `runtime-compiler prepare RECIPE`, waits for compiler diagnostics and required publication
to finish, then looks up and validates the image again before guest entry. The worker delegates to
the ordinary prepare driver. Guest argv is applied after validation and does not affect image keys.

`runtime-runner image-exec WORKER RECIPE GUEST_ARGV...` uses the same hit path. On a miss it
replaces itself with `runtime-compiler run RECIPE GUEST_ARGV...`. That worker uses the normal driver
and executes after compiler diagnostics and session destruction, without returning to a parent to
reload the image. This run retains compiler-library mappings. Replacement preserves the process
PID, cwd, environment and signal termination observed by the caller. Compiler arguments and guest
arguments remain separate; no shell reparsing occurs.

Warning-bearing sessions deliberately do not publish reusable images. The prepare/reload route
therefore reports a missing image and does not enter the guest. The exec route preserves the
ordinary driver's behavior: emit diagnostics and run without caching. Every subsequent source run
emits its warnings again. A compilation error prevents guest entry on both routes.

Transfer uses the existing owned image and relocation representation. The worker passes no native
address, compiler allocation or compiler session to the runtime. A changed source/environment/native
input must miss through the same validator. A compilation failure produces no guest side effects;
successful worker exit without a current image is an error.

Compiler and runtime builds from this source share a build identity; omitting compiler services
does not change the bytecode contract. `identity` prints that identity and target for the harness.
The dependency list and an executing guest's process mappings must both show that the independent
runner loads neither `librustc_driver` nor LLVM. The reference runner retains those dependencies and
uses the same entry protocol. The ordinary monolithic CLI is a third control, including in-process
preparation on a miss.

## Reproduce

Use the pinned toolchain on Linux x86_64. Keep build outputs, temporary files and results in the
intended development filesystem. The managed home and its selected immutable MIR sysroot generation
must already exist; this experiment does not build a sysroot or resolve a project.

```sh
experiment_dir=/path/to/experiment
mkdir -p "$experiment_dir/bin" "$experiment_dir/tmp"
export TMPDIR="$experiment_dir/tmp"
export CARGO_TARGET_DIR="$experiment_dir/target"
cargo build --release --locked --bin mirvm --example runtime-compiler --example runtime-runner
cp "$CARGO_TARGET_DIR/release/mirvm" "$experiment_dir/bin/"
cp "$CARGO_TARGET_DIR/release/examples/runtime-compiler" "$experiment_dir/bin/"
cp "$CARGO_TARGET_DIR/release/examples/runtime-runner" "$experiment_dir/bin/compiler-runner"
cargo build --release --locked --no-default-features --features cranelift --example runtime-runner
cp "$CARGO_TARGET_DIR/release/examples/runtime-runner" "$experiment_dir/bin/runtime-runner"
python3 experiments/runtime-boundary/measure.py \
    "$experiment_dir/bin/runtime-runner" "$experiment_dir/bin/compiler-runner" \
    "$experiment_dir/bin/runtime-compiler" "$experiment_dir/bin/mirvm" "$(rustup which rustc)" \
    /path/to/managed-home /path/to/selected-sysroot-generation/payload "$experiment_dir/results"
```

The matrix uses five alternating rounds for a minimal-output workload and recursive `fib(32)`, each
through image hits, packages and image misses. Misses also compare exec replacement, for 100 timing
samples in total. The empty workload prints `ready`. Every sample is a
fresh process and must agree with native stdout and exit status. Hit runs cannot start a compiler
worker; ordinary CLI hits must report cache loading without frontend work. Misses remove only the
probe's named bound image; installed std, program deltas and other prepared products are retained.
These are prepared-product misses, not empty-store builds. OS caches are retained and persistent
JIT caching is disabled. Complete time includes child startup, preparation, image transfer,
revalidation and worker/Engine shutdown where those steps apply. Exec misses include replacement
and in-process execution. Heat orders and OS page caches are retained. Run timing separately from
builds, linting and CPU profiling; `--timings-only` skips the separate contract probes when those
have already qualified the exact binaries. All valid outliers remain in the data.

Separate probes inspect mappings during guest execution, invalidate an edited source, reject a
compile error before guest output, and exercise inline assembly, syscall stubs, global assembly
calling guest code, generic templates, concurrent entries, callbacks, fork and panic exit. The
generic artifact is prepared with sharing enabled and executed without enabling the exporter.
Additional probes cover warning-bearing sessions, source and compile-time environment invalidation,
runtime-only environment changes without a worker, cwd, empty/spaced arguments, exec-miss mappings
and SIGTERM status. Main return/panic classification also runs through the compiler miss path.
Results and binary hashes remain outside git; conclusions belong to
[current-status.md](../../docs/current-status.md#runtime-boundary-prototype).

## Ordinary CLI

The lightweight CLI retains project/script resolution, build-script/proc-macro scheduling, input
validation and command reports. Valid package/image hits execute without compiler libraries. A
miss replaces the process with a matching compiler worker, transferring exact arguments, scope,
prepare/pack/capture state, unit inputs and the Cargo runner's original guest environment/cwd.
The managed-input lease survives exec; the worker restores close-on-exec before guest execution.
Private request files are consumed once. Diagnostic capture transfers its attachment and finalizer
to the worker. See [the design](../../docs/designs/frontend-boundary.md#5-proposed-handoff) for ownership.

```sh
cargo build --release --locked --bin mirvm --bin mirvm-compiler
cp "$CARGO_TARGET_DIR/release/mirvm" "$experiment_dir/bin/mirvm-full"
cp "$CARGO_TARGET_DIR/release/mirvm-compiler" "$experiment_dir/bin/mirvm-compiler"
cargo build --release --locked --no-default-features --features cli,cranelift --bin mirvm
cp "$CARGO_TARGET_DIR/release/mirvm" "$experiment_dir/bin/mirvm-light"
python3 experiments/runtime-boundary/cli.py \
    "$experiment_dir/bin/mirvm-full" "$experiment_dir/bin/mirvm-light" \
    "$experiment_dir/bin/mirvm-compiler" "$(rustup which rustc)" \
    /path/to/managed-home "$experiment_dir/cli-contracts"
python3 experiments/runtime-boundary/cli-measure.py \
    "$experiment_dir/bin/mirvm-full" "$experiment_dir/bin/mirvm-light" "$(rustup which rustc)" \
    /path/to/compiled-shared-results /path/to/corpus-results "$experiment_dir/cli-timings"
```

The worker is resolved from `MIRVM_COMPILER` or beside the lightweight binary. It must match the
runtime's source/toolchain build identity. Misses retain compiler mappings through guest exit;
hits require no worker. `cache` commands also use the worker. No persistent compiler process or
new artifact format is introduced. The default build remains compiler-linked.

`cli.py` checks both dependency tracks, host proc macros/build scripts, frontmatter, source and
compile/runtime environment changes, private request modes, input leases across exec, executing
process maps, cwd/arguments, warning/error diagnostics, capture, packaging, interpreter/strict-JIT
panic status, SIGTERM/PID continuity and worker rejection. Panic text normalizes only its process
ID. Native environment bytes round-trip through worker requests; the existing Cargo build sidecar
still requires a UTF-8 environment, so that track uses a UTF-8 value. Relative frontmatter path
dependencies resolve against the materialized script project; this fixture uses an absolute path.

`cli-measure.py` takes ten alternating hit rounds and five program-miss rounds over minimal output,
fib(32), generic calls, serde and flate, checking native stdout and phase routing. Program misses use
`MIRVM_NO_IR_CACHE=1` while retaining installed std/dependencies; they are not empty-store builds.
Both binaries share identical prepared products. Persistent JIT caching is disabled; OS caches and
heat orders remain. Whole time brackets process creation through exit. Experiment builds, checks
and profiling run separately; shared-host load is uncontrolled and all valid outliers remain.

The pure runtime library (`cranelift` alone) also excludes dependency-resolution/fetching and report
dependencies. Session-dependent artifact producers follow `compiler`; fixture producers remain in
the tests that need them. Instance layouts, serialized formats and emitted assembly are shared.
`repo-quality` lints all three feature configurations with warnings denied and runs release runtime
library tests. These focused experiments do not qualify every CLI/library behavior, distribution
layout, cache failure or full signal/fork/capture interaction, and do not select default routing.
