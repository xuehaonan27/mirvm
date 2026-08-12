#!/usr/bin/env bash
# C/C-unwind 跨语言异常合同：固定 rustc+C++ 是权威；解释器与真实同步 JIT
# 必须保留 C++ 异常身份、Rust panic 身份、Drop 次数、普通 C 的终止边界，
# 并在进入 libffi 前拒绝非 C/System ABI。
set -u
. "$(dirname "${BASH_SOURCE[0]}")/../../support/harness.sh"
test_enter_repo

MIRVM=${MIRVM:-$(pwd)/target/release/mirvm}
CARGO=${CARGO:-$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo}
RUSTC=${RUSTC:-$(dirname "$CARGO")/rustc}
FIXTURE=$(pwd)/tests/fixtures/c_unwind_contract
CONTRACT_HOME=${MIRVM_CONTRACT_HOME:-${MIRVM_HOME:-$HOME/.mirvm}}

[ -x "$MIRVM" ] || { echo "c_unwind: $MIRVM 不存在" >&2; exit 69; }
[ -x "$CARGO" ] || { echo "c_unwind: 固定 Cargo 不存在: $CARGO" >&2; exit 69; }
command -v "${CXX:-c++}" >/dev/null 2>&1 || { echo "c_unwind: C++ 编译器不可用" >&2; exit 69; }
command -v "${AR:-ar}" >/dev/null 2>&1 || { echo "c_unwind: ar 不可用" >&2; exit 69; }
ensure_test_sysroot "$MIRVM" "$CONTRACT_HOME" "$RUSTC" || exit $?
CONTRACT_SYSROOT=$TEST_SYSROOT

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

# 固定 Cargo 只负责生成 native oracle。MIRVM 产品腿仍走默认 cargoless，
# 并使用独立 target 目录，避免一条腿的构建产物替另一条腿作答。
CARGO_TARGET_DIR="$TMP/native-target" "$CARGO" build --quiet --locked \
    --manifest-path "$FIXTURE/Cargo.toml" >"$TMP/native-build.out" 2>"$TMP/native-build.err"
native_build=$?
if [ "$native_build" -ne 0 ]; then
    echo "c_unwind: native fixture 构建失败" >&2
    cat "$TMP/native-build.err" >&2
    exit 1
fi
NATIVE="$TMP/native-target/debug/c-unwind-contract"

# 先构建一次 cargoless 项目。后续正式运行不应夹带 build.rs/rustc 输出。
MIRVM_HOME="$TMP/mirvm-home" MIRVM_SYSROOT="$CONTRACT_SYSROOT" \
    MIRVM_TARGET_DIR="$TMP/mirvm-target" MIRVM_DEPS=self MIRVM_JIT=off \
    "$MIRVM" run "$FIXTURE" -- no-throw \
    >"$TMP/prime.out" 2>"$TMP/prime.err"
prime_code=$?
if [ "$prime_code" -ne 0 ]; then
    echo "c_unwind: cargoless fixture 预备失败（exit=$prime_code）" >&2
    tail -40 "$TMP/prime.err" >&2
    exit 1
fi

run_native() { # <name> <mode>
    local name=$1 mode=$2
    env -u RUST_BACKTRACE "$NATIVE" "$mode" \
        >"$TMP/$name.native.out" 2>"$TMP/$name.native.err"
    printf '%s' "$?"
}

run_mirvm_command() { # <name> <interp|jit> <mirvm run args...>
    local name=$1 engine=$2
    shift 2
    if [ "$engine" = interp ]; then
        env -u RUST_BACKTRACE MIRVM_HOME="$TMP/mirvm-home" \
            MIRVM_SYSROOT="$CONTRACT_SYSROOT" MIRVM_TARGET_DIR="$TMP/mirvm-target" \
            MIRVM_DEPS=self MIRVM_JIT=off \
            "$MIRVM" run "$@" \
            >"$TMP/$name.interp.out" 2>"$TMP/$name.interp.err"
    else
        env -u RUST_BACKTRACE MIRVM_HOME="$TMP/mirvm-home" \
            MIRVM_SYSROOT="$CONTRACT_SYSROOT" MIRVM_TARGET_DIR="$TMP/mirvm-target" \
            MIRVM_DEPS=self MIRVM_JIT=on MIRVM_JIT_SYNC=1 MIRVM_JIT_THRESHOLD=1 \
            MIRVM_JIT_DEBUG=1 \
            "$MIRVM" run "$@" \
            >"$TMP/$name.jit.out" 2>"$TMP/$name.jit.err"
    fi
    printf '%s' "$?"
}

run_mirvm() { # <name> <mode> <interp|jit>
    run_mirvm_command "$1" "$3" "$FIXTURE" -- "$2"
}

run_mirvm_bin() { # <name> <bin> <interp|jit>
    run_mirvm_command "$1" "$3" "$FIXTURE" --bin "$2"
}

check_success() { # <name> <mode> <expected stdout> <JIT function fragment>
    local name=$1 mode=$2 expected=$3 jit_function=$4 nc ic jc
    nc=$(run_native "$name" "$mode")
    ic=$(run_mirvm "$name" "$mode" interp)
    jc=$(run_mirvm "$name" "$mode" jit)
    if [ "$nc" -ne 0 ] || [ "$ic" -ne 0 ] || [ "$jc" -ne 0 ]; then
        bad "$name 退出码 native=$nc interp=$ic jit=$jc，期望 0"
        tail -30 "$TMP/$name.interp.err"
        tail -30 "$TMP/$name.jit.err"
    elif [ "$(cat "$TMP/$name.native.out")" != "$expected" ] \
        || ! cmp -s "$TMP/$name.native.out" "$TMP/$name.interp.out" \
        || ! cmp -s "$TMP/$name.native.out" "$TMP/$name.jit.out"; then
        bad "$name stdout 未保持 native 结果"
        for leg in native interp jit; do echo "--- $leg"; cat "$TMP/$name.$leg.out"; done
    elif rg -q 'release=false|failed to be compiled|panicked at .*translate.rs' \
        "$TMP/$name.jit.err"; then
        bad "$name 强制 JIT 编译失败"
        tail -40 "$TMP/$name.jit.err"
    elif ! rg -q "release=true.*${jit_function}" "$TMP/$name.jit.err"; then
        bad "$name 目标函数没有机器码发布证据: $jit_function"
        tail -40 "$TMP/$name.jit.err"
    else
        ok "$name"
    fi
}

check_abort() { # <name> <mode> <native stderr regexp> <mirvm stderr regexp> <JIT function fragment>
    local name=$1 mode=$2 native_pattern=$3 mirvm_pattern=$4 jit_function=$5 nc ic jc
    nc=$(run_native "$name" "$mode")
    ic=$(run_mirvm "$name" "$mode" interp)
    jc=$(run_mirvm "$name" "$mode" jit)
    if [ "$nc" -eq 0 ] || [ "$ic" -eq 0 ] || [ "$jc" -eq 0 ]; then
        bad "$name 本应终止却返回 native=$nc interp=$ic jit=$jc"
    elif ! rg -q "$native_pattern" "$TMP/$name.native.err"; then
        bad "$name native 未以预期原因终止"
        tail -20 "$TMP/$name.native.err"
    elif ! rg -q "$mirvm_pattern" "$TMP/$name.interp.err" \
        || ! rg -q "$mirvm_pattern" "$TMP/$name.jit.err"; then
        bad "$name MIRVM 未以预期 ABI 原因终止"
        tail -20 "$TMP/$name.interp.err"
        tail -20 "$TMP/$name.jit.err"
    elif rg -q 'release=false|failed to be compiled|panicked at .*translate.rs' \
        "$TMP/$name.jit.err"; then
        bad "$name 强制 JIT 编译失败"
        tail -40 "$TMP/$name.jit.err"
    elif ! rg -q "release=true.*${jit_function}" "$TMP/$name.jit.err"; then
        bad "$name 目标函数没有机器码发布证据: $jit_function"
        tail -40 "$TMP/$name.jit.err"
    elif rg -q '^unexpected ' "$TMP/$name.native.out" "$TMP/$name.interp.out" "$TMP/$name.jit.out"; then
        bad "$name 异常被 C++ catch 吞掉后错误返回"
    else
        ok "$name"
    fi
}

check_reject() { # <name> <bin> <stderr regexp>
    local name=$1 bin=$2 pattern=$3 ic jc
    ic=$(run_mirvm_bin "$name" "$bin" interp)
    jc=$(run_mirvm_bin "$name" "$bin" jit)
    if [ "$ic" -eq 0 ] || [ "$jc" -eq 0 ]; then
        bad "$name 不受支持的 ABI 被静默接受 interp=$ic jit=$jc"
    elif ! rg -q "$pattern" "$TMP/$name.interp.err" \
        || ! rg -q "$pattern" "$TMP/$name.jit.err"; then
        bad "$name 未给出明确的 ABI 拒绝诊断"
        tail -20 "$TMP/$name.interp.err"
        tail -20 "$TMP/$name.jit.err"
    elif rg -q '^unexpected ' "$TMP/$name.interp.out" "$TMP/$name.jit.out"; then
        bad "$name 不受支持的 ABI 被实际调用"
    else
        ok "$name"
    fi
}

# 正常返回锁住 JIT CallForeign+Cleanup 的 TryCallRet 通道；反复执行确保发布后仍继续
# 经过目标函数。异常两项锁住对象身份与 guest Drop 次数。
check_success no_throw no-throw 'value=42 drops=30000' no_throw_with_cleanup
check_success typed_exception typed-catch 'result=1073 caught=73 drops=1' throw_foreign_from_guest
check_success panic_rethrow panic-rethrow 'payload=51 caught=888 drops=1' panic_from_guest

# 终止诊断文字不是 Rust ABI 的稳定部分；只锁定 native 与 MIRVM 各自明确的
# 终止原因、非零状态，以及 C++ catch 不得吞掉后继续返回。
check_abort panic_swallowed panic-swallow \
    'Rust panics must be rethrown' \
    'Rust panics must be rethrown|UnwindTerminate' \
    panic_swallowed_from_guest
check_abort plain_c_panic plain-c-panic \
    'panic in a function that cannot unwind|non-unwinding panic' \
    'UnwindTerminate|cannot unwind|non-unwinding panic' \
    panic_from_plain_c_guest
check_abort plain_c_foreign plain-c-foreign \
    'terminate called|cannot unwind|foreign exception' \
    'Rust cannot catch foreign exceptions|unwind 抵达 Terminate 边界' \
    foreign_through_plain_c_guest
check_abort plain_c_foreign_indirect plain-c-foreign-indirect \
    'terminate called|cannot unwind|foreign exception' \
    'Rust cannot catch foreign exceptions|unwind 抵达 Terminate 边界' \
    foreign_indirect_through_plain_c_guest
check_abort plain_c_nested_panic plain-c-nested-panic \
    'panic in a function that cannot unwind|non-unwinding panic' \
    'unwind 抵达 Terminate 边界' \
    nested_panic_through_plain_c_guest
check_abort plain_c_nested_panic_indirect plain-c-nested-panic-indirect \
    'panic in a function that cannot unwind|non-unwinding panic' \
    'unwind 抵达 Terminate 边界' \
    nested_panic_indirect_through_plain_c_guest

# libffi 只承诺 C/System 调用约定。其他 ABI 必须在 lowering 阶段明确拒绝，
# 不能借 `unwind=false` 假装成普通 C 后碰运气调用。
check_reject unsupported_outer_abi unsupported_outer_abi \
    'foreign `cpp_no_throw` ABI Rust 不支持 libffi 直通'
check_reject unsupported_callback_abi unsupported_callback_abi \
    'foreign `cpp_call_plain_c` 回调 .* ABI Rust 不支持 thunk'

suite_summary runtime.c-unwind
