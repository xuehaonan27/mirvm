#!/usr/bin/env bash
# pair: the same case through both dependency paths -- MIRVM_DEPS=cargo (the Cargo-driven path) and
# MIRVM_DEPS=self (the in-tree resolver and scheduler). The two legs must agree byte-for-byte on
# stdout, stderr and exit code, after normalize_stderr. This is what proves the Cargo-free path is a
# real substitute rather than a second implementation with its own behaviour.
#
# The self leg then runs a second time before anything is purged. A cold self run resolves the
# dependencies and writes the lock; the next one reads that lock back, which is a different path
# through the resolver and the only thing that exercises it end to end. A purge between the two would
# hide exactly that, so the warm run comes first and must agree with the cold one byte for byte.
# fields: input(required) env needs args

MODE_FIELDS="input env needs args"
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

    local leg
    for leg in cargo self; do
        disk_guard
        local -a cmd=(env -u RUST_BACKTRACE MIRVM_DEPS=$leg "$MIRVM" run "$DATA_DIR/$input")
        [ ${#args[@]} -gt 0 ] && cmd+=(-- "${args[@]}")
        run_case_cmd "$TMP/$leg" "${cmd[@]}"
        if [ "$leg" = self ]; then
            run_case_cmd "$TMP/self.warm" "${cmd[@]}"
            compare_streams "$name (self warm rerun)" "$TMP/self" "$TMP/self.warm" cold warm || return 1
        fi
        if [ -z "${MIRVM_GATE_KEEP_CACHE:-}" ]; then
            "$MIRVM" cache purge --deps --ir >/dev/null 2>&1 || true
        fi
    done

    if [ "$(cat "$TMP/cargo.code")" -eq 0 ] && [ "$(cat "$TMP/self.code")" -ne 0 ]; then
        bad "$name (cargo leg green, self leg exit=$(cat "$TMP/self.code"))"
        tail -5 "$TMP/self.err"
        return 1
    fi
    compare_streams "$name" "$TMP/cargo" "$TMP/self" cargo self
}
