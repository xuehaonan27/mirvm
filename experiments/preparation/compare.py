#!/usr/bin/env python3
"""Compare preparation costs with native outputs and fresh-process image hits or program misses."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import resource
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "compiled-shared"))
from harness import Runner

parser = argparse.ArgumentParser(description=__doc__)
paths = ("baseline", "candidate", "rustc", "previous_results", "corpus_results", "results")
for name in paths:
    parser.add_argument(name, type=Path)
parser.add_argument("--miss", action="store_true", help="retain installed dependencies but disable program IR caching")
args = parser.parse_args()
baseline, candidate, rustc, previous, corpus, root = (getattr(args, name).resolve() for name in paths)
root.mkdir(parents=True, exist_ok=False)
repo = Path(__file__).resolve().parents[2]
env = {k: v for k, v in os.environ.items() if not k.startswith("MIRVM_")}
env.update(MIRVM_HOME=str(previous / "home"), MIRVM_OFFLINE="1", MIRVM_TIMING="1",
           MIRVM_GENERIC_TEMPLATES="1", MIRVM_NO_JIT_CACHE="1")
invoke = Runner(root, env)
small = repo / "experiments/runtime-boundary/workload.rs"
native = root / "native-small"
invoke("native-build", [rustc, "--edition=2024", "-O", small, "-o", native])
cases = []
for case in ("empty", "fib"):
    wanted, _, _ = invoke(case + "-native", [native, case])
    cases.append((case, small, [case], wanted))
cases += [("many", repo / "experiments/compiled-shared/workload.rs", ["many"],
           (previous / "many-native.out").read_bytes())]
cases += [(case, corpus / "inputs" / (case + ".rs"), [], (corpus / (case + "-native.out")).read_bytes())
          for case in ("serde", "flate")]
binaries = [("baseline", baseline), ("candidate", candidate)]
for mode, binary in binaries:
    for n, path in enumerate(dict.fromkeys(path for _, path, _, _ in cases)):
        invoke(f"{mode}-install-{n}", [binary, "install", path])
if args.miss:
    env["MIRVM_NO_IR_CACHE"] = "1"
rows = []
for case, path, guest, wanted in cases:
    for repeat in range(10):
        for mode, binary in binaries if repeat % 2 == 0 else reversed(binaries):
            before = resource.getrusage(resource.RUSAGE_CHILDREN)
            _, row, log = invoke(f"{case}-{mode}-{repeat}", [binary, "run", path, "--", *guest],
                                 expected=wanted, first_byte=True)
            after = resource.getrusage(resource.RUSAGE_CHILDREN)
            row.update(user_ms=(after.ru_utime - before.ru_utime) * 1000,
                       system_ms=(after.ru_stime - before.ru_stime) * 1000)
            assert len(row["phases"]) == 1, row
            assert ("frontend" in row["phases"][0]) == args.miss, row
            assert ("cache-load" in row["phases"][0]) != args.miss, row
            detail = [line for line in log.splitlines() if line.startswith("mirvm-image-load:")]
            assert len(detail) == (0 if args.miss else 1), log
            row.update(case=case, mode=mode, repeat=repeat)
            if detail:
                row["image"] = dict((k, float(v)) for k, v in re.findall(r"([\w-]+)=([\d.]+)ms", detail[0]))
            instance = [line for line in log.splitlines() if line.startswith("mirvm-image-instance:")]
            assert len(instance) <= 1, log
            if instance:
                row["instance"] = dict((k, float(v)) for k, v in re.findall(r"([\w-]+)=([\d.]+)ms", instance[0]))
            rows.append(row)
            (root / "measurements.json").write_text(json.dumps(rows, indent=2) + "\n")
            print(json.dumps(row), flush=True)
(root / "protocol.json").write_text(json.dumps(dict(
    state="program-miss" if args.miss else "image-hit",
    binary_sha256={mode: hashlib.sha256(binary.read_bytes()).hexdigest() for mode, binary in binaries},
    worker_sha256={mode: hashlib.sha256(binary.with_name("mirvm-compiler").read_bytes()).hexdigest()
                   for mode, binary in binaries if binary.with_name("mirvm-compiler").is_file()},
    input_sha256={case: hashlib.sha256(path.read_bytes()).hexdigest() for case, path, _, _ in cases},
    scope="Ten alternating rounds over five workloads, two binaries with independent prepared-image "
          "generations. All samples match native stdout and the requested hit/miss state. Program misses "
          "retain installed std/dependency products. OS caches and heat orders "
          "retained, persistent JIT cache disabled. No overlapping experiment builds, checks or CPU "
          "captures; shared-host load is uncontrolled. "
          "Whole time brackets process creation through exit, including command routing, loading, execution "
          "and shutdown; result-file setup/close/writes are outside that clock. Capture setup and spawn "
          "latencies are reported separately. Per-child CPU usage "
          "includes waited descendants and is measured separately from elapsed time. Image timing starts "
          "after reading the file and excludes command routing. First byte is not necessarily a "
          "completed useful result, especially for flate's initial input-description line."
), indent=2) + "\n")
