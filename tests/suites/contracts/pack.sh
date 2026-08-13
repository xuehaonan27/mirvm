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
build = "build.rs"

[dependencies]
pack-dep = { path = "../dep" }
EOF
cat >"$APP/build.rs" <<'EOF'
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let root = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let object = out.join("c2_bridge.o");
    let archive = out.join("libc2_bridge.a");
    assert!(Command::new("cc")
        .args(["-fPIC", "-c"])
        .arg(root.join("c2_bridge.c"))
        .arg("-o")
        .arg(&object)
        .status()
        .unwrap()
        .success());
    assert!(Command::new("ar")
        .arg("crs")
        .arg(&archive)
        .arg(&object)
        .status()
        .unwrap()
        .success());
    println!("cargo::rerun-if-changed=c2_bridge.c");
    println!("cargo::rustc-link-search=native={}", out.display());
    println!("cargo::rustc-link-lib=static=c2_bridge");
}
EOF
cat >"$APP/c2_bridge.c" <<'EOF'
#include <fcntl.h>
#include <stdint.h>
#include <stdlib.h>
#include <unistd.h>
extern uint64_t callback(void);
static uint64_t ctor_value;
__attribute__((constructor)) static void c2_init(void) { ctor_value = callback(); }
__attribute__((destructor)) static void c2_fini(void) {
    const char *path = getenv("MIRVM_FINI_LOG");
    if (!path) return;
    int fd = open(path, O_WRONLY | O_CREAT | O_APPEND, 0600);
    if (fd < 0) return;
    char line[2] = {(char)('0' + ctor_value), '\n'};
    (void)write(fd, line, sizeof(line));
    (void)close(fd);
}
uint64_t c2_bridge(void) { return callback(); }
uint64_t c2_ctor_value(void) { return ctor_value; }
EOF
cat >"$APP/src/main.rs" <<'EOF'
use std::arch::global_asm;

static mut COUNTER: u64 = 0;

#[unsafe(no_mangle)]
pub extern "C-unwind" fn callback() -> u64 {
    unsafe {
        COUNTER += 1;
        COUNTER
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn callback_ptr() -> usize { callback as usize }

global_asm!(
    r#"
.globl bridge_callback
.type bridge_callback,@function
bridge_callback:
    sub rsp, 8
    call {callback}
    add rsp, 8
    ret
"#,
    callback = sym callback,
);

unsafe extern "C-unwind" { fn bridge_callback() -> u64; }
unsafe extern "C-unwind" { fn c2_bridge() -> u64; }
unsafe extern "C-unwind" { fn c2_ctor_value() -> u64; }

#[unsafe(no_mangle)]
pub extern "C-unwind" fn through_bridge() -> u64 { unsafe { bridge_callback() } }

#[unsafe(no_mangle)]
pub extern "C-unwind" fn through_c2() -> u64 { unsafe { c2_bridge() } }

#[unsafe(no_mangle)]
pub extern "C-unwind" fn constructor_value() -> u64 { unsafe { c2_ctor_value() } }

fn main() { println!("{}", pack_dep::value()); }
EOF
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

# Public embedding API: source-independent concurrent instances, distinct callback identity,
# per-instance global_asm bridge slots, and stable EngineClosed tombstones without ABA.
MIRVM_DIR=$(cd "$(dirname "$MIRVM")" && pwd)
MIRVM_LIB=${MIRVM_LIB:-$MIRVM_DIR/libmirvm.rlib}
MIRVM_LINK_DEPS="$MIRVM_DIR/deps"
RUSTC_SYSROOT=$("$RUSTC" --print sysroot)
EMBED="$TMP/package-embed"
"$RUSTC" tests/fixtures/package_embed.rs --edition=2024 \
    --extern "mirvm=$MIRVM_LIB" -L "dependency=$MIRVM_LINK_DEPS" \
    -C prefer-dynamic -C "link-arg=-Wl,-rpath,$RUSTC_SYSROOT/lib" \
    -o "$EMBED" >"$TMP/embed-build.out" 2>"$TMP/embed-build.err"
embed_build=$?
if [ "$embed_build" -ne 0 ]; then
    bad "公开 Package 嵌入探针编译失败"
    tail -20 "$TMP/embed-build.err"
else
    embed_output=$(MIRVM_HOME="$TMP/embed-home" "$EMBED" "$SELF_PACKAGE" \
        2>"$TMP/embed-run.err")
    embed_code=$?
    if [ "$embed_code" -eq 0 ] \
        && [ "$embed_output" = "unique=true ctor=1,1 direct=2,2 bridge=3,3 c2=4,4 live=5 fresh_ctor=1 fresh=2 closed=true aba=true fini=true" ]; then
        ok "公开 Package 双 Engine 的构造/析构/P1/global_asm/C2/关闭后 ABA 合同"
    else
        bad "公开 Package 双 Engine 结果错误: exit=$embed_code output=$embed_output"
        tail -20 "$TMP/embed-run.err"
    fi
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
