#!/usr/bin/env bash
# Program differential: every freestanding program in tests/scripts/ is compiled by the pinned
# rustc and by mirvm, then both runs must agree on stdout, stderr and exit code (native is the
# authority). The comparison is repeated on a warm run, so a cache replay cannot fake a green.
# Two kinds of file are not part of this batch: `c_*.rs` are corpus drivers (exit-code/oracle
# contract, judged by corpus.run and corpus.contract), and ecosystem/ffi_zlib carry Cargo
# frontmatter (judged by differential.cargo).
# ONLY=<name> restricts the batch to one program (fib always runs).
set -u
. "$(dirname "${BASH_SOURCE[0]}")/../../support/harness.sh"
suite_init
for src in tests/scripts/*.rs; do
    name=$(basename "$src" .rs)
    [ "$name" = "fib" ] || [ -z "${ONLY:-}" ] || [ "$name" = "$ONLY" ] || continue

    case "$name" in
        c_*) continue ;; # corpus driver
        vmcall_*) continue ;; # --vm-call probe: driven by runtime.semantics
        probe_*) continue ;; # owned by one suite (diagnostics, telemetry, x86, cargo mode)
        ecosystem | ffi_zlib)
            skip "$name (Cargo frontmatter; see differential.cargo)"
            continue
            ;;
    esac

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
        normalize_stderr "$TMP/$name.native.err" "$TMP/$name.native.err.n"
        normalize_stderr "$TMP/$name.mirvm.err" "$TMP/$name.mirvm.err.n"
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
        normalize_stderr "$TMP/$name.mirvm2.err" "$TMP/$name.mirvm2.err.n"
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
