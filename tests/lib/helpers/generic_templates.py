"""Differential checks must prove template use, not just output agreement on the eager fallback."""
import os
from pathlib import Path
import re
import subprocess
import sys

mirvm, rustc, shared_home, scratch, source = map(Path, sys.argv[1:])
env = {k: v for k, v in os.environ.items() if not k.startswith("MIRVM_")}
env.update(MIRVM_OFFLINE="1", MIRVM_TIMING="1", MIRVM_LOG="debug")


def run(name, args, extra=None, success=True):
    result = subprocess.run(list(map(str, args)), env=dict(env, **(extra or {})),
                            capture_output=True, timeout=90)
    (scratch / (name + ".out")).write_bytes(result.stdout)
    (scratch / (name + ".err")).write_bytes(result.stderr)
    assert (result.returncode == 0) == success, result.stderr.decode(errors="replace")
    return result


native = scratch / "native"
run("build-native", [rustc, "--edition=2024", "-Awarnings", source, "-o", native])
expected = run("native", [native]).stdout
expected_fork = run("native-fork", [native, "fork"]).stdout
empty = scratch / "empty.rs"
empty.write_text("fn main() {}\n")
run("prepare", [mirvm, "run", empty], {"MIRVM_HOME": str(shared_home)})


def home(name):
    path = scratch / name
    path.mkdir()
    (path / "data").symlink_to((shared_home / "data").resolve(), target_is_directory=True)
    return str(path)


for mode, settings in [
    ("eager", {"MIRVM_JIT": "off", "MIRVM_GENERIC_TEMPLATES": "0"}),
    ("templates", {"MIRVM_GENERIC_TEMPLATES": "1", "MIRVM_JIT": "off"}),
    ("jit", {"MIRVM_GENERIC_TEMPLATES": "1", "MIRVM_JIT_SYNC": "1",
             "MIRVM_JIT_THRESHOLD": "1", "MIRVM_NO_JIT_CACHE": "1"}),
]:
    settings = dict(settings, MIRVM_HOME=home(mode))
    first = run(mode, [mirvm, "run", source], settings)
    assert first.stdout == expected, (mode, first.stdout, expected)
    log = first.stderr.decode()
    specialized = "generic instance specialized:"
    if mode == "jit":
        assert any(specialized in line and "owned_identity" in line for line in log.splitlines()), log
    else:
        assert specialized not in log, log
    if mode != "eager":
        assert len(re.findall(r"generic template exported: copy_elements\n", log)) == 1, log
        copies = [line for line in log.splitlines()
                  if "generic template bound:" in line and "copy_elements" in line]
        assert len(copies) == 5 and len(set(copies)) == 5, copies
        assert "generic template exported: signed_const\n" in log, log
        assert "generic template exported: backwards\n" in log, log
        assert "generic template fallback: clone_and_unwind\n" in log, log
        for definition, count in [("slice_len", 3), ("slice_identity", 3), ("slice_data", 3),
                                  ("copy_slice", 3), ("layout_constants", 3), ("fixed_field", 3), ("fixed_ref", 3),
                                  ("witness", 3), ("clone_reference", 1), ("destroy", 2),
                                  ("index_ref", 4), ("index_mut", 4), ("array_ref", 5),
                                  ("nested_ref", 4), ("matrix_ref", 6), ("first_or", 4),
                                  ("last_or", 4), ("fixed_array_last", 4),
                                  ("middle_slice", 4), ("tail_mut", 4),
                                  ("owned_identity", 8), ("owned_forward", 8),
                                  ("owned_clone", 9), ("owned_observe", 2),
                                  ("owned_pick", 9), ("owned_select", 9), ("recursive", 2),
                                  ("construct_fields", 8), ("construct_c_fields", 8),
                                  ("construct_tuple", 8), ("generic_field", 8),
                                  ("generic_field_mut", 8), ("generic_tail", 8),
                                  ("generic_c_field", 8), ("nested_field", 8),
                                  ("into_field", 8)]:
            assert log.count("generic template exported: " + definition + "\n") == 1, definition
            bound = [line for line in log.splitlines()
                     if "generic template bound:" in line
                     and str(len(source.stem)) + source.stem in line
                     and re.search(r"(?<!\d)" + str(len(definition)) + re.escape(definition), line)]
            assert len(bound) == count and len(set(bound)) == count, (definition, bound)
        assert "generic template fallback: indirect\n" in log, log
        assert "generic template exported: caller_location\n" in log, log
        assert sum("generic template bound:" in line and "caller_location" in line
                   for line in log.splitlines()) == 1
    else:
        assert "generic template " not in log, log
    warm = run(mode + "-warm", [mirvm, "run", source], settings)
    assert warm.stdout == expected
    assert "cache-load=" in warm.stderr.decode() and "frontend=" not in warm.stderr.decode()
    assert "generic template " not in warm.stderr.decode()
    if mode == "jit":
        assert any(specialized in line and "owned_identity" in line for line in warm.stderr.decode().splitlines())
    else:
        assert specialized not in warm.stderr.decode()
    fork = run(mode + "-fork", [mirvm, "run", source, "--", "fork"], settings)
    assert fork.stdout == expected_fork
    assert "cache-load=" in fork.stderr.decode() and "frontend=" not in fork.stderr.decode()
    print("PASS", mode, "native output, export coverage, bound-image reload and fork")

# Collection must reject a bad instantiation even when runtime control flow never enters it.
invalid = scratch / "invalid.rs"
invalid.write_text("""fn bad<T>() { const { assert!(size_of::<T>() == 0); } }
fn main() { if std::hint::black_box(false) { bad::<u64>(); } }
""")
native_error = run("invalid-native", [rustc, "--edition=2024", invalid, "-o", native], success=False)
assert "E0080" in native_error.stderr.decode()
for mode, flags in [("eager", {"MIRVM_GENERIC_TEMPLATES": "0"}),
                    ("templates", {"MIRVM_GENERIC_TEMPLATES": "1"})]:
    error = run("invalid-" + mode, [mirvm, "run", invalid],
                dict(flags, MIRVM_HOME=home("invalid-" + mode)),
                success=False)
    assert "E0080" in error.stderr.decode() and not error.stdout
print("PASS unentered invalid instantiation is rejected before guest entry")
