#!/usr/bin/env bash
# compile-crate: build the crate without running it. Used where the build itself is the contract --
# a source-shared crate that must compile without the product's dependencies, for instance.
# fields: crate(required) env

MODE_FIELDS="crate env"
MODE_REQUIRED="crate"
MODE_PRODUCT=no

mode_run() {
    local crate
    crate=$(field_required crate)
    case_init --no-product
    apply_env "$(field env "")"
    local dir=$DATA_DIR/$crate
    [ -d "$dir" ] || { bad "$CASE_NAME (no crate at $crate)"; return 1; }

    local out code=0
    out=$(cd "$dir" && MIRVM_BUILD_ID=0000000000000000 "${CARGO:-cargo}" build --release --locked 2>&1) || code=$?
    if [ "$code" -eq 0 ]; then
        ok "$CASE_NAME (compiles)"
        return 0
    fi
    bad "$CASE_NAME (build failed, exit=$code)"
    printf '%s\n' "$out" | tail -20
    return 1
}
