#!/usr/bin/env bash
# M5.5 收口 gate（m5-design §7 退出判据）：
#  ① M5.1 用例可信 oracle 转绿 / ② fib(32) JIT ≤10× native / ③ 全量三重差分
#  ④ 性能上限无回归 / ⑤ TSan 零警告 + spikes + 纯度门禁
#  ——①-⑤ 全部由 m4_gate5 覆盖（m51 六 tracer、fib 硬门、diff 双态、
#  加载/rayon 硬门、gate0 纯度、gate4 TSan/spike4），本脚本复跑 gate5 全量；
#  增量 = ⑥ vmctx 检查点落笔核查 + JIT 助手频度统计冒烟 + ⑦ M5 收口条目核查。
set -u
cd "$(dirname "$0")/.."
MIRVM=${MIRVM:-target/debug/mirvm}
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
pass=0 fail=0
ok() { echo "PASS $1"; pass=$((pass + 1)); }
bad() { echo "FAIL $1"; fail=$((fail + 1)); }

# ①-⑤：gate5 全量复跑
if MIRVM="$MIRVM" bash tests/m4_gate5.sh >"$TMP/g5.out" 2>&1; then
    ok "m4_gate5 全量（退出判据①-⑤）"
else
    bad "m4_gate5（见下）"
    tail -20 "$TMP/g5.out"
fi

# ⑥ vmctx-passing §7 活检查点：T3 落笔 + 双触发器（E6 / 多 Engine）在位
if grep -q "2026-07-19 更新（T3" docs/designs/vmctx-passing.md \
    && grep -q "多 Engine 嵌入立项" docs/designs/vmctx-passing.md \
    && grep -q "E6 进场" docs/designs/vmctx-passing.md; then
    ok "vmctx-passing §7 活检查点（T 基线 + 双触发器）"
else
    bad "vmctx-passing §7 未见 T3 落笔/双触发器"
fi

# JIT 助手频度统计冒烟（M5.5 D5 计量基线）：dump 行在 + panic 密径桶非零
MIRVM_JIT_STATS=1 "$MIRVM" run demo/jit_unwind_probe.rs >"$TMP/uw.out" 2>"$TMP/uw.err"
if grep -q "^mirvm-jit-stats:" "$TMP/uw.err" && grep -q "c2i=[1-9]" "$TMP/uw.err"; then
    ok "JIT 助手频度统计冒烟（atexit dump + 桶计数非零）"
else
    bad "JIT 助手频度统计（见下）"
    tail -3 "$TMP/uw.err"
fi

# ⑦ M5 收口条目落笔（decision-history §7.20）
if grep -q "^### 7.20" docs/decision-history.md; then
    ok "M5 收口条目（decision-history §7.20）"
else
    bad "decision-history §7.20 未见"
fi

echo "---"
echo "m5-gate6(收口): $pass pass, $fail fail"
[ "$fail" -eq 0 ]
