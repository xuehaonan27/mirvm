#!/usr/bin/env bash
# JIT stats contract: stats must be printed on process exit, and the compiled-to-interpreter call bucket must be non-zero.
set -u
. "$(dirname "${BASH_SOURCE[0]}")/../../support/harness.sh"
suite_init

code=0
MIRVM_JIT_STATS=1 "$MIRVM" run tests/scripts/jit_unwind_probe.rs \
    >"$TMP/out" 2>"$TMP/err" || code=$?
if [ "$code" -eq 0 ] && grep -q '^mirvm-jit-stats:' "$TMP/err" \
    && grep -q 'c2i=[1-9]' "$TMP/err"; then
    ok "stats printed at exit and c2i bucket non-zero"
else
    bad "JIT stats missing or run failed (exit=$code)"
    tail -3 "$TMP/err"
fi

suite_summary runtime.jit-stats
