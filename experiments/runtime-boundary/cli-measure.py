#!/usr/bin/env python3
"""Compare complete ordinary CLI time with and without compiler linkage from identical sources."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import resource
import statistics
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "compiled-shared"))
from harness import Runner

parser = argparse.ArgumentParser(description=__doc__)
for name in ("full", "light", "rustc", "previous_results", "corpus_results", "results"):
    parser.add_argument(name, type=Path)
args = parser.parse_args()
full, light, rustc, previous, corpus, root = (p.resolve() for p in vars(args).values())
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
binaries = [("full", full), ("light", light)]
for n, path in enumerate(dict.fromkeys(path for _, path, _, _ in cases)):
    invoke(f"install-{n}", [light, "install", path])
rows = []
for state, rounds, flags in (("hit", 10, {}), ("program-miss", 5, {"MIRVM_NO_IR_CACHE": "1"})):
    for case, path, guest, wanted in cases:
        for repeat in range(rounds):
            for mode, binary in binaries if repeat % 2 == 0 else reversed(binaries):
                before = resource.getrusage(resource.RUSAGE_CHILDREN)
                _, row, _ = invoke(f"{state}-{case}-{mode}-{repeat}",
                                   [binary, "run", path, "--", *guest], flags, wanted, first_byte=True)
                after = resource.getrusage(resource.RUSAGE_CHILDREN)
                row.update(user_ms=(after.ru_utime - before.ru_utime) * 1000,
                           system_ms=(after.ru_stime - before.ru_stime) * 1000,
                           state=state, case=case, mode=mode, repeat=repeat)
                assert len(row["phases"]) == 1, row
                assert ("cache-load" in row["phases"][0]) == (state == "hit"), row
                rows.append(row)
                (root / "measurements.json").write_text(json.dumps(rows, indent=2) + "\n")
                print(json.dumps(row), flush=True)
summary = []
for state in ("hit", "program-miss"):
    for case, _, _, _ in cases:
        for mode, _ in binaries:
            values = [row for row in rows if (row["state"], row["case"], row["mode"]) == (state, case, mode)]
            summary.append(dict(state=state, case=case, mode=mode, samples=len(values),
                **{key: statistics.median(row[key] for row in values)
                   for key in ("wall_ms", "first_ms", "user_ms", "system_ms")},
                min_ms=min(row["wall_ms"] for row in values), max_ms=max(row["wall_ms"] for row in values)))
(root / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
(root / "protocol.json").write_text(json.dumps(dict(
    binary_sha256={mode: hashlib.sha256(binary.read_bytes()).hexdigest() for mode, binary in binaries},
    input_sha256={case: hashlib.sha256(path.read_bytes()).hexdigest() for case, path, _, _ in cases},
    scope="Identical-source full and lightweight ordinary CLI builds share prepared products. "
          "Ten alternating rounds of fresh-process bound-image hits and five rounds of program "
          "misses over five workloads, 150 native-matching samples. Program misses disable IR "
          "caching while retaining installed std/dependency products; these are not empty-store "
          "builds or OS-page-cache-cold runs. OS caches and heat orders retained; persistent JIT "
          "cache disabled. No overlapping experiment builds, checks or CPU profiles; shared-host "
          "load is uncontrolled. Whole time covers spawn "
          "through exit; result persistence is outside that clock. Child CPU includes waited "
          "descendants. First byte need not be a completed result. All valid outliers retained."
), indent=2) + "\n")
