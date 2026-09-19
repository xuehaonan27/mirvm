#!/usr/bin/env bash
# run-expect: run the program and assert declared facts about it -- exit code, stdout against an
# oracle file, and required or forbidden patterns in stdout/stderr. Use this for behaviour that is
# not a comparison against native: a diagnosis, a statistics line, a deliberate failure mode.
# fields: input(required) args guest env exit stdout stderr stderr_absent stdout_absent

MODE_FIELDS="input args guest env exit stdout stdout_match stderr stderr_absent stdout_absent"
MODE_REQUIRED="input"

mode_run() {
    local input name want_exit
    input=$(field_required input)
    name=$CASE_NAME
    want_exit=$(field exit 0)
    case_init
    apply_env "$(field env "")"

    local -a pre=() guest=()
    expand_list "$(field args "")"
    pre=(${EXPANDED[@]+"${EXPANDED[@]}"})
    expand_list "$(field guest "")"
    guest=(${EXPANDED[@]+"${EXPANDED[@]}"})
    local -a cmd=(env -u RUST_BACKTRACE "$MIRVM" run "${pre[@]}")
    cmd+=("$DATA_DIR/$input")
    [ ${#guest[@]} -gt 0 ] && cmd+=(-- "${guest[@]}")

    run_case_cmd "$TMP/run" "${cmd[@]}"
    local got_exit; got_exit=$(cat "$TMP/run.code")

    local -a problems=()
    [ "$got_exit" = "$want_exit" ] || problems+=("exit=$got_exit want=$want_exit")

    local stdout_spec; stdout_spec=$(field stdout "")
    case "$stdout_spec" in
        oracle:*)
            local oracle=$DATA_DIR/fixtures/oracles/${stdout_spec#oracle:}.txt
            [ -f "$oracle" ] || { bad "$name (oracle missing: ${stdout_spec#oracle:})"; return 1; }
            cmp -s "$oracle" "$TMP/run.out" || problems+=("stdout != oracle ${stdout_spec#oracle:}") ;;
        "") ;;
        *) [ "$(cat "$TMP/run.out")" = "$stdout_spec" ] || problems+=("stdout != '$stdout_spec'") ;;
    esac

    local pattern
    pattern=$(field stdout_match "")
    [ -n "$pattern" ] && ! grep -Eq "$pattern" "$TMP/run.out" && problems+=("stdout lacks /$pattern/")
    pattern=$(field stderr "")
    [ -n "$pattern" ] && ! grep -Eq "$pattern" "$TMP/run.err" && problems+=("stderr lacks /$pattern/")
    pattern=$(field stderr_absent "")
    [ -n "$pattern" ] && grep -Eq "$pattern" "$TMP/run.err" && problems+=("stderr contains /$pattern/")
    pattern=$(field stdout_absent "")
    [ -n "$pattern" ] && grep -Eq "$pattern" "$TMP/run.out" && problems+=("stdout contains /$pattern/")

    if [ ${#problems[@]} -eq 0 ]; then
        ok "$name"
        return 0
    fi
    bad "$name (${problems[*]})"
    echo "--- stdout ---"; head -20 "$TMP/run.out"
    echo "--- stderr ---"; head -20 "$TMP/run.err"
    return 1
}
