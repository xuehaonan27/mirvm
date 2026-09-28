#!/usr/bin/env bash
# frag-collect: collection drops what no current-generation manifest names, and only that.
#
# A fragment carries no generation: it is live exactly when a current-generation manifest names it. The
# gate walks one publish through that cycle in a home of its own, so every number is the whole store:
#   1) publish — every distinct fragment is live, none dead
#   2) drop the manifests — the same fragments are dead
#   3) `cache purge` collects them and the family empties
#   4) the next session republishes exactly the packs the first one did
# Content addressing is what makes step 4 observable: the same fragments are the same pack file.
# fields: fixture
MODE_FIELDS="fixture"
MODE_REQUIRED="fixture"

mode_run() {
    case_init
    case_fixtures
    apply_env "$(field env "")"
    cp -r "${FIXTURES[0]}" "$TMP/ws"
    WS="$TMP/ws"
    HOME_DIR=$TMP/home

    abort_test() {
        bad "$*"
        exit 1
    }

    run() { # <tag>
        MIRVM_HOME="$HOME_DIR" MIRVM_DEPS=self "$MIRVM" run "$WS/a" >"$TMP/$1.out" 2>"$TMP/$1.log" \
            || abort_test "$1 exited non-zero: $(tail -3 "$TMP/$1.log")"
    }

    status_json() { MIRVM_HOME="$HOME_DIR" "$MIRVM" cache status --json 2>/dev/null; }

    # "live_fragments live_bytes dead_bytes unique_bytes" of the fragment family.
    account() {
        status_json | sed -n 's/.*"unique_bytes":\([0-9]*\),"repeated_bytes":[0-9]*,"live_fragments":\([0-9]*\),"live_bytes":\([0-9]*\),"dead_bytes":\([0-9]*\).*/\2 \3 \4 \1/p'
    }

    packs() { ls "$HOME_DIR/cache/frags" 2>/dev/null | grep '\.pack$' | sort; }

    # 1) the first session publishes; in a home of its own every fragment it wrote is live.
    rm -rf "$HOME_DIR"
    run first
    grep -q '^unit_a: 4$' "$TMP/first.out" || abort_test "output wrong: $(cat "$TMP/first.out")"
    read -r live_frags live_bytes dead_bytes unique_bytes <<<"$(account)"
    [ -n "$unique_bytes" ] || abort_test "cache status does not account for the fragments: $(status_json)"
    [ "$live_frags" -gt 0 ] || abort_test "no fragment is live after a publish: $(account)"
    [ "$dead_bytes" -eq 0 ] || abort_test "a fresh store already has dead fragments: $(account)"
    [ "$live_bytes" -eq "$unique_bytes" ] || abort_test "live bytes are not the distinct bytes: $(account)"
    packs_before=$(packs)
    [ -n "$packs_before" ] || abort_test "the publish wrote no pack"

    # 2) the manifests are the only thing that keeps fragments alive: without them the same bytes are
    #    dead, and the store's own accounting says so. Every family that names fragments counts — the
    #    per-unit manifests, the closure manifest, and the program's own L2 entry, which became a
    #    manifest of fragments when the body store landed.
    rm -f "$HOME_DIR"/cache/units/* "$HOME_DIR"/cache/deps/* "$HOME_DIR"/cache/ir/*
    read -r live_frags live_bytes dead_bytes unique_bytes <<<"$(account)"
    [ "$live_frags" -eq 0 ] || abort_test "a fragment survived its manifest: $(account)"
    [ "$dead_bytes" -eq "$unique_bytes" ] || abort_test "dead bytes are not the distinct bytes: $(account)"

    # 3) `cache purge` collects them. With nothing live the whole family goes, and a second purge has
    #    nothing left to do.
    MIRVM_HOME="$HOME_DIR" "$MIRVM" cache purge >"$TMP/purge.out" 2>&1 \
        || abort_test "cache purge exited non-zero: $(cat "$TMP/purge.out")"
    [ -z "$(packs)" ] || abort_test "collection left packs behind: $(packs)"
    read -r live_frags live_bytes dead_bytes unique_bytes <<<"$(account)"
    [ "$unique_bytes" -eq 0 ] || abort_test "the fragment family still holds bytes: $(account)"
    MIRVM_HOME="$HOME_DIR" "$MIRVM" cache purge >"$TMP/purge2.out" 2>&1 \
        || abort_test "the second purge exited non-zero"
    grep -q 'nothing to be cleared' "$TMP/purge2.out" \
        || abort_test "a second purge still found work: $(cat "$TMP/purge2.out")"

    # 4) the same session republishes the same fragments under the same content-addressed pack name,
    #    and prints what it printed the first time.
    run second
    [ "$(packs)" = "$packs_before" ] || abort_test "the republished packs differ: $(packs) vs $packs_before"
    diff -q "$TMP/first.out" "$TMP/second.out" >/dev/null \
        || abort_test "the two sessions disagree: $(cat "$TMP/first.out") vs $(cat "$TMP/second.out")"

    ok "fragments are live exactly while a manifest names them, and collection is idempotent"
}
