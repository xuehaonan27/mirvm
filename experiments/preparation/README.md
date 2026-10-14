# Preparation comparison

`compare.py` compares two release binaries using native output agreement and ten alternating rounds
on five inputs: an empty main, fib(32), many generic instances, serde JSON and flate2 with a 2 KiB
input. Both binaries must emit the `MIRVM_TIMING` image-load ledger. It uses the managed home and
native corpus outputs established by the compiled-shared experiment's `run.py` and `controls.py`.
The shared compiled tier is disabled; generic templates and ordinary concrete JIT remain enabled.

```sh
python3 experiments/preparation/compare.py \
    /path/to/baseline /path/to/candidate "$(rustup which rustc)" \
    /path/to/reference-results /path/to/reference-controls /path/to/comparison-results
```

Run on a Linux x86_64 host with the pinned toolchain and managed MIR sysroot. Keep source, build,
temporary and result directories on the intended development filesystem. Installation prepares each
binary's own image generation. Every timing sample must hit its image and match native stdout.
OS caches and heat orders are retained and persistent JIT caching is disabled. Child user/system
CPU time, including waited descendants, is recorded independently of elapsed time. Do not overlap timing
with builds, tests, lints or CPU captures. All valid outliers remain in `measurements.json`.

The whole-process clock starts immediately before process creation and stops after observed exit,
covering command routing, image loading, execution and shutdown. Result-file setup, closing and
persistence are outside this clock; setup and spawn latency are recorded separately.
The image ledger begins after reading the file; its intervals do not sum to complete startup latency.
The first stdout byte is recorded separately and is not necessarily a useful completed result:
flate prints an input description before compression. Profiles are separate diagnostic captures.
`protocol.json` records executable and input hashes; raw stdout/stderr accompany every measurement.

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
