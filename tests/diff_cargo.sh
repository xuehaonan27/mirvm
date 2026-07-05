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

# 1) frontmatter 脚本：mirvm 物化的项目目录直接给 native cargo 用
"$MIRVM" run demo/ecosystem.rs >"$TMP/eco.mirvm" 2>/dev/null; mc=$?
D=$(ls -d ~/.cache/mirvm/scripts/*/ | head -1)
(cd "$D" && "$CARGO" run -q >"$TMP/eco.native" 2>/dev/null); nc=$?
check ecosystem "$TMP/eco.native" "$TMP/eco.mirvm" "$nc" "$mc"

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
