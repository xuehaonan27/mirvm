#!/usr/bin/env python3
"""Capture bounded startup syscall traces without writing trace files during execution."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import time

parser = argparse.ArgumentParser(description=__doc__)
for name in ("binary", "source", "native_stdout", "home", "results"):
    parser.add_argument(name, type=Path)
parser.add_argument("guest_args", nargs=argparse.REMAINDER)
args = parser.parse_args()
binary, source, expected_path, home, root = (
    getattr(args, name).resolve() for name in ("binary", "source", "native_stdout", "home", "results"))
root.mkdir(parents=True, exist_ok=False)
temporary = root / "tmp"
temporary.mkdir()
env = {k: v for k, v in os.environ.items() if not k.startswith("MIRVM_")}
env.update(MIRVM_HOME=str(home), MIRVM_OFFLINE="1", MIRVM_TIMING="1",
           MIRVM_GENERIC_TEMPLATES="1", MIRVM_NO_JIT_CACHE="1", TMPDIR=str(temporary))
expected = expected_path.read_bytes()
command = [str(binary), "run", str(source), "--", *args.guest_args]
rows = []
for mode, count in (("pipe", 6), ("strace", 8)):
    for index in range(count):
        traced = command if mode == "pipe" else [
            "strace", "-f", "-ttt", "-T", "-s", "200",
            "-e", "raw=read,write,pread64,pwrite64,readv,writev", *command]
        started = time.perf_counter_ns()
        child = subprocess.Popen(traced, env=env, stdout=subprocess.PIPE,
                                 stderr=subprocess.PIPE, start_new_session=True)
        try:
            output, diagnostics = child.communicate(timeout=35)
        except subprocess.TimeoutExpired:
            os.killpg(child.pid, signal.SIGKILL)
            output, diagnostics = child.communicate()
        elapsed = (time.perf_counter_ns() - started) / 1e6
        label = f"{mode}-{index}"
        (root / (label + ".out")).write_bytes(output)
        (root / (label + ".err")).write_bytes(diagnostics)
        assert child.returncode == 0 and output == expected, (label, child.returncode)
        log = diagnostics.decode(errors="replace")
        if mode == "pipe":
            phases = [line for line in log.splitlines() if line.startswith("mirvm-timing:")]
            assert len(phases) == 1 and "cache-load=" in phases[0], (label, phases)
            assert "frontend=" not in phases[0], (label, phases)
        slow = []
        for line in log.splitlines():
            match = re.search(r"<(\d+\.\d+)>$", line)
            if match and float(match[1]) > .02:
                slow.append(line)
        row = dict(label=label, wall_ms=elapsed, slow_syscalls=slow)
        rows.append(row)
        (root / "rows.json").write_text(json.dumps(rows, indent=2) + "\n")
        print(json.dumps(dict(label=label, wall_ms=elapsed, slow_count=len(slow))), flush=True)
(root / "protocol.json").write_text(json.dumps(dict(
    binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),
    source_sha256=hashlib.sha256(source.read_bytes()).hexdigest(),
    guest_args=args.guest_args,
    scope="Diagnostic only: six untraced then eight ptrace-instrumented fresh processes. "
          "Native stdout asserted; untraced runs must hit prepared images. Pipe capture is drained "
          "during execution; traces persist after exit. Read/write buffers use raw pointer/length "
          "arguments. OS caches retained, persistent JIT cache disabled. Syscalls over 20 ms are "
          "listed for inspection; their overlapping durations must not be summed as elapsed time. "
          "Tracing overhead precludes using these samples as ordinary latency measurements."
), indent=2) + "\n")
