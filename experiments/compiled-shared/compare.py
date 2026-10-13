#!/usr/bin/env python3
"""Check lazy frame entries and compare preparation, call bridges and concrete promotion separately."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re

from harness import Runner

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("binary", type=Path)
parser.add_argument("reference", type=Path)
parser.add_argument("rustc", type=Path)
parser.add_argument("previous_results", type=Path, help="run.py results with native outputs and managed home")
parser.add_argument("corpus_results", type=Path, help="controls.py results with prepared serde/flate inputs")
parser.add_argument("results", type=Path)
args = parser.parse_args()
binary, reference, rustc, previous, corpus, root = (p.resolve() for p in vars(args).values())
root.mkdir(parents=True, exist_ok=False)
repo = Path(__file__).resolve().parents[2]
source = Path(__file__).with_name("workload.rs").resolve()
assert hashlib.sha256(source.read_bytes()).hexdigest() == json.loads((previous / "protocol.json").read_text())["input_sha256"]
assert hashlib.sha256(reference.read_bytes()).hexdigest() == json.loads((previous / "protocol.json").read_text())["binary_sha256"]
env = {k: v for k, v in os.environ.items() if not k.startswith("MIRVM_")}
env.update(MIRVM_HOME=str(previous / "home"), MIRVM_OFFLINE="1", MIRVM_TIMING="1",
           MIRVM_GENERIC_TEMPLATES="1", MIRVM_NO_JIT_CACHE="1")
invoke = Runner(root, env)
lazy = dict(MIRVM_SHARED_REGIONS="1")
calls = dict(lazy, MIRVM_SHARED_REGION_CALLS="1")
tier = dict(calls, MIRVM_SHARED_REGION_SPECIALIZE="1")


def counters(log):
    line = re.search(r"mirvm-shared-regions: (.*)", log).group(1)
    return {k: int(v) for k, v in re.findall(r"(\w+)=(\d+)", line)}


fixture = repo / "tests/data/programs/generic_templates.rs"
native = root / "native-contract"
invoke("contract-build-native", [rustc, "--edition=2024", "-Awarnings", fixture, "-o", native])
expected, _, _ = invoke("contract-native", [native])
fork_expected, _, _ = invoke("contract-native-fork", [native, "fork"])
invoke("contract-install", [binary, "install", fixture])
for mode, flags in [("calls", calls), ("tier", tier)]:
    strict = dict(flags, MIRVM_JIT_SYNC="1", MIRVM_JIT_THRESHOLD="1", MIRVM_JIT_LEDGER="1")
    for name, guest_args, wanted in [("normal", [], expected), ("fork", ["fork"], fork_expected)]:
        _, _, log = invoke(f"contract-{mode}-{name}", [binary, "run", fixture, "--", *guest_args], strict, wanted)
        assert "cache-load=" in log and "frontend=" not in log
        stats = counters(log)
        if name == "normal":
            assert stats["entered_instances"] > stats["compiled"] > 0, stats
            assert stats["bridges"] > 0 and stats["exits"] == stats["entries"], stats
            assert (stats["concrete"] > 0) == (mode == "tier"), stats
            assert stats["translated"] < stats["templates"], stats
        print("PASS contract", mode, name, stats, flush=True)

invoke("workload-install", [binary, "install", source])
invoke("reference-workload-install", [reference, "install", source])
modes = [("direct", binary, {}), ("eager", reference, lazy), ("lazy", binary, lazy),
         ("calls", binary, calls), ("tier", binary, tier)]
cases = [(case, source, [case], (previous / (case + "-native.out")).read_bytes())
         for case in ["sparse", "many", "hot", "tiny", "native"]]
cases += [(case, corpus / "inputs" / (case + ".rs"), [],
           (corpus / (case + "-native.out")).read_bytes()) for case in ["serde", "flate"]]
for case, path, guest_args, wanted in cases:
    if case in ("serde", "flate"):
        invoke(case + "-install", [binary, "install", path])
        invoke(case + "-reference-install", [reference, "install", path])
    for repeat in range(5):
        for mode, executable, flags in modes if repeat % 2 == 0 else reversed(modes):
            _, row, _ = invoke(f"{case}-{mode}-{repeat}", [executable, "run", path, "--", *guest_args],
                               flags, wanted, first_byte=True)
            assert len(row["phases"]) == 1 and "cache-load" in row["phases"][0], row
            assert "frontend" not in row["phases"][0], row
            row.update(case=case, mode=mode, repeat=repeat)
            invoke.rows.append(row)
            (root / "measurements.json").write_text(json.dumps(invoke.rows, indent=2) + "\n")
            print(json.dumps(row), flush=True)
    for mode, executable, flags in modes:
        _, _, log = invoke(f"{case}-{mode}-ledger", [executable, "run", path, "--", *guest_args],
                           dict(flags, MIRVM_JIT_LEDGER="1"), wanted)
        if mode in ("lazy", "calls", "tier"):
            stats = counters(log)
            assert stats["translated"] < stats["templates"], stats
            if mode != "lazy":
                assert stats["exits"] == stats["entries"], stats
            if mode != "tier":
                assert stats["concrete"] == 0, stats
            if case == "hot" and mode == "tier":
                assert stats["concrete"] > 0, stats

(root / "protocol.json").write_text(json.dumps(dict(
    binary_sha256={"candidate": hashlib.sha256(binary.read_bytes()).hexdigest(),
                   "reference": hashlib.sha256(reference.read_bytes()).hexdigest()},
    input_sha256={case: hashlib.sha256(path.read_bytes()).hexdigest() for case, path, _, _ in cases},
    scope="Five alternating rounds per case and policy; candidate modes use the same bound image, native stdout "
          "comparison and no persistent JIT cache. Ledgers are separate captures. Process and engine "
          "time include preparation and worker shutdown. OS page caches retained. First stdout is "
          "after a completed synthetic batch; flate prints input information before compression. "
          "Eager is the preceding prototype binary with its separately prepared build generation; "
          "inputs are byte-identical. Other modes share the candidate binary and bound image. "
          "Concrete promotion uses existing per-instance call and interpreted-backedge thresholds, "
          "not a measured payback policy. Shared compiled loops are not instrumented for hotness."
), indent=2) + "\n")
