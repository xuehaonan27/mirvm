# shellcheck shell=bash
# tests/support/harness.sh -- shared library for the test suites (sourced, never executed directly).
#
# Provides four groups of helpers:
#   1) unified PASS/FAIL/SKIP/XFAIL accounting (ok/bad/skip/red + the summary trailer)
#   2) cases.manifest parsing (manifest_rows: single source of truth -> normalized rows)
#   3) the corpus driver runner (corpus_run: env/needs/timeout/timing/disk guard in one place)
#   4) disk and cache management (disk_guard / target_budget_check / cache_snapshot)
#
# Convention: sourcing scripts run their own set -u; counters start at 0 when this file is sourced.

TESTS_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
REPO_ROOT=$(cd "$TESTS_DIR/.." && pwd)

test_enter_repo() {
    cd "$REPO_ROOT"
    # Several suites stand up fixture registries on 127.0.0.1. Where the machine
    # reaches crates.io through an HTTP proxy, that proxy must not intercept the
    # fixtures: cargo then reports "empty reply from server" and the gate bills an
    # environment artifact as a product failure.
    export no_proxy="127.0.0.1,localhost${no_proxy:+,$no_proxy}"
    export NO_PROXY="$no_proxy"
}

require_executable() { # <description> <path>
    local label=$1 path=$2
    [ -x "$path" ] || {
        echo "ERROR $label unavailable: $path" >&2
        return 69
    }
}

ensure_test_sysroot() { # <mirvm> <test-home> <rustc>; result exported as TEST_SYSROOT
    local mirvm=$1 test_home=$2 rustc=$3 host tmp code=0 shared_sysroot
    host=$($rustc -vV | sed -n 's/^host: //p') || return 69
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

# ---- ① accounting ----
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

suite_summary() { # <suite-id>: print the unified summary and return the suite status
    local suite_id=$1
    echo "== $suite_id: $pass passed, $skip_count skipped, $xfail expected-failed, $fail failed =="
    [ "$fail" -eq 0 ]
}

# ---- ② timing ----
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

# ---- ③ cases.manifest parsing ----
# Line format (# starts a comment, whitespace separated):
#   name  tier(smoke|full|manual)  timeout-seconds  mode(exit|oracle:<name>|diff)
#         [env=K=V;K=V] [needs=<path>] [args=a;b;c] [xfail=<code>:<grep pattern>]
#         [group=<group>[,<group>...]] (group key, e.g. group=heavy; entries without
#         group= belong to the implicit light group that --group filters on)
# Output (pipe separated, no | inside a field):
#   name|tier|tmo|mode|env|needs|args|xfail|groups
# Usage: manifest_rows <comma-separated tiers|all>   -- filter by tier
# A bad field or enum value reports to stderr and exits non-zero (a registration error must fail loudly).
manifest_rows() {
    local tiers="$1"
    awk -v tiers="$tiers" '
        /^[[:space:]]*(#|$)/ { next }
        {
            name=$1; tier=$2; tmo=$3; mode=$4
            if (tier !~ /^(smoke|full|manual)$/) {
                printf "manifest: %s invalid tier %s\n", name, tier > "/dev/stderr"; bad=1; next
            }
            if (mode !~ /^(exit|diff|oracle:.+)$/) {
                printf "manifest: %s invalid mode %s\n", name, mode > "/dev/stderr"; bad=1; next
            }
            if (tmo !~ /^[0-9]+$/) {
                printf "manifest: %s invalid timeout %s\n", name, tmo > "/dev/stderr"; bad=1; next
            }
            envv=""; needs=""; args=""; xfail=""; groups=""
            for (i = 5; i <= NF; i++) {
                if ($i ~ /^env=/)       envv  = substr($i, 5)
                else if ($i ~ /^needs=/) needs = substr($i, 7)
                else if ($i ~ /^args=/)  args  = substr($i, 6)
                else if ($i ~ /^xfail=/) xfail = substr($i, 7)
                else if ($i ~ /^group=/) groups = substr($i, 7)
                else {
                    printf "manifest: %s unknown field %s\n", name, $i > "/dev/stderr"; bad=1
                }
            }
            if (tiers != "all" && index("," tiers ",", "," tier ",") == 0) next
            printf "%s|%s|%s|%s|%s|%s|%s|%s|%s\n", name, tier, tmo, mode, envv, needs, args, xfail, groups
        }
        END { exit bad }
    ' "$TESTS_DIR/suites/corpus/cases.manifest"
}

# manifest_group_rows <group> [comma-separated tiers|all] -- filter by group (the group key is
# output column 9; entries without group= match only group=light)
manifest_group_rows() {
    local group="$1" tiers="${2:-all}"
    manifest_rows "$tiers" | awk -F'|' -v g="$group" '
        { inlist = ("," $9 ",") ~ ("," g ","); if (g == "light") inlist = ($9 == "") || inlist
          if (inlist) print }
    '
}

# manifest_lookup <name> -- single-row lookup (for filtering a manual batch by name); no row -> non-zero
manifest_lookup() {
    local name="$1" row
    row=$(manifest_rows all | grep -F "|" | awk -F'|' -v n="$name" '$1 == n { print; found=1 } END { exit !found }') \
        || return 1
    printf '%s\n' "$row"
}

# ---- ④ corpus driver runner ----
# corpus_run <outdir> <name> <tmo> <env> <needs> [args...]
# Behavior:
#   - driver location: corpus/c_<name>.rs (script) or corpus/projects/<name>/ (project,
#     mirvm run <directory>); neither exists -> return 2 (registration error, caller records FAIL)
#   - needs absent -> return 77 (caller records SKIP)
#   - export each K=V in the env string item by item, unset after the run
#   - timeout <tmo> wraps mirvm run; stdout/stderr land in <outdir>/<name>.{out,err}
#   - per-driver disk discipline: nothing reads deps/ir images after a run, so purge by default
#     (MIRVM_GATE_KEEP_CACHE=1 bypasses); disk_guard runs before each driver (escalating
#     cleanup when free space bottoms out, and aborting loudly if still short); wall-clock
#     seconds go to ${CORPUS_TIMINGS_FILE:-/dev/null} as "<seconds> <name>" for the slowest list. Returns the mirvm/timeout exit code (needs absent = 77, driver absent = 2).
corpus_run() {
    local outdir="$1" name="$2" tmo="$3" envv="$4" needs="$5"
    shift 5
    local mirvm=${MIRVM:-$(pwd)/target/release/mirvm}
    local src="corpus/c_$name.rs" proj="corpus/projects/$name"
    local target=""
    if [ -f "$src" ]; then
        target="$src"
    elif [ -d "$proj" ]; then
        target="$proj"
    else
        echo "corpus_run: $name is registered in the manifest but has no driver under corpus/" >&2
        return 2
    fi
    if [ -n "$needs" ] && [ ! -e "$needs" ]; then
        return 77
    fi

    disk_guard

    local -a env_names=()
    if [ -n "$envv" ]; then
        local pair k
        local oldifs=$IFS; IFS=';'
        for pair in $envv; do
            [ -n "$pair" ] || continue
            k=${pair%%=*}
            # Decode %20 to a space (manifest fields are whitespace separated, so spaces inside values are encoded)
            pair=${pair//%20/ }
            export "$pair"
            env_names+=("$k")
        done
        IFS=$oldifs
    fi

    local t0 code
    t0=$(date +%s)
    if [ $# -gt 0 ]; then
        timeout "$tmo" "$mirvm" run "$target" -- "$@" \
            >"$outdir/$name.out" 2>"$outdir/$name.err"
    else
        timeout "$tmo" "$mirvm" run "$target" \
            >"$outdir/$name.out" 2>"$outdir/$name.err"
    fi
    code=$?
    local dur=$(( $(date +%s) - t0 ))
    printf '%s %s\n' "$dur" "$name" >>"${CORPUS_TIMINGS_FILE:-/dev/null}" 2>/dev/null || true

    local k
    for k in "${env_names[@]}"; do unset "$k"; done
    # Clear routine environment noise too; leftover flags can leak into the guest.
    unset CARGO_CFG_CURVE25519_DALEK_BACKEND RUSTFLAGS CFLAGS 2>/dev/null || true

    if [ -z "${MIRVM_GATE_KEEP_CACHE:-}" ]; then
        "$mirvm" cache purge --deps --ir >/dev/null 2>&1 || true
    fi
    return "$code"
}

# print_slowest [N] -- slowest list (called at the end of a corpus batch)
print_slowest() {
    local n=${1:-10} f=${CORPUS_TIMINGS_FILE:-}
    [ -n "$f" ] && [ -s "$f" ] || return 0
    echo "== slowest $n corpus drivers (seconds) =="
    sort -rn "$f" | head -"$n" | awk '{ printf "  %6ds  %s\n", $1, $2 }'
}

# parse_args <semicolon string> <array name>: split a manifest args string into an array;
# the {ROOT} placeholder becomes the repo root absolute path. The two legs of a project
# differential have different cwds (mirvm project mode: guest cwd = project dir; native cargo
# run: project dir or the caller's cwd), so a relative path cannot name the same file; an absolute path is cwd-independent and resolves identically.
parse_args() {
    local _root; _root=$(pwd)
    local -n _out=$2
    local _a _oldifs=$IFS; IFS=';'
    for _a in $1; do
        [ -n "$_a" ] && _out+=("${_a//\{ROOT\}/$_root}")
    done
    IFS=$_oldifs
}

# ---- ⑤ disk and cache management ----
_mirvm_home() { printf '%s' "${MIRVM_HOME:-$HOME/.mirvm}"; }

_avail_gb() {  # <path> -> available GiB (integer)
    df -Pk "$1" 2>/dev/null | awk 'NR==2{print int($4/1024/1024)}'
}

# disk_guard: when free space drops below MIRVM_DISK_MIN_GB (default 8G), escalate cleanup:
#   ① clear old generations + deps/ir (cheap to rebuild cold)  ② --target (shared dependency
#   store, expensive)  ③ still short -> exit 3 loudly (never keep running with a full disk)
disk_guard() {
    local min_gb=${MIRVM_DISK_MIN_GB:-8} home_dir
    home_dir=$(_mirvm_home)
    [ -d "$home_dir" ] || return 0
    local mirvm=${MIRVM:-$(pwd)/target/release/mirvm}
    local avail
    avail=$(_avail_gb "$home_dir")
    [ -n "$avail" ] || return 0
    [ "$avail" -ge "$min_gb" ] && return 0
    echo "disk-guard: ${avail}G available < ${min_gb}G floor; clearing old generations + deps/ir"
    "$mirvm" cache purge >/dev/null 2>&1 || true
    "$mirvm" cache purge --deps --ir >/dev/null 2>&1 || true
    avail=$(_avail_gb "$home_dir")
    if [ "$avail" -lt "$min_gb" ]; then
        echo "disk-guard: still ${avail}G; adding --target (rebuilding the shared dependency store is costly, last resort)"
        "$mirvm" cache purge --target >/dev/null 2>&1 || true
        avail=$(_avail_gb "$home_dir")
    fi
    if [ "$avail" -lt "$min_gb" ]; then
        echo "disk-guard: still ${avail}G < ${min_gb}G after both cleanup levels; aborting loudly (free disk space by hand first)" >&2
        exit 3
    fi
}

# target_budget_check: when the unified target dir exceeds MIRVM_TARGET_BUDGET_GB (default 24G),
# purge --target automatically and report. This budget gate keeps the cache from filling the disk;
# the cost is a fully cold rebuild next round, so it only triggers when over budget.
target_budget_check() {
    local budget=${MIRVM_TARGET_BUDGET_GB:-24} home_dir t mb
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

# cache_snapshot <label>: print each cache segment size and free disk (once before and after a run)
cache_snapshot() {
    local home_dir
    home_dir=$(_mirvm_home)
    [ -d "$home_dir" ] || { echo "[cache $1] $home_dir does not exist"; return 0; }
    echo "[cache $1] total $(du -sm "$home_dir" 2>/dev/null | cut -f1)MiB; disk available $(_avail_gb "$home_dir")G; segments (MiB):"
    du -sm "$home_dir"/* 2>/dev/null | sort -rn | head -8 | sed "s|$home_dir/||" | awk '{ printf "  %8d  %s\n", $1, $2 }'
}
