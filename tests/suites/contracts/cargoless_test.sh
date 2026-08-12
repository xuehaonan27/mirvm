#!/usr/bin/env bash
# `mirvm test` 的 cargoless 合同：固定 Cargo 是语义权威；compat 与 self
# 必须给出同样的测试结果，self 腿还必须在 cargo 不可用时成立。
set -u
. "$(dirname "${BASH_SOURCE[0]}")/../../support/harness.sh"
test_enter_repo

MIRVM=${MIRVM:-$(pwd)/target/debug/mirvm}
CARGO=${CARGO:-$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo}
RUSTC=${RUSTC:-$(dirname "$CARGO")/rustc}
STRACE=${STRACE:-$(command -v strace)}
FIXTURE=$(pwd)/tests/fixtures/cless_test_contract
PROC_FIXTURE=$(pwd)/tests/fixtures/cless_proc_macro_test_contract
DOC_FIXTURE=$(pwd)/tests/fixtures/cless_doctest_contract
CONTRACT_HOME=${MIRVM_CONTRACT_HOME:-${MIRVM_HOME:-$HOME/.mirvm}}
HOST=$($RUSTC -vV | sed -n 's/^host: //p')
[ -x "$MIRVM" ] || { echo "cargoless_test_contract: $MIRVM 不存在" >&2; exit 69; }
[ -x "$CARGO" ] || { echo "cargoless_test_contract: pinned Cargo $CARGO 不存在" >&2; exit 69; }
[ -x "$STRACE" ] || { echo "cargoless_test_contract: strace 不存在" >&2; exit 69; }
ensure_test_sysroot "$MIRVM" "$CONTRACT_HOME" "$RUSTC" || exit $?
CONTRACT_SYSROOT=$TEST_SYSROOT
case "$($CARGO --version)" in
    "cargo 1.98.0-nightly "*) ;;
    *) echo "ERROR cargoless_test: Cargo 版本不在合同内: $($CARGO --version)" >&2; exit 69 ;;
esac

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
mkdir -p "$TMP/no-cargo"
cat >"$TMP/no-cargo/cargo" <<'EOF'
#!/bin/sh
: >"$MIRVM_CARGO_SENTINEL"
exit 97
EOF
chmod +x "$TMP/no-cargo/cargo"
export MIRVM_CARGO_SENTINEL="$TMP/cargo-was-invoked"

normalize() {
    sed -E \
        -e 's/\([0-9]+\) panicked/(<TID>) panicked/' \
        -e 's#/tmp/rustdoctest[^/ ]+#/tmp/rustdoctest<TMP>#g' \
        -e "s#${DOC_FIXTURE//\#/\\#}/##g" \
        -e 's/finished in [0-9]+\.[0-9]+s/finished in <TIME>/' \
        -e 's/all doctests ran in [0-9]+\.[0-9]+s; merged doctests compilation took [0-9]+\.[0-9]+s/all doctests ran in <TIME>; merged doctests compilation took <TIME>/' \
        -e '/^[[:space:]]*(Compiling|Finished|Running|Executable) /d' \
        -e '/^error: test failed, to rerun pass /d' \
        -e '/^error: [0-9]+ target(s)? failed:/d' \
        -e '/^[[:space:]]+`--[^`]+`$/d'
}

# 先只准备 sysroot；输出不参与合同，正式三腿各用独立 target 目录。
env MIRVM_HOME="$CONTRACT_HOME" MIRVM_TARGET_DIR="$TMP/prime-target" \
    MIRVM_DEPS=self "$MIRVM" test "$FIXTURE" --locked --offline --lib --no-run \
    >"$TMP/prime.out" 2>"$TMP/prime.err"
if [ $? -ne 0 ]; then
    echo "cargoless_test_contract: self 预备失败" >&2
    tail -20 "$TMP/prime.err" >&2
    exit 1
fi

# <名字> <期望退出码> <cargo test/mirvm test 的共同参数...>
triplet() {
    local name=$1 expected=$2
    shift 2
    local force=${CONTRACT_FORCE_FAIL:-0}
    local project=${CONTRACT_FIXTURE:-$FIXTURE}

    if [ "$force" = 1 ]; then
        env CLESS_FORCE_FAIL=1 CLESS_DOCTEST_FORCE_FAIL=1 CARGO_TARGET_DIR="$TMP/native-target" \
            "$CARGO" test --manifest-path "$project/Cargo.toml" --locked --offline "$@" \
            >"$TMP/$name.native.out" 2>"$TMP/$name.native.err"
    else
        env -u CLESS_FORCE_FAIL -u CLESS_DOCTEST_FORCE_FAIL CARGO_TARGET_DIR="$TMP/native-target" \
            "$CARGO" test --manifest-path "$project/Cargo.toml" --locked --offline "$@" \
            >"$TMP/$name.native.out" 2>"$TMP/$name.native.err"
    fi
    local nc=$?

    if [ "$force" = 1 ]; then
        env CLESS_FORCE_FAIL=1 CLESS_DOCTEST_FORCE_FAIL=1 MIRVM_HOME="$CONTRACT_HOME" \
            MIRVM_TARGET_DIR="$TMP/compat-target" MIRVM_DEPS=cargo \
            "$MIRVM" test "$project" --locked --offline "$@" \
            >"$TMP/$name.compat.out" 2>"$TMP/$name.compat.err"
    else
        env -u CLESS_FORCE_FAIL -u CLESS_DOCTEST_FORCE_FAIL MIRVM_HOME="$CONTRACT_HOME" \
            MIRVM_TARGET_DIR="$TMP/compat-target" MIRVM_DEPS=cargo \
            "$MIRVM" test "$project" --locked --offline "$@" \
            >"$TMP/$name.compat.out" 2>"$TMP/$name.compat.err"
    fi
    local cc=$?

    if [ "$force" = 1 ]; then
        env CLESS_FORCE_FAIL=1 CLESS_DOCTEST_FORCE_FAIL=1 PATH="$TMP/no-cargo:$PATH" MIRVM_OFFLINE=1 \
            MIRVM_HOME="$TMP/self-home" MIRVM_SYSROOT="$CONTRACT_SYSROOT" MIRVM_DEPS=self \
            "$MIRVM" test "$project" --locked --offline "$@" \
            >"$TMP/$name.self.out" 2>"$TMP/$name.self.err"
    else
        env -u CLESS_FORCE_FAIL -u CLESS_DOCTEST_FORCE_FAIL PATH="$TMP/no-cargo:$PATH" MIRVM_OFFLINE=1 \
            MIRVM_HOME="$TMP/self-home" MIRVM_SYSROOT="$CONTRACT_SYSROOT" MIRVM_DEPS=self \
            "$MIRVM" test "$project" --locked --offline "$@" \
            >"$TMP/$name.self.out" 2>"$TMP/$name.self.err"
    fi
    local sc=$?

    for leg in native compat self; do
        normalize <"$TMP/$name.$leg.out" >"$TMP/$name.$leg.norm.out"
        normalize <"$TMP/$name.$leg.err" >"$TMP/$name.$leg.norm.err"
    done
    if [ "$nc" != "$expected" ] || [ "$cc" != "$expected" ] || [ "$sc" != "$expected" ]; then
        bad "$name 退出码 native=$nc compat=$cc self=$sc，期望 $expected"
    elif ! diff -q "$TMP/$name.native.norm.out" "$TMP/$name.compat.norm.out" >/dev/null \
        || ! diff -q "$TMP/$name.native.norm.out" "$TMP/$name.self.norm.out" >/dev/null; then
        bad "$name stdout 不一致"
        diff -u "$TMP/$name.native.norm.out" "$TMP/$name.self.norm.out" | head -40
    elif ! diff -q "$TMP/$name.native.norm.err" "$TMP/$name.compat.norm.err" >/dev/null \
        || ! diff -q "$TMP/$name.native.norm.err" "$TMP/$name.self.norm.err" >/dev/null; then
        bad "$name stderr 不一致"
        diff -u "$TMP/$name.native.norm.err" "$TMP/$name.self.norm.err" | head -40
    else
        ok "$name"
    fi
}

triplet all_tests 0 --tests -- --test-threads=1 --nocapture
triplet ignored 0 --lib ignored -- --ignored --test-threads=1 --nocapture
triplet custom_harness 0 --test custom -- --custom-arg
triplet example_no_run 0 --example compile_only --no-run
triplet bench 0 --bench contract_bench -- --test-threads=1 --nocapture
triplet all_targets_no_run 0 --all-targets --no-run
triplet quiet 0 --lib --quiet -- --test-threads=1 --nocapture
CONTRACT_FIXTURE="$PROC_FIXTURE" triplet root_proc_macro 0 --tests -- --test-threads=1 --nocapture
CONTRACT_FIXTURE="$DOC_FIXTURE" triplet doctest 0 --doc -- --test-threads=1 --nocapture
CONTRACT_FIXTURE="$DOC_FIXTURE" triplet doctest_default 0 -- --test-threads=1
CONTRACT_FIXTURE="$DOC_FIXTURE" CONTRACT_FORCE_FAIL=1 triplet doctest_failure 101 --doc -- --test-threads=1 --nocapture
CONTRACT_FIXTURE="$DOC_FIXTURE" triplet doctest_no_run_rejected 101 --doc --no-run
CONTRACT_FIXTURE="$DOC_FIXTURE" triplet doctest_mixed_selection_rejected 101 --doc --lib

for leg in native compat self; do
    if rg -q 'compile fail .* ok' "$TMP/doctest.$leg.out" \
        && rg -q 'should panic .* ok' "$TMP/doctest.$leg.out"; then
        ok "doctest $leg compile_fail/should_panic 由 rustdoc 判定"
    else
        bad "doctest $leg 未覆盖 compile_fail/should_panic"
    fi
done

CONTRACT_FORCE_FAIL=1 triplet fail_fast 101 --tests -- --test-threads=1 --nocapture
for leg in native compat self; do
    if grep -Fq CLESS_INTEGRATION_RAN "$TMP/fail_fast.$leg.out"; then
        bad "fail_fast $leg 在首个失败 artifact 后仍继续"
    else
        ok "fail_fast $leg 停止"
    fi
done

CONTRACT_FORCE_FAIL=1 triplet no_fail_fast 101 --tests --no-fail-fast -- --test-threads=1 --nocapture
for leg in native compat self; do
    if grep -Fq CLESS_INTEGRATION_RAN "$TMP/no_fail_fast.$leg.out"; then
        ok "no_fail_fast $leg 继续"
    else
        bad "no_fail_fast $leg 未继续到 integration test"
    fi
done

if [ -e "$MIRVM_CARGO_SENTINEL" ]; then
    bad "self 腿启动了 cargo"
else
    ok "self 腿零 cargo"
fi

env -u CLESS_FORCE_FAIL PATH="$TMP/no-cargo:$PATH" MIRVM_OFFLINE=1 \
    MIRVM_HOME="$TMP/self-home" MIRVM_SYSROOT="$CONTRACT_SYSROOT" MIRVM_DEPS=self \
    "$STRACE" -f -qq -e trace=execve -o "$TMP/self.execve" \
    "$MIRVM" test "$FIXTURE" --locked --offline --lib --no-run \
    >"$TMP/trace.out" 2>"$TMP/trace.err"
trace_code=$?
if [ "$trace_code" != 0 ]; then
    bad "self execve 审计运行失败: $trace_code"
elif rg -q 'execve\("[^"]*/cargo",' "$TMP/self.execve"; then
    bad "self 腿通过绝对路径启动了 cargo"
else
    ok "self 腿 execve 零 cargo"
fi

env -u CLESS_DOCTEST_FORCE_FAIL PATH="$TMP/no-cargo:$PATH" MIRVM_OFFLINE=1 \
    MIRVM_HOME="$TMP/self-home" MIRVM_SYSROOT="$CONTRACT_SYSROOT" MIRVM_DEPS=self \
    "$STRACE" -f -qq -e trace=execve -o "$TMP/doctest-self.execve" \
    "$MIRVM" test "$DOC_FIXTURE" --locked --offline --doc -- --test-threads=1 \
    >"$TMP/doctest-trace.out" 2>"$TMP/doctest-trace.err"
doc_trace_code=$?
if [ "$doc_trace_code" != 0 ]; then
    bad "self doctest execve 审计运行失败: $doc_trace_code"
elif rg -q 'execve\("[^"]*/cargo",' "$TMP/doctest-self.execve"; then
    bad "self doctest 通过绝对路径启动了 cargo"
else
    ok "self doctest execve 零 cargo"
fi

for leg in native compat self; do
    target="$TMP/$leg-target"
    [ "$leg" = self ] && target="$TMP/self-home/target/cargoless"
    mapfile -t counts < <(find "$target" -name contract-build-count -type f -exec cat {} \;)
    if [ "${#counts[@]}" = 1 ] && [ "${counts[0]}" = 1 ]; then
        ok "$leg build.rs warm 不重跑"
    else
        bad "$leg build.rs 执行次数不是 1: ${counts[*]:-<missing>}"
    fi
done

# Cargo 的 verbose rustc 行是结构权威；以下断言一旦随 Cargo 升级改变，必须先审阅
# 差异，再更新 self 的参数单测和本合同，不能直接刷新快照。
CARGO_TARGET_DIR="$TMP/oracle-target" "$CARGO" test --manifest-path "$FIXTURE/Cargo.toml" \
    --locked --offline --no-run -vv \
    >"$TMP/oracle.out" 2>"$TMP/oracle.err"
oracle="$TMP/oracle.err"
if rg -q 'src/lib\.rs .*--crate-type lib' "$oracle" \
    && rg -q 'src/lib\.rs .*--test .*--extern test_helper=' "$oracle" \
    && rg -q 'src/main\.rs .*--test .*--extern cless_test_contract=.*--extern test_helper=' "$oracle" \
    && rg -q 'CARGO_BIN_EXE_cless_test_contract=.*CARGO_TARGET_TMPDIR=.*tests/api\.rs .*--test' "$oracle" \
    && rg -q 'examples/compile_only\.rs .*--crate-type bin .*--extern .*test_helper=' "$oracle" \
    && rg -q 'tests/custom\.rs .*--cfg test' "$oracle"; then
    ok "pinned Cargo rustc 目标形状"
else
    bad "pinned Cargo rustc 目标形状漂移"
    tail -30 "$oracle"
fi
normal_bin=$(rg 'src/main\.rs .*--crate-type bin' "$oracle" | head -1)
if [ -n "$normal_bin" ] && [[ "$normal_bin" != *"--test"* ]] \
    && [[ "$normal_bin" != *"test_helper="* ]]; then
    ok "pinned Cargo 普通 bin 不吃 dev 依赖"
else
    bad "pinned Cargo 普通 bin 合同漂移"
fi

CARGO_TARGET_DIR="$TMP/oracle-proc-target" "$CARGO" test \
    --manifest-path "$PROC_FIXTURE/Cargo.toml" --locked --offline --no-run -vv \
    >"$TMP/oracle-proc.out" 2>"$TMP/oracle-proc.err"
proc_oracle="$TMP/oracle-proc.err"
if rg -q 'src/lib\.rs .*--crate-type proc-macro .*--extern pm_helper=.*--extern proc_macro' "$proc_oracle" \
    && rg -q 'src/lib\.rs .*--test .*--extern pm_helper=.*--extern test_helper=.*--extern proc_macro' "$proc_oracle" \
    && rg -q 'tests/use_macro\.rs .*--test .*--extern cless_proc_macro_test_contract=.*\.so .*--extern test_helper=' "$proc_oracle"; then
    ok "pinned Cargo 根 proc-macro 测试形状"
else
    bad "pinned Cargo 根 proc-macro 测试形状漂移"
    tail -30 "$proc_oracle"
fi


CARGO_TARGET_DIR="$TMP/oracle-doc-target" "$CARGO" test \
    --manifest-path "$DOC_FIXTURE/Cargo.toml" --locked --offline --doc -vv \
    >"$TMP/oracle-doc.out" 2>"$TMP/oracle-doc.err"
doc_oracle="$TMP/oracle-doc.err"
if rg -q 'rustdoc .*--crate-type lib .*--test src/lib\.rs .*--extern cless_doctest_contract=.*\.rlib .*--extern doctest_helper=.*\.rlib' "$doc_oracle" \
    && rg -q -- '--cfg cless_doctest_cfg' "$doc_oracle"; then
    ok "pinned Cargo rustdoc doctest 形状"
else
    bad "pinned Cargo rustdoc doctest 形状漂移"
    tail -30 "$doc_oracle"
fi

suite_summary contracts.cargoless-test
