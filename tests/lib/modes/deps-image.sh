#!/usr/bin/env bash
# deps-image: shared deps-image contract.
# fields: fixture
MODE_FIELDS="fixture"
MODE_REQUIRED="fixture"

mode_run() {
    case_init
    case_fixtures
    apply_env "$(field env "")"
# This gate always takes the cargo compat track: the cross-bin shared deps-image
# key = fnv(base key, --extern artifact stamp) is content-addressed per track, so
# the self track (target/cargoless) and the cargo track (target/mirvm), which have
# different artifact names/stamps, cannot share in principle; step 6 already drives
# bare cargo manually, because the self path has no --bin multi-bin selection.
# Self-track deps-image behavior is covered implicitly by the gate's corpus segment (DEPS=self full run).
export MIRVM_DEPS=cargo
cp -r ${FIXTURES[0]} "$TMP/a2_ws"
WS="$TMP/a2_ws"
HOST=$(rustc_host)
DEPS=${MIRVM_HOME:-$HOME/.mirvm}/cache/deps
SYSROOT=${MIRVM_HOME:-$HOME/.mirvm}/data/sysroot-$HOST
# Unified dependency storage: mirvm run (steps 1-5) goes through cargo_project_command into shared
# target dir; bin2 manually driven by this script must use same location, otherwise extern stamps differ and image is not shared
TARGET_MIRVM=${MIRVM_TARGET_DIR:-${MIRVM_HOME:-$HOME/.mirvm}/build/target/mirvm}

abort_test() {
    bad "$*"
    suite_summary contracts.deps-image || true
    exit 1
}

[ -d "$SYSROOT" ] || abort_test "sysroot not found: $SYSROOT"
[ -x "$CARGO" ] || abort_test "pinned-toolchain Cargo not found: $CARGO"
mkdir -p "$DEPS"
rm -f "$DEPS"/*.img

lower_ms() { sed -n 's/.*lower=\([0-9.]*\)ms.*/\1/p' "$1" | head -1; }
cache_ms() { sed -n 's/.*cache-load=\([0-9.]*\)ms.*/\1/p' "$1" | head -1; }
le300() { awk -v m="$1" 'BEGIN{exit !(m+0<=300)}'; }

# 1) cold write (L2 bypass ensures deterministic cold start): full split + image to disk
before=$(ls "$DEPS" | wc -l)
MIRVM_NO_IR_CACHE=1 MIRVM_TIMING=1 "$MIRVM" run "$WS" >"$TMP/cold.out" 2>"$TMP/cold.timing" \
    || abort_test "cold write exited non-zero"
grep -q 'a2_one: found quick at 9' "$TMP/cold.out" || abort_test "cold write output wrong: $(cat "$TMP/cold.out")"
after=$(ls "$DEPS" | wc -l)
[ "$after" -gt "$before" ] || abort_test "cold write produced no image file"

# 2) warm read (L2 bypass, force image path): image hit, only lower delta
MIRVM_NO_IR_CACHE=1 MIRVM_TIMING=1 "$MIRVM" run "$WS" >"$TMP/warm.out" 2>"$TMP/warm.timing" \
    || abort_test "warm read exited non-zero"
grep -q 'a2_one: found quick at 9' "$TMP/warm.out" || abort_test "warm read output wrong"
ms=$(lower_ms "$TMP/warm.timing")
[ -n "$ms" ] || abort_test "warm read has no lower timing (image miss?): $(cat "$TMP/warm.timing")"
le300 "$ms" || abort_test "warm read lower ${ms}ms > 300ms"

# 3) edit bin1 rerun: correct output + image not rebuilt + ≤300ms
imgs_before_edit=$(ls "$DEPS" | wc -l)
sed -i 's/found quick at/found QUICK at/' "$WS/src/bin/a2_one.rs"
MIRVM_NO_IR_CACHE=1 MIRVM_TIMING=1 "$MIRVM" run "$WS" >"$TMP/edit.out" 2>"$TMP/edit.timing" \
    || abort_test "edit rerun exited non-zero"
grep -q 'a2_one: found QUICK at 9' "$TMP/edit.out" || abort_test "edit rerun output wrong: $(cat "$TMP/edit.out")"
ms=$(lower_ms "$TMP/edit.timing")
[ -n "$ms" ] || abort_test "edit rerun has no lower timing"
le300 "$ms" || abort_test "edit rerun lower ${ms}ms > 300ms"
[ "$(ls "$DEPS" | wc -l)" -eq "$imgs_before_edit" ] || abort_test "edit rerun rebuilt the image (does the key include bin source?)"

# 4) L2 matrix: unedited rerun hits L2 while image present (first run charges, second run hits)
MIRVM_TIMING=1 "$MIRVM" run "$WS" >"$TMP/l2a.out" 2>"$TMP/l2a.timing" || abort_test "L2 charge run exited non-zero"
MIRVM_TIMING=1 "$MIRVM" run "$WS" >"$TMP/l2.out" 2>"$TMP/l2.timing" || abort_test "L2 rerun exited non-zero"
grep -q 'a2_one: found QUICK at 9' "$TMP/l2.out" || abort_test "L2 rerun output wrong"
ms=$(cache_ms "$TMP/l2.timing")
[ -n "$ms" ] || abort_test "L2 rerun did not hit (no cache-load): $(cat "$TMP/l2.timing")"
le300 "$ms" || abort_test "L2 cache-load ${ms}ms > 300ms"

# 5) dual mode: bypass vs default output consistent (cold path, L2 bypass)
MIRVM_NO_DEPS_IMAGE=1 MIRVM_NO_IR_CACHE=1 "$MIRVM" run "$WS" >"$TMP/bypass.out" 2>/dev/null \
    || abort_test "bypass run exited non-zero"
MIRVM_NO_IR_CACHE=1 "$MIRVM" run "$WS" >"$TMP/default.out" 2>/dev/null || abort_test "default run exited non-zero"
diff -q "$TMP/bypass.out" "$TMP/default.out" >/dev/null || abort_test "dual-mode outputs differ"

# 6) second bin in the same workspace (never run) gets bin1's image for free --
#    zero new image files + ≤300ms (key has no project identity, shared across bins)
imgs_before_s3c=$(ls "$DEPS" | wc -l)
(
    cd "$WS"
    env -u RUSTC_WORKSPACE_WRAPPER -u CARGO_BUILD_RUSTC_WRAPPER \
        -u CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER \
        MIRVM_CARGO_SESSION=1 MIRVM_SYSROOT="$SYSROOT" RUSTC_WRAPPER="$MIRVM" \
        MIRVM_TIMING=1 MIRVM_NO_IR_CACHE=1 \
        "$CARGO" run --target "$HOST" \
        --config "target.'cfg(all())'.runner=['$MIRVM','runner']" \
        --target-dir "$TARGET_MIRVM" --quiet --bin a2_two \
        >"$TMP/s3c.out" 2>"$TMP/s3c.timing"
) || abort_test "second-bin run exited non-zero"
grep -q 'a2_two: found box at 13' "$TMP/s3c.out" || abort_test "second-bin output wrong: $(cat "$TMP/s3c.out")"
ms=$(lower_ms "$TMP/s3c.timing")
[ -n "$ms" ] || abort_test "second bin has no lower timing (image not shared?): $(cat "$TMP/s3c.timing")"
le300 "$ms" || abort_test "second-bin lower ${ms}ms > 300ms"
[ "$(ls "$DEPS" | wc -l)" -eq "$imgs_before_s3c" ] || abort_test "second bin rebuilt the image (not shared)"

ok "cold write, warm read, edit, bypass and cross-bin sharing"
}
