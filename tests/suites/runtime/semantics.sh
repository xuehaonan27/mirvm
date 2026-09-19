#!/usr/bin/env bash
# Runtime semantics contract: pure functions, values and memory, unwind and threads.
#
# Four segments (can run single segment, default all):
#   pure    nine pure-function cases — real rustc MIR lowered then engine-executed == mathematical constant;
#           compile standalone harness at end as execution-phase purity gate (vm/ missing rustc type causes compile failure;
#           compile only, TSan execution belongs solely to threads segment, so SKIP_TSAN can fully skip).
#   digest  nine value-and-memory function digests == native (expected values embedded, from same-source rustc -O direct run;
#           changes to demo/m4/digest.rs must be regenerated in sync) + vm-stats M4.1 debt cleared.
#   unwind  nine panic/catch/rethrow cases == native (same-source rustc -O)
#           + real lang_start distinguishes main panic from normal Termination 101
#           + vm-stats M4.1/M4.2 debt cleared.
#   threads five real-thread differential cases == native + two blocking-syscall hang scenarios + rayon sub-second
#           + JIT stack-overflow diagnosis + vm-stats reachable trap-free
#           + TSan multi-thread cases (skipped with SKIP_TSAN=1).
#
# Usage: ./tests/run.sh suite runtime.semantics [pure|digest|unwind|threads|all]
set -u
. "$(dirname "${BASH_SOURCE[0]}")/../../support/harness.sh"
test_enter_repo
MIRVM=${MIRVM:-$(pwd)/target/release/mirvm}
TOOLCHAIN=${TOOLCHAIN:-nightly-2026-07-02}
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
# ---- pure ----
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

    echo "--- purity gate (standalone harness compile) ---"
    if cargo +"$TOOLCHAIN" build --manifest-path tsan/Cargo.toml --release --locked \
        >"$TMP/purity.out" 2>&1; then
        echo "purity gate PASS (vm/ zero rustc_private)"
    else
        echo "purity gate FAIL"
        tail -10 "$TMP/purity.out"
        return 1
    fi
}

# ---- digest ----
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
    # expected values = same-source native (rustc -O) direct run
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

    echo "--- vm-stats recheck (M4.1 in-scope debt cleared) ---"
    local stats
    stats=$("$MIRVM" run --engine vm --vm-stats "$SRC" 2>/dev/null | sed -n '/reachable Traps per export\/entry/,$p')
    if echo "$stats" | grep -E '_digest.*\| .*M4\.1:' >/dev/null; then
        echo "FAIL: reachable set still has M4.1 in-scope debt:"
        echo "$stats" | grep -E '_digest.*M4\.1:'
        return 1
    fi
    echo "M4.1 in-scope debt cleared PASS"
}

# ---- unwind ----
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

    # This export deliberately lets panic propagate to Engine top level. Top-level cleanup resets guest std panic
    # count before dropping payload; payload Drop then runs a normal guest call, and raises,
    # catches and releases a second panic. stdout therefore locks count reset, drop exactly once, and
    # continued execution after the same Engine cleanup.
    local expected mode code
    expected=$(printf '%s\n' \
        'top-payload-drop=1 panicking=false' \
        'normal-after-top-cleanup=42' \
        'second-panic-caught=true panicking=false' \
        'second-payload-drop=1 panicking=false' \
        'payload-drop-counts=1:1 panicking=false')
    for mode in interp jit; do
        if [ "$mode" = interp ]; then
            env -u RUST_BACKTRACE MIRVM_JIT=off \
                "$MIRVM" run --engine vm --vm-call uncaught_payload_cleanup_probe "$SRC" \
                >"$TMP/top-panic.$mode.out" 2>"$TMP/top-panic.$mode.err"
        else
            env -u RUST_BACKTRACE MIRVM_JIT=on MIRVM_JIT_SYNC=1 MIRVM_JIT_THRESHOLD=1 \
                "$MIRVM" run --engine vm --vm-call uncaught_payload_cleanup_probe "$SRC" \
                >"$TMP/top-panic.$mode.out" 2>"$TMP/top-panic.$mode.err"
        fi
        code=$?
        if [ "$code" -eq 101 ] \
            && [ "$(cat "$TMP/top-panic.$mode.out")" = "$expected" ] \
            && rg -q 'guest panic not caught' "$TMP/top-panic.$mode.err"; then
            echo "PASS uncaught payload cleanup ($mode)"
            pass=$((pass + 1))
        else
            echo "FAIL uncaught payload cleanup ($mode): exit=$code"
            cat "$TMP/top-panic.$mode.out"
            tail -20 "$TMP/top-panic.$mode.err"
            pfail=$((pfail + 1))
        fi
    done

    # Must go through real rustc lowering and std::rt::lang_start_internal. Both paths OS
    # exit code is 101; Engine structured result must still distinguish main panic from normal Termination.
    # Bypass base/L2 to prevent manual Module or old cache from faking green; JIT also requires startup closure truly published.
    local main_src=demo/main_outcome_probe.rs panic_code normal_code
    for mode in interp jit; do
        if [ "$mode" = interp ]; then
            env MIRVM_NO_BASE_IMAGE=1 MIRVM_NO_IR_CACHE=1 MIRVM_JIT=off \
                "$MIRVM" run --engine vm "$main_src" -- panic \
                >"$TMP/main-panic.$mode.out" 2>"$TMP/main-panic.$mode.err"
            panic_code=$?
            env MIRVM_NO_BASE_IMAGE=1 MIRVM_NO_IR_CACHE=1 MIRVM_JIT=off \
                "$MIRVM" run --engine vm "$main_src" \
                >"$TMP/main-101.$mode.out" 2>"$TMP/main-101.$mode.err"
            normal_code=$?
        else
            env MIRVM_NO_BASE_IMAGE=1 MIRVM_NO_IR_CACHE=1 MIRVM_JIT=on \
                MIRVM_JIT_SYNC=1 MIRVM_JIT_THRESHOLD=1 MIRVM_JIT_DEBUG=1 \
                "$MIRVM" run --engine vm "$main_src" -- panic \
                >"$TMP/main-panic.$mode.out" 2>"$TMP/main-panic.$mode.err"
            panic_code=$?
            env MIRVM_NO_BASE_IMAGE=1 MIRVM_NO_IR_CACHE=1 MIRVM_JIT=on \
                MIRVM_JIT_SYNC=1 MIRVM_JIT_THRESHOLD=1 MIRVM_JIT_DEBUG=1 \
                "$MIRVM" run --engine vm "$main_src" \
                >"$TMP/main-101.$mode.out" 2>"$TMP/main-101.$mode.err"
            normal_code=$?
        fi
        if [ "$panic_code" -eq 101 ] && [ "$normal_code" -eq 101 ] \
            && rg -q 'main outcome probe' "$TMP/main-panic.$mode.err" \
            && ! rg -q 'main outcome probe' "$TMP/main-101.$mode.err" \
            && ! rg -q 'guest panic not caught' "$TMP/main-panic.$mode.err" \
            && ! rg -q 'guest panic not caught' "$TMP/main-101.$mode.err" \
            && ! rg -q 'release=false|failed to be compiled|panicked at .*translate\.rs' \
                "$TMP/main-panic.$mode.err" \
            && { [ "$mode" = interp ] \
                || rg -q 'release=true .*\(_RNCNvNt[^ ]*_3std2rt19lang_start_internal0C' \
                    "$TMP/main-panic.$mode.err"; }; then
            echo "PASS real main panic != normal Termination 101 ($mode)"
            pass=$((pass + 1))
        else
            echo "FAIL real main outcome classification ($mode): panic=$panic_code normal=$normal_code"
            tail -20 "$TMP/main-panic.$mode.err"
            tail -20 "$TMP/main-101.$mode.err"
            pfail=$((pfail + 1))
        fi
    done
    echo "gate-unwind: $pass pass, $pfail fail"
    [ "$pfail" -eq 0 ] || return 1

    echo "--- vm-stats recheck (M4.1/M4.2 in-scope debt cleared) ---"
    local stats
    stats=$("$MIRVM" run --engine vm --vm-stats "$SRC" 2>/dev/null | sed -n '/reachable Traps per export\/entry/,$p')
    if echo "$stats" | grep -E '_digest.*\| .*M4\.[12]:' >/dev/null; then
        echo "FAIL: reachable set still has M4.1/M4.2 in-scope debt:"
        echo "$stats" | grep -E '_digest.*M4\.[12]:'
        return 1
    fi
    echo "M4.1/M4.2 in-scope debt cleared PASS"
}

# ---- threads ----
run_threads() {
    local pass=0 pfail=0
    tok() { pass=$((pass + 1)); echo "PASS $*"; }
    tbad() { pfail=$((pfail + 1)); echo "FAIL $*"; }

    # ① threads_* five-case differential (stdout + exit code + normalized stderr)
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
            tok "$name (differential == native)"
        else
            tbad "$name (native=$ncode mirvm=$mcode)"
            diff "$TMP/$name.n.out" "$TMP/$name.m.out" | head -5
        fi
    done

    # ② two blocking-syscall hang scenarios (real blocking syscall + real threads; must finish in seconds)
    local out code dt t0
    out=$(timeout 60 "$MIRVM" run corpus/c_blocking_io.rs 2>&1); code=$?
    [ $code -eq 0 ] && [ "$out" = 'got: [104, 105]' ] \
        && tok "c_blocking_io (a blocking read blocks only itself)" || tbad "c_blocking_io (exit=$code: $out)"
    out=$(timeout 60 "$MIRVM" run corpus/c_net_echo_threaded.rs 2>&1); code=$?
    [ $code -eq 0 ] && [ "$out" = 'echo = "echo"' ] \
        && tok "c_net_echo_threaded (threaded loopback server)" || tbad "c_net_echo_threaded (exit=$code: $out)"

    # ③ rayon sub-second (work-stealing pool + par_iter/par_sort)
    t0=$(date +%s%N)
    out=$(timeout 120 "$MIRVM" run corpus/c_rayon.rs 2>&1); code=$?
    dt=$(( ($(date +%s%N) - t0) / 1000000 ))
    if [ $code -eq 0 ] && echo "$out" | grep -q "par_sort ok = true" && [ $dt -lt 20000 ]; then
        tok "c_rayon (${dt}ms, < 20s hard gate)"
    else
        tbad "c_rayon (exit=$code ${dt}ms)"
    fi

    # ④ small stack + threshold=1 forces compiled code: must give clear diagnosis before large-frame prologue,
    # must not exit as SIGSEGV. recursion_deep is the existing permanent deep-recursion probe.
    out=$(env MIRVM_STACK_SIZE=1m MIRVM_JIT_THRESHOLD=1 MIRVM_JIT_SYNC=1 \
        timeout 60 "$MIRVM" run demo/recursion_deep.rs 2>&1); code=$?
    if [ $code -eq 70 ] && echo "$out" | grep -q 'guest stack overflow (JIT compiled frame hit safety margin before entry'; then
        tok "JIT stack overflow clearly diagnosed before frame prologue"
    else
        tbad "JIT stack overflow diagnosis (exit=$code: $out)"
    fi

    # ⑤ --vm-stats recheck: threads demo reachable path trap-free
    for name in threads_spawn threads_panic; do
        if "$MIRVM" run --vm-stats demo/$name.rs 2>/dev/null | grep -q "@entry: ✅ reachable path trap-free"; then
            tok "$name reachable trap-free"
        else
            tbad "$name reachable set has a Trap (vm-stats)"
        fi
    done

    # ⑥ TSan multi-thread cases (8 threads share Shared / per-thread Ctx / thunk factory concurrently; engine Sync)
    if [ -z "${SKIP_TSAN:-}" ]; then
        if bash tests/suites/runtime/tsan.sh >"$TMP/tsan.out" 2>&1; then
            tok "TSan (includes tsan_mt multi-thread real body, zero warnings)"
        else
            tbad "TSan"
            tail -10 "$TMP/tsan.out"
        fi
    else
        skip "TSan (SKIP_TSAN=1)"
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
