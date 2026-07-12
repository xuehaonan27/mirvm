#!/usr/bin/env bash
# M5.1 vector hardware tracer: feature-gated stdarch output must match native exactly.
set -euo pipefail
cd "$(dirname "$0")/.."

MIRVM=${MIRVM:-target/debug/mirvm}
RUSTC=${RUSTC:-rustc}
SRC=tests/fixtures/m51_x86_vectors.rs
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

"$RUSTC" --edition 2024 -o "$TMP/native" "$SRC"
"$TMP/native" >"$TMP/native.out" 2>"$TMP/native.err"

mirvm_code=0
"$MIRVM" run "$SRC" >"$TMP/mirvm.out" 2>"$TMP/mirvm.err" || mirvm_code=$?
if [ "$mirvm_code" -ne 0 ]; then
    echo "FAIL m51_x86_vectors: mirvm exited $mirvm_code"
    tail -5 "$TMP/mirvm.err"
    exit 1
fi
if ! diff -u "$TMP/native.out" "$TMP/mirvm.out"; then
    echo "FAIL m51_x86_vectors: stdout differs"
    exit 1
fi

for spec in pshuf:pshufb sha:sha; do
    key=${spec%%:*}
    feature=${spec#*:}
    status=$(grep -E "^${key}=([0-9a-f]{16}|unavailable)$" "$TMP/native.out" || true)
    if [ "$(printf '%s\n' "$status" | grep -c .)" -ne 1 ]; then
        echo "FAIL m51_x86_vectors/$feature: native status missing or ambiguous"
        exit 1
    fi
    if [ "$status" = "$key=unavailable" ]; then
        echo "SKIP m51_x86_vectors/$feature: host lacks $feature"
    else
        echo "PASS m51_x86_vectors/$feature"
    fi
done
