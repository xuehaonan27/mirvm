#!/usr/bin/env bash
# jit-cache: one workload through the JIT code store, twice, in a home of its own.
#
#   1) cold — every compiled function is captured and the store gains a pack of entries;
#   2) warm — the same run must publish at least one entry *from the store* (`MIRVM_JIT_DEBUG` says so)
#      and must produce byte-identical stdout and exit code, which is the stale-semantics catcher;
#   3) bypass — `MIRVM_NO_JIT_CACHE=1` must produce the same output, so a stored run and a compiling
#      one stay comparable.
#
# The case is Linux/x86_64 only: the linker applies that pair's relocation kinds, and a pair whose call
# encoding it does not apply yet (the macOS item of the design) stores nothing.
# fields: input args env needs
MODE_FIELDS="input args env needs"
MODE_REQUIRED="input"

mode_run() {
    local input name needs
    input=$(field_required input)
    name=$CASE_NAME
    needs=$(field needs "")
    needs=${needs//\{DATA\}/$DATA_DIR}
    needs=${needs//\{ROOT\}/$REPO_ROOT}
    case_init

    # The linker applies the x86_64 relocation kinds; a pair whose call encoding it does not apply yet
    # stores nothing, and this case says so rather than failing there.
    if [ -n "$needs" ] && [ ! -e "$needs" ]; then
        skip "$name (needs absent: $needs)"
        return 77
    fi
    apply_env "$(field env "")"

    local -a pre=()
    expand_list "$(field args "")"
    pre=(${EXPANDED[@]+"${EXPANDED[@]}"})

    local HOME_DIR=$TMP/home
    local JIT_DIR=$HOME_DIR/cache/jit

    abort_test() {
        bad "$*"
        exit 1
    }

    run() { # <tag> [extra env...]
        local tag=$1
        shift
        env MIRVM_HOME="$HOME_DIR" MIRVM_JIT_DEBUG=1 "$@" \
            "$MIRVM" run "${pre[@]}" "$DATA_DIR/$input" >"$TMP/$tag.out" 2>"$TMP/$tag.err"
        echo $? >"$TMP/$tag.code"
    }

    run cold
    [ "$(cat "$TMP/cold.code")" = 0 ] || abort_test "cold run exited $(cat "$TMP/cold.code"): $(tail -3 "$TMP/cold.err")"
    grep -q "published from a stored entry" "$TMP/cold.err" \
        && abort_test "the cold run reused an entry no earlier run had written"
    [ -n "$(ls "$JIT_DIR"/*.pack 2>/dev/null)" ] \
        || abort_test "the cold run stored no entry: $(ls "$JIT_DIR" 2>/dev/null | head -3)"
    "$MIRVM" cache status 2>/dev/null | grep -q "cache/jit" \
        || abort_test "cache status does not report the JIT family"

    run warm
    [ "$(cat "$TMP/warm.code")" = 0 ] || abort_test "warm run exited $(cat "$TMP/warm.code"): $(tail -3 "$TMP/warm.err")"
    grep -q "published from a stored entry" "$TMP/warm.err" \
        || abort_test "the warm run did not publish a single entry from the store"
    cmp -s "$TMP/cold.out" "$TMP/warm.out" \
        || abort_test "the stored run and the compiling run disagree on stdout"

    run bypass MIRVM_NO_JIT_CACHE=1
    [ "$(cat "$TMP/bypass.code")" = 0 ] || abort_test "bypassed run exited $(cat "$TMP/bypass.code")"
    grep -q "published from a stored entry" "$TMP/bypass.err" \
        && abort_test "MIRVM_NO_JIT_CACHE=1 still used the store"
    cmp -s "$TMP/cold.out" "$TMP/bypass.out" \
        || abort_test "the bypassed run and the stored run disagree on stdout"

    ok "cold stored, warm reused, bypass compiled, all three agree"
}
