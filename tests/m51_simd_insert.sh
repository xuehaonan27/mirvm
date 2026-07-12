#!/usr/bin/env bash
# M5.1 SIMD lane tracer: public stdarch API, native output is the oracle.
set -euo pipefail
cd "$(dirname "$0")/.."

MIRVM=${MIRVM:-target/debug/mirvm}
RUSTC=${RUSTC:-rustc}
SRC=tests/fixtures/m51_simd_insert.rs
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

"$RUSTC" --edition 2024 -o "$TMP/native" "$SRC"

native_code=0
"$TMP/native" >"$TMP/native.out" 2>"$TMP/native.err" || native_code=$?
mirvm_code=0
"$MIRVM" run "$SRC" >"$TMP/mirvm.out" 2>"$TMP/mirvm.err" || mirvm_code=$?

if [ "$native_code" -ne 0 ]; then
    echo "FAIL m51_simd_insert: native exited $native_code"
    cat "$TMP/native.err"
    exit 1
fi
if grep -Fxq 'sse4.1 unavailable' "$TMP/native.out"; then
    if [ "$mirvm_code" -eq 0 ] && diff -u "$TMP/native.out" "$TMP/mirvm.out"; then
        echo "SKIP m51_simd_insert: host lacks SSE4.1"
        exit 0
    fi
    echo "FAIL m51_simd_insert: unavailable-host behavior differs"
    cat "$TMP/mirvm.err"
    exit 1
fi
if [ "$mirvm_code" -ne 0 ]; then
    echo "FAIL m51_simd_insert: mirvm exited $mirvm_code"
    cat "$TMP/mirvm.err"
    exit 1
fi
if ! diff -u "$TMP/native.out" "$TMP/mirvm.out"; then
    echo "FAIL m51_simd_insert: stdout differs"
    exit 1
fi

echo "PASS m51_simd_insert"
