#!/usr/bin/env bash
# pack 双轨合同：缺省 cargoless 零 Cargo，显式回退仍由固定 Cargo 驱动。
set -u
. "$(dirname "${BASH_SOURCE[0]}")/../../support/harness.sh"
test_enter_repo

MIRVM=${MIRVM:-$(pwd)/target/debug/mirvm}
CARGO=${CARGO:-$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo}
RUSTC=${RUSTC:-$(dirname "$CARGO")/rustc}
STRACE=${STRACE:-$(command -v strace)}
CONTRACT_HOME=${MIRVM_CONTRACT_HOME:-${MIRVM_HOME:-$HOME/.mirvm}}
[ -x "$MIRVM" ] || { echo "pack_contract: $MIRVM 不存在" >&2; exit 69; }
[ -x "$CARGO" ] || { echo "pack_contract: pinned Cargo 不存在" >&2; exit 69; }
[ -x "$STRACE" ] || { echo "pack_contract: strace 不存在" >&2; exit 69; }
ensure_test_sysroot "$MIRVM" "$CONTRACT_HOME" "$RUSTC" || exit $?
CONTRACT_SYSROOT=$TEST_SYSROOT

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
APP="$TMP/app"
DEP="$TMP/dep"
NO_CARGO="$TMP/no-cargo"
SELF_HOME="$TMP/self-home"
CARGO_HOME_PACK="$TMP/cargo-home"
mkdir -p "$APP/src" "$DEP/src" "$NO_CARGO" "$CARGO_HOME_PACK"
cat >"$DEP/Cargo.toml" <<'EOF'
[package]
name = "pack-dep"
version = "0.1.0"
edition = "2021"
EOF
printf 'pub fn value() -> usize { 321 }\n' >"$DEP/src/lib.rs"
cat >"$APP/Cargo.toml" <<'EOF'
[package]
name = "pack-contract"
version = "0.1.0"
edition = "2021"

[dependencies]
pack-dep = { path = "../dep" }
EOF
printf 'fn main() { println!("{}", pack_dep::value()); }\n' >"$APP/src/main.rs"
printf '#!/bin/sh\n: >"$MIRVM_CARGO_SENTINEL"\nexit 97\n' >"$NO_CARGO/cargo"
chmod +x "$NO_CARGO/cargo"
export MIRVM_CARGO_SENTINEL="$TMP/cargo-was-invoked"

SELF_PACKAGE="$TMP/self.mirvm"
PATH="$NO_CARGO:$PATH" MIRVM_HOME="$SELF_HOME" MIRVM_SYSROOT="$CONTRACT_SYSROOT" \
    "$STRACE" -f -qq -e trace=execve -o "$TMP/self.execve" \
    "$MIRVM" pack "$APP" -o "$SELF_PACKAGE" >"$TMP/self-pack.out" 2>"$TMP/self-pack.err"
self_code=$?
if [ "$self_code" -eq 0 ] && [ -s "$SELF_PACKAGE" ]; then
    ok "pack 缺省 self 生成包"
else
    bad "pack 缺省 self 失败: exit=$self_code"
    tail -20 "$TMP/self-pack.err"
fi
if [ -e "$MIRVM_CARGO_SENTINEL" ] || rg -q 'execve\("[^"]*/cargo"' "$TMP/self.execve"; then
    bad "pack 缺省路径启动了 Cargo"
else
    ok "pack 缺省路径 execve 零 Cargo"
fi
self_output=$(MIRVM_HOME="$TMP/fresh-run-home" MIRVM_SYSROOT="$CONTRACT_SYSROOT" \
    "$MIRVM" run "$SELF_PACKAGE" 2>"$TMP/self-run.err")
if [ "$?" -eq 0 ] && [ "$self_output" = 321 ]; then
    ok "self 包在全新 MIRVM_HOME 自包含运行"
else
    bad "self 包运行失败: output=$self_output"
    tail -20 "$TMP/self-run.err"
fi
mapfile -t heat_files < <(find "$TMP/fresh-run-home/package-heat" -type f -name '*.order' 2>/dev/null)
if [ "${#heat_files[@]}" = 1 ] && [ -s "${heat_files[0]}" ]; then
    ok "包首次运行记录真实函数热序"
else
    bad "包首次运行未生成唯一的函数热序记录"
fi
second_output=$(MIRVM_HOME="$TMP/fresh-run-home" MIRVM_SYSROOT="$CONTRACT_SYSROOT" \
    "$MIRVM" run "$SELF_PACKAGE" 2>"$TMP/self-run-second.err")
if [ "$?" -eq 0 ] && [ "$second_output" = 321 ]; then
    ok "包按预测热序预取后二跑结果一致"
else
    bad "包预测预取后二跑失败: output=$second_output"
    tail -20 "$TMP/self-run-second.err"
fi

CARGO_PACKAGE="$TMP/cargo.mirvm"
MIRVM_HOME="$CONTRACT_HOME" MIRVM_SYSROOT="$CONTRACT_SYSROOT" \
    MIRVM_TARGET_DIR="$TMP/cargo-target" CARGO_HOME="$CARGO_HOME_PACK" MIRVM_DEPS=cargo \
    "$STRACE" -f -qq -e trace=execve -o "$TMP/cargo.execve" \
    "$MIRVM" pack "$APP" -o "$CARGO_PACKAGE" >"$TMP/cargo-pack.out" 2>"$TMP/cargo-pack.err"
cargo_code=$?
if [ "$cargo_code" -eq 0 ] && [ -s "$CARGO_PACKAGE" ] \
    && rg -q 'execve\("[^"]*/cargo"' "$TMP/cargo.execve"; then
    ok "MIRVM_DEPS=cargo 保留固定 Cargo 回退"
else
    bad "Cargo 回退 pack 未成立: exit=$cargo_code"
    tail -20 "$TMP/cargo-pack.err"
fi
cargo_output=$(MIRVM_HOME="$TMP/fresh-cargo-run-home" MIRVM_SYSROOT="$CONTRACT_SYSROOT" \
    "$MIRVM" run "$CARGO_PACKAGE" 2>"$TMP/cargo-run.err")
if [ "$?" -eq 0 ] && [ "$cargo_output" = 321 ]; then
    ok "Cargo 回退包在全新 MIRVM_HOME 自包含运行"
else
    bad "Cargo 回退包运行失败: output=$cargo_output"
    tail -20 "$TMP/cargo-run.err"
fi

if MIRVM_DEPS=invalid "$MIRVM" pack "$APP" -o "$TMP/invalid.mirvm" \
    >"$TMP/invalid.out" 2>"$TMP/invalid.err"; then
    bad "pack 接受了非法 MIRVM_DEPS"
elif rg -q 'only accepts `cargo` or `self`' "$TMP/invalid.err"; then
    ok "pack 非法 MIRVM_DEPS 明确拒绝"
else
    bad "pack 非法 MIRVM_DEPS 诊断不明确"
fi

suite_summary contracts.pack
