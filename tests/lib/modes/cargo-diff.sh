#!/usr/bin/env bash
# cargo-diff: a frontmatter script is run under mirvm and, through the project directory mirvm
# materialized, under the pinned Cargo. native `cargo run` is the authority and must first reach the
# exit code the manifest declares; then stdout, stderr and exit code must agree, and a warm rerun
# must reproduce the cold run. A registered expected-red case locks its exit code and diagnostic
# instead, and an unexpected green is a failure that demands a contract update.
# fields: input(required) stem(required) verdict expect_native expect_mirvm diagnostic env args

MODE_FIELDS="input stem verdict expect_native expect_mirvm diagnostic env args"
MODE_REQUIRED="input stem"

script_dir() { # <crate name>: the project directory mirvm materialized for this frontmatter script
    local stem=$1 cache=${SCRIPT_CACHE:-${MIRVM_HOME:-$HOME/.mirvm}/build/scripts}
    grep -l "name = \"$stem\"" "$cache"/*/Cargo.toml 2>/dev/null | head -1 | xargs -r dirname
}

mode_run() {
    local input stem verdict want_native want_mirvm diagnostic name dir
    input=$(field_required input)
    stem=$(field_required stem)
    verdict=$(field verdict green)
    want_native=$(field expect_native 0)
    want_mirvm=$(field expect_mirvm "")
    diagnostic=$(field diagnostic "")
    name=$CASE_NAME
    case_init
    apply_env "$(field env "")"

    disk_guard
    run_case_cmd "$TMP/cold" env -u RUST_BACKTRACE "$MIRVM" run "$DATA_DIR/$input"
    local mcode; mcode=$(cat "$TMP/cold.code")

    dir=$(script_dir "$stem")
    if [ -z "$dir" ]; then
        bad "$name (materialized project directory not found for crate $stem)"
        return 1
    fi
    (cd "$dir" && run_case_cmd "$TMP/native" env -u RUST_BACKTRACE RUSTC="$RUSTC" "$CARGO" run -q)
    local ncode; ncode=$(cat "$TMP/native.code")

    if [ "$verdict" = xfail ]; then
        if [ "$ncode" != "$want_native" ]; then
            bad "$name (native baseline exit=$ncode, want=$want_native)"
            return 1
        fi
        if [ "$mcode" = "$ncode" ] && cmp -s "$TMP/native.out" "$TMP/cold.out"; then
            bad "$name XPASS (remove the expected-red entry and promote it)"
            return 1
        fi
        if [ "$mcode" = "$want_mirvm" ] && grep -Fq "$diagnostic" "$TMP/cold.err"; then
            red "$name ($diagnostic; expected red)"
            return 0
        fi
        bad "$name (expected mirvm exit=$want_mirvm + '$diagnostic', native=$ncode mirvm=$mcode)"
        tail -3 "$TMP/cold.err"
        return 1
    fi

    if [ "$ncode" != "$want_native" ]; then
        bad "$name (native baseline exit=$ncode, want=$want_native)"
        tail -20 "$TMP/native.err"
        return 1
    fi
    run_case_cmd "$TMP/warm" env -u RUST_BACKTRACE "$MIRVM" run "$DATA_DIR/$input"
    compare_streams "$name (warm rerun)" "$TMP/cold" "$TMP/warm" cold warm || return 1
    compare_streams "$name" "$TMP/native" "$TMP/cold" native mirvm
}
