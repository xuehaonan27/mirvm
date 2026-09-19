#!/usr/bin/env bash
# cargoless-workspace: cargoless workspace contract.
# fields: fixture
MODE_FIELDS="fixture"
MODE_REQUIRED="fixture"

mode_run() {
    case_init
    case_fixtures
    apply_env "$(field env "")"
STRACE=${STRACE:-$(command -v strace)}
FIXTURE=${FIXTURES[0]:-}
REMAINING=${FIXTURES[1]:-}
CONTRACT_HOME=${MIRVM_CONTRACT_HOME:-${MIRVM_HOME:-$HOME/.mirvm}}
HOST=$(rustc_host)
require_executable strace "$STRACE" || exit $?
ensure_test_sysroot "$MIRVM" "$CONTRACT_HOME" "$RUSTC" || exit $?
CONTRACT_SYSROOT=$TEST_SYSROOT
require_pinned_cargo || exit $?
mkdir -p "$TMP/no-cargo"
printf '#!/bin/sh\n: >"$MIRVM_CARGO_SENTINEL"\nexit 97\n' >"$TMP/no-cargo/cargo"
chmod +x "$TMP/no-cargo/cargo"
export MIRVM_CARGO_SENTINEL="$TMP/cargo-was-invoked"
normalize() {
    sed -E \
        -e 's/\([0-9]+\) panicked/(<TID>) panicked/' \
        -e 's/finished in [0-9]+\.[0-9]+s/finished in <TIME>/' \
        -e '/^[[:space:]]*(Compiling|Finished|Running|Executable) /d' \
        -e '/^error: test failed, to rerun pass /d' \
        -e '/^error: [0-9]+ target(s)? failed:/d' \
        -e '/^[[:space:]]+`-[^`]*`$/d'
}

triplet() {
    local name=$1 project=$2 expected=$3
    shift 3
    CARGO_TARGET_DIR="$TMP/native-target" "$CARGO" test --manifest-path "$project/Cargo.toml" \
        --locked --offline "$@" >"$TMP/$name.native.out" 2>"$TMP/$name.native.err"
    local nc=$?
    MIRVM_HOME="$CONTRACT_HOME" MIRVM_TARGET_DIR="$TMP/compat-target" MIRVM_DEPS=cargo \
        "$MIRVM" test "$project" --locked --offline "$@" \
        >"$TMP/$name.compat.out" 2>"$TMP/$name.compat.err"
    local cc=$?
    PATH="$TMP/no-cargo:$PATH" MIRVM_HOME="$TMP/self-home" MIRVM_SYSROOT="$CONTRACT_SYSROOT" \
        MIRVM_DEPS=self "$MIRVM" test "$project" --locked --offline "$@" \
        >"$TMP/$name.self.out" 2>"$TMP/$name.self.err"
    local sc=$?
    for leg in native compat self; do
        normalize <"$TMP/$name.$leg.out" >"$TMP/$name.$leg.norm.out"
        normalize <"$TMP/$name.$leg.err" >"$TMP/$name.$leg.norm.err"
    done
    if [ "$nc" != "$expected" ] || [ "$cc" != "$expected" ] || [ "$sc" != "$expected" ]; then
        bad "$name exit code native=$nc compat=$cc self=$sc, expected $expected"
        tail -20 "$TMP/$name.self.err"
    elif ! diff -q "$TMP/$name.native.norm.out" "$TMP/$name.compat.norm.out" >/dev/null \
        || ! diff -q "$TMP/$name.native.norm.out" "$TMP/$name.self.norm.out" >/dev/null; then
        bad "$name stdout differs"
        diff -u "$TMP/$name.native.norm.out" "$TMP/$name.self.norm.out" | head -60
    elif ! diff -q "$TMP/$name.native.norm.err" "$TMP/$name.compat.norm.err" >/dev/null \
        || ! diff -q "$TMP/$name.native.norm.err" "$TMP/$name.self.norm.err" >/dev/null; then
        bad "$name stderr differs"
        diff -u "$TMP/$name.native.norm.err" "$TMP/$name.self.norm.err" | head -40
    else
        ok "$name"
    fi
}

triplet default_members "$FIXTURE" 0 --lib -- --test-threads=1 --nocapture
triplet member_discovery "$FIXTURE/app" 0 --lib -- --test-threads=1 --nocapture
triplet workspace "$FIXTURE" 0 --workspace --lib -- --test-threads=1 --nocapture
triplet package_tool "$FIXTURE" 0 -p workspace-tool --lib -- --test-threads=1 --nocapture
triplet package_tests "$FIXTURE" 0 -p workspace-app --tests -- --test-threads=1 --nocapture
triplet exclude_tool "$FIXTURE" 0 --workspace --exclude workspace-tool --lib -- --test-threads=1 --nocapture
triplet feature_extra "$FIXTURE" 0 -p workspace-app --features extra --test required -- --test-threads=1 --nocapture
triplet dependency_feature "$FIXTURE" 0 -p workspace-app --features shared/extra --lib -- --test-threads=1 --nocapture
triplet workspace_feature "$FIXTURE" 0 --workspace --features workspace-app/extra --test required -- --test-threads=1 --nocapture
triplet unqualified_feature "$FIXTURE" 0 --workspace --features extra --test required -- --test-threads=1 --nocapture
triplet all_features "$FIXTURE" 0 --workspace --all-features --lib -- --test-threads=1 --nocapture
triplet no_default "$FIXTURE" 0 -p workspace-app --no-default-features --lib -- --test-threads=1 --nocapture
triplet resolver_one "$REMAINING" 0 -p remaining-main@0.1.0 --lib -- --test-threads=1 --nocapture
triplet complex_glob "$REMAINING" 0 -p remaining-tool-b@0.4.0 --lib --no-run
triplet full_package_id "$REMAINING" 0 \
    -p "path+file://$REMAINING/crates/nested/member-a#remaining-main@0.1.0" --lib --no-run

WORKSPACE_FORCE_FAIL=1 triplet fail_fast "$FIXTURE" 101 --workspace --lib -- --test-threads=1 --nocapture
WORKSPACE_FORCE_FAIL=1 triplet no_fail_fast "$FIXTURE" 101 --workspace --lib --no-fail-fast -- --test-threads=1 --nocapture
for leg in native compat self; do
    if rg -q 'WORKSPACE_(APP|TOOL)' "$TMP/fail_fast.$leg.out"; then
        bad "fail_fast $leg continued after first failed package"
    else
        ok "fail_fast $leg stopped"
    fi
    if rg -q 'WORKSPACE_APP' "$TMP/no_fail_fast.$leg.out" \
        && rg -q 'WORKSPACE_TOOL' "$TMP/no_fail_fast.$leg.out"; then
        ok "no_fail_fast $leg continued other members"
    else
        bad "no_fail_fast $leg did not continue other members"
    fi
done

if [ -e "$MIRVM_CARGO_SENTINEL" ]; then
    bad "self leg launched cargo"
else
    ok "self leg zero cargo"
fi
PATH="$TMP/no-cargo:$PATH" MIRVM_HOME="$TMP/self-home" MIRVM_SYSROOT="$CONTRACT_SYSROOT" \
    MIRVM_DEPS=self "$STRACE" -f -qq -e trace=execve -o "$TMP/self.execve" \
    "$MIRVM" test "$FIXTURE" --locked --offline --workspace --lib --no-run \
    >"$TMP/trace.out" 2>"$TMP/trace.err"
trace_code=$?
if [ "$trace_code" != 0 ]; then
    bad "self execve audit run failed: $trace_code"
elif rg -q 'execve\("[^"]*/cargo",' "$TMP/self.execve"; then
    bad "self leg launched cargo via absolute path"
else
    ok "self leg execve zero cargo"
fi

FRESH="$TMP/fresh-workspace"
cp -R "$FIXTURE" "$FRESH"
unlink "$FRESH/Cargo.lock"
PATH="$TMP/no-cargo:$PATH" MIRVM_HOME="$TMP/fresh-home" MIRVM_SYSROOT="$CONTRACT_SYSROOT" \
    MIRVM_DEPS=self "$MIRVM" test "$FRESH" --offline -p workspace-app --lib --no-run \
    --features extra \
    >"$TMP/fresh.self.out" 2>"$TMP/fresh.self.err"
fresh_code=$?
if [ "$fresh_code" != 0 ] || [ ! -f "$FRESH/Cargo.lock" ]; then
    bad "self lockless workspace did not produce a usable Cargo.lock"
    tail -20 "$TMP/fresh.self.err"
elif ! CARGO_TARGET_DIR="$TMP/fresh-native-target" "$CARGO" test \
    --manifest-path "$FRESH/Cargo.toml" --locked --offline -p workspace-app --lib --no-run \
    --features extra \
    >"$TMP/fresh.native.out" 2>"$TMP/fresh.native.err"; then
    bad "self-generated workspace Cargo.lock not accepted by pinned Cargo"
    tail -20 "$TMP/fresh.native.err"
else
    ok "self lockless workspace produced a unified lock acceptable to pinned Cargo"
fi

for leg in native compat self; do
    target="$TMP/$leg-target"
    [ "$leg" = self ] && target="$TMP/self-home/target/cargoless"
    mapfile -t counts < <(find "$target" -name workspace-build-count -type f -exec awk '{print}' {} \;)
    bad_count=0
    for count in "${counts[@]}"; do
        [ "$count" = 1 ] || bad_count=1
    done
    if [ "${#counts[@]}" -gt 0 ] && [ "$bad_count" = 0 ]; then
        ok "$leg workspace build.rs executed once per compile key"
    else
        bad "$leg workspace build.rs rerun anomaly: ${counts[*]:-<missing>}"
    fi
done

CARGO_TARGET_DIR="$TMP/oracle-target" "$CARGO" test --manifest-path "$FIXTURE/Cargo.toml" \
    --locked --offline --workspace --lib --no-run -vv \
    >"$TMP/oracle.out" 2>"$TMP/oracle.err"
oracle="$TMP/oracle.err"
shared_line=$(rg 'shared/src/lib\.rs .*--crate-type lib' "$oracle" | head -1)
app_test=$(rg 'app/src/lib\.rs .*--test' "$oracle" | head -1)
tool_test=$(rg 'tool/src/lib\.rs .*--test' "$oracle" | head -1)
if [[ "$shared_line" == *'feature="app-side"'* ]] \
    && [[ "$shared_line" == *'feature="base"'* ]] \
    && [[ "$shared_line" == *'feature="default"'* ]] \
    && [[ "$shared_line" == *'feature="tool-side"'* ]] \
    && [[ "$app_test" == *'CARGO_PKG_AUTHORS='* ]] \
    && [[ "$app_test" == *'--extern test_helper='* ]] \
    && [[ "$tool_test" == *'--extern workspace_bridge='* ]]; then
    ok "pinned Cargo workspace rustc layout"
else
    bad "pinned Cargo workspace rustc layout drifted"
    tail -30 "$oracle"
fi


CARGO_TARGET_DIR="$TMP/oracle-remaining-target" "$CARGO" test \
    --manifest-path "$REMAINING/Cargo.toml" --locked --offline \
    -p remaining-main@0.1.0 --lib --no-run -vv \
    >"$TMP/oracle-remaining.out" 2>"$TMP/oracle-remaining.err"
remaining_oracle="$TMP/oracle-remaining.err"
remaining_shared=$(rg 'shared/src/lib\.rs .*--crate-type lib' "$remaining_oracle" | head -1)
remaining_root=$(rg 'member-a/src/lib\.rs .*--test' "$remaining_oracle" | head -1)
if [[ "$remaining_shared" == *'feature="build"'* ]] \
    && [[ "$remaining_shared" == *'feature="normal"'* ]] \
    && [[ "$remaining_root" == *'--warn=unexpected_cfgs'* ]] \
    && [[ "$remaining_root" == *'cfg(cless_workspace_lint)'* ]]; then
    ok "pinned Cargo resolver 1 and workspace lint shape"
else
    bad "pinned Cargo resolver 1 and workspace lint shape drift"
    tail -30 "$remaining_oracle"
fi
}
