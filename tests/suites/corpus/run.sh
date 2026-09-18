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
test_enter_repo
MIRVM=${MIRVM:-$(pwd)/target/release/mirvm}
OUT=${OUT:-/tmp/corpus-out}
mkdir -p "$OUT"
CORPUS_TIMINGS_FILE=$(mktemp)
export CORPUS_TIMINGS_FILE
trap 'rm -f "$CORPUS_TIMINGS_FILE"' EXIT

tier=all
group=""
names=()
while [ $# -gt 0 ]; do
    case "$1" in
        --tier) tier=$2; shift 2 ;;
        --tier=*) tier=${1#--tier=}; shift ;;
        --group) group=$2; shift 2 ;;
        --group=*) group=${1#--group=}; shift ;;
        *) names+=("$1"); shift ;;
    esac
done
case "$tier" in smoke|full|manual|all) ;; *)
    echo "corpus.run: invalid tier '$tier' (smoke|full|manual|all)" >&2; exit 64 ;; esac

if [ ${#names[@]} -gt 0 ]; then
    rows=$(
        for n in "${names[@]}"; do
            manifest_lookup "$n" || { echo "corpus.run: $n not registered in cases.manifest" >&2; exit 2; }
        done
    ) || exit 2
elif [ -n "$group" ]; then
    rows=$(manifest_group_rows "$group" "$tier") || exit 2
    [ -n "$rows" ] || { echo "corpus.run: group '$group' (tier=$tier) has no entries" >&2; exit 64; }
else
    rows=$(manifest_rows "$tier") || exit 2
fi

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
