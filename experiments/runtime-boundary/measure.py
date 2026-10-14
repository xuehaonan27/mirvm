#!/usr/bin/env python3
"""Measure real package/image execution with and without compiler-library loading."""
import argparse
import hashlib
import json
import os
import signal
from pathlib import Path
import subprocess
import time

parser = argparse.ArgumentParser(description=__doc__)
path_names = ("runtime", "reference", "worker", "mirvm", "rustc", "home", "sysroot", "results")
for name in path_names:
    parser.add_argument(name, type=Path)
parser.add_argument("--timings-only", action="store_true", help="skip the separate handoff and strict-JIT contract probes")
args = parser.parse_args()
runtime, reference, worker, mirvm, rustc, home, sysroot, root = (getattr(args, name).resolve() for name in path_names)
root.mkdir(parents=True, exist_ok=False)
repo = Path(__file__).resolve().parents[2]
env = {k: v for k, v in os.environ.items() if not k.startswith("MIRVM_")}
env.update(MIRVM_HOME=str(home), MIRVM_SYSROOT=str(sysroot), MIRVM_OFFLINE="1", MIRVM_NO_JIT_CACHE="1")
rows = []


def run(label, command, wanted=None, extra=None, cwd=None):
    started = time.perf_counter_ns()
    result = subprocess.run(list(map(str, command)), env=dict(env, **(extra or {})),
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=120, cwd=cwd)
    elapsed = (time.perf_counter_ns() - started) / 1e6
    (root / (label + ".out")).write_bytes(result.stdout)
    (root / (label + ".err")).write_bytes(result.stderr)
    if wanted is not None:
        assert (result.returncode, result.stdout) == wanted, (label, result.returncode, result.stderr[-4000:])
    return result, elapsed


identity, _ = run("runtime-identity", [runtime, "identity"])
other, _ = run("reference-identity", [reference, "identity"])
assert identity.returncode == other.returncode == 0 and identity.stdout == other.stdout
build_id = identity.stdout.decode().split()[0]
for label, binary in (("runtime", runtime), ("reference", reference)):
    deps, _ = run(label + "-dependencies", ["ldd", binary])
    assert deps.returncode == 0
    assert (b"librustc_driver" in deps.stdout) == (label == "reference")
    assert (b"libLLVM" in deps.stdout) == (label == "reference")


def recipe(source, name, extra=()):
    compiler = ["mirvm", str(source), "--edition=2024", "--crate-type=bin", "--sysroot",
                str(sysroot), "-Zmir-opt-level=2", *extra]
    path = root / (name + ".json")
    path.write_text(json.dumps(compiler) + "\n")
    # Same argument-key grammar as image::program, used only to remove this probe's own image.
    key = "\x1f".join([build_id, *compiler]).encode()
    digest = 0xcbf29ce484222325
    for byte in key:
        digest = ((digest ^ byte) * 0x100000001b3) & ((1 << 64) - 1)
    image = home / "cache/execution" / f"{digest:016x}.bin"
    return path, image


source = Path(__file__).with_name("workload.rs").resolve()
request, image = recipe(source, "workload")
native = root / "native"
run("native-build", [rustc, "--edition=2024", source, "-o", native], (0, b""))
goldens = {case: run(case + "-native", [native, case])[0] for case in ("empty", "fib")}
run("prepare", [worker, "prepare", request], (0, b""))
assert image.is_file()
package = root / "workload.mirvm"
packed, _ = run("pack", [mirvm, "pack", source, "-o", package])
assert packed.returncode == 0, packed.stderr[-4000:]
for label, binary in (("runtime", runtime), ("reference", reference)):
    # A nonexistent worker makes an accidental miss fail instead of polluting a hit measurement.
    command = [binary, "image", root / "missing-worker", request, source, "hold"]
    child = subprocess.Popen(list(map(str, command)), env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    try:
        assert child.stdout.readline() == b"ready\n"
        maps = Path(f"/proc/{child.pid}/maps").read_bytes()
        (root / (label + ".maps")).write_bytes(maps)
        assert (b"librustc_driver" in maps) == (label == "reference")
        assert (b"libLLVM" in maps) == (label == "reference")
        out, err = child.communicate(timeout=10)
        assert child.returncode == 0, err
    finally:
        if child.poll() is None:
            child.kill()
            child.communicate()

for case in ("empty", "fib"):
    wanted = (goldens[case].returncode, goldens[case].stdout)
    for kind in ("image-hit", "package", "image-miss"):
        modes = [("reference", reference), ("runtime", runtime), ("monolithic", mirvm)]
        if kind == "image-miss":
            modes.append(("runtime-exec", runtime))
        for repeat in range(5):
            for mode, binary in modes if repeat % 2 == 0 else reversed(modes):
                if kind == "image-miss":
                    image.unlink(missing_ok=True)
                if mode == "monolithic":
                    command = [binary, "run", package if kind == "package" else source, "--", case]
                else:
                    command = ([binary, "package", package, case] if kind == "package" else
                               [binary, "image-exec" if mode == "runtime-exec" else "image",
                                worker if kind == "image-miss" else root / "missing-worker",
                                request, source, case])
                result, elapsed = run(f"{case}-{kind}-{mode}-{repeat}", command, wanted,
                                      dict(MIRVM_TIMING="1") if mode == "monolithic" else None)
                if mode == "monolithic" and kind == "image-hit":
                    assert b"cache-load=" in result.stderr and b"frontend=" not in result.stderr
                rows.append(dict(case=case, kind=kind, mode=mode, repeat=repeat, wall_ms=elapsed))
                (root / "measurements.json").write_text(json.dumps(rows, indent=2) + "\n")
                print(json.dumps(rows[-1]), flush=True)

if not args.timings_only:
    # Replacement must retain the process observed by its caller and propagate signal termination.
    image.unlink(missing_ok=True)
    child = subprocess.Popen(list(map(str, [runtime, "image-exec", worker, request, source, "hold"])),
                             env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    try:
        assert child.stdout.readline() == b"ready\n"
        maps = Path(f"/proc/{child.pid}/maps").read_bytes()
        (root / "exec-miss.maps").write_bytes(maps)
        assert b"librustc_driver" in maps and b"libLLVM" in maps
        child.send_signal(signal.SIGTERM)
        out, err = child.communicate(timeout=10)
        assert child.returncode == -signal.SIGTERM and out == b"", (child.returncode, out, err)
    finally:
        if child.poll() is None:
            child.kill()
            child.communicate()
    assert image.is_file()
    run("exec-hit-no-worker", [runtime, "image-exec", root / "missing-worker", request, source],
        (0, b"ready\n"))

    # A warning-bearing session executes after diagnostics even though it cannot publish an image.
    warning = root / "warning.rs"
    warning.write_text('fn main() { let unused = 0; println!("guest-after-warning"); }\n')
    warning_request, warning_image = recipe(warning, "warning")
    for repeat in range(2):
        direct, _ = run(f"warning-direct-{repeat}", [worker, "run", warning_request, warning])
        actual, _ = run(f"warning-exec-{repeat}", [runtime, "image-exec", worker, warning_request, warning],
                        (direct.returncode, direct.stdout))
        assert actual.stderr == direct.stderr and b"unused variable" in actual.stderr
        assert not warning_image.exists()
    assert direct.returncode == 0 and direct.stdout == b"guest-after-warning\n"
    prepared, _ = run("warning-prepare-route", [runtime, "image", worker, warning_request, warning])
    assert prepared.returncode == 70 and prepared.stdout == b"" and b"did not publish" in prepared.stderr

    # cwd, argument boundaries and runtime environment survive replacement; compile-time env still
    # belongs to input validation. Runtime-only changes must keep hitting without a compiler.
    context = root / "context.rs"
    context.write_text('''fn main() {
        println!("{}|{}|{:?}|{}", env!("BOUNDARY_COMPILE_VALUE"),
            std::env::var("BOUNDARY_RUNTIME_VALUE").unwrap(),
            std::env::args().skip(1).collect::<Vec<_>>(),
            std::fs::read_to_string("input.txt").unwrap());
    }
    ''')
    context_request, _ = recipe(context, "context")
    cwd = root / "cwd"
    cwd.mkdir()
    (cwd / "input.txt").write_text("relative input")
    guest = ["argument with spaces", ""]
    for n, (compile_value, runtime_value) in enumerate((("first", "one"), ("first", "two"), ("second", "three"))):
        extra = dict(BOUNDARY_COMPILE_VALUE=compile_value, BOUNDARY_RUNTIME_VALUE=runtime_value)
        native_context = root / f"context-native-{n}"
        run(f"context-build-{n}", [rustc, "--edition=2024", context, "-o", native_context], (0, b""), extra)
        golden, _ = run(f"context-native-{n}", [native_context, *guest], extra=extra, cwd=cwd)
        run(f"context-exec-{n}", [runtime, "image-exec", root / "missing-worker" if n == 1 else worker,
                                  context_request, context, *guest],
            (golden.returncode, golden.stdout), extra, cwd)

    # Changed inputs must force preparation before any guest side effect.
    edited = root / "edited.rs"
    edited.write_text('fn main() { println!("before"); }\n')
    edited_request, _ = recipe(edited, "edited")
    run("edit-first", [runtime, "image-exec", worker, edited_request, edited], (0, b"before\n"))
    edited.write_text('fn main() { println!("after"); }\n')
    run("edit-invalidate", [runtime, "image-exec", worker, edited_request, edited], (0, b"after\n"))
    edited.write_text('fn main() { println!("must-not-run"); let _: u32 = "invalid"; }\n')
    invalid, _ = run("edit-invalid", [runtime, "image-exec", worker, edited_request, edited])
    assert invalid.returncode != 0 and invalid.stdout == b"" and b"mismatched types" in invalid.stderr

    for case in ("asm_probe", "asm_syscall_probe", "global_asm_guest_fn", "generic_templates", "main_outcome_probe"):
        fixture = repo / "tests/data/programs" / (case + ".rs")
        request, _ = recipe(fixture, case, ["--cfg=runtime_generic_probe"] if case == "generic_templates" else [])
        native = root / (case + "-native")
        built, _ = run(case + "-build", [rustc, "--edition=2024", fixture, "-o", native])
        assert built.returncode == 0, built.stderr[-4000:]
        run(case + "-prepare", [worker, "prepare", request], (0, b""),
            dict(MIRVM_GENERIC_TEMPLATES="1") if case == "generic_templates" else None)
        variants = [[], ["fork"]] if case == "generic_templates" else [[], ["panic"]] if case == "main_outcome_probe" else [[]]
        for n, guest in enumerate(variants):
            expected, _ = run(f"{case}-native-{n}", [native, *guest])
            actual, _ = run(f"{case}-runtime-{n}", [runtime, "image", root / "missing-worker", request, fixture, *guest],
                            (expected.returncode, expected.stdout), dict(MIRVM_JIT_SYNC="1", MIRVM_JIT_THRESHOLD="1"))
            if case == "main_outcome_probe":
                # Run the compiler driver's miss path as well as the independent hit path: its
                # release-build cleanup frame must retain panic classification.
                run(f"{case}-exec-{n}", [runtime, "image-exec", worker, request, fixture, *guest],
                    (expected.returncode, expected.stdout),
                    dict(MIRVM_NO_IR_CACHE="1", MIRVM_JIT_SYNC="1", MIRVM_JIT_THRESHOLD="1"))
                # Native and interpreted panic hooks spell the executing thread differently.
                assert (b"main outcome probe" in actual.stderr) == (b"main outcome probe" in expected.stderr)
            print("PASS contract", case, guest, flush=True)

(root / "protocol.json").write_text(json.dumps(dict(
    build_id=build_id,
    contract_probes=not args.timings_only,
    binary_sha256={name: hashlib.sha256(path.read_bytes()).hexdigest()
                   for name, path in (("runtime", runtime), ("reference", reference), ("worker", worker), ("mirvm", mirvm))},
    input_sha256=hashlib.sha256(source.read_bytes()).hexdigest(),
    scope="Five alternating rounds. Same source/build identity and real loaders/JIT; reference also links compiler libraries. "
          "Monolithic uses the ordinary mirvm CLI, including in-process preparation on misses. "
          "Runtime-exec replaces the runtime with the compiler worker on misses and runs there after compiler teardown. "
          "Fresh processes, retained OS caches, no persistent JIT cache. Image hits cannot invoke a worker. "
          "Misses remove only the named bound image; std, dependency and program products remain prepared. "
          "Whole-process times include preparation and shutdown; prepare-worker misses also include image handoff and revalidation. "
          "Native stdout/exit agreement; package runs use the same package. Maps and diagnostics are separate captures."
), indent=2) + "\n")
