#!/usr/bin/env bash
# M4.1 gate：值与内存——digest 九函数经新引擎（--vm-call）结果 == native 直跑所得
# （期望值内嵌，来自同源 rustc -O 编译直跑，2026-07-08 生成——digest.rs 改动须同步再生），
# 外加 --vm-stats 复测：九函数可达集中 M4.1 份内债务 = 0（resume/M4.2/M4.3/M4.4 归期豁免——
# 全在 panic/OS/TLS 分支；执行路径由上半段实测覆盖）。
set -u
cd "$(dirname "$0")/.."
MIRVM=${MIRVM:-$(pwd)/target/release/mirvm}
SRC=demo/m4/digest.rs
pass=0 fail=0

check() {
    local spec="$1" want="$2"
    local got
    got=$("$MIRVM" run --engine vm --vm-call "$spec" "$SRC" 2>/tmp/m4gate1.err)
    local code=$?
    if [ "$code" -eq 0 ] && [ "$got" = "$want" ]; then
        echo "PASS $spec = $got"
        pass=$((pass + 1))
    else
        echo "FAIL $spec: got '$got' (exit=$code), want $want"
        head -3 /tmp/m4gate1.err
        fail=$((fail + 1))
    fi
}

# 期望值 = 同源 native（rustc -O）直跑
check 'vec_digest(10)' 155
check 'string_digest(5)' 14655057013503867354
check 'map_digest(20)' 2490
check 'box_digest(6)' 43
check 'enum_digest(7)' 805
check 'slice_digest(7)' 284
check 'static_digest(5)' 913
check 'rawptr_digest(3)' 84
check 'float_digest(4)' 4619054945356742656

echo "---"
echo "m4-gate1(digest): $pass pass, $fail fail"
[ $fail -eq 0 ] || exit 1

# --vm-stats 复测：九函数可达集中不得再有 M4.1 份内债务（"M4.1:" 标签清零；
# M4.1+/M4.2/M4.3/M4.4/foreign 是归期豁免——见 docs/m4-log.md M4.1 条目）
echo "--- vm-stats 复测（M4.1 份内债务清零）---"
stats=$("$MIRVM" run --engine vm --vm-stats "$SRC" 2>/dev/null | sed -n '/各导出\/入口的可达 Trap/,$p')
if echo "$stats" | grep -E '_digest.*\| .*M4\.1:' >/dev/null; then
    echo "FAIL：可达集中仍有 M4.1 份内债务："
    echo "$stats" | grep -E '_digest.*M4\.1:'
    exit 1
fi
echo "M4.1 份内债务清零 PASS"
