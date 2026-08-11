#!/usr/bin/env bash
# Workspace cargoless 合同：只覆盖当前多包产品切片，固定 Cargo 是行为权威。
set -u
. "$(dirname "${BASH_SOURCE[0]}")/../../support/harness.sh"
test_enter_repo

MIRVM=${MIRVM:-$(pwd)/target/debug/mirvm}
CARGO=${CARGO:-$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo}
RUSTC=${RUSTC:-$(dirname "$CARGO")/rustc}
STRACE=${STRACE:-$(command -v strace)}
FIXTURE=$(pwd)/tests/fixtures/cless_workspace_contract
REMAINING=$(pwd)/tests/fixtures/cless_workspace_remaining_contract
CONTRACT_HOME=${MIRVM_CONTRACT_HOME:-${MIRVM_HOME:-$HOME/.mirvm}}
HOST=$($RUSTC -vV | sed -n 's/^host: //p')

[ -x "$MIRVM" ] || { echo "cargoless_workspace_contract: $MIRVM 不存在" >&2; exit 69; }
[ -x "$CARGO" ] || { echo "cargoless_workspace_contract: pinned Cargo 不存在" >&2; exit 69; }
[ -x "$STRACE" ] || { echo "cargoless_workspace_contract: strace 不存在" >&2; exit 69; }
ensure_test_sysroot "$MIRVM" "$CONTRACT_HOME" "$RUSTC" || exit $?
CONTRACT_SYSROOT=$TEST_SYSROOT
case "$($CARGO --version)" in
    "cargo 1.98.0-nightly "*) ;;
    *) echo "ERROR cargoless_workspace: Cargo 版本不在合同内" >&2; exit 69 ;;
esac

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
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
        bad "$name 退出码 native=$nc compat=$cc self=$sc，期望 $expected"
        tail -20 "$TMP/$name.self.err"
    elif ! diff -q "$TMP/$name.native.norm.out" "$TMP/$name.compat.norm.out" >/dev/null \
        || ! diff -q "$TMP/$name.native.norm.out" "$TMP/$name.self.norm.out" >/dev/null; then
        bad "$name stdout 不一致"
        diff -u "$TMP/$name.native.norm.out" "$TMP/$name.self.norm.out" | head -60
    elif ! diff -q "$TMP/$name.native.norm.err" "$TMP/$name.compat.norm.err" >/dev/null \
        || ! diff -q "$TMP/$name.native.norm.err" "$TMP/$name.self.norm.err" >/dev/null; then
        bad "$name stderr 不一致"
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
        bad "fail_fast $leg 在首个失败包后仍继续"
    else
        ok "fail_fast $leg 停止"
    fi
    if rg -q 'WORKSPACE_APP' "$TMP/no_fail_fast.$leg.out" \
        && rg -q 'WORKSPACE_TOOL' "$TMP/no_fail_fast.$leg.out"; then
        ok "no_fail_fast $leg 继续其他成员"
    else
        bad "no_fail_fast $leg 未继续其他成员"
    fi
done

if [ -e "$MIRVM_CARGO_SENTINEL" ]; then
    bad "self 腿启动了 cargo"
else
    ok "self 腿零 cargo"
fi
PATH="$TMP/no-cargo:$PATH" MIRVM_HOME="$TMP/self-home" MIRVM_SYSROOT="$CONTRACT_SYSROOT" \
    MIRVM_DEPS=self "$STRACE" -f -qq -e trace=execve -o "$TMP/self.execve" \
    "$MIRVM" test "$FIXTURE" --locked --offline --workspace --lib --no-run \
    >"$TMP/trace.out" 2>"$TMP/trace.err"
trace_code=$?
if [ "$trace_code" != 0 ]; then
    bad "self execve 审计运行失败: $trace_code"
elif rg -q 'execve\("[^"]*/cargo",' "$TMP/self.execve"; then
    bad "self 腿通过绝对路径启动了 cargo"
else
    ok "self 腿 execve 零 cargo"
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
    bad "self 无锁 workspace 未生成可用 Cargo.lock"
    tail -20 "$TMP/fresh.self.err"
elif ! CARGO_TARGET_DIR="$TMP/fresh-native-target" "$CARGO" test \
    --manifest-path "$FRESH/Cargo.toml" --locked --offline -p workspace-app --lib --no-run \
    --features extra \
    >"$TMP/fresh.native.out" 2>"$TMP/fresh.native.err"; then
    bad "self 生成的 workspace Cargo.lock 未被 pinned Cargo 接受"
    tail -20 "$TMP/fresh.native.err"
else
    ok "self 无锁 workspace 生成 pinned Cargo 可接受的统一 lock"
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
        ok "$leg workspace build.rs 每个编译键只执行一次"
    else
        bad "$leg workspace build.rs 重跑异常: ${counts[*]:-<missing>}"
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
    ok "pinned Cargo workspace rustc 结构"
else
    bad "pinned Cargo workspace rustc 结构漂移"
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
    ok "pinned Cargo resolver 1 与 workspace lint 形状"
else
    bad "pinned Cargo resolver 1 与 workspace lint 形状漂移"
    tail -30 "$remaining_oracle"
fi

suite_summary contracts.cargoless-workspace
