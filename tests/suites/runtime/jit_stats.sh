#!/usr/bin/env bash
# JIT 统计合同：进程退出时必须打印统计，且编译码到解释器调用桶非零。
set -u
. "$(dirname "${BASH_SOURCE[0]}")/../../support/harness.sh"
test_enter_repo

MIRVM=${MIRVM:-$REPO_ROOT/target/release/mirvm}
require_executable MIRVM "$MIRVM" || exit $?
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

code=0
MIRVM_JIT_STATS=1 "$MIRVM" run demo/jit_unwind_probe.rs \
    >"$TMP/out" 2>"$TMP/err" || code=$?
if [ "$code" -eq 0 ] && grep -q '^mirvm-jit-stats:' "$TMP/err" \
    && grep -q 'c2i=[1-9]' "$TMP/err"; then
    ok "退出时打印统计且 c2i 桶非零"
else
    bad "JIT 统计缺失或运行失败（exit=$code）"
    tail -3 "$TMP/err"
fi

suite_summary runtime.jit-stats
