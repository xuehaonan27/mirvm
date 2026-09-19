#!/usr/bin/env bash
# project-diff: a real Cargo project is run three ways -- cold under mirvm (must exit 0), warm under
# mirvm (must reproduce the cold run byte for byte, so a cache replay cannot fake a green), and
# natively with `cargo run` in the project directory (the authority). All three must agree on stdout,
# stderr and exit code.
# fields: input(required) env needs args group

MODE_FIELDS="input env needs args group"
MODE_REQUIRED="input"

mode_run() {
    local input name needs
    input=$(field_required input)
    name=$CASE_NAME
    needs=$(field needs "")
    needs=${needs//\{DATA\}/$DATA_DIR}
    needs=${needs//\{ROOT\}/$REPO_ROOT}
    case_init

    if [ -n "$needs" ] && [ ! -e "$needs" ]; then
        skip "$name (needs absent: $needs)"
        return 77
    fi

    apply_env "$(field env "")"
    local -a args=()
    expand_list "$(field args "")"
    args=(${EXPANDED[@]+"${EXPANDED[@]}"})
    local dir=$DATA_DIR/$input

    disk_guard
    local -a cmd=(env -u RUST_BACKTRACE "$MIRVM" run "$dir")
    [ ${#args[@]} -gt 0 ] && cmd+=(-- "${args[@]}")
    run_case_cmd "$TMP/cold" "${cmd[@]}"
    if [ "$(cat "$TMP/cold.code")" -ne 0 ]; then
        bad "$name (cold mirvm run exit=$(cat "$TMP/cold.code"))"
        tail -5 "$TMP/cold.err"
        return 1
    fi

    run_case_cmd "$TMP/warm" "${cmd[@]}"
    compare_streams "$name (warm rerun)" "$TMP/cold" "$TMP/warm" cold warm || return 1

    (cd "$dir" && run_case_cmd "$TMP/native" env -u RUST_BACKTRACE RUSTC="$RUSTC" \
        "$CARGO" run -q ${args[@]+-- "${args[@]}"})
    if [ "$(cat "$TMP/native.code")" -ne 0 ]; then
        bad "$name (native cargo run exit=$(cat "$TMP/native.code"))"
        tail -5 "$TMP/native.err"
        return 1
    fi
    compare_streams "$name" "$TMP/native" "$TMP/warm" native mirvm
}
