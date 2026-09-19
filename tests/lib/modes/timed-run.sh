#!/usr/bin/env bash
# timed-run: run the program `runs` times, keep the fastest wall clock, and require it to stay under
# the declared ceiling while still producing the declared output. Timing noise is why the fastest run
# is used, but the ceiling and the expectations are the manifest's, never this file's.
# fields: input(required) args guest env max_ms runs expect_exit expect_stdout

MODE_FIELDS="input args guest env max_ms runs expect_exit expect_stdout"
MODE_REQUIRED="input max_ms"

mode_run() {
    local input name bounds runs want_exit want_stdout
    input=$(field_required input)
    name=$CASE_NAME
    bounds=$(field_required max_ms)
    runs=$(field runs 1)
    want_exit=$(field expect_exit 0)
    want_stdout=$(field expect_stdout "")
    case_init
    apply_env "$(field env "")"

    local -a pre=() guest=()
    expand_list "$(field args "")" pre
    expand_list "$(field guest "")" guest

    local best=99999999 run ms t0 code out
    for ((run = 1; run <= runs; run++)); do
        t0=$(now_ms)
        run_case_cmd "$TMP/run" env -u RUST_BACKTRACE "$MIRVM" run "${pre[@]}" "$DATA_DIR/$input" \
            ${guest[@]+-- "${guest[@]}"}
        ms=$(( ($(now_ms) - t0) / 1000000 ))
        code=$(cat "$TMP/run.code")
        out=$(cat "$TMP/run.out")
        [ "$ms" -lt "$best" ] && best=$ms
        { [ "$code" = "$want_exit" ] && { [ -z "$want_stdout" ] || [ "$out" = "$want_stdout" ]; }; } || break
    done

    if [ "$code" != "$want_exit" ]; then
        bad "$name (exit=$code want=$want_exit)"
        head -10 "$TMP/run.err"
        return 1
    fi
    if [ -n "$want_stdout" ] && [ "$out" != "$want_stdout" ]; then
        bad "$name (stdout='$out' want='$want_stdout')"
        return 1
    fi
    if [ "$best" -le "$bounds" ]; then
        ok "$name (${best}ms <= ${bounds}ms)"
        return 0
    fi
    bad "$name (${best}ms > ${bounds}ms ceiling)"
    return 1
}
