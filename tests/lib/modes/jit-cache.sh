#!/usr/bin/env bash
# jit-cache: one workload through the JIT code store, twice, in a home of its own.
#
#   1) cold — every compiled function is captured and the store gains a pack of entries;
#   2) warm — the same run must publish at least one entry *from the store* (`MIRVM_JIT_DEBUG` says so)
#      and must produce byte-identical stdout and exit code, which is the stale-semantics catcher;
#   3) bypass — `MIRVM_NO_JIT_CACHE=1` must produce the same output, so a stored run and a compiling
#      one stay comparable;
#   4) honesty — a corrupted store must be refused rather than used: the same output again, no hit
#      counted, and the run says it compiled;
#   5) unwind — the loaded-frame unwind proof runs here, alone: it resumes through linked frames, and a
#      registration another test starts meanwhile can end the unwinder's walk early.
#
# The counters come from the `mirvm-jit-stats` line, so this case also proves that what the store
# answered is observable.
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

    run warm MIRVM_JIT_STATS=1
    [ "$(cat "$TMP/warm.code")" = 0 ] || abort_test "warm run exited $(cat "$TMP/warm.code"): $(tail -3 "$TMP/warm.err")"
    grep -q "published from a stored entry" "$TMP/warm.err" \
        || abort_test "the warm run did not publish a single entry from the store"
    cmp -s "$TMP/cold.out" "$TMP/warm.out" \
        || abort_test "the stored run and the compiling run disagree on stdout"
    # The cold run left a heat order behind, so the warm one links those entries before it serves a
    # request: that is what makes warmup a link wave instead of a compile wave, and it must be visible.
    grep -q "prelinked [1-9][0-9]* entries from the heat order" "$TMP/warm.err" \
        || abort_test "the warm run did not prelink from the heat order it should have learned"
    prelinked=$(grep -o "cache_prelinked=[0-9]*" "$TMP/warm.err" | tail -1 | cut -d= -f2)
    [ -n "$prelinked" ] && [ "$prelinked" -gt 0 ] \
        || abort_test "the warm run did not count its prelinked entries"

    run bypass MIRVM_NO_JIT_CACHE=1
    [ "$(cat "$TMP/bypass.code")" = 0 ] || abort_test "bypassed run exited $(cat "$TMP/bypass.code")"
    grep -q "published from a stored entry" "$TMP/bypass.err" \
        && abort_test "MIRVM_NO_JIT_CACHE=1 still used the store"
    cmp -s "$TMP/cold.out" "$TMP/bypass.out" \
        || abort_test "the bypassed run and the stored run disagree on stdout"

    # 4) a corrupted pack: the bytes a record carries must be verified before they are linked. One byte
    #    in the middle of the pack is enough to break the record's own hash.
    pack=$(ls "$JIT_DIR"/*.pack | head -1)
    middle=$(( $(wc -c <"$pack") / 2 ))
    printf '\xff' | dd of="$pack" bs=1 seek="$middle" conv=notrunc 2>/dev/null
    run corrupt MIRVM_JIT_STATS=1
    [ "$(cat "$TMP/corrupt.code")" = 0 ] || abort_test "corrupt-store run exited $(cat "$TMP/corrupt.code")"
    cmp -s "$TMP/cold.out" "$TMP/corrupt.out" \
        || abort_test "a corrupted entry changed the guest output"
    stats=$(grep -o "cache_hits=[0-9]* cache_misses=[0-9]* cache_refused=[0-9]*" "$TMP/corrupt.err" | tail -1)
    [ -n "$stats" ] || abort_test "the run did not report the store counters"
    hits=$(printf '%s' "$stats" | sed -n 's/.*cache_hits=\([0-9]*\).*/\1/p')
    [ "$hits" = 0 ] || abort_test "a corrupted entry was used ($stats)"

    # 5) the §4 unwind proof at loaded addresses, alone in its own process.
    ( cd "$REPO_ROOT" && "${CARGO:-cargo}" test --locked --all-features --lib -- --ignored \
        a_linked_entry_unwinds_through_a_loaded_frame >"$TMP/unwind.txt" 2>&1 ) \
        || abort_test "the loaded-frame unwind proof failed: $(tail -3 "$TMP/unwind.txt")"
    grep -q "1 passed" "$TMP/unwind.txt" \
        || abort_test "the loaded-frame unwind proof did not run: $(tail -3 "$TMP/unwind.txt")"

    ok "cold stored, warm reused, bypass compiled, corruption refused, loaded frame unwound"
}
