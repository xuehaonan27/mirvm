#!/usr/bin/env bash
# native-diff: the pinned rustc is the authority. Compile and run the program natively, run it under
# mirvm, and require stdout, stderr (after normalize_stderr) and the exit code to agree; the warm
# rerun guards against a cache replay faking a green. A host that lacks a capability the program
# itself reports as unavailable is a SKIP, never a PASS.
# fields: input(required) env calls variant skip_if skip_reason sub_caps nowarm

MODE_FIELDS="input env calls variant skip_if skip_reason sub_caps nowarm"
MODE_REQUIRED="input"

# run_subject <src> <prefix> <calls>: one mirvm run, or one per --vm-call with the outputs appended.
run_subject() {
    local src=$1 prefix=$2 calls=$3
    local -a specs=()
    [ -n "$calls" ] && { expand_list "$calls"; specs=(${EXPANDED[@]+"${EXPANDED[@]}"}); }
    : >"$prefix.out"
    : >"$prefix.err"
    local code=0 spec
    if [ ${#specs[@]} -eq 0 ]; then
        run_case_cmd "$prefix" env -u RUST_BACKTRACE "$MIRVM" run "$src"
    else
        for spec in "${specs[@]}"; do
            run_case_cmd "$prefix.one" env -u RUST_BACKTRACE "$MIRVM" run "$src" --vm-call "$spec"
            cat "$prefix.one.out" >>"$prefix.out"
            cat "$prefix.one.err" >>"$prefix.err"
            [ "$code" -eq 0 ] && code=$(cat "$prefix.one.code")
        done
        rm -f "$prefix.one.out" "$prefix.one.err" "$prefix.one.code"
        printf '%s\n' "$code" >"$prefix.code"
    fi
}

mode_run() {
    local input name base src calls unavail reason sub_caps variant
    input=$(field_required input)
    name=$CASE_NAME
    src=$DATA_DIR/$input
    calls=$(field calls "")
    case_init
    # TMP exists only after case_init, and the mode runs under `set -u`.
    base=$TMP/$name
    apply_env "$(field env "")"

    # `mirvm run` holds a guest build's own front-end diagnostics back on success — they are
    # preparation detail, released only when the build failed or under `-v` — so the native leg is
    # compiled with the same discipline and the compared stderr is the guest's own output on both
    # sides. A compiler error still fails this leg, and is reported as such.
    if ! "$RUSTC" --edition 2024 -Awarnings -o "$base.native.bin" "$src" 2>"$base.build"; then
        bad "$name (native rustc failed)"
        head -20 "$base.build"
        return 1
    fi
    run_case_cmd "$base.native" env -u RUST_BACKTRACE "$base.native.bin"
    cat "$base.build" "$base.native.err" >"$base.merged"
    mv "$base.merged" "$base.native.err"

    unavail=$(field skip_if "")
    if [ -n "$unavail" ] && grep -Fxq "$unavail" "$base.native.out"; then
        reason=$(field skip_reason "host lacks a required capability")
        run_subject "$src" "$base.mirvm" "$calls"
        if cmp -s "$base.native.out" "$base.mirvm.out"; then
            skip "$name ($reason)"
            return 0
        fi
        bad "$name (unavailable-host behaviour differs)"
        head -20 "$base.mirvm.err"
        return 1
    fi

    run_subject "$src" "$base.mirvm" "$calls"
    compare_streams "$name" "$base.native" "$base.mirvm" native mirvm || return 1

    if [ -z "$(field nowarm "")" ]; then
        run_subject "$src" "$base.warm" "$calls"
        compare_streams "$name (warm rerun)" "$base.mirvm" "$base.warm" cold warm || return 1
    fi

    sub_caps=$(field sub_caps "")
    if [ -n "$sub_caps" ]; then
        local -a caps=()
        expand_list "$sub_caps"
        caps=(${EXPANDED[@]+"${EXPANDED[@]}"})
        local cap status
        for cap in "${caps[@]}"; do
            status=$(grep -E "^${cap}=([0-9a-f]+|unavailable)$" "$base.native.out" || true)
            if [ "$(printf '%s\n' "$status" | grep -c .)" -ne 1 ]; then
                bad "$name/$cap (native status missing or ambiguous)"
                return 1
            fi
            case "$status" in
                *unavailable) skip "$name/$cap (host lacks $cap)" ;;
                *) ok "$name/$cap" ;;
            esac
        done
    fi

    variant=$(field variant "")
    if [ -n "$variant" ]; then
        apply_env "$variant"
        run_subject "$src" "$base.variant" "$calls"
        compare_streams "$name (variant)" "$base.native" "$base.variant" native mirvm || return 1
    fi
    return 0
}
