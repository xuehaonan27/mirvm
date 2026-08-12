#!/usr/bin/env bash
# cargo 模式差分：frontmatter 脚本 / cargo 项目，native cargo run vs mirvm 对拍。
# native 是 oracle，必须先达到各 fixture 明示的退出码；“双方同样构建失败”不是 PASS。
# M5.1 后 ecosystem/ffi_zlib/project 均须与 native 一致；真实项目 TDD 又加入
# ripgrep_regex 与 warning_return。保留可注入的 expected-red 模式只用于门禁自身
# 回归，以及未来滚动前沿时锁定原因/XPASS 行为。
set -u
. "$(dirname "${BASH_SOURCE[0]}")/../../support/harness.sh"
test_enter_repo
MIRVM=${MIRVM:-$(pwd)/target/debug/mirvm}
CARGO=${CARGO:-$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo}
RUSTC=${RUSTC:-$(dirname "$CARGO")/rustc}
RUSTC_APPEND_PROXY=${RUSTC_APPEND_PROXY:-$(pwd)/tests/fixtures/rustc_proxy.sh}
RUSTC_WRAPPER_PROBE=${RUSTC_WRAPPER_PROBE:-$(pwd)/tests/fixtures/rustc_wrapper_probe.sh}
SCRIPT_CACHE=${SCRIPT_CACHE:-${MIRVM_HOME:-$HOME/.mirvm}/scripts}
# 本套件是「cargo 模式差分」专轨（D15 P3 双轨纪律）：恒走 cargo 三阶段
# compat 路径——即便外层（如 gate DEPS=self 全量轮）置了 MIRVM_DEPS=self，
# 也不能让本轨静默翻成 self 路径（那等于 cargo 腿零覆盖）。
export MIRVM_DEPS=cargo
show_diff() {
    local native_out="$1" mirvm_out="$2"
    diff "$native_out" "$mirvm_out" | head -10
}

check_green() {
    local name="$1" native_out="$2" mirvm_out="$3" native_code="$4" mirvm_code="$5"
    local expected_native_code="$6" native_err="${7:-}" mirvm_err="${8:-}"
    if [ "$native_code" != "$expected_native_code" ]; then
        echo "FAIL $name (native baseline exit=$native_code, want=$expected_native_code)"
        [ -z "$native_err" ] || tail -20 "$native_err"
        fail=$((fail+1))
    elif diff -q "$native_out" "$mirvm_out" >/dev/null \
        && diff -q "$native_err" "$mirvm_err" >/dev/null \
        && [ "$native_code" = "$mirvm_code" ]; then
        echo "PASS $name"; pass=$((pass+1))
    else
        echo "FAIL $name (native=$native_code mirvm=$mirvm_code)"
        show_diff "$native_out" "$mirvm_out"
        if ! diff -q "$native_err" "$mirvm_err" >/dev/null; then
            echo "stderr mismatch:"
            show_diff "$native_err" "$mirvm_err"
        fi
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

# P3（M6 片3 调研）：L2 warm 复跑一致性。第二跑应命中 IR 缓存，其 stdout/stderr/
# 退出码必须与首跑（随后对 native 判绿）逐字节一致——防"缓存回放旧语义/快照损伤"
# 假绿；告警类项目（诚实不入缓存）第二跑=冷重演，同样必须一致。程序差分套件的 M6 片2
# 通道在 cargo 形态的对位；此前 runner 缓存路径零 gate 覆盖（环境化石化事故任何
# gate 都抓不到），本维度补上。
check_warm() {
    local name="$1" cold_out="$2" cold_err="$3" cold_code="$4"
    local warm_out="$5" warm_err="$6" warm_code="$7"
    if [ "$cold_code" = "$warm_code" ] \
        && diff -q "$cold_out" "$warm_out" >/dev/null \
        && diff -q "$cold_err" "$warm_err" >/dev/null; then
        return 0
    fi
    echo "FAIL $name (L2 warm 复跑不一致 cold=$cold_code warm=$warm_code)"
    show_diff "$cold_out" "$warm_out"
    if ! diff -q "$cold_err" "$warm_err" >/dev/null; then
        echo "warm stderr mismatch:"
        show_diff "$cold_err" "$warm_err"
    fi
    fail=$((fail+1))
    return 1
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
    local mc2=0
    if [ "$mode" != xfail ]; then
        # warm 复跑（P3）：判绿前先要求第二跑与首跑一致
        "$MIRVM" run "$src" >"$TMP/$name.mirvm2" 2>"$TMP/$name.mirvm2.err"; mc2=$?
    fi
    local D; D=$(script_dir "$stem")
    if [ -z "$D" ]; then echo "FAIL $name (未找到物化目录)"; fail=$((fail+1)); return; fi
    (cd "$D" && RUSTC="$RUSTC" "$CARGO" run -q \
        >"$TMP/$name.native" 2>"$TMP/$name.native.err"); local nc=$?
    if [ "$mode" = xfail ]; then
        check_expected_red "$name" "$TMP/$name.native" "$TMP/$name.mirvm" \
            "$TMP/$name.mirvm.err" "$nc" "$mc" "$expected_native_code" \
            "$expected_mirvm_code" "$diagnostic"
    else
        check_warm "$name" "$TMP/$name.mirvm" "$TMP/$name.mirvm.err" "$mc" \
            "$TMP/$name.mirvm2" "$TMP/$name.mirvm2.err" "$mc2" || return
        check_green "$name" "$TMP/$name.native" "$TMP/$name.mirvm" \
            "$nc" "$mc" "$expected_native_code" "$TMP/$name.native.err" \
            "$TMP/$name.mirvm.err"
    fi
}

if [ -n "${ECOSYSTEM_XFAIL_DIAGNOSTIC:-}" ]; then
    diff_script ecosystem demo/ecosystem.rs ecosystem xfail 0 70 \
        "$ECOSYSTEM_XFAIL_DIAGNOSTIC"
else
    diff_script ecosystem demo/ecosystem.rs ecosystem green 0
fi
diff_script ffi_zlib demo/ffi_zlib.rs ffi_zlib green 0
diff_script ripgrep_regex tests/fixtures/real_ripgrep_regex.rs real_ripgrep_regex green 0
diff_script warning_return tests/fixtures/cargo_warning_return.rs cargo_warning_return green 0

# 2) cargo 项目模式
PROJ="$TMP/proj"; mkdir -p "$PROJ/.cargo" "$PROJ/src"
cat > "$PROJ/Cargo.toml" <<'EOF'
[package]
name = "diffproj"
version = "0.1.0"
edition = "2024"
[dependencies]
serde_json = "1"
EOF
cat > "$PROJ/.cargo/config.toml" <<'EOF'
[build]
rustflags = [
    "--cfg=diff_cargo_project_config",
    "--check-cfg=cfg(diff_cargo_project_config)",
]
EOF
cat > "$PROJ/src/main.rs" <<'EOF'
#[cfg(not(diff_cargo_project_config))]
compile_error!("project Cargo rustflags were replaced");
#[cfg(not(diff_cargo_harness_append))]
compile_error!("harness-appended rustflags were lost");

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let caller_marker = std::fs::read_to_string("caller-marker.txt")
        .expect("cargo run should preserve its caller's working directory");
    let runtime_value = std::env::var("DIFF_RUNTIME_ENV").unwrap();
    let cargo_pkg_at_runtime = std::env::var_os("CARGO_PKG_NAME").is_some();
    let encoded_flags_at_runtime = std::env::var_os("CARGO_ENCODED_RUSTFLAGS").is_some();
    let v: serde_json::Value = serde_json::json!({"args": args.len()});
    println!(
        "{v} extra={:?} marker={} runtime={} cargo_pkg_at_runtime={cargo_pkg_at_runtime} encoded_flags_at_runtime={encoded_flags_at_runtime}",
        &args[1..], caller_marker.trim(), runtime_value,
    );
    std::process::exit(7);
}
EOF
CALLER="$TMP/caller"; mkdir -p "$CALLER"
printf 'outside-project\n' > "$CALLER/caller-marker.txt"
printf -v project_mirvm_flags '%s\x1f%s' \
    '--cfg=diff_cargo_harness_append' \
    '--check-cfg=cfg(diff_cargo_harness_append)'
printf -v project_mirvm_flags '%s\x1f%s\x1f%s' \
    "$project_mirvm_flags" \
    "--remap-path-prefix=$PROJ=/mirvm-diff-project" \
    '--remap-path-scope=diagnostics'
(cd "$CALLER" && DIFF_RUNTIME_ENV=first \
    MIRVM_ENCODED_RUSTFLAGS_APPEND="$project_mirvm_flags" \
    "$MIRVM" run "$PROJ" -- x y \
    >"$TMP/proj.mirvm" 2>"$TMP/proj.mirvm.err"); mc=$?
(cd "$CALLER" && DIFF_RUNTIME_ENV=second \
    MIRVM_ENCODED_RUSTFLAGS_APPEND="$project_mirvm_flags" \
    "$MIRVM" run "$PROJ" -- x y \
    >"$TMP/proj.mirvm2" 2>"$TMP/proj.mirvm2.err"); mc2=$?
(cd "$CALLER" && DIFF_RUNTIME_ENV=first PROJECT_SUITE_RUSTC="$RUSTC" \
    PROJECT_SUITE_ENCODED_RUSTFLAGS_APPEND="$project_mirvm_flags" \
    RUSTC="$RUSTC_APPEND_PROXY" "$CARGO" run -q \
    --manifest-path "$PROJ/Cargo.toml" --config "$PROJ/.cargo/config.toml" -- x y \
    >"$TMP/proj.native" 2>"$TMP/proj.native.err"); nc=$?
(cd "$CALLER" && DIFF_RUNTIME_ENV=second PROJECT_SUITE_RUSTC="$RUSTC" \
    PROJECT_SUITE_ENCODED_RUSTFLAGS_APPEND="$project_mirvm_flags" \
    RUSTC="$RUSTC_APPEND_PROXY" "$CARGO" run -q \
    --manifest-path "$PROJ/Cargo.toml" --config "$PROJ/.cargo/config.toml" -- x y \
    >"$TMP/proj.native2" 2>"$TMP/proj.native2.err"); nc2=$?
check_green project-cold "$TMP/proj.native" "$TMP/proj.mirvm" "$nc" "$mc" 7 \
    "$TMP/proj.native.err" "$TMP/proj.mirvm.err"
check_green project-warm-runtime-env "$TMP/proj.native2" "$TMP/proj.mirvm2" \
    "$nc2" "$mc2" 7 "$TMP/proj.native2.err" "$TMP/proj.mirvm2.err"

# Cargo fingerprint 合同：固定 Cargo 能看见的 rustflags 改变后必须重编；
# compat 轨追加在 wrapper 内的 flags 也必须进入等价的 Cargo 新鲜度判断。
FP_PROJ="$TMP/fingerprint"; mkdir -p "$FP_PROJ/src"
cat > "$FP_PROJ/Cargo.toml" <<'EOF'
[package]
name = "fingerprint"
version = "0.1.0"
edition = "2024"
EOF
cat > "$FP_PROJ/src/main.rs" <<'EOF'
#[cfg(all(diff_flag_one, diff_flag_two))]
compile_error!("rustflags from two builds were combined");
#[cfg(not(any(diff_flag_one, diff_flag_two)))]
compile_error!("rustflags were not applied");

fn main() {
    #[cfg(diff_flag_one)]
    println!("one");
    #[cfg(diff_flag_two)]
    println!("two");
}
EOF
printf -v fp_one '%s\x1f%s' '--cfg=diff_flag_one' \
    '--check-cfg=cfg(diff_flag_one,diff_flag_two)'
printf -v fp_two '%s\x1f%s' '--cfg=diff_flag_two' \
    '--check-cfg=cfg(diff_flag_one,diff_flag_two)'
(cd "$FP_PROJ" && MIRVM_ENCODED_RUSTFLAGS_APPEND="$fp_one" \
    "$MIRVM" run "$FP_PROJ" >"$TMP/fp-one.mirvm" 2>"$TMP/fp-one.mirvm.err"); fpm1=$?
(cd "$FP_PROJ" && MIRVM_ENCODED_RUSTFLAGS_APPEND="$fp_two" \
    "$MIRVM" run "$FP_PROJ" >"$TMP/fp-two.mirvm" 2>"$TMP/fp-two.mirvm.err"); fpm2=$?
(cd "$FP_PROJ" && CARGO_ENCODED_RUSTFLAGS="$fp_one" RUSTC="$RUSTC" \
    "$CARGO" run -q >"$TMP/fp-one.native" 2>"$TMP/fp-one.native.err"); fpn1=$?
(cd "$FP_PROJ" && CARGO_ENCODED_RUSTFLAGS="$fp_two" RUSTC="$RUSTC" \
    "$CARGO" run -q >"$TMP/fp-two.native" 2>"$TMP/fp-two.native.err"); fpn2=$?
check_green rustflags-fingerprint-cold "$TMP/fp-one.native" "$TMP/fp-one.mirvm" \
    "$fpn1" "$fpm1" 0 "$TMP/fp-one.native.err" "$TMP/fp-one.mirvm.err"
check_green rustflags-fingerprint-change "$TMP/fp-two.native" "$TMP/fp-two.mirvm" \
    "$fpn2" "$fpm2" 0 "$TMP/fp-two.native.err" "$TMP/fp-two.mirvm.err"

# Cargo wrapper 合同：固定 Cargo 决定普通 wrapper 在外、workspace wrapper 在内，
# 且后者只用于 workspace 成员。MIRVM 必须占据最内层编译器位置，不能拒绝、吞掉
# 或自己重排 Cargo 通过环境变量/config 得出的 wrapper 链。
WRAP_DEP="$TMP/wrapper-dep"; mkdir -p "$WRAP_DEP/src"
cat > "$WRAP_DEP/Cargo.toml" <<'EOF'
[package]
name = "wrapper_dep"
version = "0.1.0"
edition = "2024"
EOF
cat > "$WRAP_DEP/src/lib.rs" <<'EOF'
pub fn value() -> u32 { 42 }
EOF
WRAP_PROJ="$TMP/wrapper-project"; mkdir -p "$WRAP_PROJ/src" "$WRAP_PROJ/.cargo"
cat > "$WRAP_PROJ/Cargo.toml" <<'EOF'
[package]
name = "wrapper_root"
version = "0.1.0"
edition = "2024"

[dependencies]
wrapper_dep = { path = "../wrapper-dep" }
EOF
cat > "$WRAP_PROJ/src/main.rs" <<'EOF'
fn main() { println!("wrapper={}", wrapper_dep::value()); }
EOF
WRAP_TOOLS="$TMP/wrapper-tools"; mkdir -p "$WRAP_TOOLS"
ln -s "$RUSTC_WRAPPER_PROBE" "$WRAP_TOOLS/ordinary-wrapper"
ln -s "$RUSTC_WRAPPER_PROBE" "$WRAP_TOOLS/workspace-wrapper"

check_wrapper_chain() {
    local name="$1" native_log="$2" mirvm_log="$3"
    local ordinary=ordinary-wrapper workspace=workspace-wrapper
    local bad=0
    for log in "$native_log.$ordinary" "$native_log.$workspace" \
        "$mirvm_log.$ordinary" "$mirvm_log.$workspace"; do
        if [ ! -s "$log" ]; then
            echo "FAIL $name (wrapper 未执行: $log)"; bad=1
        fi
    done
    if [ "$bad" = 0 ] \
        && grep -Fq "$ordinary|$WRAP_TOOLS/$workspace|$RUSTC|" "$native_log.$ordinary" \
        && grep -Fq "$ordinary|$RUSTC|--crate-name|wrapper_dep|" "$native_log.$ordinary" \
        && grep -Fq "$workspace|$RUSTC|--crate-name|wrapper_root|" "$native_log.$workspace" \
        && ! grep -Fq '|--crate-name|wrapper_dep|' "$native_log.$workspace" \
        && grep -Fq "$ordinary|$WRAP_TOOLS/$workspace|$MIRVM|" "$mirvm_log.$ordinary" \
        && grep -Fq "$ordinary|$MIRVM|--crate-name|wrapper_dep|" "$mirvm_log.$ordinary" \
        && grep -Fq "$workspace|$MIRVM|--crate-name|wrapper_root|" "$mirvm_log.$workspace" \
        && ! grep -Fq '|--crate-name|wrapper_dep|' "$mirvm_log.$workspace"; then
        echo "PASS $name"; pass=$((pass+1))
    else
        if [ "$bad" = 0 ]; then
            echo "FAIL $name (wrapper 顺序或适用范围与固定 Cargo 不一致)"
            tail -20 "$native_log.$ordinary" "$native_log.$workspace" \
                "$mirvm_log.$ordinary" "$mirvm_log.$workspace"
        fi
        fail=$((fail+1))
    fi
}

NATIVE_ENV_LOG="$TMP/wrapper-native-env"
MIRVM_ENV_LOG="$TMP/wrapper-mirvm-env"
(cd "$WRAP_PROJ" && MIRVM_WRAPPER_PROBE_LOG="$NATIVE_ENV_LOG" RUSTC="$RUSTC" \
    RUSTC_WRAPPER="$WRAP_TOOLS/ordinary-wrapper" \
    RUSTC_WORKSPACE_WRAPPER="$WRAP_TOOLS/workspace-wrapper" \
    CARGO_TARGET_DIR="$TMP/wrapper-native-env-target" "$CARGO" run -q \
    >"$TMP/wrapper-native-env.out" 2>"$TMP/wrapper-native-env.err"); wne=$?
(cd "$WRAP_PROJ" && MIRVM_WRAPPER_PROBE_LOG="$MIRVM_ENV_LOG" \
    RUSTC_WRAPPER="$WRAP_TOOLS/ordinary-wrapper" \
    RUSTC_WORKSPACE_WRAPPER="$WRAP_TOOLS/workspace-wrapper" \
    MIRVM_TARGET_DIR="$TMP/wrapper-mirvm-env-target" "$MIRVM" run . \
    >"$TMP/wrapper-mirvm-env.out" 2>"$TMP/wrapper-mirvm-env.err"); wme=$?
check_green cargo-wrapper-env-output "$TMP/wrapper-native-env.out" \
    "$TMP/wrapper-mirvm-env.out" "$wne" "$wme" 0 \
    "$TMP/wrapper-native-env.err" "$TMP/wrapper-mirvm-env.err"
check_wrapper_chain cargo-wrapper-env-chain "$NATIVE_ENV_LOG" "$MIRVM_ENV_LOG"

cat > "$WRAP_PROJ/.cargo/config.toml" <<EOF
[build]
rustc-wrapper = "$WRAP_TOOLS/ordinary-wrapper"
rustc-workspace-wrapper = "$WRAP_TOOLS/workspace-wrapper"
EOF
NATIVE_CONFIG_LOG="$TMP/wrapper-native-config"
MIRVM_CONFIG_LOG="$TMP/wrapper-mirvm-config"
(cd "$WRAP_PROJ" && MIRVM_WRAPPER_PROBE_LOG="$NATIVE_CONFIG_LOG" RUSTC="$RUSTC" \
    CARGO_TARGET_DIR="$TMP/wrapper-native-config-target" "$CARGO" run -q \
    >"$TMP/wrapper-native-config.out" 2>"$TMP/wrapper-native-config.err"); wnc=$?
(cd "$WRAP_PROJ" && MIRVM_WRAPPER_PROBE_LOG="$MIRVM_CONFIG_LOG" \
    MIRVM_TARGET_DIR="$TMP/wrapper-mirvm-config-target" "$MIRVM" run . \
    >"$TMP/wrapper-mirvm-config.out" 2>"$TMP/wrapper-mirvm-config.err"); wmc=$?
check_green cargo-wrapper-config-output "$TMP/wrapper-native-config.out" \
    "$TMP/wrapper-mirvm-config.out" "$wnc" "$wmc" 0 \
    "$TMP/wrapper-native-config.err" "$TMP/wrapper-mirvm-config.err"
check_wrapper_chain cargo-wrapper-config-chain "$NATIVE_CONFIG_LOG" "$MIRVM_CONFIG_LOG"

suite_summary differential.cargo
