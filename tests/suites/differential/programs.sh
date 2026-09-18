#!/usr/bin/env bash
# Full differential (sole engine = M4 bytecode VM; tier-0 removed, oracle always native):
# demo/*.rs native compile+run vs mirvm main startup-chain run, compare stdout, stderr + exit code.
# Historical all-green baseline grew step by step (M4.4+ includes threads_*; M5.0+ includes asm_probe;
# real-project TDD added track_caller_fn_ptr/u128_switch/volatile_wide; M5.2+ includes intrinsic_probe/
# recursion_deep/simd_probe/atomic_order_probe/float_wide_probe/asm_extras_probe/signal_probe/fork_exec_probe/nested_dst_probe/wide_int_probe).
# P1+ includes struct_fnptr_escape (struct-embedded fn-ptr escape negative control, 31/31).
# Step 0+ includes weak_extern/global_asm_guest_fn; batch 9+ includes zst_drop; C1+ includes
# ffi_agg_probe (by-value aggregate FfiAgg synthetic matrix); C3+ includes noreturn_ud2 (asm noreturn
# terminal shape, exit=132 both sides same); E9+ includes dl_iterate_phdr_probe (native callback differential).
# ecosystem/ffi_zlib has Cargo frontmatter and is covered by differential.cargo; this suite explicitly skips it.
set -u
. "$(dirname "${BASH_SOURCE[0]}")/../../support/harness.sh"
test_enter_repo

MIRVM=${MIRVM:-target/debug/mirvm}
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
for src in demo/*.rs; do
    name=$(basename "$src" .rs)
    [ "$name" = "fib" ] || [ -z "${ONLY:-}" ] || [ "$name" = "$ONLY" ] || continue

    # These two files carry Cargo frontmatter and must be compiled by differential.cargo; any other
    # rustc failure is a real regression and must not be swallowed by SKIP.
    if [ "$name" = ecosystem ] || [ "$name" = ffi_zlib ]; then
        skip "$name (Cargo frontmatter; see differential.cargo)"
        continue
    fi

    # native (same pinned toolchain; suppress environment interference)
    rustc --edition 2024 -o "$TMP/$name" "$src" 2>"$TMP/$name.rustc.err" || {
        echo "FAIL $name: rustc compilation failed"
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
        ok=0; why="stdout differs"
    elif [ "$native_code" != "$mirvm_code" ]; then
        ok=0; why="exit code native=$native_code mirvm=$mirvm_code"
    fi

    # stderr comparison: only normalize thread names and TIDs that are inherently unstable; any extra diagnostic on either side fails.
    if [ $ok = 1 ]; then
        sed -E "s/thread '[^']*' \([0-9]+\)/thread 'T'/" "$TMP/$name.native.err" >"$TMP/$name.native.err.n"
        sed -E "s/thread '[^']*' \([0-9]+\)/thread 'T'/" "$TMP/$name.mirvm.err" >"$TMP/$name.mirvm.err.n"
        if ! diff -q "$TMP/$name.native.err.n" "$TMP/$name.mirvm.err.n" >/dev/null; then
            ok=0; why="stderr differs"
        fi
    fi

    # L2 warm rerun (M6 slice 2): second run hits IR cache (first run already committed), output must still match
    # native on all three metrics — guards against "cache replays old semantics / snapshot damage" false greens.
    # Cold/hot differences are allowed only in elapsed time.
    if [ $ok = 1 ]; then
        env -u RUST_BACKTRACE "$MIRVM" run "$src" >"$TMP/$name.mirvm2.out" 2>"$TMP/$name.mirvm2.err"
        mirvm2_code=$?
        sed -E "s/thread '[^']*' \([0-9]+\)/thread 'T'/" "$TMP/$name.mirvm2.err" >"$TMP/$name.mirvm2.err.n"
        if ! diff -q "$TMP/$name.native.out" "$TMP/$name.mirvm2.out" >/dev/null; then
            ok=0; why="L2 warm rerun stdout differs"
        elif [ "$native_code" != "$mirvm2_code" ]; then
            ok=0; why="L2 warm rerun exit code native=$native_code mirvm=$mirvm2_code"
        elif ! diff -q "$TMP/$name.native.err.n" "$TMP/$name.mirvm2.err.n" >/dev/null; then
            ok=0; why="L2 warm rerun stderr differs"
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
        if [ -f "$TMP/$name.mirvm2.out" ]; then
            echo "--- mirvm warm stdout ---"; cat "$TMP/$name.mirvm2.out"
            echo "--- mirvm warm stderr ---"; cat "$TMP/$name.mirvm2.err"
        fi
        fail=$((fail + 1))
    fi
done

suite_summary differential.programs
