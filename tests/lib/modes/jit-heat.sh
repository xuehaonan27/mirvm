#!/usr/bin/env bash
# jit-heat: successful prelinks survive a run that requests another function, then both
# entries remain reusable across further processes. Each export keeps its original result.
# fields: input first second
MODE_FIELDS="input first second"
MODE_REQUIRED="input first second"

mode_run() {
    local input first second home
    input=$(field_required input)
    first=$(field_required first)
    second=$(field_required second)
    case_init
    case "$(rustc_host)" in
        x86_64-unknown-linux-gnu) ;;
        *) skip "$CASE_NAME (persistent JIT relocation requires Linux x86_64)"; return 77 ;;
    esac
    home=$TMP/home
    ensure_test_sysroot "$MIRVM" "$home" "$RUSTC" || return 1

    run_export() {
        run_case_cmd "$TMP/$1" env MIRVM_HOME="$home" MIRVM_SYSROOT="$TEST_SYSROOT" \
            MIRVM_JIT_SYNC=1 MIRVM_JIT_THRESHOLD=1 MIRVM_JIT_DEBUG=1 \
            "$MIRVM" run "$DATA_DIR/$input" --vm-call "$2"
        [ "$(cat "$TMP/$1.code")" = 0 ] || {
            bad "$CASE_NAME ($1 failed)"
            tail -10 "$TMP/$1.err"
            return 1
        }
    }

    run_export first "$first" || return 1
    cat "$home"/cache/package-heat/*.order >"$TMP/first.order"
    [ -s "$TMP/first.order" ] || { bad "$CASE_NAME (no cold heat)"; return 1; }

    run_export second "$second" || return 1
    grep -q 'prelinked [1-9][0-9]* entries from the heat order' "$TMP/second.err" \
        || { bad "$CASE_NAME (no prelink before new demand)"; return 1; }
    cat "$home"/cache/package-heat/*.order >"$TMP/second.order"
    python3 - "$TMP/first.order" "$TMP/second.order" <<'CHECK'
from pathlib import Path
import sys
before, after = (set(Path(p).read_text().split()) for p in sys.argv[1:])
assert before < after, f"prelinked heat lost or new demand absent: {before} -> {after}"
CHECK
    [ "$?" = 0 ] || { bad "$CASE_NAME (partial demand erased prelinked heat)"; return 1; }

    stat -c '%i %y' "$home"/cache/package-heat/*.order >"$TMP/identity.before"
    run_export second_warm "$second" || return 1
    run_export first_warm "$first" || return 1
    cmp -s "$TMP/first.out" "$TMP/first_warm.out" \
        && cmp -s "$TMP/second.out" "$TMP/second_warm.out" \
        || { bad "$CASE_NAME (cached export result changed)"; return 1; }
    cat "$home"/cache/package-heat/*.order >"$TMP/final.order"
    cmp -s "$TMP/second.order" "$TMP/final.order" \
        || { bad "$CASE_NAME (repeated reuse changed heat order)"; return 1; }
    stat -c '%i %y' "$home"/cache/package-heat/*.order >"$TMP/identity.after"
    cmp -s "$TMP/identity.before" "$TMP/identity.after" \
        || { bad "$CASE_NAME (unchanged heat was republished)"; return 1; }
    ok "$CASE_NAME (prelinks and new demand retained across four processes)"
}
