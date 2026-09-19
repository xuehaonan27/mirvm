#!/usr/bin/env bash
# Diagnostics routing contract: three compilation paths keep default stderr, while capture only collects compiler and MIRVM diagnostics.
set -u
. "$(dirname "${BASH_SOURCE[0]}")/../../support/harness.sh"
test_enter_repo

MIRVM=${MIRVM:-$REPO_ROOT/target/release/mirvm}
require_executable MIRVM "$MIRVM" || exit $?

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
HOME_DIR="$TMP/home"
mkdir -p "$HOME_DIR"
ensure_test_sysroot "$MIRVM" "$HOME_DIR" "$RUSTC" || exit $?
host=$($RUSTC -vV | sed -n 's/^host: //p')
home_sysroot="$HOME_DIR/sysroot-$host"
if [ "$TEST_SYSROOT" != "$home_sysroot" ] && [ ! -e "$home_sysroot" ]; then
    ln -s "$TEST_SYSROOT" "$home_sysroot"
fi

hex_file() {
    od -An -tx1 -v "$1" | tr -d ' \n'
}

run_case() { # <name> <deps> <fixture> <unused-name> <missing-symbol> <guest-hex>
    local name=$1 deps=$2 fixture=$3 unused_name=$4 missing=$5 guest_hex=$6
    local plain="$TMP/$name-plain" captured="$TMP/$name-captured"
    local session="$TMP/$name-session" plain_code=0 capture_code=0 diag
    local stderr_hex diagnostics_hex expected_hex
    mkdir -p "$plain" "$captured" "$session"

    MIRVM_HOME="$HOME_DIR" MIRVM_DEPS="$deps" MIRVM_SYSROOT="$TEST_SYSROOT" \
        "$MIRVM" run "$fixture" >"$plain/stdout" 2>"$plain/stderr" || plain_code=$?
    MIRVM_HOME="$HOME_DIR" MIRVM_DEPS="$deps" MIRVM_SYSROOT="$TEST_SYSROOT" \
        "$MIRVM" capture -o "$session" -- run "$fixture" \
        >"$captured/stdout" 2>"$captured/stderr" || capture_code=$?

    if [ "$plain_code" -eq 70 ] && [ "$capture_code" -eq 70 ] \
        && cmp -s "$plain/stdout" "$captured/stdout" \
        && cmp -s "$plain/stderr" "$captured/stderr"; then
        ok "$name default stdout/stderr/exit byte-identical before and after capture"
    else
        bad "$name default output drifted (plain=$plain_code capture=$capture_code)"
        diff -u "$plain/stderr" "$captured/stderr" | head -40
    fi

    diag="$session/diagnostics.log"
    if [ ! -f "$diag" ]; then
        bad "$name capture did not generate diagnostics.log"
        return
    fi
    if grep -aFq "function \`$unused_name\` is never used" "$diag" \
        && grep -aFq "foreign \`$missing\` symbol not found" "$diag"; then
        ok "$name diagnostics contains compiler warning and MIRVM control error"
    else
        bad "$name diagnostics missing compiler or MIRVM diagnostic"
    fi

    if hex_file "$diag" | grep -Fq "$guest_hex"; then
        bad "$name diagnostics mixed in guest NUL/non-UTF-8/ANSI bytes"
    else
        ok "$name diagnostics contains no guest binary stderr"
    fi
    if grep -aFq 'router-guest-binary-' "$diag"; then
        bad "$name diagnostics mixed in the single write carrying the guest's own warning text"
    else
        ok "$name guest same-text warning single write not mistakenly captured"
    fi

    stderr_hex=$(hex_file "$captured/stderr")
    diagnostics_hex=$(hex_file "$diag")
    expected_hex=${stderr_hex/"$guest_hex"/}
    if [ "$expected_hex" != "$stderr_hex" ] && [ "$diagnostics_hex" = "$expected_hex" ]; then
        ok "$name diagnostics is physical stderr with the guest single write removed byte-exactly"
    else
        bad "$name compiler/control tee is not a byte-exact copy"
    fi
}

direct_guest_hex=$(printf '%s' 'warning: 1 warning emitted
router-guest-binary-direct:' | od -An -tx1 -v | tr -d ' \n')
direct_guest_hex="${direct_guest_hex}00ff1b5b33316d0a"
script_guest_hex=$(printf '%s' 'warning: 1 warning emitted
router-guest-binary-script:' | od -An -tx1 -v | tr -d ' \n')
script_guest_hex="${script_guest_hex}00ff1b5b33316d0a"

run_case direct self tests/fixtures/diagnostic_router_direct.rs \
    router_unused_direct mirvm_diagnostic_router_missing_direct "$direct_guest_hex"
run_case cargoless self tests/fixtures/diagnostic_router_script.rs \
    router_unused_script mirvm_diagnostic_router_missing_script "$script_guest_hex"
run_case cargo-runner cargo tests/fixtures/diagnostic_router_script.rs \
    router_unused_script mirvm_diagnostic_router_missing_script "$script_guest_hex"

run_early_control_case() { # <name> <exit> <MIRVM_DEPS> [run args...]
    local name=$1 expected_code=$2 deps=$3
    local plain="$TMP/$name-plain" captured="$TMP/$name-captured"
    local session="$TMP/$name-session" plain_code=0 capture_code=0
    shift 3
    mkdir -p "$plain" "$captured" "$session"
    MIRVM_HOME="$HOME_DIR" MIRVM_SYSROOT="$TEST_SYSROOT" MIRVM_DEPS="$deps" \
        "$MIRVM" run "$@" >"$plain/stdout" 2>"$plain/stderr" || plain_code=$?
    MIRVM_HOME="$HOME_DIR" MIRVM_SYSROOT="$TEST_SYSROOT" MIRVM_DEPS="$deps" \
        "$MIRVM" capture -o "$session" -- run "$@" \
        >"$captured/stdout" 2>"$captured/stderr" || capture_code=$?

    if [ "$plain_code" -eq "$expected_code" ] \
        && [ "$capture_code" -eq "$expected_code" ] \
        && cmp -s "$plain/stdout" "$captured/stdout" \
        && cmp -s "$plain/stderr" "$captured/stderr"; then
        ok "$name default stdout/stderr/exit byte-identical before and after capture"
    else
        bad "$name default output drifted (plain=$plain_code capture=$capture_code)"
    fi
    if [ -f "$session/diagnostics.log" ] \
        && cmp -s "$captured/stderr" "$session/diagnostics.log"; then
        ok "$name pre-run_driver control byte-exactly teed and published"
    else
        bad "$name pre-run_driver control did not enter diagnostics.log"
    fi
}

run_early_control_case stack-invalid 2 self \
    --stack-size bad tests/fixtures/diagnostic_router_direct.rs
run_early_control_case jit-invalid 2 self \
    --jit maybe tests/fixtures/diagnostic_router_direct.rs
run_early_control_case unknown-argument 2 self --not-a-mirvm-option
run_early_control_case missing-input 2 self
run_early_control_case invalid-deps 2 invalid tests/fixtures/diagnostic_router_direct.rs
run_early_control_case bin-on-file 2 self \
    --bin selected tests/fixtures/diagnostic_router_direct.rs
run_early_control_case missing-source 1 self "$TMP/no-such-source.rs"

runner_plain="$TMP/runner-control-plain"
runner_captured="$TMP/runner-control-captured"
runner_session="$TMP/runner-control-session"
missing_fake="$TMP/missing-fake-binary"
mkdir -p "$runner_plain" "$runner_captured" "$runner_session"
runner_plain_code=0
runner_capture_code=0
"$MIRVM" runner "$missing_fake" \
    >"$runner_plain/stdout" 2>"$runner_plain/stderr" || runner_plain_code=$?
"$MIRVM" runner --mirvm-capture-directory "$runner_session" "$missing_fake" \
    >"$runner_captured/stdout" 2>"$runner_captured/stderr" || runner_capture_code=$?

if [ "$runner_plain_code" -eq 1 ] && [ "$runner_capture_code" -eq 1 ] \
    && cmp -s "$runner_plain/stdout" "$runner_captured/stdout" \
    && cmp -s "$runner_plain/stderr" "$runner_captured/stderr"; then
    ok "forwarded runner early control default stdout/stderr/exit byte-identical"
else
    bad "forwarded runner early control default output drifted"
fi
if [ -f "$runner_session/diagnostics.log" ] \
    && cmp -s "$runner_captured/stderr" "$runner_session/diagnostics.log"; then
    ok "runner fake-binary parse error byte-exactly teed and published"
else
    bad "runner fake-binary parse error did not enter diagnostics.log"
fi

suite_summary runtime.diagnostics
