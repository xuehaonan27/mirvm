#!/usr/bin/env bash
# Git 依赖合同：固定 Cargo 是格式裁判，self 负责获取、锁定、离线与双提交图。
set -u
. "$(dirname "${BASH_SOURCE[0]}")/../../support/harness.sh"
test_enter_repo

MIRVM=${MIRVM:-$(pwd)/target/debug/mirvm}
CARGO=${CARGO:-$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo}
RUSTC=${RUSTC:-$(dirname "$CARGO")/rustc}
STRACE=${STRACE:-$(command -v strace)}
CONTRACT_HOME=${MIRVM_CONTRACT_HOME:-${MIRVM_HOME:-$HOME/.mirvm}}
HOST=$($RUSTC -vV | sed -n 's/^host: //p')

[ -x "$MIRVM" ] || { echo "cargoless_git_contract: $MIRVM 不存在" >&2; exit 69; }
[ -x "$CARGO" ] || { echo "cargoless_git_contract: pinned Cargo 不存在" >&2; exit 69; }
[ -x "$STRACE" ] || { echo "cargoless_git_contract: strace 不存在" >&2; exit 69; }
ensure_test_sysroot "$MIRVM" "$CONTRACT_HOME" "$RUSTC" || exit $?
CONTRACT_SYSROOT=$TEST_SYSROOT
case "$($CARGO --version)" in
    "cargo 1.98.0-nightly "*) ;;
    *) echo "ERROR cargoless_git: Cargo 版本不在合同内" >&2; exit 69 ;;
esac

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
REPO="$TMP/repo"
SELF_HOME="$TMP/self-home"
mkdir -p "$REPO/core/src" "$REPO/helper/src" "$TMP/no-cargo"
printf '#!/bin/sh\n: >"$MIRVM_CARGO_SENTINEL"\nexit 97\n' >"$TMP/no-cargo/cargo"
chmod +x "$TMP/no-cargo/cargo"
export MIRVM_CARGO_SENTINEL="$TMP/cargo-was-invoked"
cat >"$REPO/Cargo.toml" <<'EOF'
[workspace]
members = ["core", "helper"]
resolver = "2"
[workspace.package]
edition = "2021"
EOF
cat >"$REPO/core/Cargo.toml" <<'EOF'
[package]
name = "git-core"
version = "1.2.3"
edition.workspace = true
[features]
special = []
[dependencies]
git-helper = { path = "../helper" }
EOF
cat >"$REPO/core/src/lib.rs" <<'EOF'
pub fn value() -> usize {
    git_helper::base() + if cfg!(feature = "special") { 10 } else { 0 }
}
EOF
cat >"$REPO/helper/Cargo.toml" <<'EOF'
[package]
name = "git-helper"
version = "0.4.0"
edition.workspace = true
EOF
printf 'pub fn base() -> usize { 7 }\n' >"$REPO/helper/src/lib.rs"
git -C "$REPO" init -q -b main
git -C "$REPO" config user.name "mirvm contract"
git -C "$REPO" config user.email "mirvm@example.invalid"
git -C "$REPO" add .
git -C "$REPO" commit -q -m old
OLD=$(git -C "$REPO" rev-parse HEAD)
git -C "$REPO" tag v1
printf 'pub fn base() -> usize { 8 }\n' >"$REPO/helper/src/lib.rs"
git -C "$REPO" add .
git -C "$REPO" commit -q -m middle
MIDDLE=$(git -C "$REPO" rev-parse HEAD)

make_app() {
    local dir=$1 name=$2 dependencies=$3 body=$4
    mkdir -p "$dir/src"
    printf '[package]\nname = "%s"\nversion = "0.1.0"\nedition = "2021"\n\n[dependencies]\n' \
        "$name" >"$dir/Cargo.toml"
    printf '%b\n' "$dependencies" >>"$dir/Cargo.toml"
    printf '%s\n' "$body" >"$dir/src/main.rs"
}

URL="file://$REPO"
APP="$TMP/app"
make_app "$APP" git-contract-app \
    "chosen = { package = \"git-core\", git = \"$URL\", branch = \"main\", version = \"^1\", features = [\"special\"] }" \
    'fn main() { println!("{}", chosen::value()); }'

run_self() {
    PATH="$TMP/no-cargo:$PATH" MIRVM_HOME="$SELF_HOME" MIRVM_SYSROOT="$CONTRACT_SYSROOT" \
        MIRVM_DEPS=self "$MIRVM" run "$@"
}

first=$(run_self "$APP" 2>"$TMP/first.err")
if [ "$?" = 0 ] && [ "$first" = 18 ] && rg -q "#$MIDDLE\"" "$APP/Cargo.lock"; then
    ok "fresh branch 锁定当前 commit"
else
    bad "fresh branch 结果或 lock 错误: output=$first"
    tail -20 "$TMP/first.err"
fi
if RUSTC="$RUSTC" CARGO_HOME="$TMP/cargo-home" CARGO_TARGET_DIR="$TMP/cargo-target" \
    "$CARGO" check --manifest-path "$APP/Cargo.toml" --locked -q; then
    ok "self lock 被 pinned Cargo 接受"
else
    bad "self lock 未被 pinned Cargo 接受"
fi

printf 'pub fn base() -> usize { 9 }\n' >"$REPO/helper/src/lib.rs"
git -C "$REPO" add .
git -C "$REPO" commit -q -m new
NEW=$(git -C "$REPO" rev-parse HEAD)
locked=$(run_self "$APP" 2>"$TMP/locked.err")
offline=$(MIRVM_OFFLINE=1 run_self "$APP" 2>"$TMP/offline.err")
if [ "$locked" = 18 ] && [ "$offline" = 18 ] && rg -q "#$MIDDLE\"" "$APP/Cargo.lock"; then
    ok "分支移动后 locked 在线/离线均不漂移"
else
    bad "locked commit 漂移: online=$locked offline=$offline"
fi

FRESH="$TMP/fresh"
make_app "$FRESH" git-contract-fresh \
    "chosen = { package = \"git-core\", git = \"$URL\", branch = \"main\", version = \"^1\", features = [\"special\"] }" \
    'fn main() { println!("{}", chosen::value()); }'
fresh=$(run_self "$FRESH" 2>"$TMP/fresh.err")
if [ "$fresh" = 19 ] && rg -q "#$NEW\"" "$FRESH/Cargo.lock"; then
    ok "无锁 fresh 才跟随新 commit"
else
    bad "无锁 fresh 未跟随新 commit: output=$fresh"
fi

if MIRVM_HOME="$TMP/cold-home" MIRVM_SYSROOT="$CONTRACT_SYSROOT" MIRVM_OFFLINE=1 \
    MIRVM_DEPS=self "$MIRVM" run "$APP" >"$TMP/cold.out" 2>"$TMP/cold.err"; then
    bad "冷缓存 offline 意外成功"
elif rg -q 'locked commit .* 不在本地缓存' "$TMP/cold.err"; then
    ok "冷缓存 offline 响亮失败"
else
    bad "冷缓存 offline 错误不明确"
fi

DUAL="$TMP/dual"
make_app "$DUAL" git-contract-dual \
    "old = { package = \"git-core\", git = \"$URL\", rev = \"$OLD\", version = \"^1\", features = [\"special\"] }\nnew = { package = \"git-core\", git = \"$URL\", rev = \"$NEW\", version = \"^1\", features = [\"special\"] }" \
    'fn main() { println!("{} {}", old::value(), new::value()); }'
dual=$(run_self "$DUAL" 2>"$TMP/dual.err")
dual_offline=$(MIRVM_OFFLINE=1 run_self "$DUAL" 2>"$TMP/dual-offline.err")
if [ "$dual" = "17 19" ] && [ "$dual_offline" = "17 19" ]; then
    ok "同名同版本双 commit 在线/离线共存"
else
    bad "双 commit 图错误: online=$dual offline=$dual_offline"
fi
if RUSTC="$RUSTC" CARGO_HOME="$TMP/cargo-home" CARGO_TARGET_DIR="$TMP/dual-cargo-target" \
    "$CARGO" check --manifest-path "$DUAL/Cargo.toml" --locked -q; then
    ok "双 commit self lock 被 pinned Cargo 接受"
else
    bad "双 commit self lock 未被 pinned Cargo 接受"
fi

PATH="$TMP/no-cargo:$PATH" MIRVM_HOME="$SELF_HOME" MIRVM_SYSROOT="$CONTRACT_SYSROOT" \
    MIRVM_OFFLINE=1 MIRVM_DEPS=self "$STRACE" -f -qq -e trace=execve \
    -o "$TMP/self.execve" "$MIRVM" run "$DUAL" >"$TMP/trace.out" 2>"$TMP/trace.err"
if [ "$?" != 0 ]; then
    bad "self execve 审计运行失败"
elif [ -e "$MIRVM_CARGO_SENTINEL" ] || rg -q 'execve\("[^"]*/cargo",' "$TMP/self.execve"; then
    bad "self Git 路径启动了 Cargo"
else
    ok "self Git 路径 execve 零 Cargo"
fi

CHECKOUT=$(find "$SELF_HOME/registry/git/checkouts" -path "*/$MIDDLE/core/src/lib.rs" -print -quit)
printf 'pub fn value() -> usize { 99 }\n' >"$CHECKOUT"
if run_self "$APP" >"$TMP/tamper.out" 2>"$TMP/tamper.err"; then
    bad "Git checkout 篡改后意外成功"
elif rg -q 'Git checkout 内容被修改' "$TMP/tamper.err"; then
    ok "Git checkout 篡改被拒绝"
else
    bad "Git checkout 篡改错误不明确"
fi

suite_summary contracts.cargoless-git
