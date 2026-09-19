#!/usr/bin/env bash
# tests/run.sh -- the only test entry point. It reads tests/manifest, selects cases, dispatches each
# to its mode in tests/lib/modes/, and summarizes. It contains no case-specific knowledge: paths,
# timeouts, tiers, env, args, expectations and verdicts all come from the manifest.
#
#   ./tests/run.sh tier fast|smoke|gate     every case at that tier or below (manual excluded)
#   ./tests/run.sh case <name> [args...]    one case; extra args reach the mode
#   ./tests/run.sh mode <mode> [args...]    every case using that mode
#   ./tests/run.sh list [--mode M] [--tier T]
#   ./tests/run.sh modes                    the available modes and their purpose
#   ./tests/run.sh inventory                manifest <-> data/ cross-check, no orphans, no scripts
#   ./tests/run.sh help
#
# Makefile is the interface (`make test|smoke|gate`); this script is its implementation.
set -u

TESTS_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
LIB_DIR=$TESTS_DIR/lib
DATA_DIR=$TESTS_DIR/data
REPO_ROOT=$(cd "$TESTS_DIR/.." && pwd)
MANIFEST=$TESTS_DIR/manifest
export TESTS_DIR LIB_DIR DATA_DIR REPO_ROOT

# The shared library defines the field decoders and helpers the dispatcher also uses.
# shellcheck source=/dev/null
. "$LIB_DIR/harness.sh"

TIERS="fast smoke gate manual"
usage() {
    sed -n '2,14p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

# ---- manifest parsing ----
# Format: <name> <mode> <tier> <timeout> [key=value ...]. Comments (#) and blank lines are ignored.
# Returns normalized rows: name|mode|tier|timeout|fields.
manifest_rows() {
    awk '
        /^[[:space:]]*(#|$)/ { next }
        {
            name = $1; mode = $2; tier = $3; tmo = $4
            if (name in seen) { printf "manifest: duplicate case %s\n", name > "/dev/stderr"; bad = 1 }
            seen[name] = 1
            if (mode == "") { printf "manifest: %s has no mode\n", name > "/dev/stderr"; bad = 1 }
            if (tier !~ /^(fast|smoke|gate|manual)$/) {
                printf "manifest: %s invalid tier %s\n", name, tier > "/dev/stderr"; bad = 1
            }
            if (tmo !~ /^[0-9]+$/) {
                printf "manifest: %s invalid timeout %s\n", name, tmo > "/dev/stderr"; bad = 1
            }
            fields = ""
            for (i = 5; i <= NF; i++) {
                if ($i !~ /^[a-z_]+=/) { printf "manifest: %s bad field %s\n", name, $i > "/dev/stderr"; bad = 1 }
                fields = fields (fields == "" ? "" : " ") $i
            }
            printf "%s|%s|%s|%s|%s\n", name, mode, tier, tmo, fields
        }
        END { exit bad }
    ' "$MANIFEST"
}

# mode_meta <mode> -> "<declared fields>|<required fields>|<product>" ; also validates the mode exists
mode_meta() {
    local mode=$1 path=$LIB_DIR/modes/$mode.sh
    [ -f "$path" ] || { echo "ERROR unknown mode: $mode ($path)" >&2; return 69; }
    (
        set -u
        . "$LIB_DIR/harness.sh"
        # shellcheck source=/dev/null
        . "$path"
        printf '%s|%s|%s\n' "${MODE_FIELDS:-}" "${MODE_REQUIRED:-}" "${MODE_PRODUCT:-yes}"
    ) || return 69
}

mode_purpose() {
    sed -n '2{s/^#[[:space:]]*//;p;q;}' "$LIB_DIR/modes/$1.sh"
}

mode_needs_product() {
    local meta
    meta=$(mode_meta "$1") || return 0
    [ "${meta##*|}" = "no" ] && return 1
    return 0
}

# ---- selection ----
declare -a SELECTED=()
select_rows() { # <filter kind> <value> [extra args ignored]
    local kind=$1 value=$2 row name mode tier
    while IFS='|' read -r name mode tier _tmo _fields; do
        [ -n "$name" ] || continue
        case "$kind" in
            all) SELECTED+=("$name") ;;
            tier)
                case "$tier" in
                    manual) [ "$value" = manual ] && SELECTED+=("$name") ;;
                    fast) [ "$value" = fast ] && SELECTED+=("$name") ;;
                    smoke) case "$value" in fast | smoke) SELECTED+=("$name") ;; esac ;;
                    gate) case "$value" in fast | smoke | gate) SELECTED+=("$name") ;; esac ;;
                esac ;;
            case) [ "$name" = "$value" ] && SELECTED+=("$name") ;;
            mode) [ "$mode" = "$value" ] && SELECTED+=("$name") ;;
        esac
    done < <(manifest_rows) || exit $?
}

row_of() { # <name> -> the manifest row, non-zero when absent
    manifest_rows | awk -F'|' -v n="$1" '$1 == n { print; found = 1 } END { exit !found }'
}

# ---- execution ----
PRODUCT_READY=0
ensure_product() {
    [ "$PRODUCT_READY" -eq 0 ] || return 0
    if [ -z "${MIRVM:-}" ]; then
        echo "== build mirvm release =="
        "${CARGO:-cargo}" build --release --locked || return 1
        MIRVM=$REPO_ROOT/target/release/mirvm
    fi
    [ -x "$MIRVM" ] || { echo "ERROR product binary unavailable: $MIRVM" >&2; return 69; }
    MIRVM=$(realpath "$MIRVM")
    export MIRVM
    PRODUCT_READY=1
}

RUN_TMP=""
start_run() {
    [ -n "$RUN_TMP" ] && return 0
    RUN_TMP=$(mktemp -d)
    trap 'rm -rf "$RUN_TMP"' EXIT
}

CASE_PASS=0 CASE_FAIL=0 CASE_SKIP=0

run_case() { # <name> [extra mode args...]
    local name=$1
    shift
    local row mode tier timeout fields meta declared required
    row=$(row_of "$name") || { echo "ERROR unknown case: $name" >&2; return 64; }
    IFS='|' read -r _ mode tier timeout fields <<<"$row"
    meta=$(mode_meta "$mode") || return 69
    IFS='|' read -r declared required _ <<<"$meta"

    local field key
    for field in $fields; do
        key=${field%%=*}
        case " $declared " in
            *" $key "*) ;;
            *) echo "ERROR manifest: case $name sets $key, which mode $mode does not declare" >&2; return 69 ;;
        esac
    done
    for key in $required; do
        case " $fields " in
            *" $key"=*) ;;
            *) echo "ERROR manifest: case $name is missing required field $key" >&2; return 69 ;;
        esac
    done

    if mode_needs_product "$mode"; then
        ensure_product || { echo "FAIL $name (product build failed)"; CASE_FAIL=$((CASE_FAIL + 1)); return 0; }
    fi

    start_run
    local log=$RUN_TMP/$name.log code=0
    section_start "$name"
    CASE_NAME=$name CASE_MODE=$mode CASE_TIER=$tier CASE_TIMEOUT=$timeout \
        bash -c '
            set -u
            . "$LIB_DIR/harness.sh"
            # shellcheck source=/dev/null
            . "$LIB_DIR/modes/$CASE_MODE.sh"
            CASE_FIELDS=("$@")
            mode_run
        ' _ $fields "$@" >"$log" 2>&1 || code=$?
    cat "$log"
    section_end
    case "$code" in
        0)  ok "case $name"; CASE_PASS=$((CASE_PASS + 1)) ;;
        77) skip "case $name (host capability insufficient)"; CASE_SKIP=$((CASE_SKIP + 1)) ;;
        *)  bad "case $name (exit=$code)"; CASE_FAIL=$((CASE_FAIL + 1)) ;;
    esac
    return 0
}

run_selected() {
    local name
    start_run
    for name in "${SELECTED[@]}"; do
        run_case "$name" "$@"
    done
    print_section_report
    echo "== ${SELECTED[*]:-nothing selected}: $CASE_PASS passed, $CASE_SKIP skipped, $CASE_FAIL failed =="
    [ "$CASE_FAIL" -eq 0 ] || return 1
    [ "$CASE_PASS" -eq 0 ] && [ "$CASE_SKIP" -gt 0 ] && return 77
    return 0
}

# ---- inventory ----
# Cross-checks the manifest against data/: every referenced path exists, every file under data/ is
# covered by some reference, and no control script hides inside data/.
cmd_inventory() {
    local bad=0 row name mode tier timeout fields field key value
    local -a refs=()
    while IFS='|' read -r name mode tier _timeout fields; do
        [ -n "$name" ] || continue
        for field in $fields; do
            key=${field%%=*}
            value=${field#*=}
            case "$key" in
                input | crate)
                    case "$value" in
                        /*) continue ;;
                    esac
                    refs+=("$DATA_DIR/$value")
                    [ -e "$DATA_DIR/$value" ] || { echo "MISSING (case $name): data/$value"; bad=1; } ;;
                needs)
                    case "$value" in
                        /*) continue ;;
                    esac
                    local need=${value//\{DATA\}/$DATA_DIR}
                    need=${need//\{ROOT\}/$REPO_ROOT}
                    [ -e "$need" ] || { echo "MISSING (case $name): $value"; bad=1; } ;;
                fixture)
                    local one
                    local -a parts=()
                    expand_list "$value" parts
                    for one in ${parts[@]+"${parts[@]}"}; do
                        refs+=("$DATA_DIR/$one")
                        [ -e "$DATA_DIR/$one" ] || { echo "MISSING (case $name): data/$one"; bad=1; }
                    done ;;
                args)
                    local a
                    local -a argv=()
                    expand_list "$value" argv
                    for a in ${argv[@]+"${argv[@]}"}; do
                        case "$a" in
                            "$DATA_DIR"/*) refs+=("$a"); [ -e "$a" ] || { echo "MISSING (case $name): ${a#"$DATA_DIR"/}"; bad=1; } ;;
                        esac
                    done ;;
                verdict)
                    case "$value" in
                        oracle:*)
                            refs+=("$DATA_DIR/fixtures/oracles/${value#oracle:}.txt")
                            [ -f "$DATA_DIR/fixtures/oracles/${value#oracle:}.txt" ] ||
                                { echo "MISSING (case $name): oracles/${value#oracle:}.txt"; bad=1; } ;;
                    esac ;;
            esac
        done
    done < <(manifest_rows) || exit $?

    local file covered ref
    while IFS= read -r file; do
        covered=0
        for ref in ${refs[@]+"${refs[@]}"}; do
            case "$file" in "$ref" | "$ref"/*) covered=1; break ;; esac
        done
        [ "$covered" -eq 1 ] || { echo "ORPHAN (no manifest case references it): ${file#"$TESTS_DIR"/}"; bad=1; }
    done < <(find "$DATA_DIR" -type f ! -name '.*')

    while IFS= read -r file; do
        echo "CONTROL IN DATA (no script may live under data/): ${file#"$TESTS_DIR"/}"
        bad=1
    done < <(find "$DATA_DIR" -type f \( -name '*.sh' -o -name '*.bash' \))

    [ "$bad" -eq 0 ] && echo "inventory: manifest and data/ agree"
    return "$bad"
}

# ---- commands ----
cmd=${1:-help}
[ $# -gt 0 ] && shift
case "$cmd" in
    tier)
        [ $# -ge 1 ] || { usage >&2; exit 64; }
        select_rows tier "$1"
        run_selected
        ;;
    case)
        [ $# -ge 1 ] || { usage >&2; exit 64; }
        name=$1; shift
        select_rows case "$name"
        [ ${#SELECTED[@]} -gt 0 ] || { echo "ERROR unknown case: $name" >&2; exit 64; }
        run_selected "$@"
        ;;
    mode)
        [ $# -ge 1 ] || { usage >&2; exit 64; }
        mode=$1; shift
        select_rows mode "$mode"
        [ ${#SELECTED[@]} -gt 0 ] || { echo "ERROR no case uses mode: $mode" >&2; exit 64; }
        run_selected "$@"
        ;;
    list)
        filter_mode=""; filter_tier=""
        while [ $# -gt 0 ]; do
            case "$1" in
                --mode) filter_mode=$2; shift 2 ;;
                --tier) filter_tier=$2; shift 2 ;;
                *) usage >&2; exit 64 ;;
            esac
        done
        printf '%-34s %-20s %-7s %s\n' NAME MODE TIER TIMEOUT
        while IFS='|' read -r name mode tier timeout _fields; do
            [ -n "$name" ] || continue
            [ -n "$filter_mode" ] && [ "$mode" != "$filter_mode" ] && continue
            [ -n "$filter_tier" ] && [ "$tier" != "$filter_tier" ] && continue
            printf '%-34s %-20s %-7s %s\n' "$name" "$mode" "$tier" "$timeout"
        done < <(manifest_rows) || exit $?
        ;;
    modes)
        for path in "$LIB_DIR"/modes/*.sh; do
            mode=$(basename "$path" .sh)
            printf '%-20s %s\n' "$mode" "$(mode_purpose "$mode")"
        done
        ;;
    inventory)
        cmd_inventory
        ;;
    help | -h | --help | "")
        usage
        ;;
    *)
        echo "ERROR unknown command: $cmd" >&2
        usage >&2
        exit 64
        ;;
esac
