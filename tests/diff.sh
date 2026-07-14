#!/usr/bin/env bash
# 全量差分（唯一引擎 = M4 字节码 VM；tier-0 已移除，oracle 一直是 native）：
# demo/*.rs 原生编译运行 vs mirvm main 启动链运行，对比 stdout、stderr + 退出码。
# 全绿基线 23/23（M4.4 起含 threads_*；M5.0 起含 asm_probe；真实项目 TDD
# 新增 track_caller_fn_ptr/u128_switch/volatile_wide；M5.2 起含 intrinsic_probe/
# recursion_deep/simd_probe）。
# ecosystem/ffi_zlib 是 frontmatter/cargo
# 形态（引擎 cargo 接线 M4.5），diff.sh 同样 SKIP（见 diff_cargo.sh）。
set -u
cd "$(dirname "$0")/.."

MIRVM=${MIRVM:-target/debug/mirvm}
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
pass=0 fail=0

for src in demo/*.rs; do
    name=$(basename "$src" .rs)
    [ "$name" = "fib" ] || [ -z "${ONLY:-}" ] || [ "$name" = "$ONLY" ] || continue

    # 这两个文件带 Cargo frontmatter，必须由 diff_cargo.sh 编译；除此之外
    # 任意 rustc 失败都是真回归，不能用 SKIP 吞掉。
    if [ "$name" = ecosystem ] || [ "$name" = ffi_zlib ]; then
        echo "SKIP $name (Cargo frontmatter; see diff_cargo.sh)"
        continue
    fi

    # native（同一 pinned toolchain；关掉环境变量干扰）
    rustc --edition 2024 -o "$TMP/$name" "$src" 2>"$TMP/$name.rustc.err" || {
        echo "FAIL $name: rustc 编译失败"
        cat "$TMP/$name.rustc.err"
        fail=$((fail + 1))
        continue
    }
    env -u RUST_BACKTRACE "$TMP/$name" \
        >"$TMP/$name.native.out" 2>"$TMP/$name.native.run.err"
    native_code=$?
    cat "$TMP/$name.rustc.err" "$TMP/$name.native.run.err" >"$TMP/$name.native.err"

    env -u RUST_BACKTRACE "$MIRVM" run "$src" >"$TMP/$name.mirvm.out" 2>"$TMP/$name.mirvm.err"
    mirvm_code=$?

    ok=1
    if ! diff -q "$TMP/$name.native.out" "$TMP/$name.mirvm.out" >/dev/null; then
        ok=0; why="stdout 不一致"
    elif [ "$native_code" != "$mirvm_code" ]; then
        ok=0; why="退出码 native=$native_code mirvm=$mirvm_code"
    fi

    # stderr 对比：只规范化本来就不稳定的线程名与 TID；任一侧多出的诊断都失败。
    if [ $ok = 1 ]; then
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
