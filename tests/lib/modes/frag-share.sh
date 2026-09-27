#!/usr/bin/env bash
# frag-share: canonical fragment ids must be stable across crate versions and feature sets.
#
# The dedup probe (MIRVM_FRAG_STATS) prints one line per lowered body, so one session's fragment id
# set can be intersected with another's. The fixture gives both comparisons: two adjacent versions of
# a dependency (one function's body changed) and two feature sets (the same source with one more
# feature). The design's sharing-acceptance ratio is fixed from these numbers, which is why the
# thresholds below name what was measured rather than an expectation:
#   version: 170/171 fragments retained, 98.8% of the union
#   feature: 170/171 fragments retained, 96.6% of the union
# fields: fixture
MODE_FIELDS="fixture"
MODE_REQUIRED="fixture"

mode_run() {
    case_init
    case_fixtures
    apply_env "$(field env "")"
    cp -r "${FIXTURES[0]}" "$TMP/ws"
    WS="$TMP/ws"

    abort_test() {
        bad "$*"
        exit 1
    }

    # One probe run: the guest's output, the id set of the lowered layer, and the body/fragment
    # counts the summary reports. Cold lowering is forced (no L2, no deps image) so that every run
    # lowers the whole closure and the two id sets describe the same universe.
    probe() { # <tag>
        MIRVM_DEPS=self MIRVM_NO_DEPS_IMAGE=1 MIRVM_NO_IR_CACHE=1 MIRVM_FRAG_STATS=1 \
            "$MIRVM" run "$WS" >"$TMP/$1.out" 2>"$TMP/$1.log" \
            || abort_test "$1 run exited non-zero: $(tail -3 "$TMP/$1.log")"
        sed -n 's/^\[frag\] delta \([0-9a-f]\{64\}\) .*/\1/p' "$TMP/$1.log" | sort -u >"$TMP/$1.ids"
        [ -s "$TMP/$1.ids" ] || abort_test "$1 run reported no fragment ids: $(cat "$TMP/$1.log")"
    }

    # share <a> <b> -> "<retention> <jaccard>" on stdout: how much of the first id set survives in
    # the second, and how much of their union both hold.
    share() {
        local inter union a
        inter=$(comm -12 "$TMP/$1.ids" "$TMP/$2.ids" | wc -l | tr -d ' ')
        union=$(cat "$TMP/$1.ids" "$TMP/$2.ids" | sort -u | wc -l | tr -d ' ')
        a=$(wc -l <"$TMP/$1.ids" | tr -d ' ')
        awk -v i="$inter" -v u="$union" -v a="$a" \
            'BEGIN { printf "%.1f %.1f\n", 100 * i / a, 100 * i / u }'
    }

    # at_least <label> <value> <minimum>
    at_least() {
        awk -v v="$2" -v m="$3" 'BEGIN { exit !(v + 0 >= m + 0) }' \
            || abort_test "$1 is $2%, below the measured $3%"
    }

    probe base
    grep -q '^frag_share: ' "$TMP/base.out" || abort_test "base output wrong: $(cat "$TMP/base.out")"

    # Adjacent versions: one dependency function's body changes, everything else is untouched.
    cp "$WS/versions/lib_patched.rs" "$WS/fdep/src/lib.rs"
    probe patched

    # Feature sets: the same sources with one more feature of that dependency enabled.
    cp "${FIXTURES[0]}/fdep/src/lib.rs" "$WS/fdep/src/lib.rs"
    cp "$WS/versions/app_wide.toml" "$WS/Cargo.toml"
    probe wide
    [ "$(cat "$TMP/base.out")" != "$(cat "$TMP/wide.out")" ] \
        || abort_test "the feature set did not change the program's behaviour"

    read -r ret_v jac_v <<<"$(share base patched)"
    read -r ret_f jac_f <<<"$(share base wide)"
    echo "frag-share: version retention ${ret_v}% jaccard ${jac_v}%"
    echo "frag-share: feature retention ${ret_f}% jaccard ${jac_f}%"
    at_least "version retention" "$ret_v" 99
    at_least "version jaccard" "$jac_v" 98
    at_least "feature retention" "$ret_f" 99
    at_least "feature jaccard" "$jac_f" 96

    ok "canonical ids survive a version change and a feature change"
}
