#!/usr/bin/env bash
# prepare: the preparation/execution split. `mirvm run` is quiet on success — a guest build's own
# compiler diagnostics are preparation, not program output — while `mirvm prepare` does the same
# work, reports it, and stops before the guest starts. A build that failed still speaks on both.
# fields: fixture(required)

MODE_FIELDS="fixture"
MODE_REQUIRED="fixture"

mode_run() {
    case_init
    case_fixtures
    apply_env "$(field env "")"

    local clean=${FIXTURES[0]} warning=${FIXTURES[1]}
    local home="$TMP/home"
    mkdir -p "$home"
    ensure_test_sysroot "$MIRVM" "$home" "$RUSTC" || exit $?

    local pass=0 fail=0

    # <description> <exit> <tag> [env...] -- <args...>
    run_mirvm() {
        local desc="$1" want="$2" tag="$3"
        shift 3
        local code=0
        env -u RUST_BACKTRACE MIRVM_HOME="$home" MIRVM_SYSROOT="$TEST_SYSROOT" "$@" \
            >"$TMP/$tag.out" 2>"$TMP/$tag.err" || code=$?
        if [ "$code" = "$want" ]; then
            echo "PASS $desc (exit $code)"; pass=$((pass + 1))
        else
            echo "FAIL $desc: exit $code want $want"
            echo "--- stdout ---"; head -20 "$TMP/$tag.out"
            echo "--- stderr ---"; head -20 "$TMP/$tag.err"
            fail=$((fail + 1))
        fi
    }

    # <description> <pattern> <tag>: the captured stderr contains the pattern
    expect_stderr() {
        local desc="$1" pattern="$2" tag="$3"
        if grep -qF -- "$pattern" "$TMP/$tag.err"; then
            echo "PASS $desc"; pass=$((pass + 1))
        else
            echo "FAIL $desc: stderr lacks [$pattern]"
            head -20 "$TMP/$tag.err"
            fail=$((fail + 1))
        fi
    }

    # <description> <tag>: the captured stderr is empty — the whole point of the quiet run
    expect_silent() {
        local desc="$1" tag="$2"
        if [ ! -s "$TMP/$tag.err" ]; then
            echo "PASS $desc"; pass=$((pass + 1))
        else
            echo "FAIL $desc: stderr is not empty"
            head -20 "$TMP/$tag.err"
            fail=$((fail + 1))
        fi
    }

    # ① prepare does the whole build, reports the phases, and does not run the guest.
    run_mirvm "prepare builds and stops" 0 prep "$MIRVM" prepare "$clean"
    expect_stderr "prepare reports the compiler frontend" "frontend=" prep
    if [ ! -s "$TMP/prep.out" ]; then
        echo "PASS prepare ran no guest"; pass=$((pass + 1))
    else
        echo "FAIL prepare ran the guest"; cat "$TMP/prep.out"; fail=$((fail + 1))
    fi

    # ② what prepare built is what the next run loads: no frontend phase, just the cache.
    run_mirvm "run after prepare" 0 warm env MIRVM_TIMING=1 "$MIRVM" run "$clean"
    expect_stderr "run after prepare is a cache hit" "cache-load=" warm
    if grep -q "frontend=" "$TMP/warm.err"; then
        echo "FAIL run after prepare rebuilt the guest"
        head -5 "$TMP/warm.err"; fail=$((fail + 1))
    else
        echo "PASS run after prepare compiled nothing"; pass=$((pass + 1))
    fi

    # ③ the guest's own compiler warning is preparation detail: held back by default.
    run_mirvm "plain run of a warning guest" 0 quiet "$MIRVM" run "$warning"
    expect_silent "plain run holds build diagnostics back" quiet
    if [ "$(cat "$TMP/quiet.out")" = "prepare-warning-ran" ]; then
        echo "PASS plain run still ran the guest"; pass=$((pass + 1))
    else
        echo "FAIL plain run did not run the guest"; cat "$TMP/quiet.out"; fail=$((fail + 1))
    fi

    # ④ the two spellings that ask for that detail: prepare, and -v.
    run_mirvm "prepare shows the warning" 0 prep-warning "$MIRVM" prepare "$warning"
    expect_stderr "prepare releases the warning" "never_called_by_the_guest" prep-warning
    run_mirvm "-v shows the warning" 0 verbose "$MIRVM" run -v "$warning"
    expect_stderr "-v releases the warning" "never_called_by_the_guest" verbose
    run_mirvm "MIRVM_LOG=debug shows the warning" 0 verbose-env \
        env MIRVM_LOG=debug "$MIRVM" run "$warning"
    expect_stderr "MIRVM_LOG=debug releases the warning" "never_called_by_the_guest" verbose-env

    # ⑤ an explicit threshold outranks the command's own verbosity, and an unknown one is refused
    # rather than read as the default.
    run_mirvm "prepare under MIRVM_LOG=error" 0 prep-quiet \
        env MIRVM_LOG=error "$MIRVM" prepare "$warning"
    expect_silent "MIRVM_LOG=error silences prepare" prep-quiet
    run_mirvm "an unknown severity is a usage error" 2 bad-level \
        env MIRVM_LOG=loud "$MIRVM" run "$clean"
    expect_stderr "the rejection names the vocabulary" \
        "error, warning, note, info or debug" bad-level

    # ⑥ a build that failed is not preparation detail: the user has to see why.
    cat >"$TMP/broken.rs" <<'EOF'
fn main() {
    let x: i32 = "not an integer";
    println!("{x}");
}
EOF
    run_mirvm "a failed build still speaks" 1 broken "$MIRVM" run "$TMP/broken.rs"
    expect_stderr "failed build replays the compiler error" "error[E0308]" broken

    # ⑦ prepare rejects what only execution can honour, loudly, and names itself when it has no input.
    run_mirvm "prepare rejects an execution-only flag" 2 reject \
        "$MIRVM" prepare --vm-call main "$clean"
    expect_stderr "prepare names the flag it rejects" "--vm-call" reject
    run_mirvm "prepare without an input" 2 no-input "$MIRVM" prepare
    expect_stderr "the missing input is prepare's, not run's" "`prepare` needs an input" no-input

    # ⑧ the dependency track does not change what prepare means for a single file: a plain .rs never
    # takes the Cargo track, so this must behave exactly like the default-track prepare above. The
    # earlier prepare already warmed the cache, so the ledger reports a load rather than a build.
    run_mirvm "prepare under MIRVM_DEPS=cargo" 0 prep-cargo \
        env MIRVM_DEPS=cargo "$MIRVM" prepare "$clean"
    expect_stderr "prepare under MIRVM_DEPS=cargo reports its phases" "mirvm-timing:" prep-cargo

    echo "prepare: $pass passed, $fail failed"
    [ "$fail" -eq 0 ]
}
