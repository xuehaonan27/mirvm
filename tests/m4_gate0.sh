#!/usr/bin/env bash
# M4.0 gate：真实 rustc MIR 经 src/lower 降低、在新引擎（--engine vm）上执行，
# 结果 == 预期常量（数学常数，native 等价自明）。
# 末尾跑 TSan harness 构建 = 执行相纯度门禁（vm/ 漏 rustc 类型即编译失败）。
set -u
cd "$(dirname "$0")/.."
MIRVM=${MIRVM:-$(pwd)/target/release/mirvm}
SRC=demo/m4/pure.rs
pass=0 fail=0

check() {
    local spec="$1" want="$2"
    local got
    got=$("$MIRVM" run --engine vm --vm-call "$spec" "$SRC" 2>/tmp/m4gate.err)
    local code=$?
    if [ "$code" -eq 0 ] && [ "$got" = "$want" ]; then
        echo "PASS $spec = $got"
        pass=$((pass + 1))
    else
        echo "FAIL $spec: got '$got' (exit=$code), want $want"
        head -3 /tmp/m4gate.err
        fail=$((fail + 1))
    fi
}

check 'fib(10)' 55
check 'fib(25)' 75025
check 'gcd(1071,462)' 21
check 'gcd(17,5)' 1
check 'sum_to(1000)' 500500
check 'collatz_steps(27)' 111
check 'popcount_manual(12297829382473034410)' 32
check 'mix_signed(3,10)' 13
check 'mix_signed(10,3)' 13

echo "---"
echo "m4-gate0: $pass pass, $fail fail"
[ $fail -eq 0 ] || exit 1

# 执行相纯度门禁（引擎 = 纯 Rust；tsan crate 无 rustc_private 依赖）
echo "--- 纯度门禁（tsan harness 构建）---"
bash tests/spike4_tsan.sh >/dev/null 2>&1 && echo "纯度门禁 PASS（vm/ 零 rustc_private）" || {
    echo "纯度门禁 FAIL"
    exit 1
}
