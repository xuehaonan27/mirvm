#!/usr/bin/env bash
# 运行时语义合同：纯函数、值与内存、unwind 和线程。
#
# 四段（可单段执行，默认全量）：
#   pure    纯函数九件 —— 真实 rustc MIR 降低后经引擎执行 == 数学常量；
#           末尾编译独立 harness 作执行相纯度门禁（vm/ 漏 rustc 类型即编译失败；
#           只编译，TSan 执行唯一归 threads 段，SKIP_TSAN 因而能完整跳过）。
#   digest  值与内存九函数 digest == native（期望值内嵌，来自同源 rustc -O 直跑，
#           2026-07-08 生成——demo/m4/digest.rs 改动须同步再生）+ vm-stats M4.1 债务清零。
#   unwind  panic/catch/重抛九件 == native（同源 rustc -O，2026-07-09 生成）
#           + vm-stats M4.1/M4.2 债务清零。
#   threads 真线程五用例差分 == native + tier-0 时代挂死双场景 + rayon 秒级
#           + vm-stats 可达 trap-free + TSan 多线程用例（SKIP_TSAN=1 跳过）。
#
# 用法：./tests/run.sh suite runtime.semantics [pure|digest|unwind|threads|all]
set -u
. "$(dirname "${BASH_SOURCE[0]}")/../../support/harness.sh"
test_enter_repo
MIRVM=${MIRVM:-$(pwd)/target/release/mirvm}
TOOLCHAIN=${TOOLCHAIN:-nightly-2026-07-02}
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
# ---- pure（原 m4_gate0）----
run_pure() {
    local SRC=demo/m4/pure.rs pass=0 pfail=0
    pcheck() {
        local spec="$1" want="$2" got code
        got=$("$MIRVM" run --engine vm --vm-call "$spec" "$SRC" 2>"$TMP/pure.err")
        code=$?
        if [ "$code" -eq 0 ] && [ "$got" = "$want" ]; then
            echo "PASS $spec = $got"; pass=$((pass + 1))
        else
            echo "FAIL $spec: got '$got' (exit=$code), want $want"
            head -3 "$TMP/pure.err"; pfail=$((pfail + 1))
        fi
    }
    pcheck 'fib(10)' 55
    pcheck 'fib(25)' 75025
    pcheck 'gcd(1071,462)' 21
    pcheck 'gcd(17,5)' 1
    pcheck 'sum_to(1000)' 500500
    pcheck 'collatz_steps(27)' 111
    pcheck 'popcount_manual(12297829382473034410)' 32
    pcheck 'mix_signed(3,10)' 13
    pcheck 'mix_signed(10,3)' 13
    echo "gate-pure: $pass pass, $pfail fail"
    [ "$pfail" -eq 0 ] || return 1

    echo "--- 纯度门禁（独立 harness 编译）---"
    if cargo +"$TOOLCHAIN" build --manifest-path tsan/Cargo.toml --release --locked \
        >"$TMP/purity.out" 2>&1; then
        echo "纯度门禁 PASS（vm/ 零 rustc_private）"
    else
        echo "纯度门禁 FAIL"
        tail -10 "$TMP/purity.out"
        return 1
    fi
}

# ---- digest（原 m4_gate1）----
run_digest() {
    local SRC=demo/m4/digest.rs pass=0 pfail=0
    dcheck() {
        local spec="$1" want="$2" got code
        got=$("$MIRVM" run --engine vm --vm-call "$spec" "$SRC" 2>"$TMP/digest.err")
        code=$?
        if [ "$code" -eq 0 ] && [ "$got" = "$want" ]; then
            echo "PASS $spec = $got"; pass=$((pass + 1))
        else
            echo "FAIL $spec: got '$got' (exit=$code), want $want"
            head -3 "$TMP/digest.err"; pfail=$((pfail + 1))
        fi
    }
    # 期望值 = 同源 native（rustc -O）直跑
    dcheck 'vec_digest(10)' 155
    dcheck 'string_digest(5)' 14655057013503867354
    dcheck 'map_digest(20)' 2490
    dcheck 'box_digest(6)' 43
    dcheck 'enum_digest(7)' 805
    dcheck 'slice_digest(7)' 284
    dcheck 'static_digest(5)' 913
    dcheck 'rawptr_digest(3)' 84
    dcheck 'float_digest(4)' 4619054945356742656
    echo "gate-digest: $pass pass, $pfail fail"
    [ "$pfail" -eq 0 ] || return 1

    echo "--- vm-stats 复测（M4.1 份内债务清零）---"
    local stats
    stats=$("$MIRVM" run --engine vm --vm-stats "$SRC" 2>/dev/null | sed -n '/各导出\/入口的可达 Trap/,$p')
    if echo "$stats" | grep -E '_digest.*\| .*M4\.1:' >/dev/null; then
        echo "FAIL：可达集中仍有 M4.1 份内债务："
        echo "$stats" | grep -E '_digest.*M4\.1:'
        return 1
    fi
    echo "M4.1 份内债务清零 PASS"
}

# ---- unwind（原 m4_gate2）----
run_unwind() {
    local SRC=demo/m4/unwind.rs pass=0 pfail=0
    ucheck() {
        local spec="$1" want="$2" got code
        got=$("$MIRVM" run --engine vm --vm-call "$spec" "$SRC" 2>"$TMP/unwind.err" | tail -1)
        code=$?
        if [ "$code" -eq 0 ] && [ "$got" = "$want" ]; then
            echo "PASS $spec = $got"; pass=$((pass + 1))
        else
            echo "FAIL $spec: got '$got' (exit=$code), want $want"
            head -3 "$TMP/unwind.err"; pfail=$((pfail + 1))
        fi
    }
    ucheck 'catch_digest(3)' 100010
    ucheck 'catch_digest(2)' 7010
    ucheck 'nested_digest(1)' 77110
    ucheck 'nested_digest(2)' 5110
    ucheck 'bounds_digest(1)' 20
    ucheck 'bounds_digest(9)' 999
    ucheck 'msg_digest(42)' 71
    ucheck 'rethrow_digest(1)' 5503
    ucheck 'rethrow_digest(2)' 1103
    echo "gate-unwind: $pass pass, $pfail fail"
    [ "$pfail" -eq 0 ] || return 1

    echo "--- vm-stats 复测（M4.1/M4.2 份内债务清零）---"
    local stats
    stats=$("$MIRVM" run --engine vm --vm-stats "$SRC" 2>/dev/null | sed -n '/各导出\/入口的可达 Trap/,$p')
    if echo "$stats" | grep -E '_digest.*\| .*M4\.[12]:' >/dev/null; then
        echo "FAIL：可达集中仍有 M4.1/M4.2 份内债务："
        echo "$stats" | grep -E '_digest.*M4\.[12]:'
        return 1
    fi
    echo "M4.1/M4.2 份内债务清零 PASS"
}

# ---- threads（原 m4_gate4）----
run_threads() {
    local pass=0 pfail=0
    tok() { pass=$((pass + 1)); echo "PASS $*"; }
    tbad() { pfail=$((pfail + 1)); echo "FAIL $*"; }

    # ① threads_* 五用例差分（stdout + 退出码 + 规范化 stderr）
    local name ncode mcode
    for name in threads_spawn threads_channel threads_sync threads_time threads_panic; do
        local src=demo/$name.rs
        rustc --edition 2024 -o "$TMP/$name" "$src" 2>/dev/null || { tbad "$name (rustc)"; continue; }
        env -u RUST_BACKTRACE "$TMP/$name" >"$TMP/$name.n.out" 2>"$TMP/$name.n.err"; ncode=$?
        env -u RUST_BACKTRACE timeout 60 "$MIRVM" run "$src" >"$TMP/$name.m.out" 2>"$TMP/$name.m.err"; mcode=$?
        sed -E "s/thread '[^']*' \([0-9]+\)/thread 'T'/" "$TMP/$name.n.err" >"$TMP/$name.n.err.x"
        sed -E "s/thread '[^']*' \([0-9]+\)/thread 'T'/" "$TMP/$name.m.err" >"$TMP/$name.m.err.x"
        if diff -q "$TMP/$name.n.out" "$TMP/$name.m.out" >/dev/null \
            && [ "$ncode" = "$mcode" ] \
            && diff -q "$TMP/$name.n.err.x" "$TMP/$name.m.err.x" >/dev/null; then
            tok "$name（差分 == native）"
        else
            tbad "$name（native=$ncode mirvm=$mcode）"
            diff "$TMP/$name.n.out" "$TMP/$name.m.out" | head -5
        fi
    done

    # ② tier-0 时代挂死双场景（真阻塞 syscall + 真线程；须秒级完成）
    local out code dt t0
    out=$(timeout 60 "$MIRVM" run corpus/c_blocking_io.rs 2>&1); code=$?
    [ $code -eq 0 ] && [ "$out" = 'got: [104, 105]' ] \
        && tok "c_blocking_io（阻塞 read 只挡自己）" || tbad "c_blocking_io（exit=$code: $out）"
    out=$(timeout 60 "$MIRVM" run corpus/c_net_echo_threaded.rs 2>&1); code=$?
    [ $code -eq 0 ] && [ "$out" = 'echo = "echo"' ] \
        && tok "c_net_echo_threaded（线程化回环服务器）" || tbad "c_net_echo_threaded（exit=$code: $out）"

    # ③ rayon 秒级（tier-0 28s；work-stealing 池 + par_iter/par_sort）
    t0=$(date +%s%N)
    out=$(timeout 120 "$MIRVM" run corpus/c_rayon.rs 2>&1); code=$?
    dt=$(( ($(date +%s%N) - t0) / 1000000 ))
    if [ $code -eq 0 ] && echo "$out" | grep -q "par_sort ok = true" && [ $dt -lt 20000 ]; then
        tok "c_rayon（${dt}ms，< 20s 硬门）"
    else
        tbad "c_rayon（exit=$code ${dt}ms）"
    fi

    # ④ --vm-stats 复测：threads demo 可达路径 trap-free
    for name in threads_spawn threads_panic; do
        if "$MIRVM" run --vm-stats demo/$name.rs 2>/dev/null | grep -q "@entry: ✅ 可达路径 trap-free"; then
            tok "$name 可达 trap-free"
        else
            tbad "$name 可达集有 Trap（vm-stats）"
        fi
    done

    # ⑤ TSan 多线程用例（8 线程共享 Shared/各自 Ctx/thunk 工厂并发；引擎 Sync）
    if [ -z "${SKIP_TSAN:-}" ]; then
        if bash tests/suites/runtime/tsan.sh >"$TMP/tsan.out" 2>&1; then
            tok "TSan（含 tsan_mt 多线程真身，零警告）"
        else
            tbad "TSan"
            tail -10 "$TMP/tsan.out"
        fi
    else
        skip "TSan（SKIP_TSAN=1）"
    fi

    echo "gate-threads: $pass pass, $pfail fail"
    [ "$pfail" -eq 0 ]
}

what=${1:-all}
run_part() {
    local name=$1
    shift
    if "$@"; then
        ok "$name"
    else
        bad "$name"
    fi
}
case "$what" in
    pure)    run_part pure run_pure ;;
    digest)  run_part digest run_digest ;;
    unwind)  run_part unwind run_unwind ;;
    threads) run_part threads run_threads ;;
    all)
        echo "== pure ==";    run_part pure run_pure
        echo "== digest ==";  run_part digest run_digest
        echo "== unwind ==";  run_part unwind run_unwind
        echo "== threads =="; run_part threads run_threads
        ;;
    *) echo "usage: ./tests/run.sh suite runtime.semantics [pure|digest|unwind|threads|all]" >&2; exit 64 ;;
esac
suite_summary runtime.semantics
