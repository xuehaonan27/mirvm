#!/usr/bin/env bash
# M4.2 gate：unwind——panic 发起/传播/Drop-in-unwind/catch/重抛 经新引擎（--vm-call）
# 结果 == native 直跑所得（期望值内嵌，同源 rustc -O，2026-07-09 生成）。
# 附 --vm-stats 复测：unwind demo 可达集中 M4.2 份内债务 = 0（M4.3/M4.4 归期豁免）。
set -u
cd "$(dirname "$0")/.."
MIRVM=${MIRVM:-$(pwd)/target/release/mirvm}
SRC=demo/m4/unwind.rs
pass=0 fail=0

check() {
    local spec="$1" want="$2"
    local got
    got=$("$MIRVM" run --engine vm --vm-call "$spec" "$SRC" 2>/tmp/m4gate2.err | tail -1)
    local code=$?
    if [ "$code" -eq 0 ] && [ "$got" = "$want" ]; then
        echo "PASS $spec = $got"
        pass=$((pass + 1))
    else
        echo "FAIL $spec: got '$got' (exit=$code), want $want"
        head -3 /tmp/m4gate2.err
        fail=$((fail + 1))
    fi
}

check 'catch_digest(3)' 100010
check 'catch_digest(2)' 7010
check 'nested_digest(1)' 77110
check 'nested_digest(2)' 5110
check 'bounds_digest(1)' 20
check 'bounds_digest(9)' 999
check 'msg_digest(42)' 71
check 'rethrow_digest(1)' 5503
check 'rethrow_digest(2)' 1103

echo "---"
echo "m4-gate2(unwind): $pass pass, $fail fail"
[ $fail -eq 0 ] || exit 1

# 复测：可达集中不得有 M4.1/M4.2 份内债务（resume 已是真终止子；M4.3/M4.4 豁免）
echo "--- vm-stats 复测（M4.1/M4.2 份内债务清零）---"
stats=$("$MIRVM" run --engine vm --vm-stats "$SRC" 2>/dev/null | sed -n '/各导出\/入口的可达 Trap/,$p')
if echo "$stats" | grep -E '_digest.*\| .*M4\.[12]:' >/dev/null; then
    echo "FAIL：可达集中仍有 M4.1/M4.2 份内债务："
    echo "$stats" | grep -E '_digest.*M4\.[12]:'
    exit 1
fi
echo "M4.1/M4.2 份内债务清零 PASS"
