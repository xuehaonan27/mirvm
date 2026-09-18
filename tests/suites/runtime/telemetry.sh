#!/usr/bin/env bash
# L2 fork-generation contract: MIRVM's own capture must be complete across fork.
#
# Behavioural authority: the guest's own recordable syscalls. A capture session
# is armed around a guest that records in both the parent and a forked child.
# The parent and the child must each end up with their own event file, carrying
# their own pid and their own process generation, and each file must decode with
# committed chunks. Before L2 the child's file was empty (no session, no
# producer, no sealed page), and the parent's file must not absorb child records
# under any circumstance.
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

SESSION="$TMP/session"
mkdir -p "$SESSION"

# The fixture needs the libc crate; the registry fetch may need the network, so
# this suite can only run where dependency resolution works (same requirement as
# the differential suites).
capture_rc=0
MIRVM_HOME="$HOME_DIR" MIRVM_SYSROOT="$TEST_SYSROOT" \
    "$MIRVM" capture -o "$SESSION" -- run tests/fixtures/telemetry_fork_child.rs \
    >"$TMP/out" 2>"$TMP/err" || capture_rc=$?
if [ "$capture_rc" -eq 0 ] && grep -q '^child-exit=0$' "$TMP/out"; then
    ok "fork probe ran with a clean child exit"
else
    bad "fork probe failed (exit=$capture_rc)"
    tail -20 "$TMP/err"
fi

# Header fields of one capture file, via the product's own inspector.
file_field() { # <file> <json-key>
    "$MIRVM" log inspect "$1" 2>/dev/null | grep -m1 "\"$2\"" | sed 's/[^0-9]//g'
}

parent_file=$(ls "$SESSION"/events-*.mlog 2>/dev/null | head -1)
child_file=$(ls "$SESSION"/events-*.mlog.partial "$SESSION"/events-*.mlog 2>/dev/null \
    | grep -v "$(basename "${parent_file:-none}")" | head -1)

if [ -n "${parent_file:-}" ]; then
    ok "parent produced a capture file ($(basename "$parent_file"))"
else
    bad "parent produced no capture file"
fi

if [ -n "${child_file:-}" ]; then
    ok "fork child produced its own capture file ($(basename "$child_file"))"
else
    bad "fork child produced no capture file"
fi

# Parent: generation 0 and its own pid.
if [ -n "${parent_file:-}" ] \
    && [ "$(file_field "$parent_file" process_generation)" = "0" ]; then
    ok "parent file records generation 0"
else
    bad "parent file generation wrong"
fi

# Child: generation 1 (it inherited 0 and must advance), a pid different from
# the parent's, and committed records of its own.
if [ -n "${child_file:-}" ] && [ -n "${parent_file:-}" ]; then
    child_gen=$(file_field "$child_file" process_generation)
    child_pid=$(file_field "$child_file" pid)
    parent_pid=$(file_field "$parent_file" pid)
    child_records=$(file_field "$child_file" records)
    parent_records=$(file_field "$parent_file" records)
    if [ "$child_gen" = "1" ]; then
        ok "fork child records generation 1"
    else
        bad "fork child generation=$child_gen, want 1"
    fi
    if [ -n "$child_pid" ] && [ "$child_pid" != "$parent_pid" ]; then
        ok "fork child file carries the child pid"
    else
        bad "fork child pid=$child_pid parent=$parent_pid"
    fi
    # Each side recorded its own two instrumented syscalls.
    if [ "$parent_records" -ge 2 ] 2>/dev/null; then
        ok "parent file has its own committed records ($parent_records)"
    else
        bad "parent records=$parent_records, want >= 2"
    fi
    if [ "$child_records" -ge 2 ] 2>/dev/null; then
        ok "fork child file has its own committed records ($child_records)"
    else
        bad "fork child records=$child_records, want >= 2"
    fi
fi

suite_summary runtime.telemetry
