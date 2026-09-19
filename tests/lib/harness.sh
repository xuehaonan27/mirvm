# shellcheck shell=bash
# tests/lib/harness.sh -- shared library for every mode (sourced, never executed directly).
#
# Provides:
#   1) case_init: the bootstrap a mode starts with (paths, toolchain, product binary, temp dir)
#   2) manifest field access (field / field_required) and field decoding (expand_list)
#   3) command execution with the case's timeout (run_case_cmd) and three-way comparison
#      (normalize_stderr + compare_streams)
#   4) unified PASS/FAIL/SKIP/XFAIL accounting (ok/bad/skip/red + the summary trailer) and timing
#   5) tool and environment checks (require_executable / require_pinned_cargo / rustc_host /
#      ensure_test_sysroot)
#   6) disk and cache management (disk_guard / target_budget_check / cache_snapshot)
#
# No case-specific literal belongs here: paths, timeouts, env, args, tiers and expectations all come
# from tests/manifest. Sourcing scripts run their own `set -u`.

TESTS_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
LIB_DIR=$TESTS_DIR/lib
DATA_DIR=$TESTS_DIR/data
REPO_ROOT=$(cd "$TESTS_DIR/.." && pwd)

case_enter() {
    cd "$REPO_ROOT"
    # Several fixtures stand up registries on 127.0.0.1. Where the machine reaches crates.io through
    # an HTTP proxy, that proxy must not intercept them: Cargo then reports "empty reply from server"
    # and an environment artifact gets billed as a product failure.
    export no_proxy="127.0.0.1,localhost${no_proxy:+,$no_proxy}"
    export NO_PROXY="$no_proxy"
}

require_executable() { # <description> <path>
    local label=$1 path=$2
    [ -n "$path" ] && [ -x "$path" ] || {
        echo "ERROR $label unavailable: ${path:-<unset>}" >&2
        return 69
    }
}

# ---- ① case bootstrap ----
# case_init [--no-product]: enters the repository root, resolves the pinned toolchain, resolves and
# validates the product binary, and hands the mode a private temporary directory with cleanup armed.
#
#   - TOOLCHAIN comes from rust-toolchain.toml. CARGO/RUSTC are taken from the environment when it
#     provides them (tests/run.sh exports both, and the framework self-test drives modes with fakes),
#     and are otherwise resolved through that toolchain's sysroot, falling back to PATH.
#   - MIRVM defaults to the release build; an explicitly exported MIRVM always wins. --no-product
#     skips it for modes whose cases declare product=no in the manifest.
#   - Failure here is an environment error, never a verdict: the case exits 69.
# Exports TOOLCHAIN CARGO RUSTC TMP, plus MIRVM unless --no-product. TMP is removed on exit, so a
# mode that replaces the EXIT trap must remove "$TMP" itself.
case_init() {
    local want_product=1 sysroot
    [ "${1:-}" = "--no-product" ] && want_product=0
    case_enter

    TOOLCHAIN=${TOOLCHAIN:-$(sed -n 's/^channel *= *"\(.*\)"/\1/p' rust-toolchain.toml)}
    if [ -z "${CARGO:-}" ] || [ -z "${RUSTC:-}" ]; then
        sysroot=$(rustc +"$TOOLCHAIN" --print sysroot 2>/dev/null)
        CARGO=${CARGO:-${sysroot:+$sysroot/bin/cargo}}
        RUSTC=${RUSTC:-${sysroot:+$sysroot/bin/rustc}}
        CARGO=${CARGO:-cargo}
        RUSTC=${RUSTC:-rustc}
    fi
    export TOOLCHAIN CARGO RUSTC

    TMP=$(mktemp -d) || { echo "ERROR cannot create a temporary directory" >&2; exit 69; }
    trap 'rm -rf "$TMP"' EXIT
    export TMP

    [ "$want_product" -eq 1 ] || return 0
    MIRVM=${MIRVM:-$REPO_ROOT/target/release/mirvm}
    require_executable MIRVM "$MIRVM" || exit $?
    MIRVM=$(realpath "$MIRVM")
    export MIRVM
}

# ---- ② manifest fields ----
# The dispatcher passes each case's key=value fields as arguments. A mode declares MODE_FIELDS; the
# dispatcher refuses a field the mode does not declare, so a typo in tests/manifest fails loudly.
field() { # <key> [default] -> value on stdout, non-zero when absent and no default
    local key=$1 arg
    for arg in "${CASE_FIELDS[@]}"; do
        case "$arg" in
            "$key"=*) printf '%s\n' "${arg#*=}"; return 0 ;;
        esac
    done
    [ $# -ge 2 ] && { printf '%s\n' "$2"; return 0; }
    return 1
}

field_required() { # <key>
    local value
    value=$(field "$1") || {
        echo "ERROR manifest: case $CASE_NAME (mode $CASE_MODE) needs field $1" >&2
        exit 69
    }
    printf '%s\n' "$value"
}

# case_fixtures: expands the case's fixture field into FIXTURES, as absolute paths under data/.
case_fixtures() {
    FIXTURES=()
    local spec _rels=() _r
    spec=$(field fixture "") || return 0
    expand_list "$spec" _rels
    for _r in ${_rels[@]+"${_rels[@]}"}; do FIXTURES+=("$DATA_DIR/$_r"); done
}

# expand_list <semicolon string> <array name>: splits a manifest list field, decodes %20 to a space,
# and expands {DATA} and {ROOT}. Manifest fields cannot contain a literal space.
expand_list() {
    local _oldifs=$IFS _item
    local -n _out=$2
    IFS=';'
    for _item in $1; do
        [ -n "$_item" ] || continue
        _item=${_item//%20/ }
        _item=${_item//\{DATA\}/$DATA_DIR}
        _item=${_item//\{ROOT\}/$REPO_ROOT}
        _out+=("$_item")
    done
    IFS=$_oldifs
}

# apply_env <semicolon K=V list>: exports each assignment for the rest of the run.
apply_env() {
    local _assignments=() _pair
    [ -n "$1" ] || return 0
    expand_list "$1" _assignments
    for _pair in "${_assignments[@]}"; do
        export "$_pair"
    done
}

# ---- ③ execution and comparison ----
# run_case_cmd <out-prefix> <command...>: runs the command under the case timeout, writing
# <prefix>.out, <prefix>.err and <prefix>.code. Returns 0 unless the command could not be started;
# the guest's own status lands in <prefix>.code so a mode can assert on it.
run_case_cmd() {
    local prefix=$1 code=0
    shift
    timeout "$CASE_TIMEOUT" "$@" >"$prefix.out" 2>"$prefix.err" || code=$?
    printf '%s\n' "$code" >"$prefix.code"
    return 0
}

# normalize_stderr <file> <out>: the only stderr filter allowed by default. A panic header carries
# the thread name and TID, which drift per process; every other byte must match, so a mode that
# filters anything else has to say why in its own header.
normalize_stderr() {
    sed -E "s/thread '[^']*' \([0-9]+\)/thread 'T'/" "$1" >"$2"
}

# compare_streams <label> <authority-prefix> <subject-prefix> [authority-name] [subject-name]:
# exit codes equal, stdout identical, stderr identical after normalize_stderr. Records PASS/FAIL and
# dumps both sides on a mismatch.
compare_streams() {
    local label=$1 a=$2 s=$3 a_name=${4:-authority} s_name=${5:-subject}
    local a_code s_code why=""
    a_code=$(cat "$a.code"); s_code=$(cat "$s.code")
    if [ "$a_code" != "$s_code" ]; then
        why="exit code $a_name=$a_code $s_name=$s_code"
    elif ! cmp -s "$a.out" "$s.out"; then
        why="stdout differs"
    else
        normalize_stderr "$a.err" "$a.err.n"
        normalize_stderr "$s.err" "$s.err.n"
        cmp -s "$a.err.n" "$s.err.n" || why="stderr differs"
    fi
    if [ -z "$why" ]; then
        ok "$label"
        return 0
    fi
    bad "$label ($why)"
    echo "--- $a_name stdout ---"; head -20 "$a.out"
    echo "--- $s_name stdout ---"; head -20 "$s.out"
    echo "--- $a_name stderr ---"; head -20 "$a.err"
    echo "--- $s_name stderr ---"; head -20 "$s.err"
    return 1
}

# ---- ④ accounting and timing ----
pass=0 fail=0 skip_count=0 xfail=0
ok()   { pass=$((pass + 1)); echo "PASS $*"; }
bad()  { fail=$((fail + 1)); echo "FAIL $*"; }
skip() { skip_count=$((skip_count + 1)); echo "SKIP $*"; }
red()  { xfail=$((xfail + 1)); echo "XFAIL $*"; }

record_expected_failure() { # <label> <actual exit> <code:grep-pattern> <stderr file>
    local label=$1 code=$2 spec=$3 errfile=$4
    local want_code=${spec%%:*} want_pattern=${spec#*:}
    if [ "$code" -eq 0 ]; then
        bad "$label XPASS (now green: drop xfail= from the manifest and promote it)"
    elif [ "$code" -eq "$want_code" ] && grep -Eq "$want_pattern" "$errfile"; then
        red "$label ($want_pattern)"
    else
        bad "$label (wanted xfail $want_code/'$want_pattern', actual exit=$code): $(tail -1 "$errfile" | head -c 100)"
    fi
}

case_summary() { # <case id>: print the unified summary and return the case status
    echo "== $1: $pass passed, $skip_count skipped, $xfail expected-failed, $fail failed =="
    [ "$fail" -eq 0 ]
}

now_ms() { date +%s%N; }
SECTION_REPORT=()
_section_t0=0 _section_name=""
section_start() { _section_name="$1"; _section_t0=$(now_ms); }
section_end() {
    local ms=$(( ($(now_ms) - _section_t0) / 1000000 ))
    SECTION_REPORT+=("$(printf '%-28s %8dms' "$_section_name" "$ms")")
    echo "[timing] $_section_name ${ms}ms"
}
print_section_report() {
    [ ${#SECTION_REPORT[@]} -eq 0 ] && return 0
    echo "== section timings =="
    printf '%s\n' "${SECTION_REPORT[@]}"
}

# ---- ⑤ tools and environment ----
rustc_host() { # [rustc]: the host triple, which names the local store's sysroot-<host> directory
    "${1:-${RUSTC:-rustc}}" -vV | sed -n 's/^host: //p'
}

# The Cargo the contract modes judge against must be the pinned nightly, not whatever `cargo` is
# first on PATH. PINNED_CARGO_VERSION is paired with the channel in rust-toolchain.toml; bump both.
PINNED_CARGO_VERSION=${PINNED_CARGO_VERSION:-cargo 1.98.0-nightly}
require_pinned_cargo() {
    local got
    got=$("${CARGO:-cargo}" --version 2>/dev/null) || {
        echo "ERROR cannot run Cargo: ${CARGO:-cargo}" >&2
        return 69
    }
    case "$got" in
        "$PINNED_CARGO_VERSION "*) return 0 ;;
        *)
            echo "ERROR Cargo is not the pinned ${PINNED_CARGO_VERSION}: $got" >&2
            return 69
            ;;
    esac
}

ensure_test_sysroot() { # <mirvm> <test-home> <rustc>; result exported as TEST_SYSROOT
    local mirvm=$1 test_home=$2 rustc=$3 host tmp code=0 shared_sysroot
    host=$(rustc_host "$rustc") || return 69
    TEST_SYSROOT=${MIRVM_SYSROOT:-$test_home/sysroot-$host}
    if [ -d "$TEST_SYSROOT/lib/rustlib/$host/lib" ]; then
        export TEST_SYSROOT
        return 0
    fi
    shared_sysroot=${MIRVM_SHARED_SYSROOT:-$HOME/.mirvm/sysroot-$host}
    if [ -d "$shared_sysroot/lib/rustlib/$host/lib" ]; then
        TEST_SYSROOT=$shared_sysroot
        export TEST_SYSROOT
        return 0
    fi
    mkdir -p "$test_home"
    tmp=$(mktemp -d)
    printf 'fn main() {}\n' >"$tmp/sysroot_probe.rs"
    MIRVM_HOME="$test_home" "$mirvm" run "$tmp/sysroot_probe.rs" \
        >"$tmp/out" 2>"$tmp/err" || code=$?
    if [ "$code" -ne 0 ] || [ ! -d "$TEST_SYSROOT/lib/rustlib/$host/lib" ]; then
        echo "ERROR cannot build the test sysroot: $TEST_SYSROOT (mirvm exit=$code)" >&2
        tail -20 "$tmp/err" >&2
        rm -rf "$tmp"
        return 69
    fi
    rm -rf "$tmp"
    export TEST_SYSROOT
}

# ---- ⑥ disk and cache management ----
_mirvm_home() { printf '%s' "${MIRVM_HOME:-$HOME/.mirvm}"; }

_avail_gb() { # <path> -> available GiB (integer)
    df -Pk "$1" 2>/dev/null | awk 'NR==2{print int($4/1024/1024)}'
}

# disk_guard: below MIRVM_DISK_MIN_GB (default 8G) escalate cleanup -- first old generations plus
# deps/ir (cheap to rebuild), then --target (the shared dependency store), and abort loudly if the
# floor is still not met: never keep running with a full disk.
disk_guard() {
    local min_gb=${MIRVM_DISK_MIN_GB:-8} home_dir avail
    home_dir=$(_mirvm_home)
    [ -d "$home_dir" ] || return 0
    local mirvm=${MIRVM:-$(pwd)/target/release/mirvm}
    avail=$(_avail_gb "$home_dir")
    [ -n "$avail" ] || return 0
    [ "$avail" -ge "$min_gb" ] && return 0
    echo "disk-guard: ${avail}G available < ${min_gb}G floor; clearing old generations + deps/ir"
    "$mirvm" cache purge >/dev/null 2>&1 || true
    "$mirvm" cache purge --deps --ir >/dev/null 2>&1 || true
    avail=$(_avail_gb "$home_dir")
    if [ "$avail" -lt "$min_gb" ]; then
        echo "disk-guard: still ${avail}G; adding --target (rebuilding it is costly, last resort)"
        "$mirvm" cache purge --target >/dev/null 2>&1 || true
        avail=$(_avail_gb "$home_dir")
    fi
    if [ "$avail" -lt "$min_gb" ]; then
        echo "disk-guard: still ${avail}G < ${min_gb}G after both levels; aborting loudly" >&2
        exit 3
    fi
}

# target_budget_check: over MIRVM_TARGET_BUDGET_GB (default 24G) purge --target and report.
target_budget_check() {
    local budget=${MIRVM_TARGET_BUDGET_GB:-24} home_dir mb t
    home_dir=$(_mirvm_home)
    t="$home_dir/target"
    [ -d "$t" ] || return 0
    mb=$(du -sm "$t" 2>/dev/null | cut -f1)
    [ -n "$mb" ] || return 0
    if [ "$mb" -gt $((budget * 1024)) ]; then
        local mirvm=${MIRVM:-$(pwd)/target/release/mirvm}
        echo "target-budget: $t reached ${mb}MiB > ${budget}G budget; running cache purge --target"
        "$mirvm" cache purge --target >/dev/null 2>&1 || true
        echo "target-budget: $(du -sm "$t" 2>/dev/null | cut -f1)MiB after cleanup"
    fi
}

cache_snapshot() { # <label>: cache segment sizes and free disk
    local home_dir
    home_dir=$(_mirvm_home)
    [ -d "$home_dir" ] || { echo "[cache $1] $home_dir does not exist"; return 0; }
    echo "[cache $1] total $(du -sm "$home_dir" 2>/dev/null | cut -f1)MiB; disk available $(_avail_gb "$home_dir")G; segments (MiB):"
    du -sm "$home_dir"/* 2>/dev/null | sort -rn | head -8 | sed "s|$home_dir/||" | awk '{ printf "  %8d  %s\n", $1, $2 }'
}
