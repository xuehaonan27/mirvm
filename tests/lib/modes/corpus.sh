#!/usr/bin/env bash
# corpus: run one real-crate driver under mirvm and judge it the way the manifest says -- exit code,
# byte-equal oracle stdout, or a registered expected failure. An absent needs= path is a host
# capability SKIP. Deps/IR images are purged after each driver so one driver cannot inherit another's
# cache (MIRVM_GATE_KEEP_CACHE=1 bypasses, for triage only).
# fields: input(required) verdict env needs args group xfail

MODE_FIELDS="input verdict env needs args group xfail"
MODE_REQUIRED="input"

mode_run() {
    local input name verdict needs xfail
    input=$(field_required input)
    name=$CASE_NAME
    verdict=$(field verdict "")
    needs=$(field needs "")
    needs=${needs//\{DATA\}/$DATA_DIR}
    needs=${needs//\{ROOT\}/$REPO_ROOT}
    xfail=$(field xfail "")
    case_init

    if [ -n "$needs" ] && [ ! -e "$needs" ]; then
        skip "$name (needs absent: $needs)"
        return 77
    fi

    apply_env "$(field env "")"
    local -a args=()
    expand_list "$(field args "")"
    args=(${EXPANDED[@]+"${EXPANDED[@]}"})
    local -a cmd=(env -u RUST_BACKTRACE "$MIRVM" run "$DATA_DIR/$input")
    [ ${#args[@]} -gt 0 ] && cmd+=(-- "${args[@]}")

    disk_guard
    local t0; t0=$(date +%s)
    run_case_cmd "$TMP/run" "${cmd[@]}"
    local code; code=$(cat "$TMP/run.code")
    local secs=$(( $(date +%s) - t0 ))
    if [ -z "${MIRVM_GATE_KEEP_CACHE:-}" ]; then
        "$MIRVM" cache purge --deps --ir >/dev/null 2>&1 || true
    fi

    if [ -n "$xfail" ]; then
        record_expected_failure "$name" "$code" "$xfail" "$TMP/run.err"
        return 0
    fi

    case "$verdict" in
        ""|exit)
            if [ "$code" -eq 0 ]; then ok "$name (${secs}s)"; return 0; fi
            local first_err
            first_err=$(grep -m1 -iE 'error|panic|unsupported|unimplemented|not (yet )?(implemented|supported)|no (shim|intrinsic)|abort' "$TMP/run.err" | head -c 200)
            [ -n "$first_err" ] || first_err=$(tail -1 "$TMP/run.err" | head -c 200)
            bad "$name (${secs}s, exit=$code) :: $first_err"
            return 1 ;;
        oracle:*)
            local oracle=$DATA_DIR/fixtures/oracles/${verdict#oracle:}.txt
            [ -f "$oracle" ] || { bad "$name (oracle missing: ${verdict#oracle:})"; return 1; }
            if [ "$code" -eq 0 ] && cmp -s "$oracle" "$TMP/run.out"; then
                ok "$name (${secs}s, oracle ${verdict#oracle:})"
                return 0
            fi
            bad "$name (${secs}s, exit=$code, stdout != oracle ${verdict#oracle:})"
            diff "$oracle" "$TMP/run.out" | head -10
            return 1 ;;
        *)
            bad "$name (unknown verdict '$verdict')"
            return 1 ;;
    esac
}
