#!/usr/bin/env bash
# 时间性能硬门与空间/cache 资源计量。
#
# 时间硬门（D2/M5.3 既定锚点；SKIP_PERF=1 只跳这三件，计量照常）：
#   ① 空 main 加载相 < 1s（热 sysroot 缓存）
#   ② c_rayon < 5s（含语义 oracle 'par_sort ok = true'）
#   ③ fib(32) JIT ≤ 80ms（≈10× native；解释锚点 940ms。三跑取最小：
#     每 `mirvm run` 是新进程、JIT 后台线程重编译，满载 gate 下偶被饿死一轮
#     （回退解释 940ms）——多跑取最快杀调度毛刺，契约不变）
# 资源计量（总是执行）：
#   cache 各分部体量、磁盘可用、target 预算闸（共享 harness）。
#
# 用法：./tests/run.sh suite performance.limits [--metrics-only]
set -u
. "$(dirname "${BASH_SOURCE[0]}")/../../support/harness.sh"
test_enter_repo
MIRVM=${MIRVM:-$(pwd)/target/release/mirvm}
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

metrics_only=0
case "${1:-}" in
    "") ;;
    --metrics-only) metrics_only=1 ;;
    *) echo "usage: ./tests/run.sh suite performance.limits [--metrics-only]" >&2; exit 64 ;;
esac

if [ "$metrics_only" -eq 0 ]; then
    if [ -n "${SKIP_PERF:-}" ]; then
        skip "性能上限（SKIP_PERF=1；语义 gate 仍照常执行）"
    else
        echo 'fn main(){}' >"$TMP/empty.rs"
        t0=$(date +%s%N); "$MIRVM" run "$TMP/empty.rs" >"$TMP/load.out" 2>"$TMP/load.err"; load_code=$?
        load_ms=$(( ($(date +%s%N) - t0) / 1000000 ))
        [ $load_code -eq 0 ] && [ $load_ms -lt 1000 ] \
            && ok "加载相 ${load_ms}ms（< 1s 硬门）" \
            || bad "加载相 exit=$load_code ${load_ms}ms（要求 exit=0 且 < 1s）"
        t0=$(date +%s%N); "$MIRVM" run corpus/c_rayon.rs >"$TMP/rayon.out" 2>"$TMP/rayon.err"; rayon_code=$?
        rayon_ms=$(( ($(date +%s%N) - t0) / 1000000 ))
        if [ $rayon_code -eq 0 ] && grep -q 'par_sort ok = true' "$TMP/rayon.out" \
            && [ $rayon_ms -lt 5000 ]; then
            rayon_divisor=$rayon_ms; [ $rayon_divisor -gt 0 ] || rayon_divisor=1
            ok "rayon ${rayon_ms}ms（< 5s 硬门；tier-0 28s，≈$((28000/rayon_divisor))×）"
        else
            bad "rayon exit=$rayon_code ${rayon_ms}ms（要求语义 oracle 且 < 5s）"
        fi
        # M5.3 JIT 硬门：fib(32) ≤ 10× native ≈ ≤80ms 墙钟，三跑取最小
        fib_ms=999999 fib_code=1 fib_out=""
        for _ in 1 2 3; do
            t0=$(date +%s%N)
            fib_out=$("$MIRVM" run --vm-call 'fib(32)' demo/m4/pure.rs 2>"$TMP/fib32.err")
            fib_code=$?
            ms=$(( ($(date +%s%N) - t0) / 1000000 ))
            [ $ms -lt $fib_ms ] && fib_ms=$ms
            { [ $fib_code -eq 0 ] && [ "$fib_out" = "2178309" ]; } || break
        done
        if [ $fib_code -eq 0 ] && [ "$fib_out" = "2178309" ] && [ $fib_ms -lt 80 ]; then
            ok "fib(32) JIT ${fib_ms}ms（≤80ms=10× native 硬门；解释锚点 940ms）"
        else
            bad "fib(32) JIT exit=$fib_code out=$fib_out ${fib_ms}ms（要求 2178309 且 <80ms）"
        fi
    fi
fi

echo "== 资源计量 =="
cache_snapshot "perf"
target_budget_check
home_dir=${MIRVM_HOME:-$HOME/.mirvm}
echo "[磁盘] $(df -h "$home_dir" 2>/dev/null | awk 'NR==2{print "可用 "$4" / 共 "$2"（"$5" 已用）"}')"
suite_summary performance.limits
