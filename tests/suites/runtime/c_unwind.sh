#!/usr/bin/env bash
# C/C-unwind cross-language exception contract: pinned rustc+C++ is the authority; interpreter and real synchronous JIT
# must preserve C++ exception identity, Rust panic identity, Drop counts, plain-C termination boundaries,
# and reject non-C/System ABIs before entering libffi.
set -u
. "$(dirname "${BASH_SOURCE[0]}")/../../support/harness.sh"
suite_init
FIXTURE=$REPO_ROOT/tests/fixtures/c_unwind_contract
CONTRACT_HOME=${MIRVM_CONTRACT_HOME:-${MIRVM_HOME:-$HOME/.mirvm}}

require_executable cargo "$CARGO" || exit $?
require_executable rustc "$RUSTC" || exit $?
command -v "${CXX:-c++}" >/dev/null 2>&1 || { echo "c_unwind: C++ compiler unavailable" >&2; exit 69; }
command -v "${AR:-ar}" >/dev/null 2>&1 || { echo "c_unwind: ar unavailable" >&2; exit 69; }
ensure_test_sysroot "$MIRVM" "$CONTRACT_HOME" "$RUSTC" || exit $?
CONTRACT_SYSROOT=$TEST_SYSROOT

# Pinned Cargo only generates the native oracle. The MIRVM product leg still uses default cargoless,
# with a separate target directory so one leg's build artifacts cannot answer for the other.
CARGO_TARGET_DIR="$TMP/native-target" "$CARGO" build --quiet --locked \
    --manifest-path "$FIXTURE/Cargo.toml" >"$TMP/native-build.out" 2>"$TMP/native-build.err"
native_build=$?
if [ "$native_build" -ne 0 ]; then
    echo "c_unwind: native fixture build failed" >&2
    cat "$TMP/native-build.err" >&2
    exit 1
fi
NATIVE="$TMP/native-target/debug/c-unwind-contract"

# The CLI itself is a process boundary and cannot prove exceptions return from Engine to a C++
# typed catch in the same process. Here we directly link the libmirvm from the same build, constructing the smallest Module with one call.
MIRVM_DIR=$(cd "$(dirname "$MIRVM")" && pwd)
mirvm_dir_code=$?
if [ "$mirvm_dir_code" -ne 0 ]; then
    echo "c_unwind: cannot locate MIRVM directory: $MIRVM" >&2
    exit 69
fi
MIRVM_LIB=${MIRVM_LIB:-$MIRVM_DIR/libmirvm.rlib}
MIRVM_LIB_DIR=$(cd "$(dirname "$MIRVM_LIB")" && pwd)
mirvm_lib_dir_code=$?
if [ "$mirvm_lib_dir_code" -ne 0 ]; then
    echo "c_unwind: cannot locate MIRVM_LIB directory: $MIRVM_LIB" >&2
    exit 69
fi
MIRVM_LIB="$MIRVM_LIB_DIR/${MIRVM_LIB##*/}"
MIRVM_LINK_DEPS="$MIRVM_LIB_DIR/deps"
if [ ! -r "$MIRVM_LIB" ] || [ ! -d "$MIRVM_LINK_DEPS" ]; then
    echo "c_unwind: missing MIRVM_LIB or its sibling deps: $MIRVM_LIB" >&2
    exit 69
fi

# The explicit MIRVM may not be from the same build as rlibs left in the directory. The build id
# exposed by the executable must also actually exist in the rlib; refuse to run the embed leg if they do not match,
# rather than faking green with another engine.
mkdir -p "$TMP/build-id-home"
build_id_home_code=$?
if [ "$build_id_home_code" -ne 0 ]; then
    echo "c_unwind: cannot create build-id temporary directory" >&2
    exit 69
fi
MIRVM_HOME="$TMP/build-id-home" "$MIRVM" cache status \
    >"$TMP/mirvm-status.out" 2>"$TMP/mirvm-status.err"
mirvm_status_code=$?
if [ "$mirvm_status_code" -ne 0 ]; then
    echo "c_unwind: cannot read build id of the MIRVM under test (exit=$mirvm_status_code)" >&2
    cat "$TMP/mirvm-status.err" >&2
    exit 69
fi
MIRVM_BUILD_ID=$(sed -n '1s/^.* (build \([0-9a-f]\{16\}\))$/\1/p' \
    "$TMP/mirvm-status.out")
build_id_parse_code=$?
if [ "$build_id_parse_code" -ne 0 ] || [[ ! "$MIRVM_BUILD_ID" =~ ^[0-9a-f]{16}$ ]]; then
    echo "c_unwind: MIRVM under test did not report a recognizable build id" >&2
    cat "$TMP/mirvm-status.out" >&2
    exit 69
fi
rg -a -F -q "$MIRVM_BUILD_ID" "$MIRVM_LIB"
rlib_id_code=$?
if [ "$rlib_id_code" -ne 0 ]; then
    echo "c_unwind: MIRVM_LIB build id does not match the MIRVM under test: $MIRVM_BUILD_ID" >&2
    exit 69
fi
PROBE_ARCHIVE=$(find "$TMP/native-target/debug/build" \
    -path '*/out/libc_unwind_probe.a' -print -quit)
probe_find_code=$?
if [ "$probe_find_code" -ne 0 ] || [ -z "$PROBE_ARCHIVE" ]; then
    echo "c_unwind: native fixture did not generate C++ probe archive" >&2
    exit 1
fi
PROBE_OUT=${PROBE_ARCHIVE%/*}
RUSTC_SYSROOT=$("$RUSTC" --print sysroot)
rustc_sysroot_code=$?
if [ "$rustc_sysroot_code" -ne 0 ] || [ -z "$RUSTC_SYSROOT" ]; then
    echo "c_unwind: pinned rustc cannot report sysroot" >&2
    exit 69
fi
EMBED="$TMP/embed-engine"
"$RUSTC" "$FIXTURE/embed_engine.rs" --edition=2024 \
    --extern "mirvm=$MIRVM_LIB" -L "dependency=$MIRVM_LINK_DEPS" \
    -L "native=$PROBE_OUT" -l static=c_unwind_probe -l dylib=stdc++ \
    -C prefer-dynamic -C "link-arg=-Wl,-rpath,$RUSTC_SYSROOT/lib" \
    -o "$EMBED" >"$TMP/embed-build.out" 2>"$TMP/embed-build.err"
embed_build=$?
if [ "$embed_build" -ne 0 ]; then
    echo "c_unwind: Engine embed probe build failed" >&2
    cat "$TMP/embed-build.err" >&2
    exit 1
fi

# Build the cargoless project once first. Subsequent formal runs should not carry build.rs/rustc output.
MIRVM_HOME="$TMP/mirvm-home" MIRVM_SYSROOT="$CONTRACT_SYSROOT" \
    MIRVM_TARGET_DIR="$TMP/mirvm-target" MIRVM_DEPS=self MIRVM_JIT=off \
    "$MIRVM" run "$FIXTURE" -- no-throw \
    >"$TMP/prime.out" 2>"$TMP/prime.err"
prime_code=$?
if [ "$prime_code" -ne 0 ]; then
    echo "c_unwind: cargoless fixture priming failed (exit=$prime_code)" >&2
    tail -40 "$TMP/prime.err" >&2
    exit 1
fi

run_native() { # <name> <mode>
    local name=$1 mode=$2
    env -u RUST_BACKTRACE "$NATIVE" "$mode" \
        >"$TMP/$name.native.out" 2>"$TMP/$name.native.err"
    printf '%s' "$?"
}

run_mirvm_command() { # <name> <interp|jit> <mirvm run args...>
    local name=$1 engine=$2
    shift 2
    if [ "$engine" = interp ]; then
        env -u RUST_BACKTRACE MIRVM_HOME="$TMP/mirvm-home" \
            MIRVM_SYSROOT="$CONTRACT_SYSROOT" MIRVM_TARGET_DIR="$TMP/mirvm-target" \
            MIRVM_DEPS=self MIRVM_JIT=off \
            "$MIRVM" run "$@" \
            >"$TMP/$name.interp.out" 2>"$TMP/$name.interp.err"
    else
        env -u RUST_BACKTRACE MIRVM_HOME="$TMP/mirvm-home" \
            MIRVM_SYSROOT="$CONTRACT_SYSROOT" MIRVM_TARGET_DIR="$TMP/mirvm-target" \
            MIRVM_DEPS=self MIRVM_JIT=on MIRVM_JIT_SYNC=1 MIRVM_JIT_THRESHOLD=1 \
            MIRVM_JIT_DEBUG=1 \
            "$MIRVM" run "$@" \
            >"$TMP/$name.jit.out" 2>"$TMP/$name.jit.err"
    fi
    printf '%s' "$?"
}

run_mirvm() { # <name> <mode> <interp|jit>
    run_mirvm_command "$1" "$3" "$FIXTURE" -- "$2"
}

run_mirvm_bin() { # <name> <bin> <interp|jit>
    run_mirvm_command "$1" "$3" "$FIXTURE" --bin "$2"
}

run_embed() { # <name> <mode> <interp|jit>
    local name=$1 mode=$2 engine=$3
    if [ "$engine" = interp ]; then
        env -u RUST_BACKTRACE MIRVM_JIT=off "$EMBED" "$mode" \
            >"$TMP/$name.interp.out" 2>"$TMP/$name.interp.err"
    else
        env -u RUST_BACKTRACE MIRVM_JIT=on MIRVM_JIT_SYNC=1 MIRVM_JIT_THRESHOLD=1 \
            MIRVM_JIT_DEBUG=1 "$EMBED" "$mode" \
            >"$TMP/$name.jit.out" 2>"$TMP/$name.jit.err"
    fi
    printf '%s' "$?"
}

check_success() { # <name> <mode> <expected stdout> <JIT function fragment>
    local name=$1 mode=$2 expected=$3 jit_function=$4 nc ic jc
    nc=$(run_native "$name" "$mode")
    ic=$(run_mirvm "$name" "$mode" interp)
    jc=$(run_mirvm "$name" "$mode" jit)
    if [ "$nc" -ne 0 ] || [ "$ic" -ne 0 ] || [ "$jc" -ne 0 ]; then
        bad "$name exit code native=$nc interp=$ic jit=$jc, expected 0"
        tail -30 "$TMP/$name.interp.err"
        tail -30 "$TMP/$name.jit.err"
    elif [ "$(cat "$TMP/$name.native.out")" != "$expected" ] \
        || ! cmp -s "$TMP/$name.native.out" "$TMP/$name.interp.out" \
        || ! cmp -s "$TMP/$name.native.out" "$TMP/$name.jit.out"; then
        bad "$name stdout did not preserve native result"
        for leg in native interp jit; do echo "--- $leg"; cat "$TMP/$name.$leg.out"; done
    elif rg -q 'release=false|failed to be compiled|panicked at .*translate.rs' \
        "$TMP/$name.jit.err"; then
        bad "$name forced JIT compilation failed"
        tail -40 "$TMP/$name.jit.err"
    elif ! rg -q "release=true.*${jit_function}" "$TMP/$name.jit.err"; then
        bad "$name target function has no machine-code release evidence: $jit_function"
        tail -40 "$TMP/$name.jit.err"
    else
        ok "$name"
    fi
}

check_abort() { # <name> <mode> <native stderr regexp> <mirvm stderr regexp> <JIT function fragment>
    local name=$1 mode=$2 native_pattern=$3 mirvm_pattern=$4 jit_function=$5 nc ic jc
    nc=$(run_native "$name" "$mode")
    ic=$(run_mirvm "$name" "$mode" interp)
    jc=$(run_mirvm "$name" "$mode" jit)
    if [ "$nc" -eq 0 ] || [ "$ic" -eq 0 ] || [ "$jc" -eq 0 ]; then
        bad "$name should have aborted but returned native=$nc interp=$ic jit=$jc"
    elif ! rg -q "$native_pattern" "$TMP/$name.native.err"; then
        bad "$name native did not abort for the expected reason"
        tail -20 "$TMP/$name.native.err"
    elif ! rg -q "$mirvm_pattern" "$TMP/$name.interp.err" \
        || ! rg -q "$mirvm_pattern" "$TMP/$name.jit.err"; then
        bad "$name MIRVM did not abort for the expected ABI reason"
        tail -20 "$TMP/$name.interp.err"
        tail -20 "$TMP/$name.jit.err"
    elif rg -q 'release=false|failed to be compiled|panicked at .*translate.rs' \
        "$TMP/$name.jit.err"; then
        bad "$name forced JIT compilation failed"
        tail -40 "$TMP/$name.jit.err"
    elif ! rg -q "release=true.*${jit_function}" "$TMP/$name.jit.err"; then
        bad "$name target function has no machine-code release evidence: $jit_function"
        tail -40 "$TMP/$name.jit.err"
    elif rg -q '^unexpected ' "$TMP/$name.native.out" "$TMP/$name.interp.out" "$TMP/$name.jit.out"; then
        bad "$name exception was swallowed by C++ catch and returned incorrectly"
    else
        ok "$name"
    fi
}

check_reject() { # <name> <bin> <stderr regexp>
    local name=$1 bin=$2 pattern=$3 ic jc
    ic=$(run_mirvm_bin "$name" "$bin" interp)
    jc=$(run_mirvm_bin "$name" "$bin" jit)
    if [ "$ic" -eq 0 ] || [ "$jc" -eq 0 ]; then
        bad "$name unsupported ABI was silently accepted interp=$ic jit=$jc"
    elif ! rg -q "$pattern" "$TMP/$name.interp.err" \
        || ! rg -q "$pattern" "$TMP/$name.jit.err"; then
        bad "$name did not give a clear ABI rejection diagnosis"
        tail -20 "$TMP/$name.interp.err"
        tail -20 "$TMP/$name.jit.err"
    elif rg -q '^unexpected ' "$TMP/$name.interp.out" "$TMP/$name.jit.out"; then
        bad "$name unsupported ABI was actually called"
    else
        ok "$name"
    fi
}

check_engine_top_typed_catch() {
    local name=engine_top_typed_catch expected='result=1073 caught=73' ic jc jit_filter_code
    ic=$(run_embed "$name" engine-top interp)
    jc=$(run_embed "$name" engine-top jit)
    sed '/^mirvm-jit-debug:/d' "$TMP/$name.jit.err" >"$TMP/$name.jit.unexpected.err"
    jit_filter_code=$?
    if [ "$ic" -ne 0 ] || [ "$jc" -ne 0 ]; then
        bad "$name exit code interp=$ic jit=$jc, expected 0"
        tail -30 "$TMP/$name.interp.err"
        tail -30 "$TMP/$name.jit.err"
    elif [ "$(cat "$TMP/$name.interp.out")" != "$expected" ] \
        || ! cmp -s "$TMP/$name.interp.out" "$TMP/$name.jit.out"; then
        bad "$name did not hand back the original C++ Marker value to the Engine outer typed catch"
        for leg in interp jit; do echo "--- $leg"; cat "$TMP/$name.$leg.out"; done
    elif [ "$jit_filter_code" -ne 0 ]; then
        bad "$name cannot filter JIT evidence output"
    elif [ -s "$TMP/$name.interp.err" ] || [ -s "$TMP/$name.jit.unexpected.err" ]; then
        bad "$name non-contract stderr appeared on successful return"
        cat "$TMP/$name.interp.err"
        cat "$TMP/$name.jit.unexpected.err"
    elif rg -q 'release=false|failed to be compiled|panicked at .*translate.rs' \
        "$TMP/$name.jit.err"; then
        bad "$name forced JIT compilation failed"
        tail -40 "$TMP/$name.jit.err"
    elif ! rg -q 'release=true.*embed_engine_top_foreign' "$TMP/$name.jit.err"; then
        bad "$name target function has no machine-code release evidence: embed_engine_top_foreign"
        tail -40 "$TMP/$name.jit.err"
    else
        ok "$name"
    fi
}

check_foreign_at_guest_catch() {
    local name=foreign_at_guest_catch nc ic jc
    nc=$(run_native "$name" foreign-at-catch-unwind)
    ic=$(run_embed "$name" guest-catch interp)
    jc=$(run_embed "$name" guest-catch jit)
    if [ "$nc" -eq 0 ] || [ "$ic" -eq 0 ] || [ "$jc" -eq 0 ]; then
        bad "$name should have aborted but returned native=$nc interp=$ic jit=$jc"
    elif ! rg -q 'Rust cannot catch foreign exceptions' "$TMP/$name.native.err" \
        || ! rg -q 'Rust cannot catch foreign exceptions' "$TMP/$name.interp.err" \
        || ! rg -q 'Rust cannot catch foreign exceptions' "$TMP/$name.jit.err"; then
        bad "$name did not abort when guest catch_unwind encountered a C++ exception"
        for leg in native interp jit; do echo "--- $leg"; tail -20 "$TMP/$name.$leg.err"; done
    elif rg -q 'release=false|failed to be compiled|panicked at .*translate.rs' \
        "$TMP/$name.jit.err"; then
        bad "$name forced JIT compilation failed"
        tail -40 "$TMP/$name.jit.err"
    elif ! rg -q 'release=true.*embed_guest_try_foreign' "$TMP/$name.jit.err"; then
        bad "$name target function has no machine-code release evidence: embed_guest_try_foreign"
        tail -40 "$TMP/$name.jit.err"
    elif rg -q '^unexpected ' "$TMP/$name.native.out" \
        "$TMP/$name.interp.out" "$TMP/$name.jit.out"; then
        bad "$name C++ exception was incorrectly caught by guest catch_unwind"
    else
        ok "$name"
    fi
}

# Normal returns lock the JIT CallForeign+Cleanup TryCallRet path; repeated execution ensures the target function is still hit after release.
# The two exception cases lock object identity and guest Drop counts.
check_success no_throw no-throw 'value=42 drops=30000' no_throw_with_cleanup
check_success typed_exception typed-catch 'result=1073 caught=73 drops=1' throw_foreign_from_guest
check_success panic_rethrow panic-rethrow 'payload=51 caught=888 drops=1' panic_from_guest

# Real embed boundary: C++ exceptions can pass through the entire Engine unchanged; but once they reach guest
# std::catch_unwind, they must abort like the pinned rustc reference, not masquerade as a Rust panic.
check_engine_top_typed_catch
check_foreign_at_guest_catch

# Termination diagnostic text is not a stable part of the Rust ABI; only lock the specific
# termination reason for native and MIRVM, nonzero status, and that C++ catch must not swallow and continue.
check_abort panic_swallowed panic-swallow \
    'Rust panics must be rethrown' \
    'Rust panics must be rethrown|guest panic was caught but not rethrown|UnwindTerminate' \
    panic_swallowed_from_guest
check_abort plain_c_panic plain-c-panic \
    'panic in a function that cannot unwind|non-unwinding panic' \
    'UnwindTerminate|cannot unwind|non-unwinding panic' \
    panic_from_plain_c_guest
check_abort plain_c_foreign plain-c-foreign \
    'terminate called|cannot unwind|foreign exception' \
    'Rust cannot catch foreign exceptions|unwind reached Terminate boundary' \
    foreign_through_plain_c_guest
check_abort plain_c_foreign_indirect plain-c-foreign-indirect \
    'terminate called|cannot unwind|foreign exception' \
    'Rust cannot catch foreign exceptions|unwind reached Terminate boundary' \
    foreign_indirect_through_plain_c_guest
check_abort plain_c_nested_panic plain-c-nested-panic \
    'panic in a function that cannot unwind|non-unwinding panic' \
    'unwind reached Terminate boundary' \
    nested_panic_through_plain_c_guest
check_abort plain_c_nested_panic_indirect plain-c-nested-panic-indirect \
    'panic in a function that cannot unwind|non-unwinding panic' \
    'unwind reached Terminate boundary' \
    nested_panic_indirect_through_plain_c_guest

# libffi only promises C/System calling conventions. Other ABIs must be explicitly rejected during lowering,
# not dressed up as plain C via `unwind=false` and called speculatively.
check_reject unsupported_outer_abi unsupported_outer_abi \
    'foreign `cpp_no_throw` ABI Rust is not supported for libffi passthrough'
check_reject unsupported_callback_abi unsupported_callback_abi \
    'foreign `cpp_call_plain_c` callback `unsafe fn\(\)` ABI Rust is not supported for a thunk'

suite_summary runtime.c-unwind
