#!/usr/bin/env bash
# A2 deps-image gate（s3b-a2-design；A2-3 起默认开启）：
#   1. 冷写：split lower 全量并产出 image 文件
#   2. 热读：image 命中，加载相总账（装载+lower）≤300ms（s3b 原锚点）
#   3. 编辑 bin 重跑：image 命中（不重建）+ 输出正确 + ≤300ms
#   4. L2 矩阵：image 在场时 L2 命中（cache-load ≤300ms）
#   5. 双态：MIRVM_NO_DEPS_IMAGE=1 旁路 vs 默认输出一致
#   6. S3′c：同 workspace 第二 bin 白拿 image（键无项目身份；零新 image 文件）
# oracle = 固定输出串 + 双态一致 + 时序门 + image 文件计数；任一缺失即 FAIL
# （deps/*.img 在本 gate 起手清空——内容寻址可再生，自愈由设计保证；sysroot/底座不动）。
set -uo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/../../support/harness.sh"
test_enter_repo

MIRVM=${MIRVM:-target/release/mirvm}
RUSTC=${RUSTC:-rustc}
# 本 gate 恒走 cargo compat 轨（D15 P3 双轨纪律）：S3′c 跨 bin 共享的
# deps-image 键 = fnv(底座键, --extern 产物盖戳)——内容寻址按轨分立，
# self 轨（target/cargoless）与 cargo 轨（target/mirvm）产物名/盖戳不同，
# 跨轨共享原理上不可能；且步骤 6 本就手工驱动裸 cargo（self 路径尚无
# --bin 多 bin 选择，P4 记档）。self 轨的 deps-image 行为由 gate 的
# corpus 段（DEPS=self 全量跑）隐含覆盖。
export MIRVM_DEPS=cargo
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
cp -r tests/fixtures/a2_ws "$TMP/a2_ws"
WS="$TMP/a2_ws"
HOST=$($RUSTC -vV | sed -n 's/^host: //p')
DEPS=${MIRVM_HOME:-$HOME/.mirvm}/deps
SYSROOT=${MIRVM_HOME:-$HOME/.mirvm}/sysroot-$HOST
# 统一依赖存储（D14）：mirvm run（步骤 1-5）经 cargo_project_command 落在共享
# target dir，本脚本手工驱动的 bin2 必须同址，否则 extern 盖戳不同、image 不共享
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

# 1) 冷写（L2 旁路保证确定性冷启）：split 全量 + image 落盘
before=$(ls "$DEPS" | wc -l)
MIRVM_NO_IR_CACHE=1 MIRVM_TIMING=1 "$MIRVM" run "$WS" >"$TMP/cold.out" 2>"$TMP/cold.timing" \
    || abort_test "冷写退出非零"
grep -q 'a2_one: found quick at 9' "$TMP/cold.out" || abort_test "冷写输出错: $(cat "$TMP/cold.out")"
after=$(ls "$DEPS" | wc -l)
[ "$after" -gt "$before" ] || abort_test "冷写未产出 image 文件"

# 2) 热读（L2 旁路，强制走 image 路径）：image 命中，只降 delta
MIRVM_NO_IR_CACHE=1 MIRVM_TIMING=1 "$MIRVM" run "$WS" >"$TMP/warm.out" 2>"$TMP/warm.timing" \
    || abort_test "热读退出非零"
grep -q 'a2_one: found quick at 9' "$TMP/warm.out" || abort_test "热读输出错"
ms=$(lower_ms "$TMP/warm.timing")
[ -n "$ms" ] || abort_test "热读无 lower 计时（image 未命中？）: $(cat "$TMP/warm.timing")"
le300 "$ms" || abort_test "热读 lower ${ms}ms > 300ms"

# 3) 编辑 bin1 重跑：输出正确 + image 不重建 + ≤300ms
imgs_before_edit=$(ls "$DEPS" | wc -l)
sed -i 's/found quick at/found QUICK at/' "$WS/src/bin/a2_one.rs"
MIRVM_NO_IR_CACHE=1 MIRVM_TIMING=1 "$MIRVM" run "$WS" >"$TMP/edit.out" 2>"$TMP/edit.timing" \
    || abort_test "编辑重跑退出非零"
grep -q 'a2_one: found QUICK at 9' "$TMP/edit.out" || abort_test "编辑重跑输出错: $(cat "$TMP/edit.out")"
ms=$(lower_ms "$TMP/edit.timing")
[ -n "$ms" ] || abort_test "编辑重跑无 lower 计时"
le300 "$ms" || abort_test "编辑重跑 lower ${ms}ms > 300ms"
[ "$(ls "$DEPS" | wc -l)" -eq "$imgs_before_edit" ] || abort_test "编辑重跑重建了 image（键含 bin 源？）"

# 4) L2 矩阵：image 在场时未编辑复跑命中 L2（首跑入账、次跑命中）
MIRVM_TIMING=1 "$MIRVM" run "$WS" >"$TMP/l2a.out" 2>"$TMP/l2a.timing" || abort_test "L2 入账跑退出非零"
MIRVM_TIMING=1 "$MIRVM" run "$WS" >"$TMP/l2.out" 2>"$TMP/l2.timing" || abort_test "L2 复跑退出非零"
grep -q 'a2_one: found QUICK at 9' "$TMP/l2.out" || abort_test "L2 复跑输出错"
ms=$(cache_ms "$TMP/l2.timing")
[ -n "$ms" ] || abort_test "L2 复跑未命中（无 cache-load）: $(cat "$TMP/l2.timing")"
le300 "$ms" || abort_test "L2 cache-load ${ms}ms > 300ms"

# 5) 双态：旁路 vs 默认输出一致（冷路径，L2 旁路）
MIRVM_NO_DEPS_IMAGE=1 MIRVM_NO_IR_CACHE=1 "$MIRVM" run "$WS" >"$TMP/bypass.out" 2>/dev/null \
    || abort_test "旁路运行退出非零"
MIRVM_NO_IR_CACHE=1 "$MIRVM" run "$WS" >"$TMP/default.out" 2>/dev/null || abort_test "默认运行退出非零"
diff -q "$TMP/bypass.out" "$TMP/default.out" >/dev/null || abort_test "双态输出不一致"

# 6) S3′c：同 workspace 第二 bin（未跑过）白拿 bin1 的 image——
#    零新 image 文件 + ≤300ms（键无项目身份，跨 bin 共享）
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
) || abort_test "S3′c bin2 运行退出非零"
grep -q 'a2_two: found box at 13' "$TMP/s3c.out" || abort_test "S3′c 输出错: $(cat "$TMP/s3c.out")"
ms=$(lower_ms "$TMP/s3c.timing")
[ -n "$ms" ] || abort_test "S3′c 无 lower 计时（image 未共享？）: $(cat "$TMP/s3c.timing")"
le300 "$ms" || abort_test "S3′c lower ${ms}ms > 300ms"
[ "$(ls "$DEPS" | wc -l)" -eq "$imgs_before_s3c" ] || abort_test "S3′c bin2 重建了 image（未共享）"

ok "冷写、热读、编辑、旁路和跨 bin 共享"
suite_summary contracts.deps-image
