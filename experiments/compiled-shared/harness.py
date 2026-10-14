"""Bounded subprocess capture and paired measurements for the shared-frame experiment."""
import json
import re
import select
import subprocess
import time


class Runner:
    def __init__(self, root, env):
        self.root = root
        self.env = env
        self.rows = []

    def __call__(self, label, command, extra=None, expected=None, first_byte=False):
        capture_started = time.perf_counter_ns()
        # A compile ledger must not fill a stderr pipe while the first stdout byte is awaited.
        with (self.root / (label + ".err")).open("wb") as diagnostics:
            started = time.perf_counter_ns()
            capture_setup_ms = (started - capture_started) / 1e6
            child = subprocess.Popen(list(map(str, command)), env=dict(self.env, **(extra or {})),
                                     stdout=subprocess.PIPE, stderr=diagnostics, bufsize=0)
            spawn_ms = (time.perf_counter_ns() - started) / 1e6
            first_ms, prefix = None, b""
            try:
                if first_byte:
                    assert select.select([child.stdout], [], [], 120)[0], label
                    prefix = child.stdout.read(1)
                    first_ms = (time.perf_counter_ns() - started) / 1e6
                output, _ = child.communicate(timeout=120)
            finally:
                if child.poll() is None:
                    child.kill()
                    child.communicate()
            wall_ms = (time.perf_counter_ns() - started) / 1e6
        output = prefix + output
        (self.root / (label + ".out")).write_bytes(output)
        log = (self.root / (label + ".err")).read_text(errors="replace")
        assert child.returncode == 0, (label, child.returncode, log[-8000:])
        if expected is not None:
            assert output == expected, (label, output, expected)
        phases = [dict((k, float(v)) for k, v in re.findall(r"([\w-]+)=([\d.]+)ms", line))
                  for line in log.splitlines() if line.startswith("mirvm-timing:")]
        return output, dict(wall_ms=wall_ms, first_ms=first_ms, spawn_ms=spawn_ms,
                            capture_setup_ms=capture_setup_ms, phases=phases), log

    def measure(self, case, command, expected, modes, rounds):
        for repeat in range(rounds):
            for mode, flags in modes if repeat % 2 == 0 else reversed(modes):
                _, row, _ = self(f"{case}-{mode}-{repeat}", command, flags, expected, first_byte=True)
                assert len(row["phases"]) == 1 and "cache-load" in row["phases"][0], row
                assert "frontend" not in row["phases"][0], row
                row.update(case=case, mode=mode, repeat=repeat)
                self.rows.append(row)
                (self.root / "measurements.json").write_text(json.dumps(self.rows, indent=2) + "\n")
                print(json.dumps(row), flush=True)
        for mode, flags in modes:
            self(f"{case}-{mode}-ledger", command, dict(flags, MIRVM_JIT_LEDGER="1"), expected)
