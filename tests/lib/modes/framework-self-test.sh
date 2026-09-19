#!/usr/bin/env bash
# framework-self-test: proves the framework cannot false-green. It drives the real dispatcher and the
# real modes with a fake product and a fake Cargo, so it needs no build: a green case really passes,
# a product that produces nothing fails, both legs failing the same way fails, extra stderr alone
# fails, a failing case propagates its status and is counted, several cases aggregate into one
# verdict, an absent capability is a SKIP rather than a PASS, the expected-red decisions are exact,
# every case in the manifest validates, and no data file is unjustified or hides a script.
# fields: fixture(required)

MODE_FIELDS="fixture"
MODE_REQUIRED="fixture"
MODE_PRODUCT=no

# check <description> <expected exit> <command...>
check() {
    local desc=$1 want=$2 got=0
    shift 2
    "$@" >"$TMP/last" 2>&1 || got=$?
    if [ "$got" = "$want" ]; then
        ok "$desc (exit=$got)"
        return 0
    fi
    bad "$desc (exit=$got, want=$want)"
    head -25 "$TMP/last"
    return 1
}

# last_output <description> <fixed string that the previous check's output must contain>
last_output() {
    local desc=$1 pattern=$2
    if grep -q -- "$pattern" "$TMP/last"; then
        ok "$desc"
    else
        bad "$desc (missing '$pattern')"
        head -20 "$TMP/last"
    fi
}

mode_run() {
    local fakes fake_mirvm fake_cargo silent run manifest_count list_count decision mode_path missing
    fakes=$DATA_DIR/$(field_required fixture)
    fake_mirvm=$fakes/fake-mirvm
    fake_cargo=$fakes/fake-cargo
    silent=$fakes/silent-product
    case_init --no-product
    run=$TESTS_DIR/run.sh

    # ① the manifest: every case names a mode that exists and declares exactly the fields it uses
    check "the manifest validates" 0 "$run" validate

    # ② control and data are separate, and every data file is justified
    check "inventory agrees with data/" 0 "$run" inventory

    # ③ the inventory is generated from the manifest, and the entry point works from any directory
    manifest_count=$(awk '!/^[[:space:]]*(#|$)/ { n++ } END { print n + 0 }' "$TESTS_DIR/manifest")
    list_count=0
    if (cd /tmp && "$run" list >"$TMP/list" 2>&1); then
        list_count=$(tail -n +2 "$TMP/list" | grep -c .)
    fi
    if [ "$manifest_count" = "$list_count" ]; then
        ok "list reports every case once ($list_count) from another directory"
    else
        bad "list reported $list_count of $manifest_count cases"
        head -5 "$TMP/list"
    fi

    # ④ an unregistered case or mode is a usage error, never a silent no-op
    check "an unregistered case is a usage error" 64 "$run" case does-not-exist
    check "an unregistered mode is a usage error" 64 "$run" mode does-not-exist

    # ⑤ a green case really passes, driven by the fake product and the fake Cargo
    check "a green case passes" 0 env MIRVM="$fake_mirvm" CARGO="$fake_cargo" \
        SCRIPT_CACHE="$fakes/cache" "$run" case ffi_zlib
    last_output "and is reported as a PASS" "PASS case ffi_zlib"

    # ⑥ a product that produces nothing must fail the case
    check "a misbehaving product fails its case" 1 env MIRVM="$silent" CARGO="$fake_cargo" \
        SCRIPT_CACHE="$fakes/cache" "$run" case ffi_zlib
    last_output "and is reported as a FAIL" "FAIL case ffi_zlib"

    # ⑦ both legs failing the same way is not a PASS: the authority must reach its declared exit code
    check "both legs failing is not a green" 1 env SCENARIO=false_positive MIRVM="$fake_mirvm" \
        CARGO="$fake_cargo" SCRIPT_CACHE="$fakes/cache" "$run" case ecosystem
    last_output "the native baseline is checked first" "native baseline exit=101"

    # ⑧ identical stdout and exit code with extra product-only stderr must fail
    check "extra stderr alone fails the case" 1 env SCENARIO=stderr_only MIRVM="$fake_mirvm" \
        CARGO="$fake_cargo" SCRIPT_CACHE="$fakes/cache" "$run" case ecosystem
    last_output "the stderr difference is reported" "stderr differs"

    # ⑨ a corpus case that fails propagates its status and is reported with its own judgement
    check "a failing corpus case fails the run" 1 env MIRVM="$fake_mirvm" "$run" case signal
    last_output "the corpus judgement is reported" "FAIL case signal"
    last_output "the summary counts the failure" "1 failed"

    # ⑩ several cases aggregate into one verdict, and a failure among them propagates. The batch is
    # selected by mode, which is the same path `make smoke` uses over a tier.
    check "several cases aggregate" 0 env MIRVM="$fake_mirvm" CARGO="$fake_cargo" \
        SCRIPT_CACHE="$fakes/cache" "$run" mode cargo-diff
    last_output "every case in the batch is counted" "4 passed"
    check "a failure inside a batch propagates" 1 env MIRVM="$silent" CARGO="$fake_cargo" \
        SCRIPT_CACHE="$fakes/cache" "$run" mode cargo-diff
    last_output "the failures are counted" "4 failed"

    # ⑪ an absent capability is a SKIP, never a PASS, and a run of only SKIPs exits 77
    check "an absent capability is a SKIP" 77 env MIRVM="$fake_mirvm" "$run" case opencc
    last_output "and is reported as SKIP" "SKIP case opencc"

    # ⑫ the expected-red decisions: an exact code plus diagnostic is an XFAIL, an unexpected green is
    # a failure, and a wrong shape is a failure.
    printf 'allocator corrupted\n' >"$TMP/xfail.err"
    decision=$( (pass=0 fail=0 skip_count=0 xfail=0
        record_expected_failure synthetic 134 '134:corrupted' "$TMP/xfail.err"
        case_summary synthetic.xfail) 2>&1)
    if printf '%s\n' "$decision" | grep -q 'XFAIL synthetic (corrupted)'; then
        ok "a registered red with its exact diagnostic is an XFAIL"
    else
        bad "the XFAIL decision is wrong"
        printf '%s\n' "$decision"
    fi
    decision=$( (pass=0 fail=0 skip_count=0 xfail=0
        record_expected_failure synthetic 0 '134:corrupted' "$TMP/xfail.err"
        case_summary synthetic.xpass) 2>&1)
    if printf '%s\n' "$decision" | grep -q 'XPASS'; then
        ok "an unexpected green is an XPASS failure"
    else
        bad "the XPASS decision is wrong"
        printf '%s\n' "$decision"
    fi
    decision=$( (pass=0 fail=0 skip_count=0 xfail=0
        record_expected_failure synthetic 70 '134:corrupted' "$TMP/xfail.err"
        case_summary synthetic.wrong) 2>&1)
    if printf '%s\n' "$decision" | grep -q 'wanted xfail 134'; then
        ok "a wrong expected-red shape fails with the wanted shape named"
    else
        bad "the wrong-shape decision is wrong"
        printf '%s\n' "$decision"
    fi

    # ⑬ every mode declares its contract, so a broken mode cannot be silently skipped
    missing=0
    for mode_path in "$LIB_DIR"/modes/*.sh; do
        grep -q '^MODE_FIELDS=' "$mode_path" || { echo "no MODE_FIELDS: $mode_path"; missing=$((missing + 1)); }
        grep -q '^mode_run()' "$mode_path" || { echo "no mode_run: $mode_path"; missing=$((missing + 1)); }
    done
    if [ "$missing" -eq 0 ]; then
        ok "every mode declares MODE_FIELDS and mode_run"
    else
        bad "$missing mode-contract violations"
    fi

    [ "$fail" -eq 0 ]
}
