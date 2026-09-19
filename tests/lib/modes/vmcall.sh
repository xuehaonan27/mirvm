#!/usr/bin/env bash
# vmcall: run one exported entry point through `mirvm run --vm-call` and require the printed value to
# equal the declared expectation. The expectations are data in the manifest; nothing here knows which
# function is being called.
# fields: input(required) calls(required) env

MODE_FIELDS="input calls env"
MODE_REQUIRED="input calls"

mode_run() {
    local input calls name
    input=$(field_required input)
    calls=$(field_required calls)
    name=$CASE_NAME
    case_init
    apply_env "$(field env "")"

    local -a specs=()
    expand_list "$calls" specs
    local spec wanted got code failures=0
    for spec in "${specs[@]}"; do
        wanted=${spec##*:}
        spec=${spec%:*}
        run_case_cmd "$TMP/call" env -u RUST_BACKTRACE "$MIRVM" run --engine vm --vm-call "$spec" "$DATA_DIR/$input"
        code=$(cat "$TMP/call.code")
        got=$(cat "$TMP/call.out")
        if [ "$code" -eq 0 ] && [ "$got" = "$wanted" ]; then
            ok "$name $spec = $got"
        else
            bad "$name $spec: got '$got' (exit=$code), want $wanted"
            head -3 "$TMP/call.err"
            failures=$((failures + 1))
        fi
    done
    [ "$failures" -eq 0 ]
}
