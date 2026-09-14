#!/usr/bin/env bash
# 诊断路由合同：三种编译路径保持默认 stderr，同时 capture 只收编译器和 MIRVM 诊断。
set -u
. "$(dirname "${BASH_SOURCE[0]}")/../../support/harness.sh"
test_enter_repo

MIRVM=${MIRVM:-$REPO_ROOT/target/release/mirvm}
require_executable MIRVM "$MIRVM" || exit $?

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
HOME_DIR="$TMP/home"
mkdir -p "$HOME_DIR"
ensure_test_sysroot "$MIRVM" "$HOME_DIR" "$RUSTC" || exit $?
host=$($RUSTC -vV | sed -n 's/^host: //p')
home_sysroot="$HOME_DIR/sysroot-$host"
if [ "$TEST_SYSROOT" != "$home_sysroot" ] && [ ! -e "$home_sysroot" ]; then
    ln -s "$TEST_SYSROOT" "$home_sysroot"
fi

hex_file() {
    od -An -tx1 -v "$1" | tr -d ' \n'
}

run_case() { # <name> <deps> <fixture> <unused-name> <missing-symbol> <guest-hex>
    local name=$1 deps=$2 fixture=$3 unused_name=$4 missing=$5 guest_hex=$6
    local plain="$TMP/$name-plain" captured="$TMP/$name-captured"
    local session="$TMP/$name-session" plain_code=0 capture_code=0 diag
    local stderr_hex diagnostics_hex expected_hex
    mkdir -p "$plain" "$captured" "$session"

    MIRVM_HOME="$HOME_DIR" MIRVM_DEPS="$deps" MIRVM_SYSROOT="$TEST_SYSROOT" \
        "$MIRVM" run "$fixture" >"$plain/stdout" 2>"$plain/stderr" || plain_code=$?
    MIRVM_HOME="$HOME_DIR" MIRVM_DEPS="$deps" MIRVM_SYSROOT="$TEST_SYSROOT" \
        "$MIRVM" capture -o "$session" -- run "$fixture" \
        >"$captured/stdout" 2>"$captured/stderr" || capture_code=$?

    if [ "$plain_code" -eq 70 ] && [ "$capture_code" -eq 70 ] \
        && cmp -s "$plain/stdout" "$captured/stdout" \
        && cmp -s "$plain/stderr" "$captured/stderr"; then
        ok "$name 默认 stdout/stderr/exit 在 capture 前后逐字节不变"
    else
        bad "$name 默认输出漂移（plain=$plain_code capture=$capture_code）"
        diff -u "$plain/stderr" "$captured/stderr" | head -40
    fi

    diag="$session/diagnostics.log"
    if [ ! -f "$diag" ]; then
        bad "$name capture 未生成 diagnostics.log"
        return
    fi
    if grep -aFq "function \`$unused_name\` is never used" "$diag" \
        && grep -aFq "foreign \`$missing\` 符号不存在" "$diag"; then
        ok "$name diagnostics 含编译器 warning 与 MIRVM control error"
    else
        bad "$name diagnostics 缺编译器或 MIRVM 诊断"
    fi

    if hex_file "$diag" | grep -Fq "$guest_hex"; then
        bad "$name diagnostics 混入 guest NUL/非 UTF-8/ANSI 字节"
    else
        ok "$name diagnostics 不含 guest 二进制 stderr"
    fi
    if grep -aFq 'router-guest-binary-' "$diag"; then
        bad "$name diagnostics 混入 guest 同文 warning 所在的单次 write"
    else
        ok "$name guest 同文 warning 所在的单次 write 未被误收"
    fi

    stderr_hex=$(hex_file "$captured/stderr")
    diagnostics_hex=$(hex_file "$diag")
    expected_hex=${stderr_hex/"$guest_hex"/}
    if [ "$expected_hex" != "$stderr_hex" ] && [ "$diagnostics_hex" = "$expected_hex" ]; then
        ok "$name diagnostics 是物理 stderr 精确去掉 guest 单次 write 后的字节"
    else
        bad "$name compiler/control tee 不是逐字节副本"
    fi
}

direct_guest_hex=$(printf '%s' 'warning: 1 warning emitted
router-guest-binary-direct:' | od -An -tx1 -v | tr -d ' \n')
direct_guest_hex="${direct_guest_hex}00ff1b5b33316d0a"
script_guest_hex=$(printf '%s' 'warning: 1 warning emitted
router-guest-binary-script:' | od -An -tx1 -v | tr -d ' \n')
script_guest_hex="${script_guest_hex}00ff1b5b33316d0a"

run_case direct self tests/fixtures/diagnostic_router_direct.rs \
    router_unused_direct mirvm_diagnostic_router_missing_direct "$direct_guest_hex"
run_case cargoless self tests/fixtures/diagnostic_router_script.rs \
    router_unused_script mirvm_diagnostic_router_missing_script "$script_guest_hex"
run_case cargo-runner cargo tests/fixtures/diagnostic_router_script.rs \
    router_unused_script mirvm_diagnostic_router_missing_script "$script_guest_hex"

run_early_control_case() { # <name> <exit> <MIRVM_DEPS> [run args...]
    local name=$1 expected_code=$2 deps=$3
    local plain="$TMP/$name-plain" captured="$TMP/$name-captured"
    local session="$TMP/$name-session" plain_code=0 capture_code=0
    shift 3
    mkdir -p "$plain" "$captured" "$session"
    MIRVM_HOME="$HOME_DIR" MIRVM_SYSROOT="$TEST_SYSROOT" MIRVM_DEPS="$deps" \
        "$MIRVM" run "$@" >"$plain/stdout" 2>"$plain/stderr" || plain_code=$?
    MIRVM_HOME="$HOME_DIR" MIRVM_SYSROOT="$TEST_SYSROOT" MIRVM_DEPS="$deps" \
        "$MIRVM" capture -o "$session" -- run "$@" \
        >"$captured/stdout" 2>"$captured/stderr" || capture_code=$?

    if [ "$plain_code" -eq "$expected_code" ] \
        && [ "$capture_code" -eq "$expected_code" ] \
        && cmp -s "$plain/stdout" "$captured/stdout" \
        && cmp -s "$plain/stderr" "$captured/stderr"; then
        ok "$name 默认 stdout/stderr/exit 在 capture 前后逐字节不变"
    else
        bad "$name 默认输出漂移（plain=$plain_code capture=$capture_code）"
    fi
    if [ -f "$session/diagnostics.log" ] \
        && cmp -s "$captured/stderr" "$session/diagnostics.log"; then
        ok "$name run_driver 前 control 被逐字节 tee 并发布"
    else
        bad "$name run_driver 前 control 未进入 diagnostics.log"
    fi
}

run_early_control_case stack-invalid 2 self \
    --stack-size bad tests/fixtures/diagnostic_router_direct.rs
run_early_control_case jit-invalid 2 self \
    --jit maybe tests/fixtures/diagnostic_router_direct.rs
run_early_control_case unknown-argument 2 self --not-a-mirvm-option
run_early_control_case missing-input 2 self
run_early_control_case invalid-deps 2 invalid tests/fixtures/diagnostic_router_direct.rs
run_early_control_case bin-on-file 2 self \
    --bin selected tests/fixtures/diagnostic_router_direct.rs
run_early_control_case missing-source 1 self "$TMP/no-such-source.rs"

runner_plain="$TMP/runner-control-plain"
runner_captured="$TMP/runner-control-captured"
runner_session="$TMP/runner-control-session"
missing_fake="$TMP/missing-fake-binary"
mkdir -p "$runner_plain" "$runner_captured" "$runner_session"
runner_plain_code=0
runner_capture_code=0
"$MIRVM" runner "$missing_fake" \
    >"$runner_plain/stdout" 2>"$runner_plain/stderr" || runner_plain_code=$?
"$MIRVM" runner --mirvm-capture-directory "$runner_session" "$missing_fake" \
    >"$runner_captured/stdout" 2>"$runner_captured/stderr" || runner_capture_code=$?

if [ "$runner_plain_code" -eq 1 ] && [ "$runner_capture_code" -eq 1 ] \
    && cmp -s "$runner_plain/stdout" "$runner_captured/stdout" \
    && cmp -s "$runner_plain/stderr" "$runner_captured/stderr"; then
    ok "forwarded runner early control 默认 stdout/stderr/exit 逐字节不变"
else
    bad "forwarded runner early control 默认输出漂移"
fi
if [ -f "$runner_session/diagnostics.log" ] \
    && cmp -s "$runner_captured/stderr" "$runner_session/diagnostics.log"; then
    ok "runner 假二进制解析错误被逐字节 tee 并发布"
else
    bad "runner 假二进制解析错误未进入 diagnostics.log"
fi

suite_summary runtime.diagnostics
