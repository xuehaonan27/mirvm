#!/usr/bin/env python3
"""Qualify ordinary CLI hits and compiler-worker misses on Linux using a prepared managed home."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import select
import signal
import subprocess
import re


parser = argparse.ArgumentParser(description=__doc__)
for name in ("full", "light", "worker", "rustc", "home", "results"):
    parser.add_argument(name, type=Path)
args = parser.parse_args()
full, light, worker, rustc, home, root = (p.resolve() for p in vars(args).values())
root.mkdir(parents=True, exist_ok=False)
env = {k: v for k, v in os.environ.items() if not k.startswith("MIRVM_")}
env.update(MIRVM_HOME=str(home), MIRVM_OFFLINE="1", MIRVM_NO_JIT_CACHE="1",
           MIRVM_GENERIC_TEMPLATES="1", CARGO_TARGET_DIR=str(root / "cargo-target"),
           BOUNDARY_BUILD="compile-one", BOUNDARY_RUNTIME="runtime-one",
           BOUNDARY_RAW="native-\udcff")
env["PATH"] = str(rustc.parent) + os.pathsep + env["PATH"]
rows = []


def run(label, command, extra=None, expected=None, code=0):
    result = subprocess.run(list(map(str, command)), env=dict(env, **(extra or {})), cwd=root,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=120)
    (root / (label + ".out")).write_bytes(result.stdout)
    (root / (label + ".err")).write_bytes(result.stderr)
    assert result.returncode == code, (label, result.returncode, result.stderr[-6000:])
    if expected is not None:
        assert result.stdout == expected, (label, result.stdout, expected)
    rows.append(dict(label=label, exit=result.returncode))
    (root / "checks.json").write_text(json.dumps(rows, indent=2) + "\n")
    print(label, flush=True)
    return result


# The wrapper executes before the compiler and checks the lease at the actual exec boundary.
wrapper = root / "worker-wrapper"
worker_log = root / "worker.jsonl"
wrapper.write_text("""#!/usr/bin/env python3
import fcntl, json, os, pathlib, stat, sys
row = dict(pid=os.getpid(), action=sys.argv[3])
if sys.argv[3] == '__compiler-run':
    path = pathlib.Path(sys.argv[4])
    assert stat.S_IMODE(path.stat().st_mode) == 0o600
    request = json.loads(path.read_bytes())
    assert request['pid'] == os.getpid()
    fd = request['input_lease_fd']
    lockpath = pathlib.Path(os.environ['MIRVM_HOME']) / '.managed-inputs.lock'
    assert os.fstat(fd).st_ino == lockpath.stat().st_ino
    with lockpath.open('rb') as check:
        try:
            fcntl.flock(check, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            pass
        else:
            raise AssertionError('input lease lost across exec')
    row.update(units=len(request['units']), lease=True)
with open(os.environ['BOUNDARY_WORKER_LOG'], 'a') as output:
    output.write(json.dumps(row) + '\\n')
os.execv(os.environ['BOUNDARY_WORKER'], [os.environ['BOUNDARY_WORKER'], *sys.argv[1:]])
""")
wrapper.chmod(0o700)
env.update(MIRVM_COMPILER=str(wrapper), BOUNDARY_WORKER=str(worker),
           BOUNDARY_WORKER_LOG=str(worker_log))
missing = dict(MIRVM_COMPILER=str(root / "absent-worker"))
source = root / "process.rs"
source.write_text(r'''
fn main() {
    println!("{}|{}|{}|{:?}|{:?}", env!("BOUNDARY_BUILD"),
        std::env::var("BOUNDARY_RUNTIME").unwrap(), std::env::current_dir().unwrap().display(),
        std::env::args().skip(1).collect::<Vec<_>>(), std::env::var_os("BOUNDARY_RAW").unwrap());
    if let Some(path) = std::env::var_os("BOUNDARY_MAPS") {
        std::fs::write(path, std::fs::read("/proc/self/maps").unwrap()).unwrap();
    }
}
''')
native = root / "process-native"
run("native-build", [rustc, "--edition=2024", "-O", source, "-o", native])
wanted = run("native", [native, "space arg", ""]).stdout
command = ["run", source, "--", "space arg", ""]
run("file-miss", [light, *command], dict(BOUNDARY_MAPS=str(root / "miss.maps")), wanted)
assert "librustc_driver" in (root / "miss.maps").read_text()
run("file-hit", [light, *command], dict(missing, BOUNDARY_MAPS=str(root / "hit.maps")), wanted)
assert "librustc_driver" not in (root / "hit.maps").read_text()
assert "libLLVM" not in (root / "hit.maps").read_text()
runtime_two = dict(BOUNDARY_RUNTIME="runtime-two")
wanted_two = run("native-runtime-two", [native, "space arg", ""], runtime_two).stdout
run("runtime-env-hit", [light, *command], dict(missing, **runtime_two), wanted_two)
source.write_text(source.read_text().replace('println!("{}|', 'println!("edited:{}|'))
run("source-edit", [light, *command], expected=b"edited:" + wanted)
run("compile-env-edit", [light, *command], dict(BOUNDARY_BUILD="compile-two"),
    b"edited:" + wanted.replace(b"compile-one", b"compile-two"))

# Project scheduling includes a target dependency, a native proc macro and build-script output.
project = root / "project"
for name in ("app", "dep", "derive"):
    (project / name / "src").mkdir(parents=True)
(project / "dep/Cargo.toml").write_text('[package]\nname="boundary-dep"\nversion="0.1.0"\nedition="2024"\n')
(project / "dep/src/lib.rs").write_text('pub fn id<T>(x: T) -> T { x }\n')
(project / "derive/Cargo.toml").write_text('[package]\nname="boundary-derive"\nversion="0.1.0"\nedition="2024"\n[lib]\nproc-macro=true\n')
(project / "derive/src/lib.rs").write_text('extern crate proc_macro;\n#[proc_macro]\npub fn identity(x: proc_macro::TokenStream) -> proc_macro::TokenStream { x }\n')
app = project / "app"
(app / "Cargo.toml").write_text('''[package]
name="boundary-app"
version="0.1.0"
edition="2024"
[dependencies]
boundary-dep={path="../dep"}
boundary-derive={path="../derive"}
''')
(app / "build.rs").write_text('fn main() { println!("cargo::rustc-env=BOUNDARY_GENERATED=from-build-script"); }\n')
(app / "src/main.rs").write_text('''fn main() {
    println!("{} {} {} {:?}", boundary_derive::identity!(boundary_dep::id(123u32)),
        env!("BOUNDARY_GENERATED"), std::env::var("BOUNDARY_RUNTIME").unwrap(),
        std::env::var_os("BOUNDARY_RAW").unwrap());
}
''')
run("project-native-build", [rustc.parent / "cargo", "build", "--release", "--offline",
                            "--manifest-path", app / "Cargo.toml"])
project_wanted = run("project-native", [root / "cargo-target/release/boundary-app"]).stdout
for track in ("self", "cargo"):
    # The existing Cargo sidecar serializes UTF-8 build environments before runtime handoff.
    flags = dict(MIRVM_DEPS=track, BOUNDARY_RAW="native-utf8") if track == "cargo" else dict(MIRVM_DEPS=track)
    track_wanted = run(track + "-project-native", [root / "cargo-target/release/boundary-app"], flags).stdout
    run(track + "-project-miss", [light, "run", app], flags, track_wanted)
    run(track + "-project-hit", [light, "run", app], dict(flags, **missing), track_wanted)
    capture = root / (track + "-capture")
    run(track + "-capture", [light, "capture", "-o", capture, "--", "run", app],
        dict(flags, MIRVM_NO_IR_CACHE="1"), track_wanted)
    assert (capture / "diagnostics.log").exists()
    assert list(capture.glob("events-*.mlog"))
    assert not list(capture.glob("*.partial"))
    run(track + "-capture-inspect", [light, "log", "inspect", capture], missing)

# Frontmatter resolution uses the same ordinary dispatcher on both builds.
script = root / "script.rs"
script.write_text('---\n[dependencies]\nboundary-dep={path=' + json.dumps(str(project / "dep")) + '}\n---\n'
                  'fn main() { println!("{}", boundary_dep::id(456u32)); }\n')
run("script-miss", [light, "run", script], expected=b"456\n")
run("script-hit", [light, "run", script], missing, b"456\n")

warning = root / "warning.rs"
warning.write_text('fn main() { let unused = 1; println!("warning guest"); }\n')
reference = run("warning-full", [full, "run", "-v", warning])
for n in range(2):
    result = run("warning-light-" + str(n), [light, "run", "-v", warning], expected=reference.stdout)
    assert result.stderr == reference.stderr, (result.stderr, reference.stderr)
bad = root / "error.rs"
bad.write_text('fn main() { let _: u64 = "invalid"; }\n')
reference = run("error-full", [full, "run", bad], code=1)
result = run("error-light", [light, "run", bad], code=1, expected=b"")
assert result.stderr == reference.stderr
for mode, binary in (("full", full), ("light", light)):
    directory = root / (mode + "-warning-capture")
    run(mode + "-warning-capture", [binary, "capture", "-o", directory, "--", "run", "-v", warning],
        expected=b"warning guest\n")
assert (root / "full-warning-capture/diagnostics.log").read_bytes() == (root / "light-warning-capture/diagnostics.log").read_bytes()

package = root / "project.mirvm"
run("pack", [light, "pack", app, "-o", package])
run("package", [light, "run", package], missing, project_wanted)
panic = root / "panic.rs"
panic.write_text('fn main() { panic!("boundary panic"); }\n')
for name, flags in (("jit", {"MIRVM_JIT_SYNC": "1", "MIRVM_JIT_THRESHOLD": "1"}), ("interpreter", {"MIRVM_JIT": "0"})):
    flags["MIRVM_NO_IR_CACHE"] = "1"
    reference = run(name + "-panic-full", [full, "run", panic], flags, code=101)
    result = run(name + "-panic-light", [light, "run", panic], flags, code=101)
    assert result.stdout == reference.stdout
    normalize_pid = lambda data: re.sub(rb"thread 'main' \(\d+\)", b"thread 'main' (PID)", data)
    assert normalize_pid(result.stderr) == normalize_pid(reference.stderr)

hold = root / "hold.rs"
hold.write_text('fn main() { println!("ready"); std::thread::sleep(std::time::Duration::from_secs(30)); }\n')
with (root / "signal.err").open("wb") as stderr:
    child = subprocess.Popen([light, "run", hold], env=env, cwd=root, stdout=subprocess.PIPE, stderr=stderr)
    try:
        assert select.select([child.stdout], [], [], 60)[0]
        assert child.stdout.readline() == b"ready\n"
        records = [json.loads(line) for line in worker_log.read_text().splitlines()]
        assert records[-1]["pid"] == child.pid
        child.send_signal(signal.SIGTERM)
        assert child.wait(timeout=10) == -signal.SIGTERM
    finally:
        if child.poll() is None:
            child.kill()
            child.wait()
rows.append(dict(label="signal-and-pid", exit=-signal.SIGTERM))
result = run("missing-worker", [light, "run", hold], dict(missing, MIRVM_NO_IR_CACHE="1"), code=70)
assert b"cannot execute compiler worker" in result.stderr
result = run("runtime-worker-rejected", [light, "run", hold],
             dict(MIRVM_COMPILER=str(light), MIRVM_NO_IR_CACHE="1"), code=70)
assert b"built without compiler support" in result.stderr
result = run("worker-build-rejected", [worker, "__compiler-worker", "different-build", "cache", "status"], code=70)
assert b"build differs" in result.stderr
rejected_capture = root / "rejected-capture"
result = run("rejected-capture", [light, "capture", "-o", rejected_capture, "--", "run", hold],
             dict(MIRVM_COMPILER=str(light), MIRVM_NO_IR_CACHE="1"), code=70)
assert (rejected_capture / "diagnostics.log").read_bytes() == result.stderr
assert not list((home / "run/compiler-handoff").glob("*"))
records = [json.loads(line) for line in worker_log.read_text().splitlines()]
assert any(row.get("units", 0) > 0 for row in records)
assert all(row.get("lease", True) for row in records)
(root / "checks.json").write_text(json.dumps(rows, indent=2) + "\n")
(root / "protocol.json").write_text(json.dumps(dict(
    binary_sha256={name: hashlib.sha256(path.read_bytes()).hexdigest()
                   for name, path in (("full", full), ("light", light), ("worker", worker))},
    worker_requests=len(records), scope="Ordinary CLI contracts; prepared managed inputs; no timings."
), indent=2) + "\n")
