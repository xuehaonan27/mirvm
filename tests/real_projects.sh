#!/usr/bin/env bash
# 固定真实 Cargo 项目的 correctness / benchmark 入口。
# 当前只实现首个 tracer：prepare、native↔mirvm 精确差分，以及 correctness 失败时拒绝 bench。
set -u
cd "$(dirname "$0")/.."

usage() {
    echo "usage: bash tests/real_projects.sh <prepare|check|bench> CASE.toml" >&2
    exit 64
}

[ "$#" -eq 2 ] || usage
MODE=$1
CASE_FILE=$2
case "$MODE" in
    prepare|check|bench) ;;
    *) usage ;;
esac
[ -f "$CASE_FILE" ] || { echo "case_not_found: $CASE_FILE" >&2; exit 66; }

PROJECT_SUITE_ROOT=${PROJECT_SUITE_ROOT:-${XDG_CACHE_HOME:-$HOME/.cache}/mirvm/project-suite}
PROJECT_SUITE_ARTIFACTS=${PROJECT_SUITE_ARTIFACTS:-target/project-suite}
PROJECT_SUITE_MIRVM_CACHE=${PROJECT_SUITE_MIRVM_CACHE:-$PROJECT_SUITE_ROOT/mirvm-xdg}
MIRVM=${MIRVM:-$(pwd)/target/release/mirvm}
GIT=${GIT:-$(command -v git)}
PYTHON=${PYTHON:-$(command -v python3)}
BWRAP=${BWRAP:-$(command -v bwrap || true)}
RUSTUP=${RUSTUP:-$(command -v rustup || true)}
if [ -z "${CARGO:-}" ]; then
    if [ -n "$RUSTUP" ]; then
        CARGO=$("$RUSTUP" which cargo 2>/dev/null) || CARGO=$(command -v cargo)
    else
        CARGO=$(command -v cargo)
    fi
fi
if [ -z "${RUSTC:-}" ]; then
    if [ -n "$RUSTUP" ]; then
        RUSTC=$("$RUSTUP" which rustc 2>/dev/null) || RUSTC=$(command -v rustc)
    else
        RUSTC=$(command -v rustc)
    fi
fi
HOST_HOME=${HOME:-}
HOST_RUSTUP_HOME=${RUSTUP_HOME:-$HOST_HOME/.rustup}
HOST_TARGET=${PROJECT_SUITE_TARGET:-$("$RUSTC" -vV | sed -n 's/^host: //p')}
if [ -z "$HOST_TARGET" ]; then
    echo "host_target_unavailable: $RUSTC -vV" >&2
    exit 69
fi
SYSTEM_PATH="$(dirname "$CARGO"):$(dirname "$RUSTC"):/usr/bin:/bin"
RUSTDOC="$(dirname "$RUSTC")/rustdoc"

PARSED=$(mktemp)
trap 'rm -f "$PARSED"' EXIT
"$PYTHON" - "$CASE_FILE" >"$PARSED" <<'PY'
import pathlib
import re
import sys
import tomllib

path = pathlib.Path(sys.argv[1])
try:
    with path.open("rb") as source:
        case = tomllib.load(source)

    def reject_unknown(table: dict, allowed: set[str], scope: str = "") -> None:
        unknown = sorted(set(table) - allowed)
        if unknown:
            paths = [f"{scope}.{key}" if scope else key for key in unknown]
            label = "key" if len(paths) == 1 else "keys"
            raise ValueError(f"unknown {label}: {', '.join(paths)}")

    reject_unknown(case, {
        "name", "repo", "rev", "lock_sha256", "subdir", "args",
        "expected_exit", "timeout_seconds", "normalizers", "xfail",
        "bench", "env",
    })

    def text(name: str) -> str:
        value = case[name]
        if not isinstance(value, str) or "\0" in value:
            raise ValueError(f"{name} must be a string")
        return value

    name = text("name")
    repo = text("repo")
    rev = text("rev")
    lock_sha256 = text("lock_sha256")
    subdir = case.get("subdir", ".")
    args = case.get("args", [])
    expected_exit = case.get("expected_exit", 0)
    timeout_seconds = case.get("timeout_seconds", 60)
    normalizers = case.get("normalizers", [])
    xfail = case.get("xfail")
    bench = case.get("bench", {})
    environment = case.get("env", {})

    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]*", name):
        raise ValueError("name contains unsupported characters")
    if not re.fullmatch(r"[0-9a-fA-F]{40}", rev):
        raise ValueError("rev must be a full 40-hex commit")
    if not re.fullmatch(r"[0-9a-fA-F]{64}", lock_sha256):
        raise ValueError("lock_sha256 must be 64 hex characters")
    if not isinstance(subdir, str) or pathlib.PurePosixPath(subdir).is_absolute():
        raise ValueError("subdir must be relative")
    if ".." in pathlib.PurePosixPath(subdir).parts:
        raise ValueError("subdir must not escape the checkout")
    if not isinstance(args, list) or not all(isinstance(arg, str) for arg in args):
        raise ValueError("args must be an array of strings")
    if type(expected_exit) is not int or not 0 <= expected_exit <= 255:
        raise ValueError("expected_exit must be an integer from 0 to 255")
    reserved_exits = {124, 125, 126, 127, 137}
    if expected_exit in reserved_exits:
        raise ValueError(f"expected_exit {expected_exit} is reserved by project-suite")
    if type(timeout_seconds) is not int or timeout_seconds <= 0:
        raise ValueError("timeout_seconds must be a positive integer")
    if normalizers != []:
        raise ValueError("normalizers are not implemented by this tracer")
    if xfail is None:
        xfail_exit = ""
        xfail_diagnostic = ""
    else:
        if not isinstance(xfail, dict):
            raise ValueError("xfail must be a table")
        reject_unknown(xfail, {"mirvm_exit", "diagnostic"}, "xfail")
        xfail_exit = xfail.get("mirvm_exit")
        xfail_diagnostic = xfail.get("diagnostic")
        if type(xfail_exit) is not int or not 0 <= xfail_exit <= 255:
            raise ValueError("xfail.mirvm_exit must be an integer from 0 to 255")
        if xfail_exit in reserved_exits:
            raise ValueError(f"xfail.mirvm_exit {xfail_exit} is reserved by project-suite")
        if (
            not isinstance(xfail_diagnostic, str)
            or not xfail_diagnostic.startswith("mirvm")
            or any(char in xfail_diagnostic for char in ("\0", "\r", "\n"))
        ):
            raise ValueError("xfail.diagnostic must be one canonical mirvm line")
        xfail_exit = str(xfail_exit)
    if not isinstance(bench, dict):
        raise ValueError("bench must be a table")
    reject_unknown(bench, {"warmup", "samples"}, "bench")
    bench_warmup = bench.get("warmup", 1)
    bench_samples = bench.get("samples", 5)
    if type(bench_warmup) is not int or not 0 <= bench_warmup <= 100:
        raise ValueError("bench.warmup must be an integer from 0 to 100")
    if type(bench_samples) is not int or not 1 <= bench_samples <= 1000:
        raise ValueError("bench.samples must be an integer from 1 to 1000")
    if not isinstance(environment, dict):
        raise ValueError("env must be a table")
    reserved_env = {
        "CARGO", "CARGO_HOME", "CARGO_NET_OFFLINE", "CARGO_TARGET_DIR",
        "HOME", "LD_PRELOAD", "MIRVM", "PATH", "RUSTC", "RUSTDOC",
        "RUSTFLAGS", "RUSTUP_HOME", "TMPDIR", "XDG_CACHE_HOME",
        "XDG_CONFIG_HOME", "XDG_DATA_HOME",
    }
    env_items = []
    for key in sorted(environment):
        value = environment[key]
        if not isinstance(key, str) or not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", key):
            raise ValueError("env keys must be valid environment variable names")
        if key in reserved_env or key.startswith(("MIRVM_", "RUSTUP_")):
            raise ValueError(f"env.{key} is controlled by project-suite")
        if not isinstance(value, str) or "\0" in value:
            raise ValueError(f"env.{key} must be a string")
        env_items.extend((key, value))

    values = [
        name,
        repo,
        rev.lower(),
        lock_sha256.lower(),
        subdir,
        str(expected_exit),
        str(timeout_seconds),
        str(len(args)),
        xfail_exit,
        xfail_diagnostic,
        str(bench_warmup),
        str(bench_samples),
        str(len(env_items) // 2),
        *env_items,
        *args,
    ]
    for value in values:
        sys.stdout.buffer.write(value.encode("utf-8") + b"\0")
except (KeyError, OSError, tomllib.TOMLDecodeError, ValueError) as error:
    print(f"invalid_case: {error}", file=sys.stderr)
    sys.exit(65)
PY
parse_code=$?
if [ "$parse_code" -ne 0 ]; then
    exit "$parse_code"
fi

mapfile -d '' -t CASE_VALUES <"$PARSED"
if [ "${#CASE_VALUES[@]}" -lt 13 ]; then
    echo "invalid_case: parser returned incomplete data" >&2
    exit 65
fi
NAME=${CASE_VALUES[0]}
REPO=${CASE_VALUES[1]}
REV=${CASE_VALUES[2]}
LOCK_SHA256=${CASE_VALUES[3]}
SUBDIR=${CASE_VALUES[4]}
EXPECTED_EXIT=${CASE_VALUES[5]}
TIMEOUT_SECONDS=${CASE_VALUES[6]}
ARG_COUNT=${CASE_VALUES[7]}
XFAIL_EXIT=${CASE_VALUES[8]}
XFAIL_DIAGNOSTIC=${CASE_VALUES[9]}
BENCH_WARMUP=${CASE_VALUES[10]}
BENCH_SAMPLES=${CASE_VALUES[11]}
ENV_COUNT=${CASE_VALUES[12]}
CASE_ENV=()
value_index=13
for ((i = 0; i < ENV_COUNT; i++)); do
    CASE_ENV+=("${CASE_VALUES[value_index]}=${CASE_VALUES[value_index + 1]}")
    value_index=$((value_index + 2))
done
ARGS=("${CASE_VALUES[@]:value_index}")
if [ "${#ARGS[@]}" -ne "$ARG_COUNT" ]; then
    echo "invalid_case: parser returned the wrong argument count" >&2
    exit 65
fi

REPO_SHA256=$(printf '%s' "$REPO" | sha256sum | cut -d' ' -f1)
MIRROR="$PROJECT_SUITE_ROOT/mirrors/$NAME/$REPO_SHA256.git"
READY="$PROJECT_SUITE_ROOT/prepared/$NAME/$REPO_SHA256/$REV.ready"
CARGO_CACHE="$PROJECT_SUITE_ROOT/cargo-home"
MIRVM_SYSROOT_MARKER="$PROJECT_SUITE_MIRVM_CACHE/mirvm/sysroot-$HOST_TARGET/lib/rustlib/$HOST_TARGET/.rustc-build-sysroot-hash"

lock_path() {
    if [ "$SUBDIR" = "." ]; then
        echo Cargo.lock
    else
        echo "$SUBDIR/Cargo.lock"
    fi
}

verify_lock_file() {
    local checkout=$1 actual
    [ -f "$checkout/$SUBDIR/Cargo.lock" ] || {
        echo "lock_missing: $NAME" >&2
        return 1
    }
    actual=$(sha256sum "$checkout/$SUBDIR/Cargo.lock" | cut -d' ' -f1)
    if [ "$actual" != "$LOCK_SHA256" ]; then
        echo "lock_hash_mismatch: $NAME" >&2
        return 1
    fi
}

verify_checkout_unchanged() {
    local checkout=$1 side=$2 head
    head=$("$GIT" -C "$checkout" rev-parse HEAD 2>/dev/null) || {
        echo "FAIL $NAME reason=source_revision_unreadable side=$side"
        return 1
    }
    if [ "$head" != "$REV" ] \
        || ! "$GIT" -C "$checkout" diff --quiet --ignore-submodules -- \
        || ! "$GIT" -C "$checkout" diff --cached --quiet --ignore-submodules --; then
        echo "FAIL $NAME reason=source_tree_mutated side=$side"
        return 1
    fi
}

ensure_mirvm_cache() {
    local tmp=$1 probe="$1/mirvm-cache-probe.rs" log="$1/mirvm-cache-probe.log"
    [ -f "$MIRVM_SYSROOT_MARKER" ] && return 0
    printf 'fn main() {}\n' >"$probe"
    mkdir -p "$PROJECT_SUITE_MIRVM_CACHE" "$tmp/home" "$tmp/tmp"
    if ! env -i HOME="$tmp/home" TMPDIR="$tmp/tmp" \
        XDG_CACHE_HOME="$PROJECT_SUITE_MIRVM_CACHE" CARGO_HOME="$CARGO_CACHE" \
        PATH="$SYSTEM_PATH" LANG=C.UTF-8 LC_ALL=C.UTF-8 TZ=UTC TERM=dumb \
        RUSTC="$RUSTC" RUSTDOC="$RUSTDOC" RUSTUP_HOME="$HOST_RUSTUP_HOME" \
        "$MIRVM" run "$probe" \
        >"$log" 2>&1; then
        echo "prepare_mirvm_cache_failed: $NAME" >&2
        tail -20 "$log" >&2
        return 1
    fi
    if [ ! -f "$MIRVM_SYSROOT_MARKER" ]; then
        echo "prepare_mirvm_cache_missing_marker: $MIRVM_SYSROOT_MARKER" >&2
        return 1
    fi
}

prepare_mirvm_runtime_cache() {
    local root=$1 source="$PROJECT_SUITE_MIRVM_CACHE/mirvm" dest="$1/mirvm"
    local item base
    [ -f "$root/.project-suite-ready" ] && return 0
    [ -d "$source" ] || {
        echo "mirvm_cache_missing: $source" >&2
        return 1
    }
    rm -rf "$root"
    mkdir -p "$dest"
    for item in "$source"/*; do
        [ -e "$item" ] || continue
        base=$(basename "$item")
        case "$base" in
            sysroot-*) ln -s "$item" "$dest/$base" ;;
            *) cp -a --reflink=auto "$item" "$dest/$base" ;;
        esac || {
            rm -rf "$root"
            echo "mirvm_runtime_cache_prepare_failed: $NAME" >&2
            return 1
        }
    done
    printf 'ready\n' >"$root/.project-suite-ready"
}

prepare_fetch_env() {
    local root=$1
    shift
    if [ -z "$BWRAP" ]; then
        echo "prepare_sandbox_unavailable: install bwrap" >&2
        return 69
    fi
    "$BWRAP" --die-with-parent --ro-bind / / \
        --bind "$root" "$root" \
        --bind "$CARGO_CACHE" "$CARGO_CACHE" \
        --dev /dev --proc /proc --chdir "$PWD" \
        /usr/bin/env -i HOME="$root/home" TMPDIR="$root/tmp" \
            XDG_CACHE_HOME="$root/xdg" CARGO_HOME="$CARGO_CACHE" \
            PATH="$SYSTEM_PATH" LANG=C.UTF-8 LC_ALL=C.UTF-8 TZ=UTC TERM=dumb \
            RUSTC="$RUSTC" RUSTDOC="$RUSTDOC" RUSTUP_HOME="$HOST_RUSTUP_HOME" \
            "$@"
}

prepare_build_env() {
    local root=$1
    shift
    if [ -z "$BWRAP" ]; then
        echo "prepare_sandbox_unavailable: install bwrap" >&2
        return 69
    fi
    "$BWRAP" --die-with-parent --unshare-net --ro-bind / / \
        --bind "$root" "$root" \
        --ro-bind "$CARGO_CACHE" "$CARGO_CACHE" \
        --dev /dev --proc /proc --chdir "$PWD" \
        /usr/bin/env -i HOME="$root/home" TMPDIR="$root/tmp" \
            XDG_CACHE_HOME="$root/xdg" CARGO_HOME="$CARGO_CACHE" \
            CARGO_NET_OFFLINE=true PATH="$SYSTEM_PATH" \
            LANG=C.UTF-8 LC_ALL=C.UTF-8 TZ=UTC TERM=dumb \
            RUSTC="$RUSTC" RUSTDOC="$RUSTDOC" RUSTUP_HOME="$HOST_RUSTUP_HOME" \
            "${CASE_ENV[@]}" "$@"
}

prepare_case() {
    local tmp actual manifest
    mkdir -p "$PROJECT_SUITE_ROOT/mirrors/$NAME" \
        "$PROJECT_SUITE_ROOT/prepared/$NAME/$REPO_SHA256" \
        "$PROJECT_SUITE_ROOT/tmp" "$CARGO_CACHE" "$PROJECT_SUITE_MIRVM_CACHE"

    if [ -d "$MIRROR" ]; then
        "$GIT" --git-dir="$MIRROR" remote set-url origin "$REPO"
        "$GIT" --git-dir="$MIRROR" fetch -q --prune origin '+refs/*:refs/*' || {
            echo "prepare_fetch_failed: $NAME" >&2
            return 1
        }
    else
        "$GIT" clone -q --mirror -- "$REPO" "$MIRROR" || {
            echo "prepare_clone_failed: $NAME" >&2
            return 1
        }
    fi
    "$GIT" --git-dir="$MIRROR" cat-file -e "$REV^{commit}" 2>/dev/null || {
        echo "revision_not_found: $NAME $REV" >&2
        return 1
    }

    tmp=$(mktemp -d "$PROJECT_SUITE_ROOT/tmp/prepare.XXXXXX")
    "$GIT" clone -q --no-checkout -- "$MIRROR" "$tmp/source" \
        && "$GIT" -C "$tmp/source" checkout -q --detach "$REV" || {
        rm -rf "$tmp"
        echo "prepare_checkout_failed: $NAME" >&2
        return 1
    }
    if ! verify_lock_file "$tmp/source"; then
        rm -rf "$tmp"
        return 1
    fi
    actual=$(sha256sum "$tmp/source/$(lock_path)" | cut -d' ' -f1)
    if [ "$actual" != "$LOCK_SHA256" ]; then
        rm -rf "$tmp"
        echo "lock_hash_mismatch: $NAME" >&2
        return 1
    fi

    mkdir -p "$tmp/home" "$tmp/tmp" "$tmp/xdg"
    manifest="$tmp/source/$SUBDIR/Cargo.toml"
    if ! prepare_fetch_env "$tmp" \
        "$CARGO" fetch --locked --target "$HOST_TARGET" \
            --manifest-path "$manifest" --quiet; then
        rm -rf "$tmp"
        echo "prepare_cargo_fetch_failed: $NAME" >&2
        return 1
    fi
    if ! prepare_build_env "$tmp" \
        "$CARGO" build --locked --target "$HOST_TARGET" \
            --manifest-path "$manifest" --quiet; then
        rm -rf "$tmp"
        echo "prepare_cargo_build_failed: $NAME" >&2
        return 1
    fi
    if ! ensure_mirvm_cache "$tmp"; then
        rm -rf "$tmp"
        return 1
    fi
    printf '%s\n' "$LOCK_SHA256" >"$READY"
    rm -rf "$tmp"
    echo "PASS $NAME prepared $REV"
}

require_prepared() {
    if [ ! -f "$READY" ] || [ "$(cat "$READY")" != "$LOCK_SHA256" ] \
        || [ ! -d "$MIRROR" ]; then
        echo "case_not_prepared: $NAME" >&2
        return 1
    fi
}

isolated_env() {
    local root=$1 side_root
    side_root=$(dirname "$root")
    mkdir -p "$root/home" "$root/tmp" "$root/xdg" "$root/target"
    if [ -z "$BWRAP" ]; then
        echo "network_sandbox_unavailable: install bwrap" >&2
        return 69
    fi
    "$BWRAP" --die-with-parent --unshare-net --ro-bind / / \
        --bind "$side_root" "$side_root" \
        --dev /dev --proc /proc --chdir "$PWD" \
        /usr/bin/env -i HOME="$root/home" TMPDIR="$root/tmp" \
            XDG_CACHE_HOME="$root/xdg" \
            CARGO_HOME="$CARGO_CACHE" CARGO_TARGET_DIR="$root/target" \
            CARGO_NET_OFFLINE=true PATH="$SYSTEM_PATH" \
            LANG=C.UTF-8 LC_ALL=C.UTF-8 TZ=UTC TERM=dumb \
            RUSTC="$RUSTC" RUSTDOC="$RUSTDOC" RUSTUP_HOME="$HOST_RUSTUP_HOME" \
            "${@:2}"
}

run_isolated() {
    local env_root=$1 status_file=$2 wrapper_code kind code
    shift 2
    rm -f "$status_file"
    isolated_env "$env_root" /bin/bash -c '
        status_file=$1
        shift
        "$@"
        code=$?
        printf "exit %s\n" "$code" >"$status_file" || exit 200
        exit "$code"
    ' project-suite-runner "$status_file" "$@"
    wrapper_code=$?
    if [ ! -f "$status_file" ]; then
        return 1
    fi
    read -r kind code <"$status_file"
    if [ "$kind" != exit ] || [[ ! "$code" =~ ^[0-9]+$ ]]; then
        return 1
    fi
    # wrapper_code 允许等于 guest code；隔离层是否真实完成由 status_file 单独证明。
    [ "$wrapper_code" -eq "$code" ] || return 1
    return 0
}

measure_command() {
    local env_root=$1 cwd=$2 duration_file=$3 stdout_file=$4 stderr_file=$5
    local status_file="$3.status" kind code
    shift 5
    MEASURE_CODE=
    rm -f "$duration_file" "$stdout_file" "$stderr_file" "$status_file"
    if ! run_isolated "$env_root" "$status_file" \
        "$PYTHON" - "$cwd" "$duration_file" \
        "$stdout_file" "$stderr_file" "$@" <<'PY'
import pathlib
import subprocess
import sys
import time

cwd, duration_file, stdout_file, stderr_file, *command = sys.argv[1:]
with open(stdout_file, "wb") as stdout, open(stderr_file, "wb") as stderr:
    start = time.perf_counter_ns()
    completed = subprocess.run(
        command,
        cwd=cwd,
        stdin=subprocess.DEVNULL,
        stdout=stdout,
        stderr=stderr,
    )
    duration = time.perf_counter_ns() - start
pathlib.Path(duration_file).write_text(f"{duration}\n")
sys.exit(completed.returncode)
PY
    then
        return 1
    fi
    read -r kind code <"$status_file" || return 1
    if [ "$kind" != exit ] || [[ ! "$code" =~ ^[0-9]+$ ]] \
        || [ ! -f "$duration_file" ] || [ ! -f "$stdout_file" ] \
        || [ ! -f "$stderr_file" ]; then
        return 1
    fi
    MEASURE_CODE=$code
    return 0
}

measure_native() {
    local env_root=$1 source=$2 duration_file=$3 stdout_file=$4 stderr_file=$5
    measure_command "$env_root" "$source/$SUBDIR" "$duration_file" \
        "$stdout_file" "$stderr_file" timeout "$TIMEOUT_SECONDS" \
        env "${CASE_ENV[@]}" \
        "$CARGO" run --locked --target "$HOST_TARGET" --quiet -- "${ARGS[@]}"
}

measure_mirvm() {
    local env_root=$1 source=$2 duration_file=$3 stdout_file=$4 stderr_file=$5
    local runtime_cache="$1/mirvm-xdg"
    prepare_mirvm_runtime_cache "$runtime_cache" || return 1
    measure_command "$env_root" "$source/$SUBDIR" "$duration_file" \
        "$stdout_file" "$stderr_file" timeout "$TIMEOUT_SECONDS" \
        env "${CASE_ENV[@]}" XDG_CACHE_HOME="$runtime_cache" \
        MIRVM_CARGO_LOCKED=1 \
        "$MIRVM" run . -- "${ARGS[@]}"
}

matches_correctness_oracle() {
    local code=$1 stdout_file=$2 stderr_file=$3 oracle_code
    local oracle="$PROJECT_SUITE_ARTIFACTS/$NAME/check"
    [ -f "$oracle/native.exit" ] \
        && [ -f "$oracle/native.stdout" ] \
        && [ -f "$oracle/native.stderr" ] || return 1
    read -r oracle_code <"$oracle/native.exit"
    [[ "$oracle_code" =~ ^[0-9]+$ ]] \
        && [ "$code" -eq "$oracle_code" ] \
        && cmp -s "$stdout_file" "$oracle/native.stdout" \
        && cmp -s "$stderr_file" "$oracle/native.stderr"
}

check_case() {
    local run_root native_src mirvm_src artifact native_code mirvm_code reason
    local native_status mirvm_status mirvm_runtime_cache
    local line diagnostic_lines matching_diagnostics
    CHECK_RESULT=fail
    artifact="$PROJECT_SUITE_ARTIFACTS/$NAME/check"
    rm -rf "$artifact"
    require_prepared || return 1
    mkdir -p "$PROJECT_SUITE_ROOT/runs" "$PROJECT_SUITE_ARTIFACTS/$NAME"
    run_root=$(mktemp -d "$PROJECT_SUITE_ROOT/runs/$NAME.XXXXXX")
    native_src="$run_root/native/source"
    mirvm_src="$run_root/mirvm/source"
    mkdir -p "$run_root/native" "$run_root/mirvm"
    "$GIT" clone -q --no-checkout -- "$MIRROR" "$native_src" \
        && "$GIT" -C "$native_src" checkout -q --detach "$REV" \
        && "$GIT" clone -q --no-checkout -- "$MIRROR" "$mirvm_src" \
        && "$GIT" -C "$mirvm_src" checkout -q --detach "$REV" || {
        rm -rf "$run_root"
        echo "check_checkout_failed: $NAME" >&2
        return 1
    }
    verify_lock_file "$native_src" && verify_lock_file "$mirvm_src" || {
        rm -rf "$run_root"
        return 1
    }

    mkdir -p "$artifact"
    native_status="$run_root/native/status"
    (
        cd "$native_src/$SUBDIR" || exit 72
        run_isolated "$run_root/native/env" "$native_status" \
            timeout "$TIMEOUT_SECONDS" \
            env "${CASE_ENV[@]}" \
            "$CARGO" run --locked --target "$HOST_TARGET" --quiet -- "${ARGS[@]}" \
            </dev/null
    ) >"$artifact/native.stdout" 2>"$artifact/native.stderr"
    if [ "$?" -ne 0 ]; then
        printf 'infrastructure\n' >"$artifact/native.exit"
        rm -rf "$run_root"
        echo "FAIL $NAME reason=runner_infrastructure_failed side=native"
        return 1
    fi
    read -r _ native_code <"$native_status"
    printf '%s\n' "$native_code" >"$artifact/native.exit"
    if ! verify_checkout_unchanged "$native_src" native; then
        rm -rf "$run_root"
        return 1
    fi
    if [ "$native_code" -ne "$EXPECTED_EXIT" ]; then
        rm -rf "$run_root"
        echo "FAIL $NAME native_baseline_failed exit=$native_code expected=$EXPECTED_EXIT"
        return 1
    fi

    mirvm_status="$run_root/mirvm/status"
    mirvm_runtime_cache="$run_root/mirvm/env/mirvm-xdg"
    if ! prepare_mirvm_runtime_cache "$mirvm_runtime_cache"; then
        rm -rf "$run_root"
        echo "FAIL $NAME reason=runner_infrastructure_failed side=mirvm"
        return 1
    fi
    (
        cd "$mirvm_src/$SUBDIR" || exit 72
        run_isolated "$run_root/mirvm/env" "$mirvm_status" \
            timeout "$TIMEOUT_SECONDS" \
            env "${CASE_ENV[@]}" XDG_CACHE_HOME="$mirvm_runtime_cache" \
            MIRVM_CARGO_LOCKED=1 \
            "$MIRVM" run . -- "${ARGS[@]}" </dev/null
    ) >"$artifact/mirvm.stdout" 2>"$artifact/mirvm.stderr"
    if [ "$?" -ne 0 ]; then
        printf 'infrastructure\n' >"$artifact/mirvm.exit"
        rm -rf "$run_root"
        echo "FAIL $NAME reason=runner_infrastructure_failed side=mirvm"
        return 1
    fi
    read -r _ mirvm_code <"$mirvm_status"
    printf '%s\n' "$mirvm_code" >"$artifact/mirvm.exit"
    if ! verify_checkout_unchanged "$mirvm_src" mirvm; then
        rm -rf "$run_root"
        return 1
    fi

    if [ -n "$XFAIL_EXIT" ]; then
        if [ "$mirvm_code" -eq "$native_code" ] \
            && cmp -s "$artifact/native.stdout" "$artifact/mirvm.stdout" \
            && cmp -s "$artifact/native.stderr" "$artifact/mirvm.stderr"; then
            rm -rf "$run_root"
            echo "XPASS $NAME reason=expected_failure_disappeared"
            return 1
        elif [ "$mirvm_code" -eq "$XFAIL_EXIT" ]; then
            diagnostic_lines=0
            matching_diagnostics=0
            while IFS= read -r line || [ -n "$line" ]; do
                if [[ "$line" == mirvm* ]]; then
                    diagnostic_lines=$((diagnostic_lines + 1))
                    if [ "$line" = "$XFAIL_DIAGNOSTIC" ]; then
                        matching_diagnostics=$((matching_diagnostics + 1))
                    fi
                fi
            done < "$artifact/mirvm.stderr"
            if [ "$diagnostic_lines" -eq 1 ] \
                && [ "$matching_diagnostics" -eq 1 ]; then
                rm -rf "$run_root"
                CHECK_RESULT=xfail
                echo "XFAIL $NAME reason=$XFAIL_DIAGNOSTIC"
                return 0
            fi
            rm -rf "$run_root"
            echo "FAIL $NAME reason=xfail_diagnostic_mismatch expected=$XFAIL_DIAGNOSTIC"
            return 1
        fi
        rm -rf "$run_root"
        echo "FAIL $NAME reason=expected_xfail_not_observed"
        return 1
    fi

    reason=
    if [ "$mirvm_code" -ne "$native_code" ]; then
        reason=exit_mismatch
    elif ! cmp -s "$artifact/native.stdout" "$artifact/mirvm.stdout"; then
        reason=stdout_mismatch
    elif ! cmp -s "$artifact/native.stderr" "$artifact/mirvm.stderr"; then
        reason=stderr_mismatch
    fi
    rm -rf "$run_root"
    if [ -n "$reason" ]; then
        echo "FAIL $NAME reason=$reason native=$native_code mirvm=$mirvm_code"
        return 1
    fi
    CHECK_RESULT=pass
    echo "PASS $NAME correctness"
}

bench_case() {
    local artifact="$PROJECT_SUITE_ARTIFACTS/$NAME/bench"
    local run_root native_src mirvm_src native_scratch mirvm_scratch raw i order
    local native_code mirvm_code native_ns mirvm_ns
    local cargo_path rustc_path mirvm_path cargo_sha rustc_sha mirvm_sha
    rm -rf "$artifact"
    mkdir -p "$artifact"
    if ! check_case >&2; then
        echo "FAIL $NAME reason=correctness_not_passed" >&2
        return 1
    fi
    if [ "$CHECK_RESULT" != pass ]; then
        echo "FAIL $NAME reason=expected_red_not_benchmarkable" >&2
        return 1
    fi

    mkdir -p "$PROJECT_SUITE_ROOT/runs"
    run_root=$(mktemp -d "$PROJECT_SUITE_ROOT/runs/$NAME-bench.XXXXXX")
    native_src="$run_root/native/source"
    mirvm_src="$run_root/mirvm/source"
    native_scratch="$run_root/native/measure"
    mirvm_scratch="$run_root/mirvm/measure"
    raw="$run_root/samples.tsv"
    mkdir -p "$run_root/native" "$run_root/mirvm" \
        "$native_scratch" "$mirvm_scratch"
    : >"$raw"
    "$GIT" clone -q --no-checkout -- "$MIRROR" "$native_src" \
        && "$GIT" -C "$native_src" checkout -q --detach "$REV" \
        && "$GIT" clone -q --no-checkout -- "$MIRROR" "$mirvm_src" \
        && "$GIT" -C "$mirvm_src" checkout -q --detach "$REV" || {
        rm -rf "$run_root"
        echo "FAIL $NAME reason=bench_checkout_failed" >&2
        return 1
    }
    verify_lock_file "$native_src" && verify_lock_file "$mirvm_src" || {
        rm -rf "$run_root"
        return 1
    }

    for ((i = 0; i < BENCH_WARMUP; i++)); do
        if ! measure_native "$run_root/native/env" "$native_src" \
            "$native_scratch/time" "$native_scratch/stdout" "$native_scratch/stderr"; then
            rm -rf "$run_root"
            echo "FAIL $NAME reason=benchmark_runner_infrastructure_failed side=native phase=warmup index=$((i + 1))" >&2
            return 1
        fi
        native_code=$MEASURE_CODE
        if ! measure_mirvm "$run_root/mirvm/env" "$mirvm_src" \
            "$mirvm_scratch/time" "$mirvm_scratch/stdout" "$mirvm_scratch/stderr"; then
            rm -rf "$run_root"
            echo "FAIL $NAME reason=benchmark_runner_infrastructure_failed side=mirvm phase=warmup index=$((i + 1))" >&2
            return 1
        fi
        mirvm_code=$MEASURE_CODE
        if ! matches_correctness_oracle "$native_code" \
            "$native_scratch/stdout" "$native_scratch/stderr" \
            || ! matches_correctness_oracle "$mirvm_code" \
                "$mirvm_scratch/stdout" "$mirvm_scratch/stderr"; then
            rm -rf "$run_root"
            echo "FAIL $NAME reason=benchmark_warmup_oracle_mismatch" >&2
            return 1
        fi
        if ! verify_checkout_unchanged "$native_src" native >&2 \
            || ! verify_checkout_unchanged "$mirvm_src" mirvm >&2; then
            rm -rf "$run_root"
            return 1
        fi
    done

    for ((i = 0; i < BENCH_SAMPLES; i++)); do
        if [ $((i % 2)) -eq 0 ]; then
            order=native-first
            if ! measure_native "$run_root/native/env" "$native_src" \
                "$native_scratch/time" "$native_scratch/stdout" "$native_scratch/stderr"; then
                rm -rf "$run_root"
                echo "FAIL $NAME reason=benchmark_runner_infrastructure_failed side=native phase=sample index=$((i + 1))" >&2
                return 1
            fi
            native_code=$MEASURE_CODE
            if ! measure_mirvm "$run_root/mirvm/env" "$mirvm_src" \
                "$mirvm_scratch/time" "$mirvm_scratch/stdout" "$mirvm_scratch/stderr"; then
                rm -rf "$run_root"
                echo "FAIL $NAME reason=benchmark_runner_infrastructure_failed side=mirvm phase=sample index=$((i + 1))" >&2
                return 1
            fi
            mirvm_code=$MEASURE_CODE
        else
            order=mirvm-first
            if ! measure_mirvm "$run_root/mirvm/env" "$mirvm_src" \
                "$mirvm_scratch/time" "$mirvm_scratch/stdout" "$mirvm_scratch/stderr"; then
                rm -rf "$run_root"
                echo "FAIL $NAME reason=benchmark_runner_infrastructure_failed side=mirvm phase=sample index=$((i + 1))" >&2
                return 1
            fi
            mirvm_code=$MEASURE_CODE
            if ! measure_native "$run_root/native/env" "$native_src" \
                "$native_scratch/time" "$native_scratch/stdout" "$native_scratch/stderr"; then
                rm -rf "$run_root"
                echo "FAIL $NAME reason=benchmark_runner_infrastructure_failed side=native phase=sample index=$((i + 1))" >&2
                return 1
            fi
            native_code=$MEASURE_CODE
        fi
        if ! matches_correctness_oracle "$native_code" \
            "$native_scratch/stdout" "$native_scratch/stderr" \
            || ! matches_correctness_oracle "$mirvm_code" \
                "$mirvm_scratch/stdout" "$mirvm_scratch/stderr"; then
            rm -rf "$run_root"
            echo "FAIL $NAME reason=benchmark_sample_oracle_mismatch index=$((i + 1))" >&2
            return 1
        fi
        if ! verify_checkout_unchanged "$native_src" native >&2 \
            || ! verify_checkout_unchanged "$mirvm_src" mirvm >&2; then
            rm -rf "$run_root"
            return 1
        fi
        native_ns=$(cat "$native_scratch/time")
        mirvm_ns=$(cat "$mirvm_scratch/time")
        printf '%s\t%s\t%s\t%s\n' "$((i + 1))" "$order" "$native_ns" "$mirvm_ns" >>"$raw"
    done

    if [[ "$CARGO" == */* ]]; then
        cargo_path=$(readlink -f -- "$CARGO")
    else
        cargo_path=$(command -v -- "$CARGO")
    fi
    if [[ "$RUSTC" == */* ]]; then
        rustc_path=$(readlink -f -- "$RUSTC")
    else
        rustc_path=$(command -v -- "$RUSTC")
    fi
    if [[ "$MIRVM" == */* ]]; then
        mirvm_path=$(readlink -f -- "$MIRVM")
    else
        mirvm_path=$(command -v -- "$MIRVM")
    fi
    cargo_sha=$(sha256sum -- "$cargo_path" | cut -d' ' -f1) \
        && rustc_sha=$(sha256sum -- "$rustc_path" | cut -d' ' -f1) \
        && mirvm_sha=$(sha256sum -- "$mirvm_path" | cut -d' ' -f1) || {
        rm -rf "$run_root"
        echo "FAIL $NAME reason=benchmark_tool_identity_failed" >&2
        return 1
    }

    if ! "$PYTHON" - "$raw" "$artifact/samples.jsonl" \
        "$artifact/summary.json" "$NAME" "$REPO" "$REV" "$LOCK_SHA256" \
        "$SUBDIR" "$EXPECTED_EXIT" "$TIMEOUT_SECONDS" "$HOST_TARGET" \
        "$BENCH_WARMUP" "$BENCH_SAMPLES" \
        "$cargo_path" "$cargo_sha" "$rustc_path" "$rustc_sha" \
        "$mirvm_path" "$mirvm_sha" "$ENV_COUNT" "$ARG_COUNT" \
        "${CASE_ENV[@]}" "${ARGS[@]}" <<'PY'
import json
import math
import pathlib
import sys

(
    raw_path,
    samples_path,
    summary_path,
    name,
    repo,
    revision,
    lock_sha256,
    subdir,
    expected_exit,
    timeout_seconds,
    host_target,
    warmup,
    configured_samples,
    cargo_path,
    cargo_sha,
    rustc_path,
    rustc_sha,
    mirvm_path,
    mirvm_sha,
    env_count,
    arg_count,
    *workload,
) = sys.argv[1:]
env_count = int(env_count)
arg_count = int(arg_count)
env_items = workload[:env_count]
args = workload[env_count:]
if len(args) != arg_count:
    raise ValueError("benchmark metadata argument count mismatch")
environment = dict(item.split("=", 1) for item in env_items)
rows = []
for line in pathlib.Path(raw_path).read_text().splitlines():
    index, order, native_ns, mirvm_ns = line.split("\t")
    rows.append({
        "case": name,
        "index": int(index),
        "order": order,
        "native_ns": int(native_ns),
        "mirvm_ns": int(mirvm_ns),
    })

def median(values):
    values = sorted(values)
    middle = len(values) // 2
    if len(values) % 2:
        return values[middle]
    return (values[middle - 1] + values[middle]) // 2

def p95(values):
    values = sorted(values)
    return values[max(0, math.ceil(len(values) * 0.95) - 1)]

native = [row["native_ns"] for row in rows]
mirvm = [row["mirvm_ns"] for row in rows]
summary = {
    "schema_version": 1,
    "case": name,
    "repo": repo,
    "revision": revision,
    "lock_sha256": lock_sha256,
    "subdir": subdir,
    "args": args,
    "env": environment,
    "expected_exit": int(expected_exit),
    "timeout_seconds": int(timeout_seconds),
    "host_target": host_target,
    "benchmark": {"warmup": int(warmup), "samples": int(configured_samples)},
    "tools": {
        "cargo": {"path": cargo_path, "sha256": cargo_sha},
        "rustc": {"path": rustc_path, "sha256": rustc_sha},
        "mirvm": {"path": mirvm_path, "sha256": mirvm_sha},
    },
    "samples": len(rows),
    "unit": "ns",
    "native": {"median_ns": median(native), "p95_ns": p95(native)},
    "mirvm": {"median_ns": median(mirvm), "p95_ns": p95(mirvm)},
}
pathlib.Path(samples_path).write_text("".join(
    json.dumps(row, sort_keys=True, separators=(",", ":")) + "\n" for row in rows
))
pathlib.Path(summary_path).write_text(
    json.dumps(summary, sort_keys=True, separators=(",", ":")) + "\n"
)
PY
    then
        rm -rf "$run_root"
        echo "FAIL $NAME reason=benchmark_summary_failed" >&2
        return 1
    fi
    rm -rf "$run_root"
    cat "$artifact/summary.json"
}

case "$MODE" in
    prepare) prepare_case ;;
    check) check_case ;;
    bench) bench_case ;;
esac
