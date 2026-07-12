#!/usr/bin/env bash
# M5.1 scalar carry/borrow tracer: public stdarch API must match native exactly.
set -euo pipefail
cd "$(dirname "$0")/.."

MIRVM=${MIRVM:-target/debug/mirvm}
RUSTC=${RUSTC:-rustc}
SRC=tests/fixtures/m51_addcarry.rs
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

"$RUSTC" --edition 2024 -o "$TMP/native" "$SRC"

native_code=0
"$TMP/native" >"$TMP/native.out" 2>"$TMP/native.err" || native_code=$?
mirvm_code=0
"$MIRVM" run "$SRC" --vm-call 'add_checksum()' \
    >"$TMP/mirvm.out" 2>"$TMP/mirvm.err" || mirvm_code=$?
if [ "$mirvm_code" -eq 0 ]; then
    "$MIRVM" run "$SRC" --vm-call 'sub_checksum()' \
        >>"$TMP/mirvm.out" 2>>"$TMP/mirvm.err" || mirvm_code=$?
fi

if [ "$native_code" -ne 0 ]; then
    echo "FAIL m51_addcarry: native exited $native_code"
    cat "$TMP/native.err"
    exit 1
fi
if [ "$mirvm_code" -ne 0 ]; then
    echo "FAIL m51_addcarry: mirvm exited $mirvm_code"
    cat "$TMP/mirvm.err"
    exit 1
fi
if ! diff -u "$TMP/native.out" "$TMP/mirvm.out"; then
    echo "FAIL m51_addcarry: stdout differs"
    exit 1
fi

echo "PASS m51_addcarry"
