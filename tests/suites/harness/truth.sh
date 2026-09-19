#!/usr/bin/env bash
# Regression for the harness itself: locks false green on both-legs-fail, expected-failure
# reasons, failure-status propagation, the unified entry point's suite list and gate
# coverage, and that SKIP never impersonates a passed capability. Uses only deterministic fake runners and builds nothing.
# product: no
set -u
. "$(dirname "${BASH_SOURCE[0]}")/../../support/harness.sh"
suite_init --no-product

FIX=$REPO_ROOT/tests/suites/harness/fixtures/fake-runners
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

# Both legs exiting 101 with empty stdout must not be judged PASS.
run_diff_case false_positive 1 'FAIL ecosystem (native baseline exit=101, want=0)'
# An injected known frontier must be recorded as XFAIL with an exact reason.
run_diff_case xfail 0 'XFAIL ecosystem (synthetic.frontier; expected red)' \
    synthetic.frontier
# Once a feature turns green the expected-red list must be promoted, not silently swallow the success.
run_diff_case xpass 1 'XPASS ecosystem (remove/update expected-red frontier)' \
    synthetic.frontier
# Identical stdout/exit with extra mirvm-only stderr must also fail.
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
    && echo "$diff_stderr_out" | grep -Fq 'FAIL fib: stderr differs'; then
    ok 'differential.programs rejects extra stderr from the mirvm side alone'
else
    bad "differential.programs stderr false green (exit=$diff_stderr_code)"
    echo "$diff_stderr_out"
fi

out=$(SCENARIO=corpus_fail MIRVM="$FIX/fake_mirvm.sh" OUT="$TMP/corpus" \
    /bin/bash tests/suites/corpus/run.sh signal 2>&1)
code=$?
if [ "$code" -ne 0 ] \
    && echo "$out" | grep -Fq 'corpus.run: 0 passed, 0 skipped, 0 expected-failed, 1 failed'; then
    ok 'corpus.run propagates failure status'
else
    bad "corpus.run did not propagate failure status (exit=$code)"
    echo "$out"
fi

printf 'allocator corrupted\n' >"$TMP/xfail.err"
xfail_out=$(
    (pass=0 fail=0 skip_count=0 xfail=0
     record_expected_failure synthetic 134 '134:corrupted' "$TMP/xfail.err"
     suite_summary synthetic.xfail)
)
if echo "$xfail_out" | grep -Fq 'XFAIL synthetic (corrupted)' \
    && echo "$xfail_out" | grep -Fq '1 expected-failed, 0 failed'; then
    ok 'shared XFAIL decision locks exit code and diagnostic'
else
    bad 'shared XFAIL decision counted incorrectly'
    echo "$xfail_out"
fi

purity_log="$TMP/purity-calls"
if MIRVM="$FIX/fake_mirvm.sh" TOOLCHAIN=gate-truth-nightly \
    GATE_CALL_LOG="$purity_log" PATH="$FIX:$PATH" \
    /bin/bash tests/suites/runtime/semantics.sh pure >"$TMP/pure.out" 2>&1 \
    && grep -Fxq \
        'cargo +gate-truth-nightly build --manifest-path tests/tsan/Cargo.toml --release --locked' \
        "$purity_log" \
    && ! grep -Fq 'tests/suites/runtime/tsan.sh' "$purity_log"; then
    ok 'runtime.semantics pure compiles only the purity harness, not TSan again'
else
    bad 'runtime.semantics pure still runs TSan implicitly or did not build the purity harness'
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
        echo "list is missing auto-discovered suite: $discovered_id"
        missing_listed=1
    }
done < <(find "$REPO_ROOT/tests/suites" -mindepth 2 -maxdepth 2 -type f -name '*.sh' -print0)
if [ "$list_code" -eq 0 ] && [ "$(echo "$list_out" | awk '{print $1}' | sort | uniq -d | wc -l)" -eq 0 ] \
    && [ "$missing_listed" -eq 0 ]; then
    ok 'unified entry point lists every suite once from any directory'
else
    bad "unified entry point list wrong (exit=$list_code)"
    echo "$list_out"
fi

unknown_out=$(/bin/bash tests/run.sh suite does.not.exist 2>&1)
unknown_code=$?
if [ "$unknown_code" -eq 64 ] && echo "$unknown_out" | grep -Fq 'unknown suite'; then
    ok 'unified entry point rejects an unknown suite with a usage error'
else
    bad "unified entry point wrong status for an unknown suite (exit=$unknown_code)"
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
        grep -Fxq "$leaf" "$call_log" || { missing=1; echo "missing call: $leaf"; }
    done
    [ "$missing" -eq 0 ] && ok 'gate covers every obligation of fast, smoke and the full profile' \
        || bad 'gate is missing a leaf-suite call'
else
    bad 'gate aggregation fixture failed'
    cat "$TMP/gate.out"
fi

if grep -Fq 'SKIP TSan (SKIP_TSAN=1)' "$TMP/gate.out" \
    && ! grep -Fq 'PASS TSan' "$TMP/gate.out"; then
    ok 'gate keeps the TSan SKIP and does not fake a capability PASS'
else
    bad 'gate passes off the TSan SKIP as PASS'
fi

if run_fake_gate "$TMP/gate-probe-skip.out" X86_SKIP_PROBE=simd_shift \
    && grep -Fq 'SKIP x86_simd_shift: host lacks required CPU feature' \
        "$TMP/gate-probe-skip.out" \
    && ! grep -Fq 'PASS x86_simd_shift' "$TMP/gate-probe-skip.out"; then
    ok 'gate records a missing probe feature as its own SKIP'
else
    bad 'gate passes off the missing probe feature as PASS'
    cat "$TMP/gate-probe-skip.out"
fi

if run_fake_gate "$TMP/gate-vector-partial.out" X86_SKIP_VECTOR_FEATURE=sha \
    && grep -Fq 'PASS x86_vectors/pshufb' "$TMP/gate-vector-partial.out" \
    && grep -Fq 'SKIP x86_vectors/sha: host lacks sha' \
        "$TMP/gate-vector-partial.out" \
    && ! grep -Fxq 'PASS x86_vectors' "$TMP/gate-vector-partial.out"; then
    ok 'gate records x86 vector sub-features as separate PASS/SKIP'
else
    bad 'gate passes off the unrun SHA helper as a whole-vector PASS'
    cat "$TMP/gate-vector-partial.out"
fi

if run_fake_gate "$TMP/gate-skip-perf.out" SKIP_TSAN=1 SKIP_PERF=1 \
    && grep -Fq 'SKIP performance limits (SKIP_PERF=1; the semantics gate still runs as usual)' \
        "$TMP/gate-skip-perf.out"; then
    ok 'gate SKIP_PERF skips only the timing gate and counts separately'
else
    bad 'gate SKIP_PERF behavior wrong'
    cat "$TMP/gate-skip-perf.out"
fi

if ! run_fake_gate "$TMP/gate-fail.out" \
    GATE_FAIL_SUITE=tests/suites/contracts/cargoless_git.sh \
    && grep -Fq 'suite contracts.cargoless-git (exit=1)' "$TMP/gate-fail.out" \
    && grep -Eq 'profile\.gate: .* 1 failed' "$TMP/gate-fail.out"; then
    ok 'a leaf-suite failure makes the gate exit non-zero'
else
    bad 'gate swallowed a leaf-suite failure'
    cat "$TMP/gate-fail.out"
fi

suite_summary harness.truth
