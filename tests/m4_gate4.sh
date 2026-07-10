#!/usr/bin/env bash
# M4.4 gate：真线程——threads_* 五用例差分 == native + tier-0 时代挂死双场景 +
# rayon 秒级 + TSan 多线程用例（引擎 Sync）+ --vm-stats 复测（threads 可达 trap-free）。
# 用法：bash tests/m4_gate4.sh          # 全量（含 TSan，首次 build-std 约 1-2 分钟）
#       SKIP_TSAN=1 bash tests/m4_gate4.sh
set -u
cd "$(dirname "$0")/.."
MIRVM=${MIRVM:-$(pwd)/target/release/mirvm}
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
pass=0 fail=0

note() { echo "$@"; }
ok()   { pass=$((pass + 1)); echo "PASS $*"; }
bad()  { fail=$((fail + 1)); echo "FAIL $*"; }

# ---- ① threads_* 五用例差分（stdout + 退出码 + 规范化 stderr）----
for name in threads_spawn threads_channel threads_sync threads_time threads_panic; do
    src=demo/$name.rs
    rustc --edition 2024 -o "$TMP/$name" "$src" 2>/dev/null || { bad "$name (rustc)"; continue; }
    env -u RUST_BACKTRACE "$TMP/$name" >"$TMP/$name.n.out" 2>"$TMP/$name.n.err"; ncode=$?
    env -u RUST_BACKTRACE timeout 60 "$MIRVM" run "$src" >"$TMP/$name.m.out" 2>"$TMP/$name.m.err"; mcode=$?
    sed -E "s/thread '[^']*' \([0-9]+\)/thread 'T'/" "$TMP/$name.n.err" >"$TMP/$name.n.err.x"
    sed -E "s/thread '[^']*' \([0-9]+\)/thread 'T'/" "$TMP/$name.m.err" >"$TMP/$name.m.err.x"
    if diff -q "$TMP/$name.n.out" "$TMP/$name.m.out" >/dev/null \
        && [ "$ncode" = "$mcode" ] \
        && diff -q "$TMP/$name.n.err.x" "$TMP/$name.m.err.x" >/dev/null; then
        ok "$name（差分 == native）"
    else
        bad "$name（native=$ncode mirvm=$mcode）"
        diff "$TMP/$name.n.out" "$TMP/$name.m.out" | head -5
    fi
done

# ---- ② tier-0 时代挂死双场景（真阻塞 syscall + 真线程；须秒级完成）----
out=$(timeout 60 "$MIRVM" run corpus/c_blocking_io.rs 2>&1); code=$?
[ $code -eq 0 ] && [ "$out" = 'got: [104, 105]' ] \
    && ok "c_blocking_io（阻塞 read 只挡自己）" || bad "c_blocking_io（exit=$code: $out）"

out=$(timeout 60 "$MIRVM" run corpus/c_net_echo_threaded.rs 2>&1); code=$?
[ $code -eq 0 ] && [ "$out" = 'echo = "echo"' ] \
    && ok "c_net_echo_threaded（线程化回环服务器）" || bad "c_net_echo_threaded（exit=$code: $out）"

# ---- ③ rayon 秒级（tier-0 28s；work-stealing 池 + par_iter/par_sort）----
t0=$(date +%s%N)
out=$(timeout 120 "$MIRVM" run corpus/c_rayon.rs 2>&1); code=$?
dt=$(( ($(date +%s%N) - t0) / 1000000 ))
if [ $code -eq 0 ] && echo "$out" | grep -q "par_sort ok = true" && [ $dt -lt 20000 ]; then
    ok "c_rayon（${dt}ms，< 20s 硬门）"
else
    bad "c_rayon（exit=$code ${dt}ms）"
fi

# ---- ④ --vm-stats 复测：threads demo 可达路径 trap-free ----
for name in threads_spawn threads_panic; do
    if "$MIRVM" run --vm-stats demo/$name.rs 2>/dev/null | grep -q "@entry: ✅ 可达路径 trap-free"; then
        ok "$name 可达 trap-free"
    else
        bad "$name 可达集有 Trap（vm-stats）"
    fi
done

# ---- ⑤ TSan 多线程用例（8 线程共享 Shared/各自 Ctx/thunk 工厂并发；引擎 Sync）----
if [ -z "${SKIP_TSAN:-}" ]; then
    if bash tests/spike4_tsan.sh >"$TMP/tsan.out" 2>&1; then
        ok "TSan（含 tsan_mt 多线程真身，零警告）"
    else
        bad "TSan"
        tail -10 "$TMP/tsan.out"
    fi
else
    note "SKIP TSan（SKIP_TSAN=1）"
fi

echo "---"
echo "m4-gate4(threads): $pass pass, $fail fail"
[ $fail -eq 0 ]
