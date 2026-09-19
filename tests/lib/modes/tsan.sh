#!/usr/bin/env bash
# tsan: ThreadSanitizer is the authority for the engine's own state. The standalone crate compiles
# src/vm source-for-source under -Zsanitizer=thread -Zbuild-std; the verdict is exit 0, not one
# "WARNING: ThreadSanitizer", and a PASS line from every case id the manifest lists. A case that
# stops running or gets renamed therefore fails instead of quietly looking green.
# fields: crate(required) expect(required)

MODE_FIELDS="crate expect"
MODE_REQUIRED="crate expect"
MODE_PRODUCT=no

mode_run() {
    local crate expect
    crate=$(field_required crate)
    expect=$(field_required expect)
    case_init --no-product

    cd "$DATA_DIR/$crate" || { bad "cannot enter $crate"; return 1; }

    local -a ids=()
    expand_list "$expect"
    ids=(${EXPANDED[@]+"${EXPANDED[@]}"})

    local out code=0
    out=$(MIRVM_BUILD_ID=0000000000000000 \
        RUSTFLAGS="-Zsanitizer=thread" TSAN_OPTIONS="halt_on_error=1" \
        "${CARGO:-cargo}" +"$TOOLCHAIN" run -Zbuild-std --target x86_64-unknown-linux-gnu --release 2>&1) || code=$?
    printf '%s\n' "$out" | tail -12

    if printf '%s\n' "$out" | grep -q "WARNING: ThreadSanitizer"; then
        printf '%s\n' "$out" | grep -B 2 -A 25 "WARNING: ThreadSanitizer" | head -80
        bad "TSan reported a data race"
        return 1
    fi
    if [ "$code" -ne 0 ]; then
        bad "TSan harness failed (exit=$code)"
        return 1
    fi
    local id missing=0
    for id in "${ids[@]}"; do
        printf '%s\n' "$out" | grep -q "^PASS $id" || { echo "case did not run or did not pass: $id"; missing=$((missing + 1)); }
    done
    if [ "$missing" -ne 0 ]; then
        bad "TSan harness ran $(( ${#ids[@]} - missing ))/${#ids[@]} expected cases"
        return 1
    fi
    ok "TSan zero-race warning, ${#ids[@]} concurrency cases PASS"
}
