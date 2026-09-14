#!/usr/bin/env bash
# Spike 4 TSan 判定：引擎（src/vm）在全量插桩下零数据竞争。
# product: no
# 通过 = 退出码 0 且无 "WARNING: ThreadSanitizer"。guest 竞争按 C4 排除
# （用例设计为 guest 无竞争），故任何警告 = 引擎 bug。
set -u
. "$(dirname "${BASH_SOURCE[0]}")/../../support/harness.sh"
cd "$REPO_ROOT/tsan"
TOOLCHAIN=${TOOLCHAIN:-nightly-2026-07-02}

OUT=$(MIRVM_BUILD_ID=0000000000000000 \
    RUSTFLAGS="-Zsanitizer=thread" TSAN_OPTIONS="halt_on_error=1" \
    cargo +"$TOOLCHAIN" run -Zbuild-std --target x86_64-unknown-linux-gnu --release 2>&1)
CODE=$?

echo "$OUT" | tail -8
if [ $CODE -eq 0 ] && ! echo "$OUT" | grep -q "WARNING: ThreadSanitizer"; then
    ok "TSan 零竞争警告，引擎 Sync 判定通过"
else
    echo "$OUT" | grep -B 2 -A 25 "WARNING: ThreadSanitizer" | head -80
    bad "TSan 失败（exit=$CODE）"
fi

suite_summary runtime.tsan
