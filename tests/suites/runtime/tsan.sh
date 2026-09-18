#!/usr/bin/env bash
# Spike 4 TSan verdict: engine (src/vm) has zero data races under full instrumentation.
# product: no
# Pass = exit code 0 and no "WARNING: ThreadSanitizer". Guest races are excluded per C4
# (test cases are designed to be race-free), so any warning = engine bug.
set -u
. "$(dirname "${BASH_SOURCE[0]}")/../../support/harness.sh"
cd "$REPO_ROOT/tsan"
TOOLCHAIN=${TOOLCHAIN:-nightly-2026-07-02}

OUT=$(MIRVM_BUILD_ID=0000000000000000 \
    RUSTFLAGS="-Zsanitizer=thread" TSAN_OPTIONS="halt_on_error=1" \
    cargo +"$TOOLCHAIN" run -Zbuild-std --target x86_64-unknown-linux-gnu --release 2>&1)
CODE=$?

echo "$OUT" | tail -8
if [ $CODE -eq 0 ] && ! echo "$OUT" | grep -q "WARNING: ThreadSanitizer"; then
    ok "TSan zero-race warning, engine Sync verdict passed"
else
    echo "$OUT" | grep -B 2 -A 25 "WARNING: ThreadSanitizer" | head -80
    bad "TSan failed (exit=$CODE)"
fi

suite_summary runtime.tsan
