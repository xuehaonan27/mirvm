#!/usr/bin/env python3
"""Compare forced compilation and corpus runs after run.py has established the shared-frame gate."""
import argparse
import hashlib
import json
import os
from pathlib import Path

from harness import Runner

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("binary", type=Path)
parser.add_argument("previous_results", type=Path)
parser.add_argument("results", type=Path)
args = parser.parse_args()
binary, previous, root = (p.resolve() for p in vars(args).values())
root.mkdir(parents=True, exist_ok=False)
repo = Path(__file__).resolve().parents[2]
home = previous / "home"
env = {k: v for k, v in os.environ.items() if not k.startswith("MIRVM_")}
env.update(MIRVM_HOME=str(home), MIRVM_OFFLINE="1", MIRVM_TIMING="1",
           MIRVM_GENERIC_TEMPLATES="1", MIRVM_NO_JIT_CACHE="1")


invoke = Runner(root, env)


source = Path(__file__).with_name("workload.rs").resolve()
strict = dict(MIRVM_JIT_SYNC="1", MIRVM_JIT_THRESHOLD="1")
for case in ("many", "hot", "tiny"):
    invoke.measure(case, [binary, "run", source, "--", case], (previous / (case + "-native.out")).read_bytes(),
            [("forced-direct", strict), ("forced-regions", dict(strict, MIRVM_SHARED_REGIONS="1"))], 3)

native = root / "native"
(native / "src/bin").mkdir(parents=True)
(native / "Cargo.toml").write_text('''[package]
name = "shared-frame-corpus"
version = "0.0.0"
edition = "2024"
[dependencies]
serde_json = "1"
flate2 = "1"
''')
inputs = root / "inputs"
inputs.mkdir()
hashes = {}
for case, filename in [("serde", "c_serde_json.rs"), ("flate", "c_flate2.rs")]:
    source_text = (repo / "tests/data/programs" / filename).read_text()
    if case == "flate":
        source_text = source_text.replace('100 * 1024', '2 * 1024')
    (inputs / (case + ".rs")).write_text(source_text)
    (_, _, rust) = source_text.split('---', 2)
    (native / "src/bin" / (case + ".rs")).write_text(rust)
    hashes[case] = hashlib.sha256(source_text.encode()).hexdigest()
invoke("native-build", ["cargo", "build", "--offline", "--release", "--manifest-path", native / "Cargo.toml"],
       {"CARGO_TARGET_DIR": str(root / "native-target")})
for case in ("serde", "flate"):
    source = inputs / (case + ".rs")
    expected, _, _ = invoke(case + "-native", [root / "native-target/release" / case])
    invoke(case + "-install", [binary, "install", source])
    invoke.measure(case, [binary, "run", source], expected,
            [("interpreted", {"MIRVM_JIT": "off"}), ("direct", {}),
             ("regions", {"MIRVM_SHARED_REGIONS": "1"})], 5)

(root / "protocol.json").write_text(json.dumps(dict(
    binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(), input_sha256=hashes,
    scope="All samples are bound-image hits with no persistent JIT cache and native stdout comparison. "
          "Forced synthetic control: threshold one and synchronous compilation, three alternating pairs. "
          "Corpus: default promotion thresholds, five alternating rounds; flate input limited to 2 KiB. "
          "Ledgers captured separately; first byte and complete time include compilation and teardown."
), indent=2) + "\n")
