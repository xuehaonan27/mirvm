#!/usr/bin/env python3
"""Profile fresh processes with native output checks and process-local JIT symbol maps."""
import argparse
import bisect
from collections import Counter
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess

HEADER = re.compile(r"(.+?)\s+(\d+)/(\d+)\s+[\d.]+:\s+(\d+)\s+cycles:u:")


def resolve(stacks, temporary):
    mappings = {}
    for path in temporary.glob("perf-*.map"):
        ranges = []
        for line in path.read_text().splitlines():
            address, size, name = line.split(" ", 2)
            ranges.append((int(address, 16), int(size, 16), name))
        ranges.sort()
        mappings[int(path.stem.split("-")[1])] = ([r[0] for r in ranges], ranges)
    pid = None
    lines = []
    for line in stacks.splitlines():
        header = HEADER.match(line)
        if header:
            pid = int(header[2])
        frame = re.match(r"(\s+)([0-9a-f]+)\s+\[unknown\](.*)", line)
        if frame and pid in mappings:
            indent, ip, tail = frame.groups()
            address = int(ip, 16)
            starts, ranges = mappings[pid]
            index = bisect.bisect_right(starts, address) - 1
            if index >= 0 and address < ranges[index][0] + ranges[index][1]:
                line = f"{indent}{ip} {ranges[index][2]}{tail}"
        lines.append(line)
    return "\n".join(lines) + "\n", len(mappings)


def summarize(stacks):
    threads, symbols = Counter(), Counter()
    samples = 0
    for sample in stacks.split("\n\n"):
        lines = sample.splitlines()
        if not lines:
            continue
        header = HEADER.match(lines[0])
        if not header:
            continue
        comm, period = header[1], int(header[4])
        threads[comm] += period
        frame = re.match(r"\s+[0-9a-f]+\s+(.+) \(.+\)$", lines[1]) if len(lines) > 1 else None
        symbol = re.sub(r"\+0x[0-9a-f]+$", "", frame[1]) if frame else "[missing]"
        symbols[(comm, symbol)] += period
        samples += 1
    total = sum(threads.values())
    assert total > 0, "no cycle samples parsed"
    return dict(samples=samples, cycles=total,
                threads=[dict(comm=k, percent=v * 100 / total) for k, v in threads.most_common()],
                self_time=[dict(comm=k[0], symbol=k[1], percent=v * 100 / total)
                           for k, v in symbols.most_common()])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("binary", "source", "native_stdout", "home", "flamegraph_tools", "results"):
        parser.add_argument(name, type=Path)
    parser.add_argument("--miss", action="store_true", help="disable IR caching, retain installed dependencies")
    parser.add_argument("--repeats", type=int, default=12)
    parser.add_argument("--guest-arg", action="append", default=[])
    args = parser.parse_args()
    assert args.repeats > 0
    binary, source, expected, home, tools, root = (
        getattr(args, name).resolve() for name in
        ("binary", "source", "native_stdout", "home", "flamegraph_tools", "results"))
    root.mkdir(parents=True, exist_ok=False)
    temporary = root / "tmp"
    temporary.mkdir()
    env = {k: v for k, v in os.environ.items() if not k.startswith("MIRVM_")}
    env.update(MIRVM_HOME=str(home), MIRVM_OFFLINE="1", MIRVM_TIMING="1", MIRVM_PERF_MAP="1",
               MIRVM_GENERIC_TEMPLATES="1", MIRVM_NO_JIT_CACHE="1", TMPDIR=str(temporary))
    if args.miss:
        env["MIRVM_NO_IR_CACHE"] = "1"
    command = [str(binary), "run", str(source), "--", *args.guest_arg]
    data = root / "perf.data"
    with (root / "stdout").open("wb") as out, (root / "stderr").open("wb") as err:
        child = subprocess.run([
            "perf", "record", "-F", "1999", "-m", "1024", "-e", "cycles:u", "--call-graph",
            "dwarf,16384", "-o", str(data), "--", "bash", "-c",
            'for ((n=0; n<$1; n++)); do "${@:2}" || exit; done',
            "capture", str(args.repeats), *command], env=env, stdout=out, stderr=err, timeout=180)
    assert child.returncode == 0, (root / "stderr").read_text()
    assert (root / "stdout").read_bytes() == expected.read_bytes() * args.repeats
    phases = [line for line in (root / "stderr").read_text().splitlines()
              if line.startswith("mirvm-timing:")]
    assert len(phases) == args.repeats
    assert all(("cache-load=" in line) != args.miss for line in phases), phases

    def demangle(data):
        return subprocess.check_output(["c++filt", "-s", "rust"], input=data).decode()

    report = demangle(subprocess.check_output([
        "perf", "report", "-i", str(data), "--stdio", "--no-children", "--call-graph", "none"]))
    (root / "report.txt").write_text(report)
    assert re.search(r"# Total Lost Samples: 0\b", report), "incomplete capture: inspect report.txt"
    stacks, maps = resolve(demangle(subprocess.check_output([
        "perf", "script", "-i", str(data), "-F", "+pid,+period"])), temporary)
    (root / "stacks").write_text(stacks)
    folded = subprocess.check_output(["perl", str(tools / "stackcollapse-perf.pl")], input=stacks.encode())
    (root / "folded").write_bytes(folded)
    (root / "flame.svg").write_bytes(subprocess.check_output([
        "perl", str(tools / "flamegraph.pl"), "--title", "MIRVM fresh-process user CPU cycles",
        "--countname", "cycles"], input=folded))
    (root / "summary.json").write_text(json.dumps(summarize(stacks), indent=2) + "\n")
    worker = binary.with_name("mirvm-compiler")
    (root / "protocol.json").write_text(json.dumps(dict(
        binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),
        worker_sha256=hashlib.sha256(worker.read_bytes()).hexdigest() if args.miss and worker.is_file() else None,
        source_sha256=hashlib.sha256(source.read_bytes()).hexdigest(),
        native_stdout_sha256=hashlib.sha256(expected.read_bytes()).hexdigest(),
        repeats=args.repeats, program_miss=args.miss, guest_args=args.guest_arg, perf_maps=maps,
        scope="Fresh processes with native stdout agreement. Installed std/dependencies, OS caches "
              "and heat orders retained; persistent JIT caching disabled. Cycles:u at 1999 Hz with "
              "DWARF 16384. JIT addresses resolved only against the sampled process's private map. "
              "Flame widths and self-time summaries use sample periods (cycles), not sample counts. "
              "Unresolved frames remain explicit; maps name code but cannot repair missing caller "
              "unwinding. CPU cycles exclude I/O/scheduling waits; instrumented timings are not "
              "latency controls. Keep builds, tests and unprofiled timings separate."
    ), indent=2) + "\n")


if __name__ == "__main__":
    main()
