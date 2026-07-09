#!/usr/bin/env bash
# M4.3 gate：全量差分（新引擎 --engine vm）——demo/*.rs 原生编译运行 vs 新引擎
# main 启动链运行，对比 stdout + 退出码。
# 预期红（记账）：threads_*（真线程 M4.4）；ecosystem/ffi_zlib 是 frontmatter/cargo
# 形态（引擎 cargo 接线 M4.5），diff.sh 同样 SKIP。
set -u
cd "$(dirname "$0")/.."

MIRVM=${MIRVM:-target/debug/mirvm}
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
pass=0 fail=0

for src in demo/*.rs; do
    name=$(basename "$src" .rs)
    [ "$name" = "fib" ] || [ -z "${ONLY:-}" ] || [ "$name" = "$ONLY" ] || continue

    # native（同一 pinned toolchain；关掉环境变量干扰）
    rustc --edition 2024 -o "$TMP/$name" "$src" 2>"$TMP/$name.rustc.err" || {
        echo "SKIP $name (rustc 编译失败)"; continue; }
    env -u RUST_BACKTRACE "$TMP/$name" >"$TMP/$name.native.out" 2>"$TMP/$name.native.err"
    native_code=$?

    env -u RUST_BACKTRACE "$MIRVM" run --engine vm "$src" >"$TMP/$name.mirvm.out" 2>"$TMP/$name.mirvm.err"
    mirvm_code=$?

    ok=1
    if ! diff -q "$TMP/$name.native.out" "$TMP/$name.mirvm.out" >/dev/null; then
        ok=0; why="stdout 不一致"
    elif [ "$native_code" != "$mirvm_code" ]; then
        ok=0; why="退出码 native=$native_code mirvm=$mirvm_code"
    fi

    # stderr 弱对比：规范化线程名与 TID 后比较（仅当 native 有 stderr 输出时）
    if [ $ok = 1 ] && [ -s "$TMP/$name.native.err" ]; then
        sed -E "s/thread '[^']*' \([0-9]+\)/thread 'T'/" "$TMP/$name.native.err" >"$TMP/$name.native.err.n"
        sed -E "s/thread '[^']*' \([0-9]+\)/thread 'T'/" "$TMP/$name.mirvm.err" >"$TMP/$name.mirvm.err.n"
        if ! diff -q "$TMP/$name.native.err.n" "$TMP/$name.mirvm.err.n" >/dev/null; then
            ok=0; why="stderr 不一致"
        fi
    fi

    if [ $ok = 1 ]; then
        echo "PASS $name"
        pass=$((pass + 1))
    else
        echo "FAIL $name: $why"
        echo "--- native stdout ---"; cat "$TMP/$name.native.out"
        echo "--- mirvm stdout ---"; cat "$TMP/$name.mirvm.out"
        echo "--- native stderr ---"; cat "$TMP/$name.native.err"
        echo "--- mirvm stderr ---"; cat "$TMP/$name.mirvm.err"
        fail=$((fail + 1))
    fi
done

echo "== $pass passed, $fail failed =="
[ $fail = 0 ]
