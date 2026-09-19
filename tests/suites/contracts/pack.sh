#!/usr/bin/env bash
# pack dual-track contract: default is cargoless zero-Cargo, explicit fallback still driven by pinned Cargo.
set -u
. "$(dirname "${BASH_SOURCE[0]}")/../../support/harness.sh"
suite_init
STRACE=${STRACE:-$(command -v strace)}
CONTRACT_HOME=${MIRVM_CONTRACT_HOME:-${MIRVM_HOME:-$HOME/.mirvm}}
require_executable strace "$STRACE" || exit $?
ensure_test_sysroot "$MIRVM" "$CONTRACT_HOME" "$RUSTC" || exit $?
CONTRACT_SYSROOT=$TEST_SYSROOT
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
    ok "pack default self produced package"
else
    bad "pack default self failed: exit=$self_code"
    tail -20 "$TMP/self-pack.err"
fi
if [ -e "$MIRVM_CARGO_SENTINEL" ] || rg -q 'execve\("[^"]*/cargo"' "$TMP/self.execve"; then
    bad "pack default path launched Cargo"
else
    ok "pack default path execve zero Cargo"
fi
self_output=$(MIRVM_HOME="$TMP/fresh-run-home" MIRVM_SYSROOT="$CONTRACT_SYSROOT" \
    "$MIRVM" run "$SELF_PACKAGE" 2>"$TMP/self-run.err")
if [ "$?" -eq 0 ] && [ "$self_output" = 321 ]; then
    ok "self package self-contained run in fresh MIRVM_HOME"
else
    bad "self package run failed: output=$self_output"
    tail -20 "$TMP/self-run.err"
fi
mapfile -t heat_files < <(find "$TMP/fresh-run-home/package-heat" -type f -name '*.order' 2>/dev/null)
if [ "${#heat_files[@]}" = 1 ] && [ -s "${heat_files[0]}" ]; then
    ok "package recorded real function heat order on first run"
else
    bad "package first run did not produce a unique function heat record"
fi
second_output=$(MIRVM_HOME="$TMP/fresh-run-home" MIRVM_SYSROOT="$CONTRACT_SYSROOT" \
    "$MIRVM" run "$SELF_PACKAGE" 2>"$TMP/self-run-second.err")
if [ "$?" -eq 0 ] && [ "$second_output" = 321 ]; then
    ok "package second run consistent after predicted heat prefetch"
else
    bad "package predicted prefetch second run failed: output=$second_output"
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
    bad "public Package embed probe compilation failed"
    tail -20 "$TMP/embed-build.err"
else
    embed_output=$(MIRVM_HOME="$TMP/embed-home" "$EMBED" "$SELF_PACKAGE" \
        2>"$TMP/embed-run.err")
    embed_code=$?
    if [ "$embed_code" -eq 0 ] \
        && [ "$embed_output" = "unique=true ctor=1,1 direct=2,2 bridge=3,3 c2=4,4 live=5 fresh_ctor=1 fresh=2 closed=true aba=true fini=true" ]; then
        ok "public Package dual-engine construction/destruction/P1/global_asm/C2/post-close ABA contract"
    else
        bad "public Package dual-engine result wrong: exit=$embed_code output=$embed_output"
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
    ok "MIRVM_DEPS=cargo keeps pinned Cargo fallback"
else
    bad "Cargo fallback pack did not hold: exit=$cargo_code"
    tail -20 "$TMP/cargo-pack.err"
fi
cargo_output=$(MIRVM_HOME="$TMP/fresh-cargo-run-home" MIRVM_SYSROOT="$CONTRACT_SYSROOT" \
    "$MIRVM" run "$CARGO_PACKAGE" 2>"$TMP/cargo-run.err")
if [ "$?" -eq 0 ] && [ "$cargo_output" = 321 ]; then
    ok "Cargo fallback package self-contained run in fresh MIRVM_HOME"
else
    bad "Cargo fallback package run failed: output=$cargo_output"
    tail -20 "$TMP/cargo-run.err"
fi

if MIRVM_DEPS=invalid "$MIRVM" pack "$APP" -o "$TMP/invalid.mirvm" \
    >"$TMP/invalid.out" 2>"$TMP/invalid.err"; then
    bad "pack accepted invalid MIRVM_DEPS"
elif rg -q 'only accepts `cargo` or `self`' "$TMP/invalid.err"; then
    ok "pack invalid MIRVM_DEPS rejected cleanly"
else
    bad "pack invalid MIRVM_DEPS diagnosis unclear"
fi

suite_summary contracts.pack
