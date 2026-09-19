#!/usr/bin/env bash
# Differential: MIRVM_DEPS=self (zero-cargo driver) vs MIRVM_DEPS=cargo (the
# three-phase path) must agree byte-for-byte on stdout/stderr/exit. The cases:
# a frontmatter script (fresh resolution), a registry project (with lock), a path
# proc-macro project (the proc-macro is host-compiled by real rustc), a registry
# build.rs script (libc's full build.rs chain), a path build.rs project
# (OUT_DIR/rustc-cfg/rustc-env/DEP_* propagation), and a serde derive script
# (proc-macro2 and serde_core build.rs + both host/target sides + facade re-exporting a proc-macro).
# Zero-cargo proof: the self leg runs with a temp PATH holding only mirvm plus MIRVM_OFFLINE=1,
# so cargo is absent and a self path that secretly launched it shows up immediately
# (itoa/memchr/cfg-if are already in the local registry, enough for an offline read-through).
# The warm-up run before the real self leg (normal PATH, online) only fills the local registry
# index/src cache. The cargo leg of a script case may use the network for fresh resolution.
set -u
. "$(dirname "${BASH_SOURCE[0]}")/../../support/harness.sh"
test_enter_repo
MIRVM=${MIRVM:-$(pwd)/target/debug/mirvm}
[ -x "$MIRVM" ] || { echo "diff_cless: $MIRVM not found (build it first)" >&2; exit 69; }
MIRVM=$(realpath "$MIRVM")
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT

# Self-leg environment: PATH holds only mirvm (neither cargo nor rustc is on PATH)
mkdir -p "$TMP/bin"
ln -s "$MIRVM" "$TMP/bin/mirvm"

# <name> <cargo exit> <self exit>: byte-for-byte three-way comparison; mismatches print details
check_pair() {
    local name="$1" cc="$2" sc="$3"
    local ok=1 why=""
    if [ "$cc" != "$sc" ]; then ok=0; why="exit code cargo=$cc self=$sc"
    elif ! diff -q "$TMP/$name.cargo.out" "$TMP/$name.self.out" >/dev/null; then ok=0; why="stdout differs"
    elif ! diff -q "$TMP/$name.cargo.err" "$TMP/$name.self.err" >/dev/null; then ok=0; why="stderr differs"
    fi
    if [ $ok = 1 ]; then
        echo "PASS $name"; pass=$((pass + 1))
    else
        echo "FAIL $name: $why"
        echo "--- cargo stdout ---"; cat "$TMP/$name.cargo.out"
        echo "--- self stdout ---"; cat "$TMP/$name.self.out"
        echo "--- cargo stderr ---"; cat "$TMP/$name.cargo.err"
        echo "--- self stderr ---"; cat "$TMP/$name.self.err"
        fail=$((fail + 1))
    fi
}

# <name> <target> <expected warm-up exit> [extra env for the cargo leg...]
# Warm-up (fills the local registry cache) -> cargo leg -> self leg (restricted PATH + offline) -> compare
diff_cless() {
    local name="$1" target="$2" prime_code="$3"; shift 3
    # Warm-up: the self leg's first run may fetch index+src and compile deps; the artifacts are discarded
    env -u RUST_BACKTRACE MIRVM_DEPS=self "$MIRVM" run "$target" \
        >"$TMP/$name.prime.out" 2>"$TMP/$name.prime.err"
    local pc=$?
    if [ "$pc" != "$prime_code" ]; then
        echo "FAIL $name: self-leg warm-up failed (exit=$pc, want $prime_code = guest run code)"
        tail -20 "$TMP/$name.prime.err"
        fail=$((fail + 1))
        return
    fi
    # Cargo leg (script cases may use the network; project cases use --locked)
    env -u RUST_BACKTRACE MIRVM_DEPS=cargo "$@" "$MIRVM" run "$target" \
        >"$TMP/$name.cargo.out" 2>"$TMP/$name.cargo.err"
    local cc=$?
    # Self leg (zero-cargo proof: cargo absent from PATH + offline)
    env -u RUST_BACKTRACE PATH="$TMP/bin" MIRVM_OFFLINE=1 MIRVM_DEPS=self \
        "$MIRVM" run "$target" \
        >"$TMP/$name.self.out" 2>"$TMP/$name.self.err"
    local sc=$?
    check_pair "$name" "$cc" "$sc"
}

# 1) Frontmatter script fixture (fresh resolution; guest exit 4)
diff_cless cless_script tests/fixtures/cless_script.rs 4

# 2) Cargo project fixture (with lock; copy first to keep the repo clean; cargo leg --locked; guest exit 3)
cp -r tests/fixtures/cless_proj "$TMP/proj"
diff_cless cless_proj "$TMP/proj" 3 MIRVM_CARGO_LOCKED=1

# 3) Path proc-macro project fixture (with lock; cargo leg --locked; guest exit 5)
cp -r tests/fixtures/cless_pm "$TMP/pm"
diff_cless cless_pm "$TMP/pm" 5 MIRVM_CARGO_LOCKED=1

# 4) Registry build.rs script fixture (libc's full build.rs chain; guest exit 6)
diff_cless cless_libc tests/fixtures/cless_libc.rs 6

# 5) Path build.rs project fixture (OUT_DIR/rustc-cfg/rustc-env/DEP_* propagation;
#    with lock; cargo leg --locked; guest exit 7)
cp -r tests/fixtures/cless_br "$TMP/br"
diff_cless cless_br "$TMP/br" 7 MIRVM_CARGO_LOCKED=1

# 6) Serde derive full-chain script fixture (proc-macro2/serde_core build.rs +
#    both host/target sides + facade re-exporting a proc-macro; guest exit 8)
diff_cless cless_serde tests/fixtures/cless_serde.rs 8

# 7) --bin multi-target selection (a2_ws has two bins + default-run; both legs
#    compare cargo run --bin semantics byte-for-byte). diff_cless() takes no extra mirvm flags, so this case is separate.
cp -r tests/fixtures/a2_ws "$TMP/a2ws"
for leg in cargo self; do
    if [ "$leg" = cargo ]; then
        env -u RUST_BACKTRACE MIRVM_DEPS=cargo MIRVM_CARGO_LOCKED=1 \
            "$MIRVM" run "$TMP/a2ws" --bin a2_two \
            >"$TMP/binsel.$leg.out" 2>"$TMP/binsel.$leg.err"
        eval "${leg}_code=$?"
    else
        env -u RUST_BACKTRACE PATH="$TMP/bin" MIRVM_OFFLINE=1 MIRVM_DEPS=self \
            "$MIRVM" run "$TMP/a2ws" --bin a2_two \
            >"$TMP/binsel.$leg.out" 2>"$TMP/binsel.$leg.err"
        eval "${leg}_code=$?"
    fi
done
check_pair binsel "$cargo_code" "$self_code"

suite_summary differential.cargoless
