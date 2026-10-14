#!/usr/bin/env python3
"""Separate admission decisions, sampling cost, and reference-binary overhead on prepared inputs."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re

from harness import Runner

parser = argparse.ArgumentParser(description=__doc__)
for name in ("binary", "reference", "rustc", "previous_results", "corpus_results", "results"):
    parser.add_argument(name, type=Path)
args = parser.parse_args()
binary, reference, rustc, previous, corpus, root = (p.resolve() for p in vars(args).values())
root.mkdir(parents=True, exist_ok=False)
repo = Path(__file__).resolve().parents[2]
source = Path(__file__).with_name("workload.rs").resolve()
probe = Path(__file__).with_name("policy_workload.rs").resolve()
assert hashlib.sha256(source.read_bytes()).hexdigest() == json.loads((previous / "protocol.json").read_text())["input_sha256"]
env = {k: v for k, v in os.environ.items() if not k.startswith("MIRVM_")}
env.update(MIRVM_HOME=str(previous / "home"), MIRVM_OFFLINE="1", MIRVM_TIMING="1",
           MIRVM_GENERIC_TEMPLATES="1", MIRVM_NO_JIT_CACHE="1")
invoke = Runner(root, env)
fixed = dict(MIRVM_SHARED_REGIONS="1", MIRVM_SHARED_REGION_CALLS="1", MIRVM_SHARED_REGION_SPECIALIZE="1")
profile = dict(fixed, MIRVM_SHARED_REGION_PROFILE="1")
policy = dict(fixed, MIRVM_SHARED_REGION_POLICY="1")
fixture = repo / "tests/data/programs/generic_templates.rs"
native = root / "native-contract"
invoke("contract-build-native", [rustc, "--edition=2024", "-Awarnings", fixture, "-o", native])
expected, _, _ = invoke("contract-native", [native])
fork_expected, _, _ = invoke("contract-native-fork", [native, "fork"])
invoke("contract-install", [binary, "install", fixture])
for mode, flags in [("strict", dict(policy, MIRVM_JIT_SYNC="1", MIRVM_JIT_THRESHOLD="1")),
                    ("policy", policy)]:
    for name, guest_args, wanted in [("normal", [], expected), ("fork", ["fork"], fork_expected)]:
        _, _, log = invoke(f"contract-{mode}-{name}", [binary, "run", fixture, "--", *guest_args],
                           dict(flags, MIRVM_JIT_LEDGER="1"), wanted)
        assert "cache-load=" in log and "frontend=" not in log
        assert f"enforce={'false' if mode == 'strict' else 'true'}" in log
        print("PASS contract", mode, name, flush=True)

native = root / "native-policy"
invoke("policy-build-native", [rustc, "--edition=2024", "-O", probe, "-o", native])
for executable, label in [(binary, "candidate"), (reference, "reference")]:
    for path, name in [(source, "workload"), (probe, "policy")]:
        invoke(label + "-" + name + "-install", [executable, "install", path])
    for case in ("serde", "flate"):
        invoke(label + "-" + case + "-install", [executable, "install", corpus / "inputs" / (case + ".rs")])

modes = [("reference", reference, fixed), ("direct", binary, {}), ("fixed", binary, fixed),
         ("profile", binary, profile), ("policy", binary, policy)]
cases = [(case, source, [case], (previous / (case + "-native.out")).read_bytes())
         for case in ("sparse", "many", "hot", "tiny", "native")]
cases += [(case, corpus / "inputs" / (case + ".rs"), [], (corpus / (case + "-native.out")).read_bytes())
          for case in ("serde", "flate")]
for case in ("single", "sleep"):
    wanted, _, _ = invoke(case + "-native", [native, case])
    cases.append((case, probe, [case], wanted))
for case, path, guest_args, wanted in cases:
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
        if mode == "policy":
            assert "mirvm-shared-policy: enforce=true" in log
            if case in ("single", "sleep"):
                rows = [line for line in log.splitlines() if line.startswith("mirvm-shared-work-row:")
                        and ("7compute" if case == "single" else "12slow_wrapper") in line]
                assert rows, (case, "missing observed guest rows")
                if case == "single":
                    assert len(rows) == 1, rows
                    assert "compiled_entries=0" in rows[0], rows
                else:
                    own = sum(int(re.search(r"interpreted_ns=(\d+)", line).group(1))
                              + int(re.search(r"compiled_ns=(\d+)", line).group(1)) for line in rows)
                    assert own < 50_000_000, ("callee sleep leaked into own-body work", own)

(root / "protocol.json").write_text(json.dumps(dict(
    binary_sha256={"candidate": hashlib.sha256(binary.read_bytes()).hexdigest(),
                   "reference": hashlib.sha256(reference.read_bytes()).hexdigest()},
    input_sha256={case: hashlib.sha256(path.read_bytes()).hexdigest() for case, path, _, _ in cases},
    scope="Five alternating rounds; all samples are fresh-process bound-image hits with native stdout "
          "agreement, no persistent JIT cache and retained OS page caches. Candidate direct, fixed, "
          "sampling-only and work-policy modes use the same executable/images. The preceding fixed "
          "reference has its own prepared build generation. Diagnostic ledgers are separate. "
          "First stdout is a completed synthetic batch; flate prints input information first. "
          "The single-instance probe links two compute instances and executes one. The sleep probe "
          "checks that expensive callees cannot fund their wrapper's compilation."
), indent=2) + "\n")
