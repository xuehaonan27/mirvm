#!/usr/bin/env bash
# 测试框架自身的回归：锁住双方同败假绿、预期失败原因、失败状态传播，
# 统一入口的套件清单和 gate 覆盖，以及 SKIP 不会冒充具体能力已通过。
# 全部使用确定性 fake runner，不构建项目。
# product: no
set -u
. "$(dirname "${BASH_SOURCE[0]}")/../../support/harness.sh"
test_enter_repo

FIX=$(pwd)/tests/fixtures/gate_truth
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
run_diff_case() {
    local scenario="$1" want_code="$2" want_line="$3"
    local diagnostic="${4:-}" out code
    out=$(SCENARIO="$scenario" ECOSYSTEM_XFAIL_DIAGNOSTIC="$diagnostic" \
        MIRVM="$FIX/fake_mirvm.sh" CARGO="$FIX/fake_cargo.sh" \
        SCRIPT_CACHE="$FIX/cache" /bin/bash tests/suites/differential/cargo.sh 2>&1)
    code=$?
    if [ "$code" -eq "$want_code" ] && echo "$out" | grep -Fq "$want_line"; then
        ok "differential.cargo $scenario"
    else
        bad "differential.cargo $scenario (exit=$code, want=$want_code)"
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
# stdout/exit 相同但 mirvm 多出 stderr 也必须失败。
run_diff_case stderr_only 1 'FAIL ecosystem (native=0 mirvm=0)'

DIFF_STDERR_MIRVM="$TMP/diff-stderr-mirvm"
cat >"$DIFF_STDERR_MIRVM" <<'EOF'
#!/usr/bin/env bash
printf 'fib(0..10) = [0, 1, 1, 2, 3, 5, 8, 13, 21, 34], sum = 88\n'
printf 'unexpected mirvm diagnostic\n' >&2
EOF
chmod +x "$DIFF_STDERR_MIRVM"
diff_stderr_out=$(ONLY=fib MIRVM="$DIFF_STDERR_MIRVM" \
    /bin/bash tests/suites/differential/programs.sh 2>&1)
diff_stderr_code=$?
if [ "$diff_stderr_code" -ne 0 ] \
    && echo "$diff_stderr_out" | grep -Fq 'FAIL fib: stderr 不一致'; then
    ok 'differential.programs 拒绝 mirvm 单侧多出的 stderr'
else
    bad "differential.programs stderr false green (exit=$diff_stderr_code)"
    echo "$diff_stderr_out"
fi

out=$(SCENARIO=corpus_fail MIRVM="$FIX/fake_mirvm.sh" OUT="$TMP/corpus" \
    /bin/bash tests/suites/corpus/run.sh signal 2>&1)
code=$?
if [ "$code" -ne 0 ] \
    && echo "$out" | grep -Fq 'corpus.run: 0 passed, 0 skipped, 0 expected-failed, 1 failed'; then
    ok 'corpus.run 传播失败状态'
else
    bad "corpus.run 未传播失败状态 (exit=$code)"
    echo "$out"
fi

printf 'allocator corrupted\n' >"$TMP/xfail.err"
xfail_out=$(
    (pass=0 fail=0 skip_count=0 xfail=0
     record_expected_failure synthetic 134 '134:corrupted' "$TMP/xfail.err"
     suite_summary synthetic.xfail)
)
if echo "$xfail_out" | grep -Fq 'XFAIL synthetic（corrupted）' \
    && echo "$xfail_out" | grep -Fq '1 expected-failed, 0 failed'; then
    ok '共享 XFAIL 判定锁定退出码和诊断'
else
    bad '共享 XFAIL 判定未正确计数'
    echo "$xfail_out"
fi

purity_log="$TMP/purity-calls"
if MIRVM="$FIX/fake_mirvm.sh" TOOLCHAIN=gate-truth-nightly \
    GATE_CALL_LOG="$purity_log" PATH="$FIX:$PATH" \
    /bin/bash tests/suites/runtime/semantics.sh pure >"$TMP/pure.out" 2>&1 \
    && grep -Fxq \
        'cargo +gate-truth-nightly build --manifest-path tsan/Cargo.toml --release --locked' \
        "$purity_log" \
    && ! grep -Fq 'tests/suites/runtime/tsan.sh' "$purity_log"; then
    ok 'runtime.semantics pure 只编译纯度 harness，不重复 TSan'
else
    bad 'runtime.semantics pure 仍隐式执行 TSan 或未构建纯度 harness'
    cat "$TMP/pure.out"
    test -f "$purity_log" && cat "$purity_log"
fi

run_fake_gate() {
    local out=$1
    shift
    env "$@" MIRVM_HOME="$TMP/mirvm-home" MIRVM="$FIX/fake_mirvm.sh" \
        CARGO="$FIX/fake_cargo.sh" RUSTC=/bin/true GATE_CALL_LOG="$call_log" \
        PATH="$FIX:$PATH" /bin/bash tests/run.sh gate >"$out" 2>&1
}

list_out=$(cd /tmp && /bin/bash "$REPO_ROOT/tests/run.sh" list 2>&1)
list_code=$?
missing_listed=0
while IFS= read -r -d '' leaf; do
    relative=${leaf#"$REPO_ROOT/tests/suites/"}
    discovered_id=${relative%.sh}
    discovered_id=${discovered_id//\//.}
    discovered_id=${discovered_id//_/-}
    echo "$list_out" | awk '{print $1}' | grep -Fxq "$discovered_id" || {
        echo "list 缺自动发现套件: $discovered_id"
        missing_listed=1
    }
done < <(find "$REPO_ROOT/tests/suites" -mindepth 2 -maxdepth 2 -type f -name '*.sh' -print0)
if [ "$list_code" -eq 0 ] && [ "$(echo "$list_out" | awk '{print $1}' | sort | uniq -d | wc -l)" -eq 0 ] \
    && [ "$missing_listed" -eq 0 ]; then
    ok '统一入口可从任意目录自动发现且不重复地列出全部套件'
else
    bad "统一入口 list 错误（exit=$list_code）"
    echo "$list_out"
fi

unknown_out=$(/bin/bash tests/run.sh suite does.not.exist 2>&1)
unknown_code=$?
if [ "$unknown_code" -eq 64 ] && echo "$unknown_out" | grep -Fq '未知套件'; then
    ok '统一入口拒绝未知套件并返回用法错误'
else
    bad "统一入口未知套件状态错误（exit=$unknown_code）"
fi

call_log="$TMP/gate-calls"
: >"$call_log"
if run_fake_gate "$TMP/gate.out" SKIP_TSAN=1; then
    missing=0
    for leaf in tests/suites/quality/rust.sh \
        tests/suites/differential/programs.sh tests/suites/differential/cargo.sh \
        tests/suites/differential/cargoless.sh tests/suites/contracts/cargoless_test.sh \
        tests/suites/contracts/cargoless_workspace.sh tests/suites/contracts/cargoless_git.sh \
        tests/suites/contracts/cargoless_sources.sh tests/suites/contracts/pack.sh \
        tests/suites/contracts/build_script_rerun.sh tests/suites/contracts/deps_image.sh \
        tests/suites/corpus/contract.sh tests/suites/runtime/x86_features.sh \
        tests/suites/runtime/semantics.sh tests/suites/runtime/c_unwind.sh \
        tests/suites/runtime/jit_stats.sh \
        tests/suites/performance/limits.sh tests/suites/harness/truth.sh; do
        grep -Fxq "$leaf" "$call_log" || { missing=1; echo "缺调用: $leaf"; }
    done
    [ "$missing" -eq 0 ] && ok 'gate 覆盖 fast、smoke 和完整门禁的全部义务' \
        || bad 'gate 缺少叶套件调用'
else
    bad 'gate 聚合 fixture 执行失败'
    cat "$TMP/gate.out"
fi

if grep -Fq 'SKIP TSan（SKIP_TSAN=1）' "$TMP/gate.out" \
    && ! grep -Fq 'PASS TSan' "$TMP/gate.out"; then
    ok 'gate 保留 TSan SKIP，不伪造具体能力 PASS'
else
    bad 'gate 把 TSan SKIP 冒充 PASS'
fi

if run_fake_gate "$TMP/gate-probe-skip.out" M51_SKIP_PROBE=simd_shift \
    && grep -Fq 'SKIP m51_simd_shift: host lacks required CPU feature' \
        "$TMP/gate-probe-skip.out" \
    && ! grep -Fq 'PASS m51_simd_shift' "$TMP/gate-probe-skip.out"; then
    ok 'gate 将探针特性缺失独立记作 SKIP'
else
    bad 'gate 把探针特性缺失冒充 PASS'
    cat "$TMP/gate-probe-skip.out"
fi

if run_fake_gate "$TMP/gate-vector-partial.out" M51_SKIP_VECTOR_FEATURE=sha \
    && grep -Fq 'PASS m51_x86_vectors/pshufb' "$TMP/gate-vector-partial.out" \
    && grep -Fq 'SKIP m51_x86_vectors/sha: host lacks sha' \
        "$TMP/gate-vector-partial.out" \
    && ! grep -Fxq 'PASS m51_x86_vectors' "$TMP/gate-vector-partial.out"; then
    ok 'gate 将 x86 vector 子特性分别记为 PASS/SKIP'
else
    bad 'gate 把未执行的 SHA helper 冒充 vector 整体 PASS'
    cat "$TMP/gate-vector-partial.out"
fi

if run_fake_gate "$TMP/gate-skip-perf.out" SKIP_TSAN=1 SKIP_PERF=1 \
    && grep -Fq 'SKIP 性能上限（SKIP_PERF=1；语义 gate 仍照常执行）' \
        "$TMP/gate-skip-perf.out"; then
    ok 'gate SKIP_PERF 只跳过时序门并独立计数'
else
    bad 'gate SKIP_PERF 行为错误'
    cat "$TMP/gate-skip-perf.out"
fi

if ! run_fake_gate "$TMP/gate-fail.out" \
    GATE_FAIL_SUITE=tests/suites/contracts/cargoless_git.sh \
    && grep -Fq 'suite contracts.cargoless-git（exit=1）' "$TMP/gate-fail.out" \
    && grep -Eq 'profile\.gate: .* 1 failed' "$TMP/gate-fail.out"; then
    ok '叶套件失败会让 gate 非零退出'
else
    bad 'gate 吞掉了叶套件失败'
    cat "$TMP/gate-fail.out"
fi

suite_summary harness.truth
