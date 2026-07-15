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
set -euo pipefail
cd "$(dirname "$0")/.."

MIRVM=${MIRVM:-target/release/mirvm}
WS=tests/fixtures/a2_ws
HOST=$(rustc -vV | sed -n 's/^host: //p')
DEPS=$HOME/.cache/mirvm/deps
SYSROOT=$HOME/.cache/mirvm/sysroot-$HOST
CHANNEL=$(sed -n 's/^channel *= *"\(.*\)"/\1/p' rust-toolchain.toml)
CARGO=$HOME/.rustup/toolchains/$CHANNEL-$HOST/bin/cargo
MIRVM_ABS=$(cd "$(dirname "$MIRVM")" && pwd)/$(basename "$MIRVM")
TMP=$(mktemp -d)
cp "$WS/src/bin/a2_one.rs" "$TMP/a2_one.rs.bak"
trap 'cp "$TMP/a2_one.rs.bak" "$WS/src/bin/a2_one.rs" 2>/dev/null || true; rm -rf "$TMP" tests/fixtures/a2_ws/target' EXIT

fail() { echo "FAIL a2_deps_image: $*"; exit 1; }

[ -x "$MIRVM" ] || fail "mirvm 不存在: $MIRVM（先 cargo build --release）"
[ -d "$SYSROOT" ] || fail "sysroot 不存在: $SYSROOT（先跑一次任意 mirvm run）"
[ -x "$CARGO" ] || fail "锁定工具链 cargo 不存在: $CARGO"
mkdir -p "$DEPS"
rm -f "$DEPS"/*.img

lower_ms() { sed -n 's/.*lower=\([0-9.]*\)ms.*/\1/p' "$1" | head -1; }
cache_ms() { sed -n 's/.*cache-load=\([0-9.]*\)ms.*/\1/p' "$1" | head -1; }
le300() { awk -v m="$1" 'BEGIN{exit !(m+0<=300)}'; }

# 1) 冷写（L2 旁路保证确定性冷启）：split 全量 + image 落盘
before=$(ls "$DEPS" | wc -l)
MIRVM_NO_IR_CACHE=1 MIRVM_TIMING=1 "$MIRVM" run "$WS" >"$TMP/cold.out" 2>"$TMP/cold.timing" \
    || fail "冷写退出非零"
grep -q 'a2_one: found quick at 9' "$TMP/cold.out" || fail "冷写输出错: $(cat "$TMP/cold.out")"
after=$(ls "$DEPS" | wc -l)
[ "$after" -gt "$before" ] || fail "冷写未产出 image 文件"

# 2) 热读（L2 旁路，强制走 image 路径）：image 命中，只降 delta
MIRVM_NO_IR_CACHE=1 MIRVM_TIMING=1 "$MIRVM" run "$WS" >"$TMP/warm.out" 2>"$TMP/warm.timing" \
    || fail "热读退出非零"
grep -q 'a2_one: found quick at 9' "$TMP/warm.out" || fail "热读输出错"
ms=$(lower_ms "$TMP/warm.timing")
[ -n "$ms" ] || fail "热读无 lower 计时（image 未命中？）: $(cat "$TMP/warm.timing")"
le300 "$ms" || fail "热读 lower ${ms}ms > 300ms"

# 3) 编辑 bin1 重跑：输出正确 + image 不重建 + ≤300ms
imgs_before_edit=$(ls "$DEPS" | wc -l)
sed -i 's/found quick at/found QUICK at/' "$WS/src/bin/a2_one.rs"
MIRVM_NO_IR_CACHE=1 MIRVM_TIMING=1 "$MIRVM" run "$WS" >"$TMP/edit.out" 2>"$TMP/edit.timing" \
    || fail "编辑重跑退出非零"
grep -q 'a2_one: found QUICK at 9' "$TMP/edit.out" || fail "编辑重跑输出错: $(cat "$TMP/edit.out")"
ms=$(lower_ms "$TMP/edit.timing")
[ -n "$ms" ] || fail "编辑重跑无 lower 计时"
le300 "$ms" || fail "编辑重跑 lower ${ms}ms > 300ms"
[ "$(ls "$DEPS" | wc -l)" -eq "$imgs_before_edit" ] || fail "编辑重跑重建了 image（键含 bin 源？）"

# 4) L2 矩阵：image 在场时未编辑复跑命中 L2（首跑入账、次跑命中）
MIRVM_TIMING=1 "$MIRVM" run "$WS" >"$TMP/l2a.out" 2>"$TMP/l2a.timing" || fail "L2 入账跑退出非零"
MIRVM_TIMING=1 "$MIRVM" run "$WS" >"$TMP/l2.out" 2>"$TMP/l2.timing" || fail "L2 复跑退出非零"
grep -q 'a2_one: found QUICK at 9' "$TMP/l2.out" || fail "L2 复跑输出错"
ms=$(cache_ms "$TMP/l2.timing")
[ -n "$ms" ] || fail "L2 复跑未命中（无 cache-load）: $(cat "$TMP/l2.timing")"
le300 "$ms" || fail "L2 cache-load ${ms}ms > 300ms"

# 5) 双态：旁路 vs 默认输出一致（冷路径，L2 旁路）
MIRVM_NO_DEPS_IMAGE=1 MIRVM_NO_IR_CACHE=1 "$MIRVM" run "$WS" >"$TMP/bypass.out" 2>/dev/null \
    || fail "旁路运行退出非零"
MIRVM_NO_IR_CACHE=1 "$MIRVM" run "$WS" >"$TMP/default.out" 2>/dev/null || fail "默认运行退出非零"
diff -q "$TMP/bypass.out" "$TMP/default.out" >/dev/null || fail "双态输出不一致"

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
        --target-dir target/mirvm --quiet --bin a2_two \
        >"$TMP/s3c.out" 2>"$TMP/s3c.timing"
) || fail "S3′c bin2 运行退出非零"
grep -q 'a2_two: found box at 13' "$TMP/s3c.out" || fail "S3′c 输出错: $(cat "$TMP/s3c.out")"
ms=$(lower_ms "$TMP/s3c.timing")
[ -n "$ms" ] || fail "S3′c 无 lower 计时（image 未共享？）: $(cat "$TMP/s3c.timing")"
le300 "$ms" || fail "S3′c lower ${ms}ms > 300ms"
[ "$(ls "$DEPS" | wc -l)" -eq "$imgs_before_s3c" ] || fail "S3′c bin2 重建了 image（未共享）"

echo "PASS a2_deps_image"
