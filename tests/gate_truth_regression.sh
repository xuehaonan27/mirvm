#!/usr/bin/env bash
# 门禁自身的回归：锁住 native 双失败假 PASS、expected-red 原因/XPASS、失败状态传播，
# gate5 确实调用 gate0、gate1、gate2，以及子 gate 的 SKIP 不会冒充 PASS。
# 全部使用确定性 fake runner，不构建项目。
set -u
cd "$(dirname "$0")/.."

FIX=$(pwd)/tests/fixtures/gate_truth
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
pass=0 fail=0
ok() { pass=$((pass + 1)); echo "PASS $*"; }
bad() { fail=$((fail + 1)); echo "FAIL $*"; }

run_diff_case() {
    local scenario="$1" want_code="$2" want_line="$3"
    local diagnostic="${4:-}" out code
    out=$(SCENARIO="$scenario" ECOSYSTEM_XFAIL_DIAGNOSTIC="$diagnostic" \
        MIRVM="$FIX/fake_mirvm.sh" CARGO="$FIX/fake_cargo.sh" \
        SCRIPT_CACHE="$FIX/cache" /bin/bash tests/diff_cargo.sh 2>&1)
    code=$?
    if [ "$code" -eq "$want_code" ] && echo "$out" | grep -Fq "$want_line"; then
        ok "diff_cargo $scenario"
    else
        bad "diff_cargo $scenario (exit=$code, want=$want_code)"
        echo "$out"
    fi
}

# 旧实现会把双方 exit=101、空 stdout 判成 PASS。
run_diff_case false_positive 1 'FAIL ecosystem (native baseline exit=101, want=0)'
# 注入的已知前沿必须以精确原因记作 XFAIL。
run_diff_case xfail 0 'XFAIL ecosystem (synthetic.frontier; M5.1 expected red)' \
    synthetic.frontier
# 一旦功能转绿，必须推动 expected-red 清单，而不是永久吞掉成功。
run_diff_case xpass 1 'XPASS ecosystem (remove/update expected-red frontier)' \
    synthetic.frontier

out=$(SCENARIO=corpus_fail MIRVM="$FIX/fake_mirvm.sh" OUT="$TMP/corpus" \
    /bin/bash tests/corpus.sh signal 2>&1)
code=$?
if [ "$code" -ne 0 ] && echo "$out" | grep -Fq 'corpus: 0 pass, 1 fail'; then
    ok 'corpus.sh 传播失败状态'
else
    bad "corpus.sh 未传播失败状态 (exit=$code)"
    echo "$out"
fi

gate0_log="$TMP/gate0-calls"
if MIRVM="$FIX/fake_mirvm.sh" TOOLCHAIN=gate-truth-nightly \
    GATE_CALL_LOG="$gate0_log" PATH="$FIX:$PATH" \
    /bin/bash tests/m4_gate0.sh >"$TMP/gate0.out" 2>&1 \
    && grep -Fxq \
        'cargo +gate-truth-nightly build --manifest-path tsan/Cargo.toml --release --locked' \
        "$gate0_log" \
    && ! grep -Fq 'tests/spike4_tsan.sh' "$gate0_log"; then
    ok 'gate0 纯度门只编译 harness，不再隐式重复 TSan'
else
    bad 'gate0 仍隐式执行 TSan 或未构建纯度 harness'
    cat "$TMP/gate0.out"
    test -f "$gate0_log" && cat "$gate0_log"
fi

call_log="$TMP/gate-calls"
if CORPUS_PROGS=volatile MIRVM="$FIX/fake_mirvm.sh" SKIP_TSAN=1 \
    GATE_CALL_LOG="$call_log" PATH="$FIX:$PATH" /bin/bash tests/m4_gate5.sh \
    >"$TMP/gate5.out" 2>&1; then
    missing=0
    for gate in 0 1 2; do
        grep -Fxq "tests/m4_gate$gate.sh" "$call_log" || missing=1
    done
    if [ "$missing" -eq 0 ]; then
        ok 'gate5 调用 gate0 + gate1 + gate2'
    else
        bad 'gate5 缺少 gate0/1/2 调用'
        cat "$call_log"
    fi
else
    bad 'gate5 聚合 fixture 执行失败'
    cat "$TMP/gate5.out"
fi

if grep -Fq 'SKIP gate4' "$TMP/gate5.out" \
    && ! grep -Fq 'PASS gate4' "$TMP/gate5.out" \
    && grep -Eq '1 skip, 0 fail$' "$TMP/gate5.out"; then
    ok 'gate5 将 gate4 内 TSan SKIP 独立记作 SKIP'
else
    bad 'gate5 把 gate4 内 TSan SKIP 冒充 PASS'
    cat "$TMP/gate5.out"
fi

if CORPUS_PROGS=volatile MIRVM="$FIX/fake_mirvm.sh" M51_SKIP_PROBE=simd_shift \
    GATE_CALL_LOG="$call_log" PATH="$FIX:$PATH" /bin/bash tests/m4_gate5.sh \
    >"$TMP/gate5-m51-skip.out" 2>&1 \
    && grep -Fq 'SKIP m51_simd_shift: host lacks required CPU feature' \
        "$TMP/gate5-m51-skip.out" \
    && ! grep -Fq 'PASS m51_simd_shift' "$TMP/gate5-m51-skip.out" \
    && grep -Eq '1 skip, 0 fail$' "$TMP/gate5-m51-skip.out"; then
    ok 'gate5 将 m51 特性缺失独立记作 SKIP'
else
    bad 'gate5 把 m51 特性缺失冒充 PASS'
    cat "$TMP/gate5-m51-skip.out"
fi

if CORPUS_PROGS=volatile MIRVM="$FIX/fake_mirvm.sh" M51_SKIP_VECTOR_FEATURE=sha \
    GATE_CALL_LOG="$call_log" PATH="$FIX:$PATH" /bin/bash tests/m4_gate5.sh \
    >"$TMP/gate5-vector-partial.out" 2>&1 \
    && grep -Fq 'PASS m51_x86_vectors/pshufb' "$TMP/gate5-vector-partial.out" \
    && grep -Fq 'SKIP m51_x86_vectors/sha: host lacks sha' \
        "$TMP/gate5-vector-partial.out" \
    && ! grep -Fxq 'PASS m51_x86_vectors' "$TMP/gate5-vector-partial.out" \
    && grep -Eq '1 skip, 0 fail$' "$TMP/gate5-vector-partial.out"; then
    ok 'gate5 将 x86 vector 子特性分别记为 PASS/SKIP'
else
    bad 'gate5 把未执行的 SHA helper 冒充 vector 整体 PASS'
    cat "$TMP/gate5-vector-partial.out"
fi

if CORPUS_PROGS=volatile MIRVM="$FIX/fake_mirvm.sh" SKIP_TSAN=1 SKIP_PERF=1 \
    GATE_CALL_LOG="$call_log" PATH="$FIX:$PATH" /bin/bash tests/m4_gate5.sh \
    >"$TMP/gate5-skip-perf.out" 2>&1 \
    && grep -Fq 'SKIP 性能上限（SKIP_PERF=1；语义 gate 仍照常执行）' \
        "$TMP/gate5-skip-perf.out" \
    && grep -Eq '2 skip, 0 fail$' "$TMP/gate5-skip-perf.out"; then
    ok 'gate5 SKIP_PERF 只跳过时序门并独立计数'
else
    bad 'gate5 SKIP_PERF 行为错误'
    cat "$TMP/gate5-skip-perf.out"
fi

echo "---"
echo "gate-truth-regression: $pass pass, $fail fail"
[ "$fail" -eq 0 ]
