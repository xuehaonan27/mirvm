# Preparation comparison

`compare.py` compares two release binaries using native output agreement and ten alternating rounds
on five inputs: an empty main, fib(32), many generic instances, serde JSON and flate2 with a 2 KiB
input. Both binaries must emit `MIRVM_TIMING`. It uses the managed home and
native corpus outputs established by the compiled-shared experiment's `run.py` and `controls.py`.
The shared compiled tier is disabled; generic templates and ordinary concrete JIT remain enabled.

```sh
python3 experiments/preparation/compare.py \
    /path/to/baseline /path/to/candidate "$(rustup which rustc)" \
    /path/to/reference-results /path/to/reference-controls /path/to/comparison-results
```

Run on a Linux x86_64 host with the pinned toolchain and managed MIR sysroot. Keep source, build,
temporary and result directories on the intended development filesystem. Installation prepares each
binary's own image generation. By default every timing sample must hit its image and emit the
image-load ledger. Add `--miss` to bypass program IR caching only during measurement while retaining
installed std and dependency products; every sample must then report a frontend phase instead of a
cache load. Both paths must match native stdout. A lightweight binary needs its matching
`mirvm-compiler` sibling for installation and misses.
OS caches and heat orders are retained and persistent JIT caching is disabled. Child user/system
CPU time, including waited descendants, is recorded independently of elapsed time. Do not overlap timing
with builds, tests, lints or CPU captures. All valid outliers remain in `measurements.json`.

The whole-process clock starts immediately before process creation and stops after observed exit,
covering command routing, image loading, execution and shutdown. Result-file setup, closing and
persistence are outside this clock; setup and spawn latency are recorded separately.
The image ledger begins after reading the file; its intervals do not sum to complete startup latency.
When present, the nested `mirvm-image-instance` ledger is saved separately as `instance`: it splits
restoration, semantic verification and lazy function-table initialization inside the image ledger's
`instantiate-verify`. These intervals are already included in the outer time, not additional costs.
The first stdout byte is recorded separately and is not necessarily a useful completed result:
flate prints an input description before compression. Profiles are separate diagnostic captures.
`protocol.json` records the hit/miss state, executable/worker and input hashes; raw stdout/stderr
accompany every measurement.

For CPU attribution, prepare the input with the selected binary, then capture fresh processes:

```sh
python3 experiments/preparation/profile.py /path/to/mirvm /path/to/script.rs \
    /path/to/native.out /path/to/managed-home /path/to/FlameGraph /path/to/profile-results
```

Use `--miss` to disable program IR caching while retaining installed std/dependency products;
`--repeats N` selects the number of processes and repeated `--guest-arg VALUE` passes guest arguments.
The instrument checks native stdout, the requested hit/miss path and zero lost samples. It stores
the raw perf capture, driver phase output, symbol report, stacks, an interactive flame graph, and a
JSON summary of thread and symbol self time. `perf`, a Rust-capable `c++filt`, Perl and the two
FlameGraph scripts are required; none is downloaded. JIT maps stay in the result's physical temporary
directory and are resolved only against the sampled PID. Flame widths and summaries are weighted by
the recorded cycle period. Unknown frames remain visible: symbols cannot reconstruct missing caller
unwinding. These instrumented runs identify CPU work, not normal elapsed latency or I/O waits; keep
them separate from the comparison above and builds/checks.

To distinguish functions that were not executed from functions safe to remove, install an input
with `MIRVM_TIMING=1`. `mirvm-image-roots` counts conservative entry/address-taken roots;
`mirvm-image-store` reports their transitive function closure and the metadata/code section sizes.
This walk includes native callbacks, allocator/panic roots and concrete generic call adapters, and
does not decode archived bodies or prune anything. A closure covering every slot rules out simple
static function pruning for that input; it does not imply every function executes during the run.

For a local attribution probe, copy the candidate source to a separate checkout without `.git`,
then select a cached JSON-lines registry index file:

```sh
python3 experiments/preparation/decoder_probe.py /path/to/disposable-source /path/to/index-file
```

This installs one ignored diagnostic test in that copy. It compares the retained value parser with
the direct schema parser, and ordinary byte-sequence decoding with bulk snapshot decoding. A
thread-local allocation counter records allocation/reallocation calls and requested bytes in one
untimed invocation; it is disabled during 32 alternating timing pairs. The input file is read before
measurement, and decoded values must agree. Times include result destruction. Snapshot probes use
16 KiB and 1 MiB synthetic buffers covering every byte value. These measurements isolate decoder
work and do not establish complete startup payback. Keep this probe separate from product binaries
and end-to-end timing; its global allocator exists only in the disposable test executable.

For host waits outside the driver ledger, first install the script with the binary being examined,
then run a bounded syscall capture (Linux and `strace` required):

```sh
python3 experiments/preparation/waits.py /path/to/mirvm /path/to/script.rs \
    /path/to/native.out /path/to/managed-home /path/to/wait-results [GUEST_ARGS...]
```

Six untraced processes verify prepared-image hits; eight traced processes follow all threads and
children with timestamps and syscall durations. All stdout must match the native result. Both pipes
are drained during execution and files persist after exit, so trace writes do not wait on the result
filesystem. Read/write buffer arguments are raw pointers and lengths. Rows list syscalls longer than
20 ms for inspection; overlapping parent/child waits cannot be summed. This is an attribution probe,
not a latency comparison: ptrace overhead changes scheduling and syscall costs, and failure to
reproduce a rare stall does not explain it. Compiler-query children, file/lock waits, and guest/JIT
thread joins must be distinguished by their position in the trace.
