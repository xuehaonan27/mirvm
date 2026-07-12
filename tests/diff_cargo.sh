#!/usr/bin/env bash
# cargo 模式差分：frontmatter 脚本 / cargo 项目，native cargo run vs mirvm 对拍。
# native 是 oracle，必须先达到各 fixture 明示的退出码；“双方同样构建失败”不是 PASS。
# M5.1 后 ecosystem/ffi_zlib/project 均须与 native 一致。保留可注入的 expected-red
# 模式只用于门禁自身回归，以及未来滚动前沿时锁定原因/XPASS 行为。
set -u
cd "$(dirname "$0")/.."
MIRVM=${MIRVM:-$(pwd)/target/debug/mirvm}
CARGO=${CARGO:-$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo}
SCRIPT_CACHE=${SCRIPT_CACHE:-${XDG_CACHE_HOME:-$HOME/.cache}/mirvm/scripts}
pass=0 xfail=0 fail=0

show_diff() {
    local native_out="$1" mirvm_out="$2"
    diff "$native_out" "$mirvm_out" | head -10
}

check_green() {
    local name="$1" native_out="$2" mirvm_out="$3" native_code="$4" mirvm_code="$5"
    local expected_native_code="$6"
    if [ "$native_code" != "$expected_native_code" ]; then
        echo "FAIL $name (native baseline exit=$native_code, want=$expected_native_code)"
        fail=$((fail+1))
    elif diff -q "$native_out" "$mirvm_out" >/dev/null && [ "$native_code" = "$mirvm_code" ]; then
        echo "PASS $name"; pass=$((pass+1))
    else
        echo "FAIL $name (native=$native_code mirvm=$mirvm_code)"
        show_diff "$native_out" "$mirvm_out"
        fail=$((fail+1))
    fi
}

check_expected_red() {
    local name="$1" native_out="$2" mirvm_out="$3" mirvm_err="$4"
    local native_code="$5" mirvm_code="$6" expected_native_code="$7"
    local expected_mirvm_code="$8" diagnostic="$9"
    if [ "$native_code" != "$expected_native_code" ]; then
        echo "FAIL $name (native baseline exit=$native_code, want=$expected_native_code)"
        fail=$((fail+1))
    elif [ "$mirvm_code" = "$native_code" ] && diff -q "$native_out" "$mirvm_out" >/dev/null; then
        echo "XPASS $name (remove/update expected-red frontier)"
        fail=$((fail+1))
    elif [ "$mirvm_code" = "$expected_mirvm_code" ] \
        && grep -Fq "$diagnostic" "$mirvm_err"; then
        echo "XFAIL $name ($diagnostic; M5.1 expected red)"
        xfail=$((xfail+1))
    else
        echo "FAIL $name (expected mirvm exit=$expected_mirvm_code + '$diagnostic', native=$native_code mirvm=$mirvm_code)"
        show_diff "$native_out" "$mirvm_out"
        tail -3 "$mirvm_err"
        fail=$((fail+1))
    fi
}

TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT

# 定位 frontmatter 脚本物化出的项目目录（按 Cargo.toml 里的 crate 名匹配）
script_dir() {
    local stem="$1"
    grep -l "name = \"$stem\"" "$SCRIPT_CACHE"/*/Cargo.toml 2>/dev/null |
        head -1 | xargs -r dirname
}

# frontmatter 脚本：mirvm 物化的项目目录直接给 native cargo 用
diff_script() {
    local name="$1" src="$2" stem="$3" mode="$4" expected_native_code="$5"
    local expected_mirvm_code="${6:-}" diagnostic="${7:-}"
    "$MIRVM" run "$src" >"$TMP/$name.mirvm" 2>"$TMP/$name.mirvm.err"; local mc=$?
    local D; D=$(script_dir "$stem")
    if [ -z "$D" ]; then echo "FAIL $name (未找到物化目录)"; fail=$((fail+1)); return; fi
    (cd "$D" && "$CARGO" run -q >"$TMP/$name.native" 2>"$TMP/$name.native.err"); local nc=$?
    if [ "$mode" = xfail ]; then
        check_expected_red "$name" "$TMP/$name.native" "$TMP/$name.mirvm" \
            "$TMP/$name.mirvm.err" "$nc" "$mc" "$expected_native_code" \
            "$expected_mirvm_code" "$diagnostic"
    else
        check_green "$name" "$TMP/$name.native" "$TMP/$name.mirvm" \
            "$nc" "$mc" "$expected_native_code"
    fi
}

if [ -n "${ECOSYSTEM_XFAIL_DIAGNOSTIC:-}" ]; then
    diff_script ecosystem demo/ecosystem.rs ecosystem xfail 0 70 \
        "$ECOSYSTEM_XFAIL_DIAGNOSTIC"
else
    diff_script ecosystem demo/ecosystem.rs ecosystem green 0
fi
diff_script ffi_zlib demo/ffi_zlib.rs ffi_zlib green 0

# 2) cargo 项目模式
PROJ="$TMP/proj"; mkdir -p "$PROJ/src"
cat > "$PROJ/Cargo.toml" <<'EOF'
[package]
name = "diffproj"
version = "0.1.0"
edition = "2024"
[dependencies]
serde_json = "1"
EOF
cat > "$PROJ/src/main.rs" <<'EOF'
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let v: serde_json::Value = serde_json::json!({"args": args.len()});
    println!("{v} extra={:?}", &args[1..]);
    std::process::exit(7);
}
EOF
"$MIRVM" run "$PROJ" -- x y >"$TMP/proj.mirvm" 2>"$TMP/proj.mirvm.err"; mc=$?
(cd "$PROJ" && "$CARGO" run -q -- x y >"$TMP/proj.native" 2>"$TMP/proj.native.err"); nc=$?
check_green project "$TMP/proj.native" "$TMP/proj.mirvm" "$nc" "$mc" 7

echo "== $pass passed, $xfail expected-red, $fail failed =="
[ $fail = 0 ]
