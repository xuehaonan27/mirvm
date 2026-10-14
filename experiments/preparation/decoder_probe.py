#!/usr/bin/env python3
"""Install the decoder allocation probe in a disposable checkout, then run its one ignored test."""
import argparse
import os
from pathlib import Path
import shutil
import subprocess

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("checkout", type=Path)
parser.add_argument("index", type=Path)
args = parser.parse_args()
root = args.checkout.resolve()
if (root / ".git").exists():
    raise SystemExit("use a disposable source copy without .git")
registry = root / "src/cargoless/registry.rs"
source = registry.read_text()
if "mod preparation_probe;" in source:
    raise SystemExit("probe already installed")
shutil.copyfile(Path(__file__).with_name("decoders.rs"), root / "src/cargoless/preparation_probe.rs")
registry.write_text(source + '\n#[cfg(test)]\n#[path = "preparation_probe.rs"]\nmod preparation_probe;\n')
env = dict(os.environ, PREPARATION_INDEX_PATH=str(args.index.resolve()))
subprocess.run(["cargo", "test", "--release", "--locked", "--lib",
                "cargoless::registry::preparation_probe::preparation_decoders_profile",
                "--", "--exact", "--ignored", "--nocapture", "--test-threads=1"],
               cwd=root, env=env, check=True)
