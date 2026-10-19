#!/usr/bin/env bash
# Validate the own-track launch shortcut separately from dependency preparation.
MODE_FIELDS=""
MODE_REQUIRED=""

mode_run() {
    case_init
    local home="$TMP/home"
    mkdir -p "$home"
    ensure_test_sysroot "$MIRVM" "$home" "$RUSTC" || return $?
    python3 "$REPO_ROOT/tests/lib/helpers/prepared_launch.py" \
        "$MIRVM" "$home" "$TEST_SYSROOT" "$TMP"
}
