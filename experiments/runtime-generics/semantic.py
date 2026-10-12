#!/usr/bin/env python3
"""Export actual generic Rust MIR, then execute the owned artifact in fresh processes."""
import argparse
import json
import os
from pathlib import Path
import subprocess

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("binary", type=Path)
parser.add_argument("source", type=Path)
parser.add_argument("output", type=Path)
parser.add_argument("--rustc", default="rustc")
args = parser.parse_args()
out = args.output.resolve()
out.mkdir(parents=True, exist_ok=True)
binary = args.binary.resolve()
source = args.source.resolve()
sysroot = subprocess.check_output([args.rustc, "--print", "sysroot"], text=True).strip()
env = dict(os.environ, MIRVM_JIT="on", MIRVM_JIT_THRESHOLD="4294967295",
           MIRVM_NO_JIT_CACHE="1", MIRVM_HOME=str(out / "home"))


def run(name, command, success=True):
    result = subprocess.run(list(map(str, command)), env=env, capture_output=True, text=True, timeout=45)
    (out / f"{name}.stdout").write_text(result.stdout)
    (out / f"{name}.stderr").write_text(result.stderr)
    if (result.returncode == 0) != success:
        raise SystemExit(f"{name}: unexpected exit {result.returncode}\n{result.stderr}")
    return result


run("native-build", [args.rustc, source, "--edition=2024", "-Copt-level=0", "-Coverflow-checks=off", "-o", out / "native"])
native = run("native", [out / "native"])
expected = sorted(tuple(map(int, line.split())) for line in native.stdout.splitlines())
exports = []
for repeat in range(3):
    path = out / f"export-{repeat}.json"
    run(f"export-{repeat}", [binary, "export", path, source, "--sysroot", sysroot])
    artifact = json.loads(path.read_text())
    layouts = sorted((d["stride"], d["align"], d["length"]) for d in artifact["descriptors"])
    assert layouts == expected, (layouts, expected)
    assert len(artifact["symbols"]) == len(set(artifact["symbols"]))
    exports.append({k: artifact[k] for k in ("export_ns", "descriptors_ns", "validation_ns")})

rows = []
for mode in ["native", "shared", "eager", "tiered", "shared-jit", "eager-jit"]:
    result = run(mode, [binary, mode, "array", 999, 0, 5, 5, 4, 1, 0, out / "export-0.json"])
    row = json.loads(result.stdout)
    assert row["exported"]
    rows.append(row)
assert len({r["costs"]["checksum"] for r in rows}) == 1
assert next(r for r in rows if r["mode"] == "tiered")["costs"]["promoted"] == 5

invalid = out / "invalid.rs"
invalid.write_text(source.read_text().replace("fn main() {", """
fn bad<T>() { const { assert!(size_of::<T>() == 0); } }
fn main() {
    if std::hint::black_box(false) { bad::<u64>(); }
"""))
rejected = out / "rejected.json"
assert not rejected.exists()
result = run("invalid", [binary, "export", rejected, invalid, "--sysroot", sysroot], success=False)
assert "E0080" in result.stderr and not rejected.exists()

unsupported = out / "unsupported.rs"
unsupported.write_text(source.read_text().replace("while index < N {", "while std::hint::black_box(index) < N {"))
rejected = out / "unsupported.json"
assert not rejected.exists()
run("unsupported", [binary, "export", rejected, unsupported, "--sysroot", sysroot], success=False)
assert not rejected.exists()
(out / "summary.json").write_text(json.dumps({
    "exports": exports, "layouts": expected, "rows": rows,
    "invalid_unentered_instance_rejected": True, "unsupported_shape_rejected": True,
    "contract": "One real generic MIR template, five concrete layout/const descriptors; compiler finishes before publication; each executor is a fresh process.",
}, indent=2) + "\n")
print("generic MIR export, five layouts, six execution modes and two rejection checks passed")
