#!/usr/bin/env bash
# cargo 模式差分：frontmatter 脚本 / cargo 项目，native cargo run vs mirvm 对拍。
set -u
cd "$(dirname "$0")/.."
MIRVM=${MIRVM:-$(pwd)/target/debug/mirvm}
CARGO=~/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo
pass=0 fail=0

check() {
    local name="$1" native_out="$2" mirvm_out="$3" native_code="$4" mirvm_code="$5"
    if diff -q "$native_out" "$mirvm_out" >/dev/null && [ "$native_code" = "$mirvm_code" ]; then
        echo "PASS $name"; pass=$((pass+1))
    else
        echo "FAIL $name (native=$native_code mirvm=$mirvm_code)"
        diff "$native_out" "$mirvm_out" | head -10
        fail=$((fail+1))
    fi
}

TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT

# 定位 frontmatter 脚本物化出的项目目录（按 Cargo.toml 里的 crate 名匹配）
script_dir() {
    local stem="$1"
    grep -l "name = \"$stem\"" ~/.cache/mirvm/scripts/*/Cargo.toml 2>/dev/null |
        head -1 | xargs -r dirname
}

# frontmatter 脚本：mirvm 物化的项目目录直接给 native cargo 用
diff_script() {
    local name="$1" src="$2" stem="$3"
    "$MIRVM" run "$src" >"$TMP/$name.mirvm" 2>/dev/null; local mc=$?
    local D; D=$(script_dir "$stem")
    if [ -z "$D" ]; then echo "FAIL $name (未找到物化目录)"; fail=$((fail+1)); return; fi
    (cd "$D" && "$CARGO" run -q >"$TMP/$name.native" 2>/dev/null); local nc=$?
    check "$name" "$TMP/$name.native" "$TMP/$name.mirvm" "$nc" "$mc"
}

diff_script ecosystem demo/ecosystem.rs ecosystem
diff_script ffi_zlib demo/ffi_zlib.rs ffi_zlib

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
"$MIRVM" run "$PROJ" -- x y >"$TMP/proj.mirvm" 2>/dev/null; mc=$?
(cd "$PROJ" && "$CARGO" run -q -- x y >"$TMP/proj.native" 2>/dev/null); nc=$?
check project "$TMP/proj.native" "$TMP/proj.mirvm" "$nc" "$mc"

echo "== $pass passed, $fail failed =="
[ $fail = 0 ]
