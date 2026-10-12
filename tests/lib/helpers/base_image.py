"""Exercise the shared base through real compiler sessions and cache publication."""
import json
import os
from pathlib import Path
import subprocess
import sys

mirvm, shared_home, scratch = map(Path, sys.argv[1:])
env = {k: v for k, v in os.environ.items() if not k.startswith("MIRVM_")}
env.update(MIRVM_OFFLINE="1", MIRVM_TIMING="1")
empty = scratch / "empty.rs"
empty.write_text("fn main() {}\n")


def invoke(home, command, expected=b"", extra=None, success=True):
    result = subprocess.run(
        [str(mirvm), *map(str, command)],
        env=dict(env, MIRVM_HOME=str(home), **(extra or {})),
        capture_output=True, timeout=60,
    )
    assert (result.returncode == 0) == success, result.stderr.decode(errors="replace")
    assert result.stdout == expected, (command, result.stdout, expected)
    return result.stderr.decode()


# Let the product resolve or prepare its managed sysroot; each test gets an empty derived cache.
invoke(shared_home, ["run", empty])


def home(name):
    path = scratch / name
    path.mkdir()
    (path / "data").symlink_to((shared_home / "data").resolve(), target_is_directory=True)
    return path


def base_file(path):
    images = list((path / "cache/base").glob("*.img"))
    assert len(images) == 1, images
    return images[0]


def built(timing):
    assert "base-build=" in timing and "cache-store=" in timing, timing


first = home("first")
built(invoke(first, ["run", empty]))
image = base_file(first)
canonical = image.read_bytes()
isolated = scratch / "isolated.img"
invoke(first, ["__build-base-image", isolated], extra={"MIRVM_THREADS": "2"})
assert isolated.read_bytes() == canonical, "isolated and in-session bases differ"
print("PASS isolated and in-session roots are byte-identical")

sources = {
    "result": 'fn main() -> Result<(), std::io::Error> { println!("result"); Ok(()) }',
    "termination": '''struct Exit;
impl std::process::Termination for Exit {
    fn report(self) -> std::process::ExitCode { std::process::ExitCode::SUCCESS }
}
#[unsafe(no_mangle)] pub extern "C" fn user_export() -> u32 { 17 }
fn main() -> Exit { assert_eq!(user_export(), 17); println!("termination"); Exit }''',
    "allocator": '''use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
struct Alloc;
static COUNT: AtomicUsize = AtomicUsize::new(0);
unsafe impl GlobalAlloc for Alloc {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        COUNT.fetch_add(1, Ordering::SeqCst); unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) { unsafe { System.dealloc(p, l) } }
}
#[global_allocator] static GLOBAL: Alloc = Alloc;
fn main() {
    let before = COUNT.load(Ordering::SeqCst);
    let value = std::hint::black_box(Box::new([3u8; 128]));
    assert_eq!(value[127], 3);
    assert!(COUNT.load(Ordering::SeqCst) > before);
    drop(value);
    std::panic::set_hook(Box::new(|_| {}));
    assert!(std::panic::catch_unwind(|| panic!("caught")).is_err());
    println!("allocator");
}''',
}
for name, source in sources.items():
    path = scratch / (name + ".rs")
    path.write_text(source)
    expected = (name + "\n").encode()
    fresh = home(name)
    built(invoke(fresh, ["run", path], expected))
    assert base_file(fresh).read_bytes() == canonical, name + " changed the std base"
    reused = invoke(first, ["run", path], expected)
    assert "frontend=" in reused and "base-build=" not in reused, reused
    assert image.read_bytes() == canonical
    warm = invoke(first, ["run", path], expected)
    assert "cache-load=" in warm and "frontend=" not in warm, warm
    print("PASS", name, "cold, shared-base and bound-image execution")

edited = scratch / "edited.rs"
edited.write_text('fn main() { println!("edited"); }')
mtime = image.stat().st_mtime_ns
invoke(first, ["run", edited], b"edited\n")
edited.write_text('fn main() { println!("changed"); }')
timing = invoke(first, ["run", edited], b"changed\n")
assert "frontend=" in timing and "base-build=" not in timing, timing
assert image.stat().st_mtime_ns == mtime
print("PASS source edits retain the shared base")

# A corrupt file must rebuild and converge to the same content identity.
image.write_bytes(canonical[:-1] + bytes([canonical[-1] ^ 1]))
edited.write_text('fn main() { println!("recovered"); }')
built(invoke(first, ["run", edited], b"recovered\n"))
assert image.read_bytes() == canonical
print("PASS corrupt base recovery and publication")

# A file obstructing the directory exercises publication failure even when tests run as root.
blocked = home("blocked")
(blocked / "cache").mkdir()
(blocked / "cache/base").write_bytes(b"unwritable cache family")
built(invoke(blocked, ["run", empty]))
assert "cache-load=" in invoke(blocked, ["run", empty])
print("PASS execution and bound-image reuse after base publication failure")

invalid = scratch / "invalid.rs"
invalid.write_text('fn main() { let _: u8 = "invalid"; }')
failed = home("invalid")
invoke(failed, ["run", invalid], success=False)
assert not list((failed / "cache/base").glob("*.img"))
print("PASS frontend failure does not build a base")

# Runner recipes expose rustc profile and native-link inputs without depending on Cargo downloads.
version = subprocess.check_output([os.environ.get("RUSTC", "rustc"), "-vV"], text=True)
host = next(line.removeprefix("host: ") for line in version.splitlines() if line.startswith("host: "))
sysroot = shared_home / "data" / ("sysroot-" + host)
profile_source = scratch / "profile.rs"
profile_source.write_text('''fn main() {
    std::panic::set_hook(Box::new(|_| {}));
    let overflow = std::panic::catch_unwind(|| std::hint::black_box(255u8) + 1).is_err();
    println!("{overflow}");
}''')
profiles = home("profiles")
recipe = scratch / "profile.json"
for checks, expected in [("yes", b"true\n"), ("no", b"false\n"), ("yes", b"true\n")]:
    recipe.write_text(json.dumps({"args": [str(profile_source), "--edition=2024",
        "-Zmir-opt-level=2", "-Coverflow-checks=" + checks], "env": list(env.items())}))
    invoke(profiles, ["runner", recipe], expected, {"MIRVM_SYSROOT": str(sysroot)})
assert len(list((profiles / "cache/base").glob("*.img"))) == 2
print("PASS incompatible profiles occupy separate base entries")

# Keep sysroot provenance identical for both runner products: an explicitly selected external
# sysroot can have a different stamp from the managed generation selected by plain `run`.
recipe.write_text(json.dumps({"args": [str(empty), "--edition=2024", "-Zmir-opt-level=2"],
    "env": list(env.items())}))
runner_home = home("runner")
built(invoke(runner_home, ["runner", recipe], extra={"MIRVM_SYSROOT": str(sysroot)}))
runner_canonical = base_file(runner_home).read_bytes()
recipe.write_text(json.dumps({"args": [str(profile_source), "--edition=2024",
    "-Cembed-bitcode=no", "-Cdebug-assertions=yes", "-Coverflow-checks=yes",
    "--remap-path-prefix=" + str(scratch) + "/="], "env": list(env.items())}))
compatible = invoke(runner_home, ["runner", recipe], b"true\n",
    {"MIRVM_SYSROOT": str(sysroot)})
assert "frontend=" in compatible and "base-build=" not in compatible, compatible
assert base_file(runner_home).read_bytes() == runner_canonical
print("PASS compatible script and Cargo defaults reuse the same base")


native = scratch / "native.c"
native.write_text("int user_native_marker(void) { return 23; }\n")
subprocess.run(["cc", "-fPIC", "-c", str(native), "-o", str(scratch / "native.o")], check=True)
subprocess.run(["ar", "crs", str(scratch / "libbase_native.a"), str(scratch / "native.o")], check=True)
native_source = scratch / "native.rs"
native_source.write_text('''unsafe extern "C" { fn user_native_marker() -> i32; }
fn main() { assert_eq!(unsafe { user_native_marker() }, 23); }''')
recipe.write_text(json.dumps({"args": [str(native_source), "--edition=2024",
    "-Zmir-opt-level=2", "-Lnative=" + str(scratch), "-lstatic=base_native"], "env": list(env.items())}))
native_home = home("native")
built(invoke(native_home, ["runner", recipe], extra={"MIRVM_SYSROOT": str(sysroot)}))
assert base_file(native_home).read_bytes() == runner_canonical
print("PASS user native inputs stay out of the canonical base")
