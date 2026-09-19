#!/usr/bin/env bash
# Corpus dependency-path differential: each entry of the selected tier runs both Cargo and cargoless paths,
# two legs (MIRVM_DEPS=cargo three-stage legacy path vs MIRVM_DEPS=self zero-cargo own scheduler),
# PASS only if stdout/stderr/exit are byte-for-byte identical.
#
# Usage: ./tests/run.sh suite corpus.deps-pair [--tier smoke|full|all] [--group group] [name ...]
#   --group heavy only runs manifest group=heavy entries (light = entries with no group= key);
#   --tier and --group can be combined (intersection); running by name ignores group filters.
# Environment matches corpus.run (MIRVM / MIRVM_DISK_MIN_GB / MIRVM_TARGET_BUDGET_GB etc.).
# Disk discipline: running both legs = two passes cost; cache cleanup scope is the same as corpus_run
# (deps/ir cleared per run, cargoless/cargo target stores kept for reuse).
set -u
. "$(dirname "${BASH_SOURCE[0]}")/../../support/harness.sh"
suite_init
CORPUS_TIMINGS_FILE="$TMP/corpus-timings"; export CORPUS_TIMINGS_FILE

rows=$(corpus_select corpus.deps-pair smoke "$@") || exit $?

OUT_C=$(mktemp -d /tmp/corpair-cargo-XXXXXX)
OUT_S=$(mktemp -d /tmp/corpair-self-XXXXXX)
# KEEP_OUT=1: preserve both legs' output scenes (for triage); default removes on exit.
if [ "${KEEP_OUT:-0}" = 1 ]; then
    trap 'rm -rf "$TMP"; echo "corpair scene preserved: $OUT_C $OUT_S" >&2' EXIT
else
    trap 'rm -rf "$TMP" "$OUT_C" "$OUT_S"' EXIT
fi

while IFS='|' read -r name _tier tmo mode envv needs args _xfail _groups; do
    [ -n "$name" ] || continue
    argv=()
    [ -n "$args" ] && parse_args "$args" argv

    MIRVM_DEPS=cargo corpus_run "$OUT_C" "$name" "$tmo" "$envv" "$needs" ${argv[@]+"${argv[@]}"}
    cc=$?
    MIRVM_DEPS=self corpus_run "$OUT_S" "$name" "$tmo" "$envv" "$needs" ${argv[@]+"${argv[@]}"}
    sc=$?

    if [ "$cc" -eq 77 ] || [ "$sc" -eq 77 ]; then
        skip "$name (needs missing: $needs)"
        continue
    fi
    ok=1 why=""
    # Before comparing stderr, normalize panic-header thread names/TIDs (the differential suite already has the same precedent:
    # TID drifts naturally per process and is not byte-comparable; only normalize the inherently unstable part, other differences stay red).
    normalize_stderr "$OUT_C/$name.err" "$OUT_C/$name.err.n"
    normalize_stderr "$OUT_S/$name.err" "$OUT_S/$name.err.n"
    if [ "$cc" != "$sc" ]; then ok=0; why="exit codes cargo=$cc self=$sc"
    elif ! diff -q "$OUT_C/$name.out" "$OUT_S/$name.out" >/dev/null; then ok=0; why="stdout differs"
    elif ! diff -q "$OUT_C/$name.err.n" "$OUT_S/$name.err.n" >/dev/null; then ok=0; why="stderr differs"
    fi
    if [ "$ok" = 1 ]; then
        echo "PASS  $name"
        pass=$((pass + 1))
    elif [ "$cc" -eq 0 ] && [ "$sc" -ne 0 ] \
        && grep -q "deferred to P5" "$OUT_S/$name.err" 2>/dev/null; then
        red "$name (known boundary: deferred to P5)"
    else
        echo "FAIL  $name: $why"
        echo "--- cargo stderr tail ---"; tail -5 "$OUT_C/$name.err" 2>/dev/null
        echo "--- self stderr tail ---"; tail -5 "$OUT_S/$name.err" 2>/dev/null
        fail=$((fail + 1))
    fi
done <<< "$rows"

target_budget_check
suite_summary corpus.deps-pair
