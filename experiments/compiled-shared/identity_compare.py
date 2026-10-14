#!/usr/bin/env python3
"""Measure content identity against address grouping in the same shared-tier binary."""
import argparse
import hashlib
import json
import os
from pathlib import Path

from harness import Runner

parser = argparse.ArgumentParser(description=__doc__)
for name in ("binary", "rustc", "previous_results", "corpus_results", "results"):
    parser.add_argument(name, type=Path)
args = parser.parse_args()
binary, rustc, previous, corpus, root = (p.resolve() for p in vars(args).values())
root.mkdir(parents=True, exist_ok=False)
repo = Path(__file__).resolve().parents[2]
source = Path(__file__).with_name("workload.rs").resolve()
env = {k: v for k, v in os.environ.items() if not k.startswith("MIRVM_")}
env.update(MIRVM_HOME=str(previous / "home"), MIRVM_OFFLINE="1", MIRVM_TIMING="1",
           MIRVM_GENERIC_TEMPLATES="1", MIRVM_NO_JIT_CACHE="1")
invoke = Runner(root, env)
shared = dict(MIRVM_SHARED_REGIONS="1", MIRVM_SHARED_REGION_CALLS="1")
local = dict(shared, MIRVM_SHARED_REGION_LOCAL_IDENTITY="1")
policy = dict(shared, MIRVM_SHARED_REGION_SPECIALIZE="1", MIRVM_SHARED_REGION_POLICY="1")
local_policy = dict(policy, MIRVM_SHARED_REGION_LOCAL_IDENTITY="1")
fixture = repo / "tests/data/programs/generic_templates.rs"
native = root / "native-contract"
invoke("contract-build-native", [rustc, "--edition=2024", "-Awarnings", fixture, "-o", native])
invoke("contract-install", [binary, "install", fixture])
package = root / "contract.mirvm"
invoke("contract-pack", [binary, "pack", fixture, "-o", package])
for label, guest in (("normal", []), ("fork", ["fork"])):
    expected, _, _ = invoke("contract-native-" + label, [native, *guest])
    for mode, flags in (("memory", shared), ("ssa", dict(shared, MIRVM_SHARED_REGION_SSA="1"))):
        strict = dict(flags, MIRVM_JIT_SYNC="1", MIRVM_JIT_THRESHOLD="1", MIRVM_JIT_LEDGER="1")
        _, _, log = invoke(f"contract-{mode}-{label}", [binary, "run", fixture, "--", *guest],
                           strict, expected)
        assert "cache-load=" in log and "frontend=" not in log
        invoke(f"package-{mode}-{label}", [binary, "run", package, "--", *guest], strict, expected)
        print("PASS contract and package", mode, label, flush=True)
cases = [(case, source, [case], (previous / (case + "-native.out")).read_bytes())
         for case in ("many", "hot", "tiny")]
cases += [(case, corpus / "inputs" / (case + ".rs"), [], (corpus / (case + "-native.out")).read_bytes())
          for case in ("serde", "flate")]
for n, path in enumerate(dict.fromkeys(path for _, path, _, _ in cases)):
    invoke(f"install-{n}", [binary, "install", path])
modes = [("local", local), ("content", shared), ("local-policy", local_policy), ("content-policy", policy)]
for case, path, guest, wanted in cases:
    invoke.measure(case, [binary, "run", path, "--", *guest], wanted, modes, 5)
(root / "protocol.json").write_text(json.dumps(dict(
    binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),
    input_sha256={case: hashlib.sha256(path.read_bytes()).hexdigest() for case, path, _, _ in cases},
    scope="Five alternating rounds over five workloads and four modes. Each mode uses the same "
          "binary and prepared image. Fresh-process bound-image hits, native stdout agreement, "
          "retained OS caches and heat orders, no persistent JIT cache. Ledgers are separate. "
          "Address grouping is the control; policy and fixed nomination are separate comparisons. "
          "Complete time includes compilation and shutdown. No build or lint runs overlap timings."
), indent=2) + "\n")
