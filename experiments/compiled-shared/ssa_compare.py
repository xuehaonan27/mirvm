#!/usr/bin/env python3
"""Compare shared frame memory with scalar SSA, including admission and corpus costs."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re

from harness import Runner

parser = argparse.ArgumentParser(description=__doc__)
for name in ("binary", "rustc", "previous_results", "corpus_results", "results"):
    parser.add_argument(name, type=Path)
args = parser.parse_args()
binary, rustc, previous, corpus, root = (p.resolve() for p in vars(args).values())
root.mkdir(parents=True, exist_ok=False)
repo = Path(__file__).resolve().parents[2]
source = Path(__file__).with_name("workload.rs").resolve()
probe = Path(__file__).with_name("policy_workload.rs").resolve()
assert hashlib.sha256(source.read_bytes()).hexdigest() == json.loads((previous / "protocol.json").read_text())["input_sha256"]
env = {k: v for k, v in os.environ.items() if not k.startswith("MIRVM_")}
env.update(MIRVM_HOME=str(previous / "home"), MIRVM_OFFLINE="1", MIRVM_TIMING="1",
           MIRVM_GENERIC_TEMPLATES="1", MIRVM_NO_JIT_CACHE="1")
invoke = Runner(root, env)
memory = dict(MIRVM_SHARED_REGIONS="1", MIRVM_SHARED_REGION_CALLS="1")
ssa = dict(memory, MIRVM_SHARED_REGION_SSA="1")
policy = dict(memory, MIRVM_SHARED_REGION_SPECIALIZE="1", MIRVM_SHARED_REGION_POLICY="1")
policy_ssa = dict(policy, MIRVM_SHARED_REGION_SSA="1")
fixture = repo / "tests/data/programs/generic_templates.rs"
native = root / "native-contract"
invoke("contract-build-native", [rustc, "--edition=2024", "-Awarnings", fixture, "-o", native])
invoke("contract-install", [binary, "install", fixture])
for label, guest in (("normal", []), ("fork", ["fork"])):
    expected, _, _ = invoke("contract-native-" + label, [native, *guest])
    for mode, flags in (("strict", dict(ssa, MIRVM_JIT_SYNC="1", MIRVM_JIT_THRESHOLD="1")),
                        ("policy", policy_ssa)):
        _, _, log = invoke(f"contract-{mode}-{label}", [binary, "run", fixture, "--", *guest],
                           dict(flags, MIRVM_JIT_LEDGER="1"), expected)
        assert "cache-load=" in log and "frontend=" not in log
        if mode == "strict":
            assert int(re.search(r"scalar_slots=(\d+)", log).group(1)) > 0
        print("PASS contract", mode, label, flush=True)

native = root / "native-policy"
invoke("policy-build-native", [rustc, "--edition=2024", "-O", probe, "-o", native])
cases = [(case, source, [case], (previous / (case + "-native.out")).read_bytes())
         for case in ("sparse", "many", "hot", "tiny", "native")]
cases += [(case, corpus / "inputs" / (case + ".rs"), [], (corpus / (case + "-native.out")).read_bytes())
          for case in ("serde", "flate")]
for case in ("single", "sleep"):
    wanted, _, _ = invoke(case + "-native", [native, case])
    cases.append((case, probe, [case], wanted))
for n, path in enumerate(dict.fromkeys(path for _, path, _, _ in cases)):
    invoke(f"install-{n}", [binary, "install", path])
modes = [("direct", {}), ("memory", memory), ("ssa", ssa), ("policy", policy), ("policy-ssa", policy_ssa)]
for case, path, guest, wanted in cases:
    invoke.measure(case, [binary, "run", path, "--", *guest], wanted, modes, 5)

# Synchronous controls force actual shared machine code; policy timing alone can bypass that tier.
strict = dict(MIRVM_JIT_SYNC="1", MIRVM_JIT_THRESHOLD="1")
for case in ("many", "hot", "tiny"):
    wanted = (previous / (case + "-native.out")).read_bytes()
    invoke.measure("forced-" + case, [binary, "run", source, "--", case], wanted,
                   [("memory", dict(memory, **strict)), ("ssa", dict(ssa, **strict))], 3)

(root / "protocol.json").write_text(json.dumps(dict(
    binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),
    input_sha256={case: hashlib.sha256(path.read_bytes()).hexdigest() for case, path, _, _ in cases},
    scope="Five alternating rounds over nine workloads and five modes, plus three forced shared-code "
          "pairs on many/hot/tiny. All modes share one binary and prepared image per input. "
          "Fresh-process bound-image hits, native stdout agreement, retained OS caches and heat orders, "
          "no persistent JIT cache. Ledgers are separate; complete time includes compilation and shutdown. "
          "Synthetic first stdout follows a completed batch; flate prints input information first."
), indent=2) + "\n")
