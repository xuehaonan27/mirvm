#!/usr/bin/env python3
"""Check the real shared-frame boundary, then compare three execution policies."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re

from harness import Runner

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("binary", type=Path)
parser.add_argument("rustc", type=Path)
parser.add_argument("managed_home", type=Path)
parser.add_argument("results", type=Path)
args = parser.parse_args()
binary, rustc, managed_home, root = (p.resolve() for p in vars(args).values())
root.mkdir(parents=True, exist_ok=False)
repo = Path(__file__).resolve().parents[2]
home = root / "home"
home.mkdir()
(home / "data").symlink_to(managed_home / "data", target_is_directory=True)
env = {k: v for k, v in os.environ.items() if not k.startswith("MIRVM_")}
env.update(MIRVM_HOME=str(home), MIRVM_OFFLINE="1", MIRVM_TIMING="1",
           MIRVM_GENERIC_TEMPLATES="1", MIRVM_NO_JIT_CACHE="1")


invoke = Runner(root, env)


fixture = repo / "tests/data/programs/generic_templates.rs"
native = root / "native-contract"
invoke("contract-build-native", [rustc, "--edition=2024", "-Awarnings", fixture, "-o", native])
expected, _, _ = invoke("contract-native", [native])
fork_expected, _, _ = invoke("contract-native-fork", [native, "fork"])
invoke("contract-install", [binary, "install", fixture])
strict = dict(MIRVM_SHARED_REGIONS="1", MIRVM_JIT_SYNC="1", MIRVM_JIT_THRESHOLD="1",
              MIRVM_JIT_LEDGER="1", MIRVM_LOG="debug")
for name, guest_args, wanted in [("normal", [], expected), ("fork", ["fork"], fork_expected)]:
    _, _, log = invoke("contract-" + name, [binary, "run", fixture, "--", *guest_args], strict, wanted)
    assert "cache-load=" in log and "frontend=" not in log
    assert "mirvm-shared-regions:" in log
    if name == "normal":
        counters = re.search(r"mirvm-shared-regions: (.*)", log).group(1)
        counters = dict((k, int(v)) for k, v in re.findall(r"(\w+)=(\d+)", counters))
        assert counters["entered_instances"] > counters["compiled"], counters
        assert counters["exits"] > counters["entries"], counters
        assert "generic instance specialized:" not in log, "shared path instantiated concrete IR"
    print("PASS contract", name, "native output and bound-image hit", flush=True)

source = Path(__file__).with_name("workload.rs").resolve()
native = root / "native-workload"
invoke("workload-build-native", [rustc, "--edition=2024", "-O", source, "-o", native])
invoke("workload-install", [binary, "install", source])
modes = [("interpreted", {"MIRVM_JIT": "off"}), ("direct", {}),
         ("regions", {"MIRVM_SHARED_REGIONS": "1"})]
for case in ["sparse", "many", "hot", "tiny", "native"]:
    expected, _, _ = invoke(case + "-native", [native, case])
    command = [binary, "run", source, "--", case]
    invoke.measure(case, command, expected, modes, 5)


protocol = dict(binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),
                input_sha256=hashlib.sha256(source.read_bytes()).hexdigest(),
                scope="Bound-image hit, five alternating rounds, persistent JIT cache disabled, "
                      "first stdout byte after a completed batch, complete wall time includes worker "
                      "shutdown, native outputs checked, separate diagnostic ledgers. "
                      "No OS page-cache flushing. Experimental transformation includes preparation cost.")
(root / "protocol.json").write_text(json.dumps(protocol, indent=2) + "\n")
