#!/usr/bin/env mirvm
---
[dependencies]
---
// c_ugrep_bin: differential against the real ugrep binary (C++, not a crate).
// The driver writes a fixed fixture directory of text files, runs ugrep as a
// guest subprocess through std::process::Command for pattern, count and context
// searches, and anchors the captured stdout byte-for-byte.
// Version pin: this driver has zero crate dependencies (empty frontmatter); the
// subject under test is the ugrep C++ binary. The system has no ugrep ("which
// ugrep" is empty and there is no same-named crate on crates.io), so following the
// c_opencc precedent the machine-side prefix is built from source: GitHub
// Genivia/ugrep tag v7.8.2, configured with defaults and installed to
// /tmp/ugrep-local by g++ 11.4.0. Both artifacts are hash-anchored:
//   source tarball /tmp/ugrep-7.8.2.tar.gz
//     sha256 = f991cc6c61dbc5af5a3b3939083e917df4113509549670fb400d121f639f69f6
//   binary /tmp/ugrep-local/bin/ugrep
//     sha256 = 5740c878b8a2664662a6c96c54fd7c842c3104657135cdd70bc147eef52efa24
// It links only the system liblzma/libz/libstdc++/libgcc_s/libc/libm, and all
// three dimensions use that one absolute path, so the version is fixed. Case 0
// prints the whole `ugrep --version` output (4 lines, no machine paths), which
// makes a replaced or upgraded binary immediately visible.
// Determinism (how the plain-text output becomes reproducible): three fixture
// files are hardcoded ASCII (alpha.txt 11 lines, beta.txt 6 lines,
// sub/gamma.txt 3 lines) and are removed and rebuilt at the start of each
// dimension, so their content and directory entry order are constant; both the
// binary and the fixture use fixed absolute paths, so the embedded paths match
// even though each dimension runs from a different cwd.
// The subprocess gets env_clear() with only LC_ALL=C set (locking case folding
// and word-character classes) and the command line always carries
// --color=never --no-config (piped stdout is already non-TTY and colourless, and
// this also rules out a .ugrep config file or HOME).
// Concurrency: ugrep searches with several threads by default, so its output
// order is unstable; the official help states that -J1 may be specified to
// produce replicable results, so every case passes -J1 and the recursive case
// adds --sort=name (file order by path, independent of readdir order).
// Each case prints its argv, status, stdout length plus FNV-1a/64, the full
// stdout (ASCII, from_utf8_lossy with zero replacements) and the stderr length
// (always 0 in practice; if it were not, the full text is printed and remains
// deterministic). Exit codes are printed numerically: 0 for a match, 1 for none,
// with case 7 specifically testing 1. No wall clock, randomness, raw address or
// HashMap order is involved.
// Coverage: case 0 prints --version (the version anchor); case 1 matches -n
// needle in one file (5 lines); case 2 counts -c needle across two files (5/2 in
// argv order); case 3 uses -n -C2 'needle[0-9]' for two hit groups and the GNU
// "--" group separator (lines 3 and 10); case 4 recurses with -r --sort=name -n
// needle over the whole fixture (9 lines in alpha/beta/sub/gamma order); case 5
// folds case with -i -n needle (6 lines); case 6 inverts with -v -n needle on
// beta.txt (4 lines); case 7 has no match (status=1, empty stdout); case 8 uses
// word matching with -w -n needle (3 lines, needle123 and needle9 excluded).
// No known frontier issues: this fixture is a probe for the guest subprocess
// chain (spawn, env_clear, argv passing, stdout capture and exit-code return).
// The fixture "c_ugrep_bin" is a "mirvm" frontmatter script with no crate
// dependencies, and every anchor above is compared byte-for-byte with native.
//
//
//
//
//
//
//
//
//
//
//
//
//
//
//
//
//
//
//

use std::fs;
use std::path::Path;
use std::process::Command;

/// The machine-side ugrep binary (see the version pin in the header).
const UGREP: &str = "/tmp/ugrep-local/bin/ugrep";
/// Fixed fixture root (absolute, the same path for every dimension).
const FIXTURE: &str = "/tmp/mirvm_corpus_ugrep_fixture";
/// Absolute fixture paths, the same literals that argv printing and ugrep output use.
const ALPHA_PATH: &str = "/tmp/mirvm_corpus_ugrep_fixture/alpha.txt";
const BETA_PATH: &str = "/tmp/mirvm_corpus_ugrep_fixture/beta.txt";

const ALPHA: &str = "needle at top\n\
                     hay line one\n\
                     needle123 tail\n\
                     NEEDLE upper\n\
                     no match here\n\
                     needle! punct\n\
                     last needle\n\
                     pure hay\n\
                     more hay lines\n\
                     needle9 far away\n\
                     tail hay\n";

const BETA: &str = "beta line\n\
                    needle mid\n\
                    more hay\n\
                    Needle mixed\n\
                    plain text here\n\
                    final needle again\n";

const GAMMA: &str = "sub needle deep\n\
                     sub hay\n\
                     needle again in sub\n";

/// FNV-1a 64 over the subprocess stdout (no dependency, bit-exact).
fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Rebuilds the fixture per dimension: clear first, so create order stays constant.
fn setup_fixture() {
    let root = Path::new(FIXTURE);
    if root.exists() {
        fs::remove_dir_all(root).expect("clearing the fixture failed");
    }
    fs::create_dir_all(root.join("sub")).expect("creating the fixture dir failed");
    fs::write(root.join("alpha.txt"), ALPHA).unwrap();
    fs::write(root.join("beta.txt"), BETA).unwrap();
    fs::write(root.join("sub").join("gamma.txt"), GAMMA).unwrap();
}

/// Runs one ugrep case and anchors the output. The flag prefix is always
/// -J1 --color=never --no-config and the subprocess env holds only LC_ALL=C.
fn run_case(idx: usize, desc: &str, args: &[&str]) {
    let out = Command::new(UGREP)
        .args(["-J1", "--color=never", "--no-config"])
        .args(args)
        .env_clear()
        .env("LC_ALL", "C")
        .output()
        .unwrap_or_else(|e| panic!("spawning ugrep case {idx} failed (is {UGREP} there?): {e}"));

    println!("== case {idx}: {desc} ==");
    println!("argv: {}", args.join(" "));
    match out.status.code() {
        Some(c) => println!("status: {c}"),
        None => println!("status: signal"),
    }
    println!("stdout: len={} fnv={:016x}", out.stdout.len(), fnv1a(&out.stdout));
    println!("--- stdout begin ---");
    print!("{}", String::from_utf8_lossy(&out.stdout));
    println!("--- stdout end ---");
    println!("stderr: len={}", out.stderr.len());
    if !out.stderr.is_empty() {
        print!("{}", String::from_utf8_lossy(&out.stderr));
    }
    println!();
}

fn main() {
    assert!(Path::new(UGREP).exists(), "missing ugrep binary {UGREP} (see the header)");
    setup_fixture();

    run_case(0, "version", &["--version"]);
    run_case(1, "pattern -n single", &["-n", "needle", ALPHA_PATH]);
    run_case(2, "count -c multi", &["-c", "needle", ALPHA_PATH, BETA_PATH]);
    run_case(3, "context -C2 regex", &["-n", "-C2", "needle[0-9]", ALPHA_PATH]);
    run_case(4, "recursive -r sort", &["-r", "--sort=name", "-n", "needle", FIXTURE]);
    run_case(5, "ignore-case -i", &["-i", "-n", "needle", ALPHA_PATH]);
    run_case(6, "invert -v", &["-v", "-n", "needle", BETA_PATH]);
    run_case(7, "no-match exit 1", &["-n", "zzz_no_such_word", ALPHA_PATH]);
    run_case(8, "word -w", &["-w", "-n", "needle", ALPHA_PATH]);
}
