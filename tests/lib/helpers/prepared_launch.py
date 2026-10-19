"""Exercise launch hits, invalidation and the return to ordinary preparation in fresh processes."""
import os
from pathlib import Path
import shutil
import subprocess
import sys

mirvm, home, sysroot, scratch = map(Path, sys.argv[1:])
project, dep, cwd = (scratch / name for name in ("project", "dependency", "caller"))
for directory in (project / "src", dep / "src", cwd):
    directory.mkdir(parents=True)
(dep / "Cargo.toml").write_text('[package]\nname="launch_dep"\nversion="0.1.0"\nedition="2024"\n')
source = dep / "src/lib.rs"
source.write_text('pub fn value() -> u32 { 101 }\n')
(project / "Cargo.toml").write_text('''[package]
name="launch_probe"
version="0.1.0"
edition="2024"
[dependencies]
launch_dep={path="../dependency"}
''')
(project / "src/main.rs").write_text('''fn main() {
    println!("{}|{}|{}|{}|{}|{}|{}", launch_dep::value(), env!("BUILD_VALUE"),
        env!("SOURCE_VALUE"), option_env!("COMPILE_INPUT").unwrap_or("unset"),
        std::env::var("GUEST_VALUE").unwrap(), std::env::args().nth(1).unwrap(),
        std::env::current_dir().unwrap().display());
}
''')
(project / "build.rs").write_text('''fn main() {
    use std::io::Write;
    let mut counter = std::fs::OpenOptions::new().create(true).append(true)
        .open(std::env::var("LAUNCH_COUNTER").unwrap()).unwrap();
    writeln!(counter, "build").unwrap();
    println!("cargo::rerun-if-changed=../trigger.txt");
    println!("cargo::rerun-if-env-changed=SOURCE_VALUE");
    println!("cargo::rustc-env=BUILD_VALUE={}:{}", std::env::var("SOURCE_VALUE").unwrap(),
        std::fs::read_to_string("../trigger.txt").unwrap());
    println!("cargo::rustc-env=SOURCE_VALUE=injected");
}
''')
trigger = scratch / "trigger.txt"
trigger.write_text("one")
counter = scratch / "build-count.txt"
env = {k: v for k, v in os.environ.items() if not k.startswith("MIRVM_")}
env.update(MIRVM_HOME=str(home), MIRVM_SYSROOT=str(sysroot), MIRVM_TIMING="1",
           MIRVM_OFFLINE="1", MIRVM_DEPS="self", SOURCE_VALUE="caller",
           GUEST_VALUE="first", LAUNCH_COUNTER=str(counter))
for name in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "COMPILE_INPUT"):
    env.pop(name, None)


def run(name, command="run", target=project, extra=None, hit=None, output=None,
        success=True, args=(), guest_arg="argument"):
    argv = [str(mirvm), command, str(target), *args]
    if command == "run":
        argv += ["--", guest_arg]
    result = subprocess.run(argv, cwd=cwd, env=dict(env, **(extra or {})),
                            capture_output=True, timeout=120)
    (scratch / (name + ".out")).write_bytes(result.stdout)
    (scratch / (name + ".err")).write_bytes(result.stderr)
    log = result.stderr.decode(errors="replace")
    assert (result.returncode == 0) == success, (name, result.returncode, log)
    if hit is not None:
        assert ("mirvm-launch: hit" in log) == hit, (name, log)
    if hit:
        assert "cache-load=" in log and "frontend=" not in log, (name, log)
    if output is not None:
        assert result.stdout.decode().strip() == output, (name, result.stdout, output)
    print("PASS", name, flush=True)
    return result


def expected(value=101, build="caller:one", compiled="unset", guest="first", arg="argument"):
    return f"{value}|{build}|injected|{compiled}|{guest}|{arg}|{cwd}"


run("install", command="install", output="")
run("first-process-launch", hit=True, output=expected())
count = counter.read_bytes()
run("live-guest-environment", extra={"GUEST_VALUE": "second"}, hit=True,
    output=expected(guest="second", arg="new-argument"), guest_arg="new-argument")
assert counter.read_bytes() == count, "a launch hit reran the build script"

# Losing the image must restore SOURCE_VALUE before the ordinary build-script rerun gate.
for image in (home / "cache/execution").glob("*.bin"):
    image.unlink()
run("missing-image-restores-caller-environment", hit=False, output=expected())
assert counter.read_bytes() == count, "image fallback leaked the injected environment"
run("repaired-image-launch", hit=True, output=expected())

run("compile-time-environment", extra={"COMPILE_INPUT": "changed"}, hit=False,
    output=expected(compiled="changed"))
env["COMPILE_INPUT"] = "changed"
run("new-compile-time-environment-hit", hit=True, output=expected(compiled="changed"))

stat = source.stat()
source.write_text(source.read_text().replace("101", "202"))
os.utime(source, ns=(stat.st_atime_ns, stat.st_mtime_ns))
run("same-length-restored-mtime-dependency", hit=False,
    output=expected(value=202, compiled="changed"))
run("dependency-edit-hit", hit=True, output=expected(value=202, compiled="changed"))

trigger.write_text("two")
run("external-build-script-input", hit=False,
    output=expected(value=202, build="caller:two", compiled="changed"))
env["SOURCE_VALUE"] = "new"
run("build-script-environment", hit=False,
    output=expected(value=202, build="new:two", compiled="changed"))

# A newly discovered config and an optional include outside the source tree both invalidate.
# Config discovery starts at the project, so put the configuration in its shared ancestor.
config = scratch / ".cargo"
config.mkdir()
(config / "config.toml").write_text('include=[{path="optional.toml",optional=true}]\n')
run("new-ancestor-config", hit=False)
run("ancestor-config-hit", hit=True)
(config / "optional.toml").write_text('[resolver]\nincompatible-rust-versions="allow"\n')
run("new-optional-include", hit=False)
run("optional-include-hit", hit=True)
(project / "Cargo.lock").write_text((project / "Cargo.lock").read_text() + "\n# lock edit\n")
run("lockfile-edit", hit=False)
run("lockfile-edit-hit", hit=True)

run("different-bin-is-not-replayed", args=("--bin", "missing"), success=False, hit=False)
run("rustflags-change", extra={"RUSTFLAGS": "--cfg launch_changed"}, hit=False)

# Corrupt optional records/images must be misses and repair through the regular route.
for record in (home / "cache/launch").glob("*.bin"):
    record.write_bytes(b"corrupt")
run("corrupt-launch-record", hit=False)
run("repaired-launch-record", hit=True)
for image in (home / "cache/execution").glob("*.bin"):
    image.write_bytes(b"corrupt")
run("corrupt-execution-image", hit=False)
run("repaired-execution-image", hit=True)

# The fast path cannot conceal a dependency that has disappeared.
saved = dep.with_name("dependency-saved")
dep.rename(saved)
run("missing-dependency-rejected", success=False, hit=False)
saved.rename(dep)

# Frontmatter is guarded by the original file as well as its materialized source.
script = scratch / "script.rs"
script.write_text('---\n[dependencies]\n---\nfn main() { println!("script-one"); }\n')
run("script-install", command="install", target=script, output="")
run("script-first-launch", target=script, hit=True, output="script-one")
script.write_text(script.read_text().replace("script-one", "script-two"))
run("script-edit", target=script, hit=False, output="script-two")
run("script-edit-hit", target=script, hit=True, output="script-two")
shutil.rmtree(home / "cache/launch")
run("removed-launch-family", target=script, hit=False, output="script-two")
