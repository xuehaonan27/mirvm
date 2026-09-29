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
#      registration another test starts meanwhile can end the unwinder's walk early;
#   6) determinism — two cold runs in homes of their own, one with one frontend thread and one with
#      eight, store byte-identical entries: an entry is a function of the fragment and the key, not of
#      the process that compiled it nor of how the session was threaded.
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

    run cold MIRVM_JIT_STATS=1
    [ "$(cat "$TMP/cold.code")" = 0 ] || abort_test "cold run exited $(cat "$TMP/cold.code"): $(tail -3 "$TMP/cold.err")"
    # Nothing is known about this program yet, so every function starts at the cheap tier.
    grep -qE "tier_baseline=[1-9]" "$TMP/cold.err" \
        || abort_test "the cold run did not compile at the baseline tier"
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
    # The heat order learned last run is what asks for the optimized tier: the warm run must show both
    # the linked entries and the tier split the policy produced.
    grep -qE "cache_hits=[1-9]" "$TMP/warm.err" \
        || abort_test "the warm run linked nothing from the store"
    grep -qE "tier_optimized=[1-9]" "$TMP/warm.err" \
        || abort_test "no function reached the optimized tier"

    run bypass MIRVM_NO_JIT_CACHE=1
    [ "$(cat "$TMP/bypass.code")" = 0 ] || abort_test "bypassed run exited $(cat "$TMP/bypass.code")"
    grep -q "published from a stored entry" "$TMP/bypass.err" \
        && abort_test "MIRVM_NO_JIT_CACHE=1 still used the store"
    cmp -s "$TMP/cold.out" "$TMP/bypass.out" \
        || abort_test "the bypassed run and the stored run disagree on stdout"

    # 4) honesty: what the store holds must be verified before it is used. A byte in the middle of
    #    every pack breaks the record it lands in, so those entries are refused — the run still produces
    #    the same output, and it says it refused something instead of linking it.
    for pack in "$JIT_DIR"/*.pack; do
        middle=$(( $(wc -c <"$pack") / 2 ))
        printf '\xff' | dd of="$pack" bs=1 seek="$middle" conv=notrunc 2>/dev/null
    done
    run corrupt MIRVM_JIT_STATS=1
    [ "$(cat "$TMP/corrupt.code")" = 0 ] || abort_test "corrupt-store run exited $(cat "$TMP/corrupt.code")"
    cmp -s "$TMP/cold.out" "$TMP/corrupt.out" \
        || abort_test "a corrupted entry changed the guest output"
    grep -qE "cache_refused=[1-9]" "$TMP/corrupt.err" \
        || abort_test "no corrupted entry was refused: $(tail -1 "$TMP/corrupt.err")"

    # 4b) and an empty store is a miss, not a mystery: the same output again, nothing reused.
    rm -f "$JIT_DIR"/*.pack
    run missing MIRVM_JIT_STATS=1
    [ "$(cat "$TMP/missing.code")" = 0 ] || abort_test "missing-store run exited $(cat "$TMP/missing.code")"
    cmp -s "$TMP/cold.out" "$TMP/missing.out" \
        || abort_test "an empty store changed the guest output"
    grep -qE "cache_hits=0 cache_misses=[1-9]" "$TMP/missing.err" \
        || abort_test "an empty store did not miss: $(tail -1 "$TMP/missing.err")"

    # 5) the §4 unwind proof at loaded addresses, alone in its own process.
    ( cd "$REPO_ROOT" && "${CARGO:-cargo}" test --locked --all-features --lib -- --ignored \
        a_linked_entry_unwinds_through_a_loaded_frame >"$TMP/unwind.txt" 2>&1 ) \
        || abort_test "the loaded-frame unwind proof failed: $(tail -3 "$TMP/unwind.txt")"
    grep -q "1 passed" "$TMP/unwind.txt" \
        || abort_test "the loaded-frame unwind proof did not run: $(tail -3 "$TMP/unwind.txt")"

    # 6) an entry is a function of the fragment and the key — not of the process that compiled it, and
    #    not of how that session was threaded: two cold runs in homes of their own, one with one
    #    frontend thread and one with eight, must store byte-identical entries. A compiler address
    #    baked into the code is what this catches, and inside one process it is invisible, because the
    #    code runs on the address it was built with.
    for pair in a:1 b:8; do
        local tag=${pair%%:*} threads=${pair##*:}
        rm -rf "$TMP/ident-$tag"
        mkdir -p "$TMP/ident-$tag/cache"
        cp -r "$HOME_DIR/data" "$TMP/ident-$tag/data"
        cp -r "$HOME_DIR/cache/base" "$TMP/ident-$tag/cache/base"
        env MIRVM_HOME="$TMP/ident-$tag" MIRVM_JIT_SYNC=1 MIRVM_JIT_THRESHOLD=1 \
            MIRVM_THREADS="$threads" \
            "$MIRVM" run "${pre[@]}" "$DATA_DIR/$input" >"$TMP/ident-$tag.out" 2>&1 \
            || abort_test "the determinism run $tag (threads=$threads) failed: $(tail -3 "$TMP/ident-$tag.out")"
    done
    cmp -s "$TMP/ident-a.out" "$TMP/ident-b.out" \
        || abort_test "the one-thread and eight-thread runs disagree on stdout"
    python3 - "$TMP/ident-a" "$TMP/ident-b" >"$TMP/ident.txt" 2>&1 <<'IDENTITY'
import glob, struct, sys

def load(home):
    entries = {}
    for path in glob.glob(home + "/cache/jit/*.pack"):
        raw = open(path, "rb").read()
        count = struct.unpack_from("<I", raw, 12)[0]
        index = struct.unpack_from("<Q", raw, len(raw) - 20)[0]
        for i in range(count):
            at = index + i * 76
            key = raw[at:at + 32].hex()
            offset, length = struct.unpack_from("<QI", raw, at + 64)
            entries[key] = raw[offset + 36:offset + 36 + length]
    return entries

first, second = (load(home) for home in sys.argv[1:3])
if set(first) != set(second):
    print("the two stores hold different keys")
    sys.exit(1)
differing = [key for key in first if first[key] != second[key]]
if differing:
    print(f"{len(differing)} of {len(first)} entries differ between two cold processes")
    sys.exit(1)
if not first:
    print("the cold runs stored nothing, so the comparison proves nothing")
    sys.exit(1)
print(f"{len(first)} entries identical")
IDENTITY
    grep -qE "^[1-9][0-9]* entries identical$" "$TMP/ident.txt" \
        || abort_test "the two cold stores differ: $(tail -2 "$TMP/ident.txt")"

    ok "cold stored, warm reused, bypass compiled, corruption refused, misses rebuilt, frame unwound, entries identical across processes and thread counts"
}
