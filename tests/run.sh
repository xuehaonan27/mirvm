#!/usr/bin/env bash
# mirvm standard test entry point. Users and CI run tests only through here.
set -u
. "$(dirname "${BASH_SOURCE[0]}")/support/harness.sh"
test_enter_repo

TEST_STATE_DIR=${MIRVM_TEST_STATE_DIR:-$REPO_ROOT/target/test-state}
mkdir -p "$TEST_STATE_DIR"
MIRVM_CONTRACT_HOME=${MIRVM_CONTRACT_HOME:-${TMPDIR:-/tmp}/mirvm-contract-home}
mkdir -p "$MIRVM_CONTRACT_HOME"
export MIRVM_CONTRACT_HOME
if [ -z "${CARGO_HOME:-}" ] && { [ ! -d "$HOME/.cargo" ] || [ ! -w "$HOME/.cargo" ]; }; then
    export CARGO_HOME="$TEST_STATE_DIR/cargo-home"
    mkdir -p "$CARGO_HOME"
fi
if [ -z "${MIRVM_HOME:-}" ] && [ -d "$HOME/.mirvm" ] && [ ! -w "$HOME/.mirvm" ]; then
    export MIRVM_HOME="$MIRVM_CONTRACT_HOME"
    mkdir -p "$MIRVM_HOME"
fi

TOOLCHAIN=${TOOLCHAIN:-$(sed -n 's/^channel *= *"\(.*\)"/\1/p' rust-toolchain.toml)}
if [ -z "${CARGO:-}" ] || [ -z "${RUSTC:-}" ]; then
    TOOLCHAIN_ROOT=$(rustc +"$TOOLCHAIN" --print sysroot 2>/dev/null) || {
        echo "ERROR pinned Rust toolchain unavailable: $TOOLCHAIN" >&2
        exit 69
    }
    CARGO=${CARGO:-$TOOLCHAIN_ROOT/bin/cargo}
    RUSTC=${RUSTC:-$TOOLCHAIN_ROOT/bin/rustc}
fi
export TOOLCHAIN CARGO RUSTC

suite_record() { # <suite-id>; output: repo-relative script path|purpose
    local id=$1 category name path description
    [[ "$id" =~ ^[a-z0-9]+\.[a-z0-9][a-z0-9-]*$ ]] || return 1
    category=${id%%.*}
    name=${id#*.}
    path="$TESTS_DIR/suites/$category/${name//-/_}.sh"
    [ -f "$path" ] || return 1
    description=$(sed -n '2{s/^#[[:space:]]*//;p;q;}' "$path")
    [ -n "$description" ] || {
        echo "ERROR suite second line missing purpose description: ${path#"$REPO_ROOT"/}" >&2
        return 1
    }
    printf '%s|%s\n' "${path#"$REPO_ROOT"/}" "$description"
}

suite_ids() {
    local path relative id
    while IFS= read -r -d '' path; do
        relative=${path#"$TESTS_DIR/suites/"}
        id=${relative%.sh}
        id=${id//\//.}
        printf '%s\n' "${id//_/-}"
    done < <(find "$TESTS_DIR/suites" -mindepth 2 -maxdepth 2 -type f -name '*.sh' -print0 | sort -z)
}

list_suites() {
    local id record
    while IFS= read -r id; do
        record=$(suite_record "$id") || return 1
        printf '  %-34s %s\n' "$id" "${record#*|}"
    done < <(suite_ids)
}

usage() {
    cat <<'EOF'
Usage:
  ./tests/run.sh fast|smoke|gate
  ./tests/run.sh suite <suite-id> [suite args...]
  ./tests/run.sh list
  ./tests/run.sh help

fast is the daily commit check; smoke adds small real-world loads; gate is the full final gate.
EOF
}

MIRVM_EXPLICIT=0
[ -n "${MIRVM:-}" ] && MIRVM_EXPLICIT=1
PRODUCT_READY=0
ensure_product() {
    [ "$PRODUCT_READY" -eq 0 ] || return 0
    if [ "$MIRVM_EXPLICIT" -eq 0 ]; then
        echo "== build mirvm release =="
        "$CARGO" build --release --locked || return 1
        MIRVM="$REPO_ROOT/target/release/mirvm"
    fi
    require_executable MIRVM "$MIRVM" || return $?
    MIRVM=$(realpath "$MIRVM")
    export MIRVM
    PRODUCT_READY=1
}

suite_needs_product() {
    local record path
    record=$(suite_record "$1") || return 0
    path="$REPO_ROOT/${record%%|*}"
    ! grep -Fxq '# product: no' "$path"
}

TMPDIR_RUN=""
start_run() {
    [ -n "$TMPDIR_RUN" ] && return 0
    TMPDIR_RUN=$(mktemp -d)
    trap 'rm -rf "$TMPDIR_RUN"' EXIT
}

run_logged() { # <title> <command...>
    local title=$1 code=0 log_name
    shift
    start_run
    log_name=${title//[^a-zA-Z0-9_.-]/_}
    section_start "$title"
    "$@" >"$TMPDIR_RUN/$log_name.log" 2>&1 || code=$?
    cat "$TMPDIR_RUN/$log_name.log"
    case "$code" in
        0)  ok "suite $title" ;;
        77) skip "suite $title (host capability insufficient)" ;;
        *)  bad "suite $title（exit=$code）" ;;
    esac
    section_end
    return 0
}

run_suite() { # <display-title> <suite-id> [args...]
    local title=$1 id=$2 record path
    shift 2
    record=$(suite_record "$id") || {
        echo "ERROR unknown suite: $id" >&2
        return 64
    }
    path=${record%%|*}
    if suite_needs_product "$id"; then
        ensure_product || {
            bad "suite $title (product build or MIRVM check failed)"
            return 0
        }
    fi
    run_logged "$title" bash "$path" "$@"
}

run_program_variant() { # <title> [env vars...]
    local title=$1 record path
    shift
    ensure_product || { bad "suite $title (product build failed)"; return 0; }
    record=$(suite_record differential.programs)
    path=${record%%|*}
    run_logged "$title" env "$@" bash "$path"
}

run_fast_obligations() {
    run_suite quality.rust quality.rust
    run_program_variant differential.programs
    run_program_variant differential.programs.jit-sync MIRVM_JIT_SYNC=1 MIRVM_JIT_THRESHOLD=1
    run_suite differential.cargo differential.cargo
    run_suite differential.cargoless differential.cargoless
    run_suite contracts.cargoless-test contracts.cargoless-test
    run_suite contracts.cargoless-workspace contracts.cargoless-workspace
    run_suite contracts.cargoless-git contracts.cargoless-git
    run_suite contracts.cargoless-sources contracts.cargoless-sources
    run_suite contracts.pack contracts.pack
    run_suite contracts.build-script-rerun contracts.build-script-rerun
    run_suite runtime.c-unwind runtime.c-unwind
    run_suite runtime.diagnostics runtime.diagnostics
    run_suite harness.truth harness.truth
}

run_profile() {
    local profile=$1
    start_run
    cache_snapshot "profile $profile before start"
    disk_guard
    case "$profile" in
        fast)
            run_fast_obligations
            ;;
        smoke)
            run_fast_obligations
            run_suite corpus.run.smoke corpus.run --tier smoke
            run_suite runtime.x86-features runtime.x86-features
            run_suite runtime.semantics runtime.semantics
            ;;
        gate)
            run_suite quality.rust quality.rust
            run_program_variant differential.programs
            run_program_variant differential.programs.no-base MIRVM_NO_BASE_IMAGE=1 MIRVM_NO_IR_CACHE=1 ONLY=fib
            run_program_variant differential.programs.jit-sync MIRVM_JIT_SYNC=1 MIRVM_JIT_THRESHOLD=1
            run_program_variant differential.programs.jit-off MIRVM_JIT=off MIRVM_NO_IR_CACHE=1 ONLY=fib
            run_suite differential.cargo differential.cargo
            run_suite differential.cargoless differential.cargoless
            run_suite contracts.cargoless-test contracts.cargoless-test
            run_suite contracts.cargoless-workspace contracts.cargoless-workspace
            run_suite contracts.cargoless-git contracts.cargoless-git
            run_suite contracts.cargoless-sources contracts.cargoless-sources
            run_suite contracts.pack contracts.pack
            run_suite contracts.build-script-rerun contracts.build-script-rerun
            run_suite contracts.deps-image contracts.deps-image
            run_suite corpus.contract corpus.contract
            run_suite runtime.x86-features runtime.x86-features
            run_suite runtime.semantics runtime.semantics
            run_suite runtime.c-unwind runtime.c-unwind
            run_suite runtime.diagnostics runtime.diagnostics
            run_suite runtime.jit-stats runtime.jit-stats
            run_suite performance.limits performance.limits
            run_suite harness.truth harness.truth
            ;;
    esac
    target_budget_check
    cache_snapshot "profile $profile after finish"
    print_section_report
    suite_summary "profile.$profile"
}

cmd=${1:-help}
[ $# -gt 0 ] && shift
case "$cmd" in
    fast|smoke|gate)
        [ $# -eq 0 ] || { usage >&2; exit 64; }
        run_profile "$cmd"
        ;;
    suite)
        [ $# -ge 1 ] || { usage >&2; exit 64; }
        id=$1; shift
        start_run
        run_suite "$id" "$id" "$@" || exit $?
        print_section_report
        suite_summary "run.$id"
        ;;
    list)
        [ $# -eq 0 ] || { usage >&2; exit 64; }
        list_suites
        ;;
    help|-h|--help)
        usage
        ;;
    corpus)
        start_run
        run_suite corpus.run corpus.run "$@"
        suite_summary run.corpus
        ;;
    perf)
        start_run
        run_suite performance.limits performance.limits "$@"
        suite_summary run.performance
        ;;
    *)
        echo "ERROR unknown command: $cmd" >&2
        usage >&2
        exit 64
        ;;
esac
