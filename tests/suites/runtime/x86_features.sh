#!/usr/bin/env bash
# x86 特性探针。
# 每件以 native 编译直跑为 oracle，逐字节对比 mirvm；宿主缺 CPU 特性时该件
# 必须打 SKIP（不得冒充 PASS），x86_vectors 的 pshufb/sha 两个子能力分别记账。
# 每项能力分别记账，宿主不支持时记 SKIP。
set -u
. "$(dirname "${BASH_SOURCE[0]}")/../../support/harness.sh"
test_enter_repo
MIRVM=${MIRVM:-$(pwd)/target/release/mirvm}
RUSTC=${RUSTC:-rustc}
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
# probe_diff <名字> <fixture> <unavailable 行(可空)> [vm-call...]
# 通用骨架：native 编译直跑 → mirvm run（可多次 vm-call 追加输出）→ 逐字节 diff。
# 打印 PASS|SKIP|FAIL m51_<名字>(: 原因) 一行；返回 0=PASS/SKIP，1=FAIL。
probe_diff() {
    local name="$1" src="tests/fixtures/$2" unavail="$3"
    shift 3
    "$RUSTC" --edition 2024 -o "$TMP/$name.native" "$src" 2>"$TMP/$name.rustc.err" || {
        echo "FAIL m51_$name: rustc 编译失败"; cat "$TMP/$name.rustc.err"; return 1; }
    local native_code=0 mirvm_code=0
    "$TMP/$name.native" >"$TMP/$name.native.out" 2>"$TMP/$name.native.err" || native_code=$?
    if [ $# -gt 0 ]; then
        : >"$TMP/$name.mirvm.out"; : >"$TMP/$name.mirvm.err"
        local call
        for call in "$@"; do
            [ "$mirvm_code" -eq 0 ] || break
            "$MIRVM" run "$src" --vm-call "$call" \
                >>"$TMP/$name.mirvm.out" 2>>"$TMP/$name.mirvm.err" || mirvm_code=$?
        done
    else
        "$MIRVM" run "$src" >"$TMP/$name.mirvm.out" 2>"$TMP/$name.mirvm.err" || mirvm_code=$?
    fi
    if [ "$native_code" -ne 0 ]; then
        echo "FAIL m51_$name: native exited $native_code"; cat "$TMP/$name.native.err"; return 1
    fi
    if [ -n "$unavail" ] && grep -Fxq "$unavail" "$TMP/$name.native.out"; then
        if [ "$mirvm_code" -eq 0 ] && diff -u "$TMP/$name.native.out" "$TMP/$name.mirvm.out" >/dev/null; then
            case "$unavail" in
                'avx2 unavailable')    skip "m51_$name: host lacks AVX2" ;;
                'sse4.1 unavailable')  skip "m51_$name: host lacks SSE4.1" ;;
                'avx unavailable')     skip "m51_$name: host lacks AVX" ;;
                'xgetbv unavailable')  skip "m51_$name: host CPUID lacks XSAVE/OSXSAVE" ;;
                *)                     skip "m51_$name: host lacks required CPU feature" ;;
            esac
            return 0
        fi
        echo "FAIL m51_$name: unavailable-host behavior differs"; cat "$TMP/$name.mirvm.err"; return 1
    fi
    if [ "$mirvm_code" -ne 0 ]; then
        echo "FAIL m51_$name: mirvm exited $mirvm_code"; cat "$TMP/$name.mirvm.err"; return 1
    fi
    if ! diff -u "$TMP/$name.native.out" "$TMP/$name.mirvm.out" >/dev/null; then
        echo "FAIL m51_$name: stdout differs"; diff -u "$TMP/$name.native.out" "$TMP/$name.mirvm.out" | head -10; return 1
    fi
    ok "m51_$name"
}

# x86_vectors：feature-gated stdarch；native 输出携带 pshuf=/sha= 状态行，
# diff 一致后按子能力分别打 PASS/SKIP。
probe_x86_vectors() {
    local name=x86_vectors src=tests/fixtures/m51_x86_vectors.rs
    "$RUSTC" --edition 2024 -o "$TMP/$name.native" "$src" 2>"$TMP/$name.rustc.err" || {
        echo "FAIL m51_x86_vectors: rustc 编译失败"; cat "$TMP/$name.rustc.err"; return 1; }
    "$TMP/$name.native" >"$TMP/$name.native.out" 2>"$TMP/$name.native.err"
    local mirvm_code=0
    "$MIRVM" run "$src" >"$TMP/$name.mirvm.out" 2>"$TMP/$name.mirvm.err" || mirvm_code=$?
    if [ "$mirvm_code" -ne 0 ]; then
        echo "FAIL m51_x86_vectors: mirvm exited $mirvm_code"; tail -5 "$TMP/$name.mirvm.err"; return 1
    fi
    if ! diff -u "$TMP/$name.native.out" "$TMP/$name.mirvm.out" >/dev/null; then
        echo "FAIL m51_x86_vectors: stdout differs"; return 1
    fi
    local spec key feature status
    for spec in pshuf:pshufb sha:sha; do
        key=${spec%%:*}
        feature=${spec#*:}
        status=$(grep -E "^${key}=([0-9a-f]{16}|unavailable)$" "$TMP/$name.native.out" || true)
        if [ "$(printf '%s\n' "$status" | grep -c .)" -ne 1 ]; then
            echo "FAIL m51_x86_vectors/$feature: native status missing or ambiguous"
            return 1
        fi
        if [ "$status" = "$key=unavailable" ]; then
            skip "m51_x86_vectors/$feature: host lacks $feature"
        else
            ok "m51_x86_vectors/$feature"
        fi
    done
}

probe_diff addcarry m51_addcarry.rs '' 'add_checksum()' 'sub_checksum()' || fail=$((fail + 1))
probe_diff xgetbv m51_xgetbv.rs 'xgetbv unavailable' || fail=$((fail + 1))
probe_diff simd_insert m51_simd_insert.rs 'sse4.1 unavailable' || fail=$((fail + 1))
probe_diff simd_shift m51_simd_shift.rs 'avx2 unavailable' || fail=$((fail + 1))
probe_diff vzeroupper m51_vzeroupper.rs 'avx unavailable' || fail=$((fail + 1))
probe_x86_vectors || fail=$((fail + 1))

suite_summary runtime.x86-features
