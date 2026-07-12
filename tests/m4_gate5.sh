#!/usr/bin/env bash
# M4.5 gate（M4 收官，随 M5 前沿滚动）：corpus required-green + 明示 XFAIL
# + diff_cargo + 性能上限
# + 全量回归。
# 前沿记账（透明）：M5.1 已清掉 blake3/sha2/numbigint/ecosystem 的滚动红；
# signal guest handler 与 guest backtrace/frame-IP 属独立边界，仍以原因锁定的
# XFAIL 诚实记账。
# 用法：bash tests/m4_gate5.sh          # 全量
#       SKIP_TSAN=1 bash tests/m4_gate5.sh
#       SKIP_PERF=1 bash tests/m4_gate5.sh  # 共享 CI runner：跳过时序门，不跳语义门
set -u
cd "$(dirname "$0")/.."
MIRVM=${MIRVM:-$(pwd)/target/release/mirvm}
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
pass=0 xfail=0 skip_count=0 fail=0
ok()  { pass=$((pass + 1)); echo "PASS $*"; }
red() { xfail=$((xfail + 1)); echo "XFAIL $*"; }
skip() { skip_count=$((skip_count + 1)); echo "SKIP $*"; }
bad() { fail=$((fail + 1)); echo "FAIL $*"; }
NUMBIGINT_ORACLE='50! = 30414093201713378043612608166064768844377641568960512000000000000
2^1000 mod 1e9+7 = 688423210
a^2 = 15241578753238836750495351562536198787501905199875019052100'
BLAKE3_ORACLE='blake3(0..1000 le) = 62c6155664d5beb5b693c552de9f614cb6e224461a87c045e3f45cae91fe2bd8
blake3("") = af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262
blake3("hello") = ea8f163db38682925e4491c5e58d4bb3506ef8c14eb78a86e908c5624a67200f'
SHA2_ORACLE='sha256(hello world) = b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9
sha256() = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855'

# ---- ① corpus：required-green + 原因锁定的 expected-red ----
echo "== corpus =="
CORPUS_PROGS=${CORPUS_PROGS:-"itertools anyhow rayon chrono indexmap clap csv crossbeam tokio blake3 \
tempfile walkdir numbigint smallvec bytes sha2 petgraph net_tcp net_udp \
process tokio_mt mmap blocking_io net_echo_threaded signal backtrace volatile"}
for p in $CORPUS_PROGS; do
    src="corpus/c_$p.rs"
    [ -f "$src" ] || continue
    timeout 90 "$MIRVM" run "$src" >"$TMP/corpus-$p.out" 2>"$TMP/corpus-$p.err"; code=$?
    stdout=$(cat "$TMP/corpus-$p.out")
    out=$(cat "$TMP/corpus-$p.out" "$TMP/corpus-$p.err")
    red_pattern="" red_label=""
    case "$p" in
        signal)
            red_pattern='unsupported builtin.*signal|unsupported.*signal.*builtin'
            red_label='异步 signal 尚未支持；后续里程碑'
            ;;
        backtrace)
            red_pattern='unsupported builtin.*_Unwind_Backtrace'
            red_label='guest frame/IP 映射尚未支持；禁止伪造宿主回溯'
            ;;
    esac
    if [ "$p" = numbigint ] && { [ $code -ne 0 ] \
        || [ "$stdout" != "$NUMBIGINT_ORACLE" ]; }; then
        bad "c_numbigint oracle (exit=$code stdout='$stdout')"
    elif [ "$p" = blake3 ] && { [ $code -ne 0 ] \
        || [ "$stdout" != "$BLAKE3_ORACLE" ]; }; then
        bad "c_blake3 oracle (exit=$code stdout='$stdout')"
    elif [ "$p" = sha2 ] && { [ $code -ne 0 ] \
        || [ "$stdout" != "$SHA2_ORACLE" ]; }; then
        bad "c_sha2 oracle (exit=$code stdout='$stdout')"
    elif [ "$p" = volatile ] && { [ $code -ne 0 ] \
        || [ "$stdout" != 'volatile aligned=0x0123456789abcdef unaligned=0x89abcdef pair=0x90a0b0c0d0e0f000:0x1020304050607080' ]; }; then
        bad "c_volatile oracle (exit=$code stdout='$stdout')"
    elif [ $code -eq 0 ] && [ -n "$red_pattern" ]; then
        bad "c_$p XPASS（请移出 expected-red 并建立 native 差分）"
    elif [ $code -eq 0 ]; then
        ok "c_$p"
    elif [ -n "$red_pattern" ] && [ $code -eq 70 ] \
        && echo "$out" | grep -Eq "$red_pattern"; then
        red "c_$p（$red_label）"
    else
        bad "c_$p (exit=$code): $(echo "$out" | tail -1 | head -c 100)"
    fi
done

# ---- ② diff_cargo：三个 cargo 形态均与 native 一致 ----
echo "== diff_cargo =="
if MIRVM="$MIRVM" bash tests/diff_cargo.sh >"$TMP/diff_cargo.out" 2>&1 \
    && grep -q "PASS ecosystem" "$TMP/diff_cargo.out" \
    && grep -q "PASS ffi_zlib" "$TMP/diff_cargo.out" \
    && grep -q "PASS project" "$TMP/diff_cargo.out" \
    && grep -q "== 3 passed, 0 expected-red, 0 failed ==" "$TMP/diff_cargo.out"; then
    ok "diff_cargo 3/3"
else
    bad "diff_cargo"
    grep -E "PASS|XFAIL|XPASS|FAIL" "$TMP/diff_cargo.out"
fi

# ---- ③ 性能上限（D2：加载 ≤1s / rayon ≤5s；热 sysroot 缓存）----
echo "== 性能上限 =="
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
fi

# ---- ④ 全量回归 ----
echo "== 全量回归 =="
if MIRVM="$MIRVM" bash tests/diff.sh >"$TMP/diff.out" 2>&1 \
    && grep -Eq "== [0-9]+ passed, 0 failed ==" "$TMP/diff.out"; then
    diff_summary=$(grep -Eo '[0-9]+ passed, 0 failed' "$TMP/diff.out" | tail -1)
    ok "diff.sh $diff_summary"
else
    bad "diff.sh 回归"
fi
for probe in addcarry xgetbv simd_insert simd_shift vzeroupper x86_vectors; do
    probe_code=0
    MIRVM="$MIRVM" bash "tests/m51_$probe.sh" >"$TMP/m51-$probe.out" 2>&1 \
        || probe_code=$?
    if [ "$probe_code" -ne 0 ]; then
        bad "m51_$probe"
        tail -5 "$TMP/m51-$probe.out"
        continue
    fi

    # 同一 vector fixture 含 SSSE3 pshufb 与 SHA-NI 两个独立 CPU 能力；
    # 宿主可能只支持其一，因此必须分别 PASS/SKIP，不得以一个
    # 总 PASS 掩盖 SHA helper 完全未执行。
    if [ "$probe" = x86_vectors ]; then
        for feature in pshufb sha; do
            status_count=$(grep -Ec "^(PASS|SKIP) m51_x86_vectors/$feature(:|$)" \
                "$TMP/m51-$probe.out")
            if [ "$status_count" -ne 1 ]; then
                bad "m51_x86_vectors/$feature（退出 0 但 PASS/SKIP 状态不唯一）"
            elif grep -Eq "^SKIP m51_x86_vectors/$feature(:|$)" \
                "$TMP/m51-$probe.out"; then
                status_line=$(grep -E "^SKIP m51_x86_vectors/$feature(:|$)" \
                    "$TMP/m51-$probe.out")
                skip "${status_line#SKIP }"
            else
                ok "m51_x86_vectors/$feature"
            fi
        done
        continue
    fi

    status_count=$(grep -Ec "^(PASS|SKIP) m51_$probe(:|$)" \
        "$TMP/m51-$probe.out")
    if [ "$status_count" -ne 1 ]; then
        bad "m51_$probe（退出 0 但 PASS/SKIP 状态不唯一）"
        tail -5 "$TMP/m51-$probe.out"
    elif grep -Eq "^SKIP m51_$probe(:|$)" "$TMP/m51-$probe.out"; then
        status_line=$(grep -E "^SKIP m51_$probe(:|$)" "$TMP/m51-$probe.out")
        skip "${status_line#SKIP }"
    else
        ok "m51_$probe"
    fi
done
for gate in 0 1 2; do
    if MIRVM="$MIRVM" bash "tests/m4_gate$gate.sh" >"$TMP/gate$gate.out" 2>&1; then
        ok "gate$gate"
    else
        bad "gate$gate"
        tail -3 "$TMP/gate$gate.out"
    fi
done
gate4_code=0
MIRVM="$MIRVM" bash tests/m4_gate4.sh >"$TMP/gate4.out" 2>&1 || gate4_code=$?
if [ "$gate4_code" -ne 0 ]; then
    bad "gate4"
    tail -3 "$TMP/gate4.out"
else
    tsan_status_count=$(grep -Ec '^(PASS|SKIP) TSan' "$TMP/gate4.out")
    if [ "$tsan_status_count" -ne 1 ]; then
        bad "gate4（退出 0 但 TSan PASS/SKIP 状态不唯一）"
        tail -5 "$TMP/gate4.out"
    elif grep -q '^SKIP TSan' "$TMP/gate4.out"; then
        tsan_status=$(grep '^SKIP TSan' "$TMP/gate4.out")
        skip "gate4（真线程子门通过；${tsan_status#SKIP }）"
    else
        ok "gate4（真线程/TSan）"
    fi
fi

echo "---"
echo "m4-gate5(收官): $pass pass, $xfail expected-red, $skip_count skip, $fail fail"
[ $fail -eq 0 ]
