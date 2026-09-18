#!/usr/bin/env bash
# A2 deps-image gate (s3b-a2-design; default enabled since A2-3):
#   1. cold write: full split lower and produce image files
#   2. warm read: image hit, load-phase total (load+lower) ≤300ms (original s3b anchor)
#   3. edit bin rerun: image hit (no rebuild) + correct output + ≤300ms
#   4. L2 matrix: L2 hit when image present (cache-load ≤300ms)
#   5. dual mode: MIRVM_NO_DEPS_IMAGE=1 bypass vs default output consistent
#   6. S3′c: second bin in same workspace gets image for free (key has no project identity; zero new image files)
# oracle = fixed output string + dual-mode consistency + timing gate + image file count; any missing = FAIL
# (deps/*.img cleared at start of this gate — content-addressable and regeneratable, self-heal guaranteed by design; sysroot/base unchanged).
set -uo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/../../support/harness.sh"
test_enter_repo

MIRVM=${MIRVM:-target/release/mirvm}
RUSTC=${RUSTC:-rustc}
# This gate always takes the cargo compat track (D15 P3 dual-track discipline): S3′c cross-bin shared
# deps-image key = fnv(base key, --extern artifact stamp) — content-addressed per-track,
# self track (target/cargoless) and cargo track (target/mirvm) have different artifact names/stamps,
# cross-track sharing impossible in principle; and step 6 already manually drives bare cargo (self path has no
# --bin multi-bin selection, filed in P4). self-track deps-image behavior is covered by the gate's
# corpus segment (DEPS=self full run) implicitly.
export MIRVM_DEPS=cargo
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
cp -r tests/fixtures/a2_ws "$TMP/a2_ws"
WS="$TMP/a2_ws"
HOST=$($RUSTC -vV | sed -n 's/^host: //p')
DEPS=${MIRVM_HOME:-$HOME/.mirvm}/deps
SYSROOT=${MIRVM_HOME:-$HOME/.mirvm}/sysroot-$HOST
# Unified dependency storage (D14): mirvm run (steps 1-5) goes through cargo_project_command into shared
# target dir; bin2 manually driven by this script must use same location, otherwise extern stamps differ and image is not shared
TARGET_MIRVM=${MIRVM_TARGET_DIR:-${MIRVM_HOME:-$HOME/.mirvm}/target/mirvm}
CHANNEL=$(sed -n 's/^channel *= *"\(.*\)"/\1/p' rust-toolchain.toml)
CARGO=${CARGO:-$HOME/.rustup/toolchains/$CHANNEL-$HOST/bin/cargo}
MIRVM_ABS=$(cd "$(dirname "$MIRVM")" && pwd)/$(basename "$MIRVM")

abort_test() {
    bad "$*"
    suite_summary contracts.deps-image || true
    exit 1
}

[ -x "$MIRVM" ] || abort_test "mirvm 不存在: $MIRVM"
[ -d "$SYSROOT" ] || abort_test "sysroot 不存在: $SYSROOT"
[ -x "$CARGO" ] || abort_test "固定工具链 Cargo 不存在: $CARGO"
mkdir -p "$DEPS"
rm -f "$DEPS"/*.img

lower_ms() { sed -n 's/.*lower=\([0-9.]*\)ms.*/\1/p' "$1" | head -1; }
cache_ms() { sed -n 's/.*cache-load=\([0-9.]*\)ms.*/\1/p' "$1" | head -1; }
le300() { awk -v m="$1" 'BEGIN{exit !(m+0<=300)}'; }

# 1) cold write (L2 bypass ensures deterministic cold start): full split + image to disk
before=$(ls "$DEPS" | wc -l)
MIRVM_NO_IR_CACHE=1 MIRVM_TIMING=1 "$MIRVM" run "$WS" >"$TMP/cold.out" 2>"$TMP/cold.timing" \
    || abort_test "cold write exited non-zero"
grep -q 'a2_one: found quick at 9' "$TMP/cold.out" || abort_test "冷写输出错: $(cat "$TMP/cold.out")"
after=$(ls "$DEPS" | wc -l)
[ "$after" -gt "$before" ] || abort_test "冷写未产出 image 文件"

# 2) warm read (L2 bypass, force image path): image hit, only lower delta
MIRVM_NO_IR_CACHE=1 MIRVM_TIMING=1 "$MIRVM" run "$WS" >"$TMP/warm.out" 2>"$TMP/warm.timing" \
    || abort_test "warm read exited non-zero"
grep -q 'a2_one: found quick at 9' "$TMP/warm.out" || abort_test "warm read output wrong"
ms=$(lower_ms "$TMP/warm.timing")
[ -n "$ms" ] || abort_test "热读无 lower 计时（image 未命中？）: $(cat "$TMP/warm.timing")"
le300 "$ms" || abort_test "热读 lower ${ms}ms > 300ms"

# 3) edit bin1 rerun: correct output + image not rebuilt + ≤300ms
imgs_before_edit=$(ls "$DEPS" | wc -l)
sed -i 's/found quick at/found QUICK at/' "$WS/src/bin/a2_one.rs"
MIRVM_NO_IR_CACHE=1 MIRVM_TIMING=1 "$MIRVM" run "$WS" >"$TMP/edit.out" 2>"$TMP/edit.timing" \
    || abort_test "edit rerun exited non-zero"
grep -q 'a2_one: found QUICK at 9' "$TMP/edit.out" || abort_test "edit rerun output wrong: $(cat "$TMP/edit.out")"
ms=$(lower_ms "$TMP/edit.timing")
[ -n "$ms" ] || abort_test "编辑重跑无 lower 计时"
le300 "$ms" || abort_test "编辑重跑 lower ${ms}ms > 300ms"
[ "$(ls "$DEPS" | wc -l)" -eq "$imgs_before_edit" ] || abort_test "编辑重跑重建了 image（键含 bin 源？）"

# 4) L2 matrix: unedited rerun hits L2 while image present (first run charges, second run hits)
MIRVM_TIMING=1 "$MIRVM" run "$WS" >"$TMP/l2a.out" 2>"$TMP/l2a.timing" || abort_test "L2 入账跑退出非零"
MIRVM_TIMING=1 "$MIRVM" run "$WS" >"$TMP/l2.out" 2>"$TMP/l2.timing" || abort_test "L2 复跑退出非零"
grep -q 'a2_one: found QUICK at 9' "$TMP/l2.out" || abort_test "L2 rerun output wrong"
ms=$(cache_ms "$TMP/l2.timing")
[ -n "$ms" ] || abort_test "L2 复跑未命中（无 cache-load）: $(cat "$TMP/l2.timing")"
le300 "$ms" || abort_test "L2 cache-load ${ms}ms > 300ms"

# 5) dual mode: bypass vs default output consistent (cold path, L2 bypass)
MIRVM_NO_DEPS_IMAGE=1 MIRVM_NO_IR_CACHE=1 "$MIRVM" run "$WS" >"$TMP/bypass.out" 2>/dev/null \
    || abort_test "bypass run exited non-zero"
MIRVM_NO_IR_CACHE=1 "$MIRVM" run "$WS" >"$TMP/default.out" 2>/dev/null || abort_test "默认运行退出非零"
diff -q "$TMP/bypass.out" "$TMP/default.out" >/dev/null || abort_test "双态输出不一致"

# 6) S3′c: second bin in same workspace (never run) gets bin1's image for free —
#    zero new image files + ≤300ms (key has no project identity, shared across bins)
imgs_before_s3c=$(ls "$DEPS" | wc -l)
(
    cd "$WS"
    env -u RUSTC_WORKSPACE_WRAPPER -u CARGO_BUILD_RUSTC_WRAPPER \
        -u CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER \
        MIRVM_CARGO_SESSION=1 MIRVM_SYSROOT="$SYSROOT" RUSTC_WRAPPER="$MIRVM_ABS" \
        MIRVM_TIMING=1 MIRVM_NO_IR_CACHE=1 \
        "$CARGO" run --target "$HOST" \
        --config "target.'cfg(all())'.runner=['$MIRVM_ABS','runner']" \
        --target-dir "$TARGET_MIRVM" --quiet --bin a2_two \
        >"$TMP/s3c.out" 2>"$TMP/s3c.timing"
) || abort_test "S3′c bin2 run exited non-zero"
grep -q 'a2_two: found box at 13' "$TMP/s3c.out" || abort_test "S3′c output wrong: $(cat "$TMP/s3c.out")"
ms=$(lower_ms "$TMP/s3c.timing")
[ -n "$ms" ] || abort_test "S3′c 无 lower 计时（image 未共享？）: $(cat "$TMP/s3c.timing")"
le300 "$ms" || abort_test "S3′c lower ${ms}ms > 300ms"
[ "$(ls "$DEPS" | wc -l)" -eq "$imgs_before_s3c" ] || abort_test "S3′c bin2 重建了 image（未共享）"

ok "cold write, warm read, edit, bypass and cross-bin sharing"
suite_summary contracts.deps-image
