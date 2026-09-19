#!/usr/bin/env bash
# Strict corpus contract: run the smoke+full entries and accept them the way the manifest specifies.
#
# Verdict: exit code, fixed oracle output, or three-way comparison of stdout/stderr/exit against native.
# xfail must lock the exit code and diagnostic; an unexpected green counts as failure and requires a contract update.
# Always invoke via ./tests/run.sh suite corpus.contract [names...].
# Disk discipline: clear deps/ir after each driver (MIRVM_GATE_KEEP_CACHE=1 bypasses);
#   when free space drops below MIRVM_DISK_MIN_GB (default 8G) cleanup escalates and aborts loudly if still short;
#   a target dir over MIRVM_TARGET_BUDGET_GB (default 24G) triggers purge --target.
set -u
. "$(dirname "${BASH_SOURCE[0]}")/../../support/harness.sh"
suite_init
export CORPUS_TIMINGS_FILE="$TMP/corpus-timings"
# gix needs a committer identity to write the reflog (corpus c_gix_pure; the commit
# hash comes from a fixed in-driver signature, and this identity reaches only the
# reflog, never any compared output). It does not depend on the host ~/.gitconfig, so any machine/CI matches.
export GIT_AUTHOR_NAME=mirvm-test GIT_AUTHOR_EMAIL=mirvm@test.local
export GIT_COMMITTER_NAME=mirvm-test GIT_COMMITTER_EMAIL=mirvm@test.local

cache_snapshot "gate start"
target_budget_check

# ---- corpus: manifest full defense line ----
echo "== corpus =="
section_start "corpus(smoke+full)"
if [ $# -gt 0 ]; then
    corpus_rows=$(
        for p in "$@"; do
            manifest_lookup "$p" || { echo "corpus.contract: $p is not in cases.manifest" >&2; exit 2; }
        done
    ) || exit 2
elif [ -n "${CORPUS_PROGS:-}" ]; then
    corpus_rows=$(
        for p in $CORPUS_PROGS; do
            manifest_lookup "$p" || { echo "corpus.contract: $p is not in cases.manifest" >&2; exit 2; }
        done
    ) || exit 2
else
    corpus_rows=$(manifest_rows "smoke,full") || exit 2
fi
while IFS='|' read -r name _tier tmo mode envv needs args xfail_spec _groups; do
    [ -n "$name" ] || continue
    argv=()
    [ -n "$args" ] && parse_args "$args" argv
    code=0
    if [ "$mode" = diff ]; then
        # Project three-way comparison: cold run (builds the cache + asserts only exit 0; cold
        # stderr carries dependency-build warning noise, a build event rather than program
        # behavior) -> warm rerun (cargo silent, stderr is the program's own output) -> native
        # baseline. Green = warm three-way == native three-way + warm stdout == cold stdout.
        MIRVM_CORPUS_NO_PURGE=1 corpus_run "$TMP" "$name" "$tmo" "$envv" "$needs" ${argv[@]+"${argv[@]}"} || code=$?
        if [ "$code" -eq 0 ]; then
            cp "$TMP/$name.out" "$TMP/$name.cold.out"
            warm_code=0
            corpus_run "$TMP" "$name" "$tmo" "$envv" "$needs" ${argv[@]+"${argv[@]}"} || warm_code=$?
            if [ "$warm_code" -ne 0 ] \
                || ! diff -q "$TMP/$name.cold.out" "$TMP/$name.out" >/dev/null; then
                bad "c_$name (L2 warm rerun differs cold=$code warm=$warm_code)"
                continue
            fi
            proj="tests/projects/$name"
            ncode=0
            # Both legs use the project directory as cwd (mirvm project mode: guest cwd = cd proj
            # && cargo run); fixture paths are absolutized through {ROOT}, so cwd does not matter.
            # --cap-lints silences upstream warnings (the stderr comparison carries only the
            # program's own output, no compile noise). RUSTC must be pinned explicitly: the
            # rustup proxy resolves by the cwd of each call, and a registry dependency compiles
            # with a cwd outside the repo (~/.cargo/registry), which would fall to the rustup
            # default (stable) and mix an in-repo nightly with an out-of-repo stable (E0514).
            (cd "$proj" && CARGO_TARGET_DIR="${MIRVM_HOME:-$HOME/.mirvm}/target/native" \
                RUSTFLAGS="--cap-lints allow" \
                RUSTC="$RUSTC" \
                timeout "$tmo" "$CARGO" run -q --locked -- ${argv[@]+"${argv[@]}"} \
                >"$TMP/$name.native.out" 2>"$TMP/$name.native.err") || ncode=$?
            if [ "$ncode" -ne 0 ]; then
                bad "c_$name (native baseline exit=$ncode; both failing is not PASS)"
                tail -5 "$TMP/$name.native.err"
            elif diff -q "$TMP/$name.native.out" "$TMP/$name.out" >/dev/null \
                && diff -q "$TMP/$name.native.err" "$TMP/$name.err" >/dev/null; then
                ok "c_$name (three-way comparison == native)"
            else
                bad "c_$name (three-way comparison differs from native)"
                diff "$TMP/$name.native.out" "$TMP/$name.out" | head -5
                diff "$TMP/$name.native.err" "$TMP/$name.err" | head -5
            fi
        elif [ "$code" -eq 77 ]; then
            skip "c_$name (needs absent: $needs)"
        else
            bad "c_$name (mirvm exit=$code): $(tail -1 "$TMP/$name.err" | head -c 100)"
        fi
        continue
    fi
    corpus_run "$TMP" "$name" "$tmo" "$envv" "$needs" ${argv[@]+"${argv[@]}"} || code=$?
    stdout=$(cat "$TMP/$name.out" 2>/dev/null)
    if [ "$code" -eq 77 ]; then
        skip "c_$name (needs absent: $needs)"
    elif [ "$code" -eq 2 ]; then
        bad "c_$name (registered in the manifest but no driver file)"
    elif [ -n "$xfail_spec" ]; then
        record_expected_failure "c_$name" "$code" "$xfail_spec" "$TMP/$name.err"
    elif [ "$code" -ne 0 ]; then
        bad "c_$name (exit=$code): $(tail -1 "$TMP/$name.err" | head -c 100)"
    elif [[ "$mode" == oracle:* ]]; then
        oname=${mode#oracle:}
        oracle=$(cat "tests/suites/corpus/fixtures/oracles/$oname.txt")
        if [ "$stdout" = "$oracle" ]; then
            ok "c_$name"
        else
            bad "c_$name oracle mismatch (see tests/suites/corpus/fixtures/oracles/$oname.txt)"
        fi
    else
        ok "c_$name"
    fi
done <<< "$corpus_rows"
section_end
target_budget_check
cache_snapshot "gate end"
print_slowest 10
print_section_report
suite_summary corpus.contract
