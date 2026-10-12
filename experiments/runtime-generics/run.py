#!/usr/bin/env python3
"""Run bounded native differentials and alternating execution measurements."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import time

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("binary", type=Path)
parser.add_argument("output", type=Path)
parser.add_argument("--repeats", type=int, default=3)
args = parser.parse_args()
binary = args.binary.resolve()
out = args.output.resolve()
out.mkdir(parents=True, exist_ok=True)
env = dict(os.environ, MIRVM_JIT="on", MIRVM_JIT_THRESHOLD="4294967295",
           MIRVM_NO_JIT_CACHE="1", MIRVM_HOME=str(out / "home"))
modes = ["native", "shared", "eager", "tiered", "shared-jit", "eager-jit"]
rows = []


def run(group, case, mode, values, repeat=0):
    key = f"{group}-{case}-{mode}-{repeat}"
    start = time.perf_counter_ns()
    result = subprocess.run(["/usr/bin/time", "-f", "%M", "-o", str(out / f"{key}.rss"),
                             str(binary), mode, *map(str, values)],
                            env=env, capture_output=True, text=True, timeout=45)
    wall = time.perf_counter_ns() - start
    (out / f"{key}.stdout").write_text(result.stdout)
    (out / f"{key}.stderr").write_text(result.stderr)
    if result.returncode:
        raise SystemExit(f"{key}: exit {result.returncode}\n{result.stderr[-4000:]}")
    row = json.loads(result.stdout)
    row.update(group=group, case=case, repeat=repeat, wall_ns=wall,
               max_rss_kib=int((out / f"{key}.rss").read_text()))
    rows.append(row)
    (out / "measurements.json").write_text(json.dumps(rows, indent=2) + "\n")
    print(key, "ok", flush=True)
    return row


# Each mixed case really calls the same template with different concrete layouts.
checks = {
    "mixed-copy": ["copy", 999, 17, 5, 5, 4, 17, 0],
    "mixed-array": ["array", 999, 17, 5, 5, 4, 17, 0],
    "mixed-clone-unwind": ["clone", 999, 7, 3, 3, 4, 7, 1],
    "empty-copy": ["copy", 999, 0, 5, 5, 3, 1, 0],
    "single-long-loop": ["copy", 8, 16384, 8, 1, 1, 64, 0],
}
for case, values in checks.items():
    reference = None
    for mode in modes:
        row = run("check", case, mode, values)
        reference = row["costs"]["checksum"] if reference is None else reference
        assert row["costs"]["checksum"] == reference, (case, mode)
        if mode == "tiered":
            assert row["costs"]["promoted"] == (0 if case == "single-long-loop" else values[4])
        if values[-1]:
            assert row["costs"]["unwind_checked"]

workloads = {
    "sparse-preparation": ["array", 8, 1, 4096, 1, 1, 1024, 0],
    "short-calls": ["copy", 8, 1, 64, 1, 2048, 64, 0],
    "copy-loop": ["copy", 8, 256, 64, 1, 256, 1024, 0],
    "clone-loop": ["clone", 64, 32, 64, 1, 64, 128, 0],
    "dense-cold": ["array", 8, 32, 64, 64, 2, 1024, 0],
}
for repeat in range(args.repeats):
    for case, values in workloads.items():
        reference = None
        for mode in (modes if repeat % 2 == 0 else list(reversed(modes))):
            row = run("timing", case, mode, values, repeat)
            reference = row["costs"]["checksum"] if reference is None else reference
            assert row["costs"]["checksum"] == reference, (case, mode)

(out / "protocol.json").write_text(json.dumps({
    "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
    "repeats": args.repeats, "rows": len(rows),
    "scope": "Hand-authored owned templates; existing interpreter and JIT; no rustc template exporter.",
    "policy": "Serial, entry-only, synchronous promotion after per-instance element-work threshold.",
    "controls": "No JIT cache or automatic call/backedge requests; shared-jit compiles the template only.",
    "timing": "Native rows compare kernel execution only; all modes construct an Engine. Audit serialization is separately recorded. Whole-process wall includes startup, audit, oracle, buffers and close.",
    "semantics": "Byte copy and const length; native Rust Clone/Drop witnesses and native Rust C-unwind, not guest panic ownership or a general trait resolver.",
}, indent=2) + "\n")
