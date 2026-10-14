# Compiler-independent runtime boundary

This probe builds the product library with and without the `compiler` feature. Both forms use the
same package/image readers, input validation, native materialization, Engine, Cranelift JIT, guest
stack and process-exit handling. The normal `mirvm` CLI requires `compiler`, which remains a default
feature. The runner is an experiment entry point, not a replacement CLI.

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

## Integration limits

The compiler-independent feature is an experimental build boundary. Base/dependency layers and program-delta preparation are compiler-only. Other unused producer/tool
helpers still compile and produce dead-code warnings in that configuration; it does not yet meet the
warnings-denied runtime-library gate. The default compiler build keeps its existing lint gate.
`repo-quality` also runs `cargo test --release --no-default-features --features cranelift --lib`.
This executes the runtime/native/telemetry/managed-input test inventory without compiler crates;
only tests of compiler-owned sysroot production remain behind the compiler feature. The ordinary
compiler test and lint gates remain in place. This library gate does not qualify CLI routing.

The probe requires an explicit argument recipe and a prepared sysroot. Ordinary project/script
dispatch and build-script/proc-macro scheduling still need a production handoff contract, including
their distinct compile/guest environments. Capture routing and the full signal/close contract remain
unqualified. The exec protocol covers plain recipes, not those orchestration paths. No persistent compiler process or new artifact format is introduced.
The narrow contract probes do not qualify every library/CLI behavior or select default routing.
