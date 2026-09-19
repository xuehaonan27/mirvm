#!/usr/bin/env bash
# Time performance hard gate and space/cache resource metering.
#
# Time hard gates (established D2/M5.3 anchors; SKIP_PERF=1 skips only these three, metering still runs):
#   ① empty main load phase < 1s (warm sysroot cache)
#   ② c_rayon < 5s (including semantic oracle 'par_sort ok = true')
#   ③ fib(32) JIT ≤ 80ms (≈10× native; interpreter anchor 940ms. Three runs, take minimum:
#     each `mirvm run` is a fresh process, JIT background thread recompiles, under full gate load it may be starved once
#     (falling back to interpreter 940ms) — multiple runs take the fastest to kill scheduling noise, contract unchanged)
# Resource metering (always runs):
#   cache sub-section sizes, disk availability, target budget gate (shared harness).
#
# Usage: ./tests/run.sh suite performance.limits [--metrics-only]
set -u
. "$(dirname "${BASH_SOURCE[0]}")/../../support/harness.sh"
suite_init

metrics_only=0
case "${1:-}" in
    "") ;;
    --metrics-only) metrics_only=1 ;;
    *) echo "usage: ./tests/run.sh suite performance.limits [--metrics-only]" >&2; exit 64 ;;
esac

if [ "$metrics_only" -eq 0 ]; then
    if [ -n "${SKIP_PERF:-}" ]; then
        skip "performance ceiling (SKIP_PERF=1; semantic gate still runs normally)"
    else
        echo 'fn main(){}' >"$TMP/empty.rs"
        t0=$(date +%s%N); "$MIRVM" run "$TMP/empty.rs" >"$TMP/load.out" 2>"$TMP/load.err"; load_code=$?
        load_ms=$(( ($(date +%s%N) - t0) / 1000000 ))
        [ $load_code -eq 0 ] && [ $load_ms -lt 1000 ] \
            && ok "load phase ${load_ms}ms (< 1s hard gate)" \
            || bad "load phase exit=$load_code ${load_ms}ms (requires exit=0 and < 1s)"
        t0=$(date +%s%N); "$MIRVM" run tests/scripts/c_rayon.rs >"$TMP/rayon.out" 2>"$TMP/rayon.err"; rayon_code=$?
        rayon_ms=$(( ($(date +%s%N) - t0) / 1000000 ))
        if [ $rayon_code -eq 0 ] && grep -q 'par_sort ok = true' "$TMP/rayon.out" \
            && [ $rayon_ms -lt 5000 ]; then
            rayon_divisor=$rayon_ms; [ $rayon_divisor -gt 0 ] || rayon_divisor=1
            ok "rayon ${rayon_ms}ms (< 5s hard gate; tier-0 28s, ≈$((28000/rayon_divisor))×)"
        else
            bad "rayon exit=$rayon_code ${rayon_ms}ms (requires semantic oracle and < 5s)"
        fi
        # M5.3 JIT hard gate: fib(32) ≤ 10× native ≈ ≤80ms wall clock, three runs take minimum
        fib_ms=999999 fib_code=1 fib_out=""
        for _ in 1 2 3; do
            t0=$(date +%s%N)
            fib_out=$("$MIRVM" run --vm-call 'fib(32)' tests/scripts/vmcall_pure.rs 2>"$TMP/fib32.err")
            fib_code=$?
            ms=$(( ($(date +%s%N) - t0) / 1000000 ))
            [ $ms -lt $fib_ms ] && fib_ms=$ms
            { [ $fib_code -eq 0 ] && [ "$fib_out" = "2178309" ]; } || break
        done
        if [ $fib_code -eq 0 ] && [ "$fib_out" = "2178309" ] && [ $fib_ms -lt 80 ]; then
            ok "fib(32) JIT ${fib_ms}ms (≤80ms = 10× native hard gate; interpreter anchor 940ms)"
        else
            bad "fib(32) JIT exit=$fib_code out=$fib_out ${fib_ms}ms (requires 2178309 and <80ms)"
        fi
    fi
fi

echo "== Resource metering =="
cache_snapshot "perf"
target_budget_check
home_dir=${MIRVM_HOME:-$HOME/.mirvm}
echo "[disk] $(df -h "$home_dir" 2>/dev/null | awk 'NR==2{print "available "$4" / total "$2" ("$5" used)"}')"
suite_summary performance.limits
