#!/usr/bin/env bash
# framework-self-test: proves the framework itself cannot false-green. A happy case really passes, a
# case whose product misbehaves really fails, an absent capability is a SKIP rather than a PASS, an
# unregistered case or mode is a usage error, and the manifest agrees with data/. The fake product
# and fake Cargo are data under fixtures/, so this runs without building anything.
# fields: fixture(required)

MODE_FIELDS="fixture"
MODE_REQUIRED="fixture"
MODE_PRODUCT=no

mode_run() {
    local fakes fake_mirvm fake_cargo
    fakes=$DATA_DIR/$(field_required fixture)
    fake_mirvm=$fakes/fake-mirvm
    fake_cargo=$fakes/fake-cargo
    case_init --no-product

    if "$TESTS_DIR/run.sh" inventory >"$TMP/inv" 2>&1; then
        ok "manifest and data/ agree"
    else
        bad "inventory disagrees"
        cat "$TMP/inv"
    fi

    local manifest_count list_count
    manifest_count=$(awk '!/^[[:space:]]*(#|$)/ { n++ } END { print n + 0 }' "$TESTS_DIR/manifest")
    list_count=$("$TESTS_DIR/run.sh" list | tail -n +2 | grep -c . || true)
    if [ "$manifest_count" = "$list_count" ]; then
        ok "list reports every manifest case once ($list_count)"
    else
        bad "list reports $list_count cases, manifest has $manifest_count"
    fi

    local code=0
    "$TESTS_DIR/run.sh" case does-not-exist >/dev/null 2>&1 || code=$?
    [ "$code" -eq 64 ] && ok "an unregistered case is a usage error" || bad "unknown case exit=$code (want 64)"
    code=0
    "$TESTS_DIR/run.sh" mode does-not-exist >/dev/null 2>&1 || code=$?
    [ "$code" -eq 64 ] && ok "an unregistered mode is a usage error" || bad "unknown mode exit=$code (want 64)"

    # A green case driven by the fake product and fake Cargo must pass without building mirvm.
    code=0
    MIRVM="$fake_mirvm" CARGO="$fake_cargo" SCRIPT_CACHE="$fakes/cache" \
        "$TESTS_DIR/run.sh" case ffi_zlib >"$TMP/green" 2>&1 || code=$?
    if [ "$code" -eq 0 ] && grep -q '^PASS case ffi_zlib' "$TMP/green"; then
        ok "a green case passes end to end"
    else
        bad "a green case did not pass (exit=$code)"
        cat "$TMP/green"
    fi

    # The same case with a product that produces nothing must FAIL: this is the false-green probe.
    code=0
    MIRVM=/bin/true CARGO="$fake_cargo" SCRIPT_CACHE="$fakes/cache" \
        "$TESTS_DIR/run.sh" case ffi_zlib >"$TMP/red" 2>&1 || code=$?
    if [ "$code" -ne 0 ] && grep -q '^FAIL case ffi_zlib' "$TMP/red"; then
        ok "a misbehaving product fails the case"
    else
        bad "false green: exit=$code"
        cat "$TMP/red"
    fi

    # An absent capability must be a SKIP, and a run with nothing but SKIPs must exit 77.
    code=0
    "$TESTS_DIR/run.sh" case opencc >"$TMP/skip" 2>&1 || code=$?
    if [ "$code" -eq 77 ] && grep -q '^SKIP case opencc' "$TMP/skip"; then
        ok "an absent capability is a SKIP, reported as 77"
    else
        bad "absent capability handling wrong (exit=$code)"
        cat "$TMP/skip"
    fi

    [ "$fail" -eq 0 ]
}
