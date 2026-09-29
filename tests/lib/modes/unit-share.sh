#!/usr/bin/env bash
# unit-share: a unit is shared between programs, not re-lowered per program.
#
# The fixture holds two packages with overlapping closures: `a` uses memchr, `b` uses memchr and itoa.
# Two different programs therefore have two different unit tables, and the memchr unit must be one
# manifest that `b` loads rather than lowers again. The same run also checks the equivalence the
# design requires: a session that loaded a unit produces the same guest output as one that lowered
# everything.
# fields: fixture
MODE_FIELDS="fixture"
MODE_REQUIRED="fixture"

mode_run() {
    case_init
    case_fixtures
    apply_env "$(field env "")"
    cp -r "${FIXTURES[0]}" "$TMP/ws"
    WS="$TMP/ws"
    HOME_DIR=${MIRVM_HOME:-$HOME/.mirvm}
    UNITS=$HOME_DIR/cache/units

    abort_test() {
        bad "$*"
        exit 1
    }

    # The unit store is derived state, and this case is about what one run leaves for the next.
    rm -rf "$UNITS"

    run() { # <package> <tag> [extra env...]
        local package=$1 tag=$2
        shift 2
        MIRVM_DEPS=self MIRVM_A2_DEBUG=1 MIRVM_TIMING=1 env "$@" \
            "$MIRVM" run "$WS/$package" >"$TMP/$tag.out" 2>"$TMP/$tag.log" \
            || abort_test "$tag exited non-zero: $(tail -3 "$TMP/$tag.log")"
    }

    # unit_key -> how many manifests that unit has
    manifests() {
        ls "$UNITS" 2>/dev/null | sed 's/-[0-9a-f]\{64\}\.unit$//' | sort | uniq -c
    }

    # 1) the memchr-only program publishes the memchr unit
    run a a
    grep -q '^unit_a: 4$' "$TMP/a.out" || abort_test "unit_a output wrong: $(cat "$TMP/a.out")"
    [ -d "$UNITS" ] || abort_test "the first run wrote no unit manifest"
    memchr_key=$(manifests | awk '$2 != "" {print $2}' | head -1)
    [ -n "$memchr_key" ] || abort_test "no unit key in $(ls "$UNITS")"
    memchr_a=$(ls "$UNITS" | grep "^$memchr_key-" | sort)
    [ -n "$memchr_a" ] || abort_test "the memchr unit has no manifest"

    # 2) the memchr+itoa program loads that unit instead of lowering it again: its run publishes only
    #    what it added, and every manifest the first run wrote is still there.
    run b b
    grep -q '^unit_b: 4 4$' "$TMP/b.out" || abort_test "unit_b output wrong: $(cat "$TMP/b.out")"
    grep -q "layer loaded: $memchr_key-" "$TMP/b.log" \
        || abort_test "the second program did not load the shared unit: $(grep -c 'layer loaded' "$TMP/b.log") loads"
    for digest in $memchr_a; do
        [ -f "$UNITS/$digest" ] || abort_test "the second run removed the manifest $digest"
    done
    [ "$(manifests | awk -v key="$memchr_key" '$2 == key {print $1}')" -ge 1 ] \
        || abort_test "the memchr unit lost its manifest"

    # 3) equivalence: the same program with the layer stack bypassed (everything lowered) must print
    #    exactly what the session that loaded a unit printed.
    MIRVM_DEPS=self MIRVM_NO_DEPS_IMAGE=1 "$MIRVM" run "$WS/b" >"$TMP/b-cold.out" 2>/dev/null \
        || abort_test "the bypassed run exited non-zero"
    diff -q "$TMP/b.out" "$TMP/b-cold.out" >/dev/null \
        || abort_test "the loaded-unit run and the fully lowered run disagree"

    # 4) the same program twice: the second run is a pure load (no new manifest for either unit)
    before=$(ls "$UNITS" | sort)
    run b b-again
    [ "$(ls "$UNITS" | sort)" = "$before" ] \
        || abort_test "the warm rerun stored a manifest the store already had"

    # 5) the §5 unit determinism gate: one unit built twice, in separate sessions whose scheduling
    #    differs (one job against eight), yields one manifest digest and one fragment set. The
    #    manifests are content-named, so equal names are equal digests; the packs are content-named
    #    too, so a second build that adds no pack stored no fragment the first one had not.
    rm -rf "$UNITS"
    run a a-one MIRVM_CLESS_JOBS=1
    units_one=$(ls "$UNITS" | sort)
    frags_one=$(ls "$HOME_DIR/cache/frags" 2>/dev/null | sort)
    rm -rf "$UNITS"
    run a a-eight MIRVM_CLESS_JOBS=8
    units_eight=$(ls "$UNITS" | sort)
    frags_eight=$(ls "$HOME_DIR/cache/frags" 2>/dev/null | sort)
    [ -n "$units_one" ] || abort_test "the determinism run stored no unit manifest"
    [ "$units_one" = "$units_eight" ] \
        || abort_test "one unit built twice has two manifest digests: $units_one vs $units_eight"
    [ "$frags_one" = "$frags_eight" ] \
        || abort_test "one unit built twice stored new fragments: $(comm -13 <(echo "$frags_one") <(echo "$frags_eight"))"

    ok "one manifest per unit, shared across programs, with identical output and one digest per unit"
}
