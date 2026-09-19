#!/usr/bin/env bash
# corpus exploratory batch runner (sole source of truth: suites/corpus/cases.manifest).
# Purpose is "discover what real crates demand from the abstract machine/VM boundary", not to chase pass rate;
# green criterion is the registered exit code; registered XFAIL also locks exit code and diagnostics.
# Byte-for-byte oracle/diff judgement belongs to corpus.contract.
#
# Usage:
#   ./tests/run.sh suite corpus.run --tier smoke
#   ./tests/run.sh suite corpus.run --group heavy
#   ./tests/run.sh suite corpus.run tempfile walkdir
#   --tier and --group stack (intersection); running by name ignores group filtering
# Environment: MIRVM (default target/release/mirvm), OUT (default /tmp/corpus-out),
#   MIRVM_GATE_KEEP_CACHE=1 (debug bypass to clear cache per driver),
#   MIRVM_DISK_MIN_GB / MIRVM_TARGET_BUDGET_GB (disk guardrails see shared harness).
set -u
. "$(dirname "${BASH_SOURCE[0]}")/../../support/harness.sh"
suite_init
OUT=${OUT:-/tmp/corpus-out}
mkdir -p "$OUT"
CORPUS_TIMINGS_FILE="$TMP/corpus-timings"
export CORPUS_TIMINGS_FILE

rows=$(corpus_select corpus.run all "$@") || exit $?

cache_snapshot "corpus before start"
while IFS='|' read -r name _tier tmo mode envv needs args xfail_spec _groups; do
    [ -n "$name" ] || continue
    argv=()
    [ -n "$args" ] && parse_args "$args" argv
    code=0
    corpus_run "$OUT" "$name" "$tmo" "$envv" "$needs" ${argv[@]+"${argv[@]}"} || code=$?
    dur=$(awk -v n="$name" '$2==n{s=$1} END{print s+0}' "$CORPUS_TIMINGS_FILE")
    if [ "$code" -eq 77 ]; then
        skip "$name (needs absent: $needs)"
    elif [ "$code" -eq 2 ]; then
        echo "FAIL  $name (manifest registered but driver file missing)"
        fail=$((fail + 1))
    elif [ -n "$xfail_spec" ]; then
        record_expected_failure "$name" "$code" "$xfail_spec" "$OUT/$name.err"
    elif [ "$code" -eq 0 ]; then
        echo "PASS  $name  (${dur}s)"
        pass=$((pass + 1))
    else
        first_err=$(grep -m1 -iE 'error|panic|unsupported|unimplemented|not (yet )?(implemented|supported)|no (shim|intrinsic)|abort' "$OUT/$name.err" | head -c 200)
        [ -z "$first_err" ] && first_err=$(tail -1 "$OUT/$name.err" | head -c 200)
        echo "FAIL  $name  (${dur}s, exit=$code)  ::  $first_err"
        fail=$((fail + 1))
    fi
done <<< "$rows"

print_slowest 10
target_budget_check
cache_snapshot "corpus after finish"
suite_summary corpus.run
