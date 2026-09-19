#!/usr/bin/env bash
# TSan verdict for the execution phase: src/vm is compiled source-for-source under
# -Zsanitizer=thread and every concurrency case must run and pass with zero
# "WARNING: ThreadSanitizer". Cases keep guest memory race-free by construction, so any
# warning is an engine bug.
# product: no
set -u
. "$(dirname "${BASH_SOURCE[0]}")/../../support/harness.sh"
suite_init --no-product
cd "$REPO_ROOT/tests/tsan"

# Contract: every id here must print a PASS line. A case that stops running, or is renamed
# without updating this list, fails the suite instead of silently looking like a pass.
EXPECTED=(
    mixed-stack-fib
    atomic-cross-tier
    blocking-io-liveness
    mixed-stack-unwind
    engine-atomics-thunk-cache
    capture-session-lifecycle
    engine-close-race
    guest-threads
    signal-delivery
    fork-guard
)

OUT=$(MIRVM_BUILD_ID=0000000000000000 \
    RUSTFLAGS="-Zsanitizer=thread" TSAN_OPTIONS="halt_on_error=1" \
    cargo +"$TOOLCHAIN" run -Zbuild-std --target x86_64-unknown-linux-gnu --release 2>&1)
CODE=$?

echo "$OUT" | tail -12

if echo "$OUT" | grep -q "WARNING: ThreadSanitizer"; then
    echo "$OUT" | grep -B 2 -A 25 "WARNING: ThreadSanitizer" | head -80
    bad "TSan reported a data race"
elif [ "$CODE" -ne 0 ]; then
    bad "TSan harness failed (exit=$CODE)"
else
    missing=0
    for id in "${EXPECTED[@]}"; do
        if ! echo "$OUT" | grep -q "^PASS $id"; then
            echo "case did not run or did not pass: $id"
            missing=$((missing + 1))
        fi
    done
    if [ "$missing" -ne 0 ]; then
        bad "TSan harness ran $(( ${#EXPECTED[@]} - missing ))/${#EXPECTED[@]} expected cases"
    else
        ok "TSan zero-race warning, ${#EXPECTED[@]} concurrency cases PASS"
    fi
fi

suite_summary runtime.tsan
