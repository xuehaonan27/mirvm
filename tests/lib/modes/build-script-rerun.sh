#!/usr/bin/env bash
# build-script-rerun: build.rs rerun-if incremental contract.
# fields: fixture
MODE_FIELDS="fixture"
MODE_REQUIRED="fixture"

mode_run() {
    case_init
    case_fixtures
    apply_env "$(field env "")"
cp -r ${FIXTURES[0]} "$TMP/br"

# <description> <pattern> <file>: grep -F fixed-point assertion
expect() {
    local desc="$1" pat="$2" file="$3"
    if grep -qF -- "$pat" "$file"; then
        echo "PASS $desc"; pass=$((pass + 1))
    else
        echo "FAIL $desc: missing [$pat] ($file)"
        echo "--- $file ---"; cat "$file"
        fail=$((fail + 1))
    fi
}
# <expected exit> <tag> [env...]: run mirvm with MIRVM_DEPS=self, saving <tag>.out/.err
run_br() {
    local want="$1" tag="$2"; shift 2
    env -u RUST_BACKTRACE MIRVM_DEPS=self MIRVM_DEBUG_BLDRS=1 "$@" "$MIRVM" run "$TMP/br" \
        >"$TMP/$tag.out" 2>"$TMP/$tag.err"
    local code=$?
    if [ "$code" = "$want" ]; then
        echo "PASS run $tag exit $code"; pass=$((pass + 1))
    else
        echo "FAIL run $tag: exit $code (want $want)"
        tail -20 "$TMP/$tag.err"; fail=$((fail + 1))
    fi
}
# <description> <fileA> <fileB> same|diff
expect_cmp() {
    local desc="$1" a="$2" b="$3" want="$4"
    local eq=1; diff -q "$a" "$b" >/dev/null || eq=0
    if { [ "$want" = same ] && [ $eq = 1 ]; } || { [ "$want" = diff ] && [ $eq = 0 ]; }; then
        echo "PASS $desc"; pass=$((pass + 1))
    else
        echo "FAIL $desc ($a vs $b, want $want)"
        echo "--- A ---"; cat "$a"; echo "--- B ---"; cat "$b"
        fail=$((fail + 1))
    fi
}

# ① first pass: everything runs (no-record)
run_br 7 first
expect "① bdep run" "bldrs run bdep no-record" "$TMP/first.err"
expect "① root package run" "bldrs run cless_br no-record" "$TMP/first.err"

# ② second pass: everything skips + byte-identical output
run_br 7 second
expect "② bdep skip (default face tree identical)" "bldrs skip bdep default-tree-intact" "$TMP/second.err"
expect "② root package skip (default face tree identical + env unchanged)" "bldrs skip cless_br default-tree-intact" "$TMP/second.err"
expect_cmp "② output byte-identical to ①" "$TMP/first.out" "$TMP/second.out" same

# ③ touch bdep/build.rs => fingerprint changes => both bdep and the root run (no-record)
touch "$TMP/br/bdep/build.rs"
run_br 7 third
expect "③ bdep run" "bldrs run bdep no-record" "$TMP/third.err"
expect "③ root package run (dep fingerprint propagates)" "bldrs run cless_br no-record" "$TMP/third.err"
expect_cmp "③ output still byte-identical to ②" "$TMP/second.out" "$TMP/third.out" same

# ④ BR_TOGGLE=xyz => root runs (env:BR_TOGGLE) and output changes; bdep skips
run_br 7 fourth BR_TOGGLE=xyz
expect "④ root package run env:BR_TOGGLE" "bldrs run cless_br env:BR_TOGGLE" "$TMP/fourth.err"
expect "④ bdep skip (neither env nor tree moved)" "bldrs skip bdep default-tree-intact" "$TMP/fourth.err"
expect "④ output contains toggle=xyz" "toggle=xyz" "$TMP/fourth.out"
expect_cmp "④ output differs from ③ (env enters output)" "$TMP/third.out" "$TMP/fourth.out" diff

# ⑤ BR_TOGGLE=xyz unchanged => root skips, output byte-identical to ④
run_br 7 fifth BR_TOGGLE=xyz
expect "⑤ root package skip" "bldrs skip cless_br default-tree-intact" "$TMP/fifth.err"
expect_cmp "⑤ output byte-identical to ④ (archive replay equivalent)" "$TMP/fourth.out" "$TMP/fifth.out" same

# ⑥ registry face: cless_libc's second pass must skip libc (the global cache or an
#    existing archive may already cover it; env -u clears BR_TOGGLE so faces cannot bleed together)
for i in 1 2; do
    env -u RUST_BACKTRACE -u BR_TOGGLE MIRVM_DEPS=self MIRVM_DEBUG_BLDRS=1 "$MIRVM" run \
        ${FIXTURES[1]} >"$TMP/libc$i.out" 2>"$TMP/libc$i.err"
    code=$?
    if [ "$code" != 6 ]; then
        echo "FAIL libc pass $i: exit $code (want 6)"; tail -20 "$TMP/libc$i.err"
        fail=$((fail + 1))
    fi
done
expect "⑥ registry libc second pass skips" "bldrs skip libc" "$TMP/libc2.err"
expect_cmp "⑥ libc output byte-identical across both passes" "$TMP/libc1.out" "$TMP/libc2.out" same
}
