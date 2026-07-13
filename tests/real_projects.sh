#!/usr/bin/env bash
# 固定真实 Cargo 项目的 correctness / benchmark 入口。
# 实现分层内容身份、native↔mirvm 精确差分，以及 correctness-gated benchmark evidence。
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
RUSTC_REMAP_PROXY="$(pwd)/tests/project_suite_rustc_proxy.sh"
EVIDENCE_HELPER="$(pwd)/tests/project_suite_evidence.py"
if [ ! -x "$RUSTC_REMAP_PROXY" ]; then
    echo "rustc_remap_proxy_unavailable: $RUSTC_REMAP_PROXY" >&2
    exit 69
fi

reject_run_path() {
    local label=$1 path=$2 resolved
    resolved=$(realpath -m -- "$path") || {
        echo "host_path_unresolvable: $label=$path" >&2
        exit 69
    }
    case "$resolved" in
        /run|/run/*)
            echo "host_path_hidden_by_run_tmpfs: $label=$resolved" >&2
            exit 69
            ;;
    esac
}

# isolated_env overlays /run with a private tmpfs. Fail before prepare/check if
# that would hide any controller, toolchain, cache, or suite path we need.
reject_run_path workspace "$(pwd)"
reject_run_path project_suite_root "$PROJECT_SUITE_ROOT"
reject_run_path project_suite_mirvm_cache "$PROJECT_SUITE_MIRVM_CACHE"
reject_run_path git "$GIT"
reject_run_path cargo "$CARGO"
reject_run_path rustc "$RUSTC"
reject_run_path mirvm "$MIRVM"
reject_run_path python "$PYTHON"
reject_run_path rustup_home "$HOST_RUSTUP_HOME"

PARSED=$(mktemp)
EVIDENCE_STAGE=
EVIDENCE_LOCK_FD=
CACHE_LOCK_FD=
cleanup() {
    rm -f "$PARSED"
    if [ -n "$EVIDENCE_STAGE" ] && [ -e "$EVIDENCE_STAGE" ]; then
        chmod -R u+w -- "$EVIDENCE_STAGE" 2>/dev/null || true
        rm -rf "$EVIDENCE_STAGE"
    fi
}
trap cleanup EXIT
"$PYTHON" - "$CASE_FILE" >"$PARSED" <<'PY'
import hashlib
import json
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
        "bench", "env", "workload_tools",
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
    workload_tools = case.get("workload_tools", [])

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
    subdir = str(pathlib.PurePosixPath(subdir))
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
    if not isinstance(workload_tools, list) or not all(
        isinstance(tool, str) for tool in workload_tools
    ):
        raise ValueError("workload_tools must be an array of command names")
    if len(set(workload_tools)) != len(workload_tools):
        raise ValueError("workload_tools must not contain duplicates")
    for tool in workload_tools:
        if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._+-]*", tool) \
            or tool in {".", ".."}:
            raise ValueError("workload_tools entries must be command basenames")
    workload_tools = sorted(workload_tools)
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
        "CARGO", "CARGO_ENCODED_RUSTFLAGS", "CARGO_HOME", "CARGO_NET_OFFLINE",
        "CARGO_TARGET_DIR",
        "HOME", "LD_PRELOAD", "MIRVM", "PATH", "RUSTC", "RUSTDOC",
        "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "RUSTFLAGS",
        "RUSTUP_HOME", "TMPDIR", "XDG_CACHE_HOME",
        "XDG_CONFIG_HOME", "XDG_DATA_HOME",
    }
    env_items = []
    for key in sorted(environment):
        value = environment[key]
        if not isinstance(key, str) or not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", key):
            raise ValueError("env keys must be valid environment variable names")
        if key in reserved_env or key.startswith(("MIRVM_", "PROJECT_SUITE_", "RUSTUP_")):
            raise ValueError(f"env.{key} is controlled by project-suite")
        if not isinstance(value, str) or "\0" in value:
            raise ValueError(f"env.{key} must be a string")
        env_items.extend((key, value))

    canonical_case = {
        "schema_version": 1,
        "repo": repo,
        "revision": rev.lower(),
        "lock_sha256": lock_sha256.lower(),
        "subdir": subdir,
        "args": args,
        "env": {key: environment[key] for key in sorted(environment)},
        "expected_exit": expected_exit,
        "timeout_seconds": timeout_seconds,
        "normalizers": [],
        "stdin": {"kind": "eof"},
        "xfail": None if xfail is None else {
            "mirvm_exit": int(xfail_exit),
            "diagnostic": xfail_diagnostic,
        },
    }
    canonical_bytes = json.dumps(
        canonical_case,
        ensure_ascii=False,
        sort_keys=True,
        separators=(",", ":"),
    ).encode("utf-8")
    case_id = hashlib.sha256(
        b"mirvm/project-suite/case/v1\0" + canonical_bytes
    ).hexdigest()

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
        case_id,
        str(len(workload_tools)),
        *workload_tools,
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
if [ "${#CASE_VALUES[@]}" -lt 15 ]; then
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
CASE_ID=${CASE_VALUES[13]}
WORKLOAD_TOOL_COUNT=${CASE_VALUES[14]}
WORKLOAD_TOOL_NAMES=()
CASE_ENV=()
value_index=15
for ((i = 0; i < WORKLOAD_TOOL_COUNT; i++)); do
    WORKLOAD_TOOL_NAMES+=("${CASE_VALUES[value_index]}")
    value_index=$((value_index + 1))
done
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
CHECK_ID=
CASE_ARTIFACT_ROOT="$PROJECT_SUITE_ARTIFACTS/$NAME"
CHECK_EVIDENCE=
CHECK_EVIDENCE_ID=
BENCH_ID=
BENCH_EVIDENCE=
BENCH_EVIDENCE_ID=
EVIDENCE_SCHEMA=3
WORKLOAD_TOOL_ID_PATHS=()
WORKLOAD_TOOL_ID_SHAS=()
WORKLOAD_TOOL_EVIDENCE_ARGS=()

acquire_evidence_lock() {
    mkdir -p "$CASE_ARTIFACT_ROOT" || return 69
    exec {EVIDENCE_LOCK_FD}>"$CASE_ARTIFACT_ROOT/.lock" || return 69
    if ! flock -n "$EVIDENCE_LOCK_FD"; then
        echo "evidence_busy: $NAME" >&2
        return 75
    fi
}

acquire_cache_lock() {
    mkdir -p "$PROJECT_SUITE_ROOT" {EVIDENCE_LOCK_FD}>&- || return 69
    exec {CACHE_LOCK_FD}<>"$PROJECT_SUITE_ROOT/.cache.lock" || return 69
    case "$MODE" in
        prepare)
            if ! flock -n -x "$CACHE_LOCK_FD" {EVIDENCE_LOCK_FD}>&-; then
                echo "cache_busy: prepare" >&2
                return 75
            fi
            ;;
        check|bench)
            if ! flock -n -s "$CACHE_LOCK_FD" {EVIDENCE_LOCK_FD}>&-; then
                echo "cache_busy: $MODE" >&2
                return 75
            fi
            ;;
    esac
}

git_control() {
    env -i HOME="$HOST_HOME" PATH="$SYSTEM_PATH" \
        LANG=C.UTF-8 LC_ALL=C.UTF-8 TZ=UTC TERM=dumb \
        GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null \
        "$GIT" -c core.autocrlf=false "$@" \
        {EVIDENCE_LOCK_FD}>&- {CACHE_LOCK_FD}>&-
}

python_control() {
    "$PYTHON" "$@" {EVIDENCE_LOCK_FD}>&- {CACHE_LOCK_FD}>&-
}

# Redirections attached to a function call are restored by Bash and may keep a
# hidden duplicate of each lock in a command-substitution or grouping subshell.
# `exec` makes the close permanent in that child shell without touching the
# workflow's lock-owning parent shell.
close_workflow_locks() {
    exec {EVIDENCE_LOCK_FD}>&- {CACHE_LOCK_FD}>&-
}

identity_tool_path() {
    local executable=$1
    if [ -z "$executable" ]; then
        printf '\n'
    elif [[ "$executable" == */* ]]; then
        readlink -f -- "$executable"
    else
        command -v -- "$executable"
    fi
}

identity_tool_sha() {
    local path=$1 digest
    if [ -n "$path" ] && [ -f "$path" ]; then
        digest=$(sha256sum -- "$path") || return 1
        printf '%s\n' "${digest%% *}"
    else
        printf 'unavailable\n'
    fi
}

resolve_workload_tool_path() {
    local name=$1 directory candidate
    local -a search_path
    IFS=: read -r -a search_path <<<"$SYSTEM_PATH"
    for directory in "${search_path[@]}"; do
        [ -n "$directory" ] || directory=.
        candidate="$directory/$name"
        if [ -f "$candidate" ] && [ -x "$candidate" ]; then
            printf '%s\n' "$candidate"
            return 0
        fi
    done
    return 1
}

compute_workload_tool_identity() {
    local index name path resolved sha
    EVIDENCE_SCHEMA=3
    WORKLOAD_TOOL_ID_PATHS=()
    WORKLOAD_TOOL_ID_SHAS=()
    WORKLOAD_TOOL_EVIDENCE_ARGS=()
    [ "$WORKLOAD_TOOL_COUNT" -gt 0 ] || return 0
    EVIDENCE_SCHEMA=4
    for ((index = 0; index < WORKLOAD_TOOL_COUNT; index++)); do
        name=${WORKLOAD_TOOL_NAMES[index]}
        path=$(close_workflow_locks; resolve_workload_tool_path "$name") || {
            echo "workload_tool_unavailable: $name" >&2
            return 1
        }
        resolved=$(close_workflow_locks; readlink -f -- "$path") || {
            echo "workload_tool_unavailable: $name" >&2
            return 1
        }
        case "$path" in
            /run|/run/*)
                echo "host_path_hidden_by_run_tmpfs: workload_tool.$name=$path" >&2
                return 1
                ;;
        esac
        case "$resolved" in
            /run|/run/*)
                echo "host_path_hidden_by_run_tmpfs: workload_tool.$name=$resolved" >&2
                return 1
                ;;
        esac
        [ -f "$resolved" ] && [ -x "$resolved" ] || {
            echo "workload_tool_unavailable: $name" >&2
            return 1
        }
        sha=$(close_workflow_locks; identity_tool_sha "$resolved") || return 1
        [[ "$sha" =~ ^[0-9a-f]{64}$ ]] || {
            echo "workload_tool_unavailable: $name" >&2
            return 1
        }
        WORKLOAD_TOOL_ID_PATHS+=("$path")
        WORKLOAD_TOOL_ID_SHAS+=("$sha")
        WORKLOAD_TOOL_EVIDENCE_ARGS+=("$name" "$path" "$sha")
    done
}

compute_check_identity() {
    CARGO_ID_PATH=$(close_workflow_locks; identity_tool_path "$CARGO") || return 1
    RUSTC_ID_PATH=$(close_workflow_locks; identity_tool_path "$RUSTC") || return 1
    RUSTDOC_ID_PATH=$(close_workflow_locks; identity_tool_path "$RUSTDOC") || return 1
    MIRVM_ID_PATH=$(close_workflow_locks; identity_tool_path "$MIRVM") || return 1
    BWRAP_ID_PATH=$(close_workflow_locks; identity_tool_path "$BWRAP") || return 1
    PYTHON_ID_PATH=$(close_workflow_locks; identity_tool_path "$PYTHON") || return 1
    GIT_ID_PATH=$(close_workflow_locks; identity_tool_path "$GIT") || return 1
    PROXY_ID_PATH=$(close_workflow_locks; identity_tool_path "$RUSTC_REMAP_PROXY") || return 1
    HARNESS_ID_PATH=$(close_workflow_locks; readlink -f -- tests/real_projects.sh) || return 1
    EVIDENCE_HELPER_ID_PATH=$(close_workflow_locks; readlink -f -- "$EVIDENCE_HELPER") || return 1
    MIRVM_SYSROOT_ID_PATH=$(close_workflow_locks; realpath -m -- "$MIRVM_SYSROOT_MARKER") || return 1
    CARGO_ID_SHA=$(close_workflow_locks; identity_tool_sha "$CARGO_ID_PATH") || return 1
    RUSTC_ID_SHA=$(close_workflow_locks; identity_tool_sha "$RUSTC_ID_PATH") || return 1
    RUSTDOC_ID_SHA=$(close_workflow_locks; identity_tool_sha "$RUSTDOC_ID_PATH") || return 1
    MIRVM_ID_SHA=$(close_workflow_locks; identity_tool_sha "$MIRVM_ID_PATH") || return 1
    BWRAP_ID_SHA=$(close_workflow_locks; identity_tool_sha "$BWRAP_ID_PATH") || return 1
    PYTHON_ID_SHA=$(close_workflow_locks; identity_tool_sha "$PYTHON_ID_PATH") || return 1
    GIT_ID_SHA=$(close_workflow_locks; identity_tool_sha "$GIT_ID_PATH") || return 1
    PROXY_ID_SHA=$(close_workflow_locks; identity_tool_sha "$PROXY_ID_PATH") || return 1
    HARNESS_ID_SHA=$(close_workflow_locks; identity_tool_sha "$HARNESS_ID_PATH") || return 1
    EVIDENCE_HELPER_ID_SHA=$(close_workflow_locks; identity_tool_sha "$EVIDENCE_HELPER_ID_PATH") \
        || return 1
    MIRVM_SYSROOT_ID_SHA=$(close_workflow_locks; identity_tool_sha "$MIRVM_SYSROOT_ID_PATH") \
        || return 1
    compute_workload_tool_identity || return 1
    CHECK_ID=$(close_workflow_locks; python_control - "$CASE_ID" "$HOST_TARGET" \
        "$CARGO_ID_SHA" "$RUSTC_ID_SHA" "$RUSTDOC_ID_SHA" \
        "$MIRVM_ID_SHA" "$BWRAP_ID_SHA" "$PYTHON_ID_SHA" "$GIT_ID_SHA" \
        "$PROXY_ID_SHA" "$HARNESS_ID_SHA" "$EVIDENCE_HELPER_ID_SHA" \
        "$MIRVM_SYSROOT_ID_SHA" "$EVIDENCE_SCHEMA" "$SYSTEM_PATH" \
        "$WORKLOAD_TOOL_COUNT" "${WORKLOAD_TOOL_EVIDENCE_ARGS[@]}" <<'PY'
import hashlib
import json
import sys

(
    case_id,
    host_target,
    cargo_sha,
    rustc_sha,
    rustdoc_sha,
    mirvm_sha,
    bwrap_sha,
    python_sha,
    git_sha,
    proxy_sha,
    harness_sha,
    evidence_helper_sha,
    mirvm_sysroot_marker_sha,
    schema_version,
    system_path,
    workload_tool_count,
    *workload_tools,
) = sys.argv[1:]
schema_version = int(schema_version)
workload_tool_count = int(workload_tool_count)
if len(workload_tools) != workload_tool_count * 3:
    raise ValueError("workload tool identity argument count mismatch")
workload_tool_map = {
    workload_tools[index]: {
        "path": workload_tools[index + 1],
        "sha256": workload_tools[index + 2],
    }
    for index in range(0, len(workload_tools), 3)
}
descriptor = {
    "schema_version": schema_version,
    "case_id": case_id,
    "host_target": host_target,
    "tools": {
        "cargo": cargo_sha,
        "rustc": rustc_sha,
        "rustdoc": rustdoc_sha,
        "mirvm": mirvm_sha,
        "bwrap": bwrap_sha,
        "python": python_sha,
        "git": git_sha,
        "rustc_proxy": proxy_sha,
        "harness": harness_sha,
        "evidence_helper": evidence_helper_sha,
    },
    "inputs": {
        "mirvm_sysroot_marker": mirvm_sysroot_marker_sha,
    },
}
if schema_version == 4:
    descriptor["workload_tools"] = workload_tool_map
    descriptor["inputs"]["system_path"] = system_path
elif schema_version != 3 or workload_tool_map:
    raise ValueError("unsupported check identity schema")
canonical = json.dumps(
    descriptor, ensure_ascii=False, sort_keys=True, separators=(",", ":")
)
print(hashlib.sha256(
    b"mirvm/project-suite/check/v1\0" + canonical.encode("utf-8")
).hexdigest())
PY
    ) || return 1
    [[ "$CHECK_ID" =~ ^[0-9a-f]{64}$ ]]
}

verify_check_identity_unchanged() {
    local expected_check_id=$CHECK_ID
    local expected_evidence_schema=$EVIDENCE_SCHEMA
    local expected_cargo_path=$CARGO_ID_PATH expected_cargo_sha=$CARGO_ID_SHA
    local expected_rustc_path=$RUSTC_ID_PATH expected_rustc_sha=$RUSTC_ID_SHA
    local expected_rustdoc_path=$RUSTDOC_ID_PATH expected_rustdoc_sha=$RUSTDOC_ID_SHA
    local expected_mirvm_path=$MIRVM_ID_PATH expected_mirvm_sha=$MIRVM_ID_SHA
    local expected_bwrap_path=$BWRAP_ID_PATH expected_bwrap_sha=$BWRAP_ID_SHA
    local expected_python_path=$PYTHON_ID_PATH expected_python_sha=$PYTHON_ID_SHA
    local expected_git_path=$GIT_ID_PATH expected_git_sha=$GIT_ID_SHA
    local expected_proxy_path=$PROXY_ID_PATH expected_proxy_sha=$PROXY_ID_SHA
    local expected_harness_path=$HARNESS_ID_PATH expected_harness_sha=$HARNESS_ID_SHA
    local expected_evidence_helper_path=$EVIDENCE_HELPER_ID_PATH
    local expected_evidence_helper_sha=$EVIDENCE_HELPER_ID_SHA
    local expected_sysroot_path=$MIRVM_SYSROOT_ID_PATH
    local expected_sysroot_sha=$MIRVM_SYSROOT_ID_SHA
    local -a expected_workload_tool_paths=("${WORKLOAD_TOOL_ID_PATHS[@]}")
    local -a expected_workload_tool_shas=("${WORKLOAD_TOOL_ID_SHAS[@]}")
    local -a expected_workload_tool_args=("${WORKLOAD_TOOL_EVIDENCE_ARGS[@]}")
    local actual_check_id=
    if compute_check_identity; then
        actual_check_id=$CHECK_ID
    fi
    CHECK_ID=$expected_check_id
    EVIDENCE_SCHEMA=$expected_evidence_schema
    CARGO_ID_PATH=$expected_cargo_path
    CARGO_ID_SHA=$expected_cargo_sha
    RUSTC_ID_PATH=$expected_rustc_path
    RUSTC_ID_SHA=$expected_rustc_sha
    RUSTDOC_ID_PATH=$expected_rustdoc_path
    RUSTDOC_ID_SHA=$expected_rustdoc_sha
    MIRVM_ID_PATH=$expected_mirvm_path
    MIRVM_ID_SHA=$expected_mirvm_sha
    BWRAP_ID_PATH=$expected_bwrap_path
    BWRAP_ID_SHA=$expected_bwrap_sha
    PYTHON_ID_PATH=$expected_python_path
    PYTHON_ID_SHA=$expected_python_sha
    GIT_ID_PATH=$expected_git_path
    GIT_ID_SHA=$expected_git_sha
    PROXY_ID_PATH=$expected_proxy_path
    PROXY_ID_SHA=$expected_proxy_sha
    HARNESS_ID_PATH=$expected_harness_path
    HARNESS_ID_SHA=$expected_harness_sha
    EVIDENCE_HELPER_ID_PATH=$expected_evidence_helper_path
    EVIDENCE_HELPER_ID_SHA=$expected_evidence_helper_sha
    MIRVM_SYSROOT_ID_PATH=$expected_sysroot_path
    MIRVM_SYSROOT_ID_SHA=$expected_sysroot_sha
    WORKLOAD_TOOL_ID_PATHS=("${expected_workload_tool_paths[@]}")
    WORKLOAD_TOOL_ID_SHAS=("${expected_workload_tool_shas[@]}")
    WORKLOAD_TOOL_EVIDENCE_ARGS=("${expected_workload_tool_args[@]}")
    [ -n "$actual_check_id" ] && [ "$actual_check_id" = "$expected_check_id" ]
}

retire_evidence_view() {
    local phase=$1 view="$CASE_ARTIFACT_ROOT/$1" target
    if [ -L "$view" ]; then
        rm -f -- "$view" || return 1
        fsync_evidence_directory "$CASE_ARTIFACT_ROOT" || return 1
    elif [ -e "$view" ]; then
        mkdir -p "$CASE_ARTIFACT_ROOT/legacy" || return 1
        fsync_evidence_directory "$CASE_ARTIFACT_ROOT/legacy" || return 1
        fsync_evidence_directory "$CASE_ARTIFACT_ROOT" || return 1
        target=$(close_workflow_locks; mktemp -d \
            "$CASE_ARTIFACT_ROOT/legacy/$phase.pre-identity.XXXXXX") || return 1
        rmdir -- "$target" || return 1
        mv -- "$view" "$target" || return 1
        fsync_evidence_directory "$CASE_ARTIFACT_ROOT/legacy" || return 1
        fsync_evidence_directory "$CASE_ARTIFACT_ROOT" || return 1
    fi
}

seal_evidence_directory() {
    python_control "$EVIDENCE_HELPER" seal-directory "$1"
}

fsync_evidence_directory() {
    python_control "$EVIDENCE_HELPER" fsync-directory "$1"
}

relocate_evidence_stage() {
    local parent=$1 temporary
    temporary="$parent/.staging.$BASHPID.$RANDOM.$RANDOM"
    [ ! -e "$temporary" ] && [ ! -L "$temporary" ] || return 1
    mv -T -- "$EVIDENCE_STAGE" "$temporary" \
        {EVIDENCE_LOCK_FD}>&- {CACHE_LOCK_FD}>&- || return 1
    EVIDENCE_STAGE=$temporary
    fsync_evidence_directory "$CASE_ARTIFACT_ROOT/.staging" || return 1
    fsync_evidence_directory "$parent" || return 1
}

recover_evidence_attempts() {
    local orphan temporary parent recovered_parents=()
    mkdir -p "$CASE_ARTIFACT_ROOT/.staging" || return 1
    # The name lock proves no live workflow owns these private attempts. A hard
    # crash can leave them behind, but they never contain a published view.
    for orphan in "$CASE_ARTIFACT_ROOT/.staging"/* \
        "$CASE_ARTIFACT_ROOT"/objects/check/*/.staging.* \
        "$CASE_ARTIFACT_ROOT"/objects/bench/*/.staging.*; do
        [ -e "$orphan" ] || [ -L "$orphan" ] || continue
        parent=${orphan%/*}
        chmod -R u+w -- "$orphan" 2>/dev/null || true
        rm -rf -- "$orphan" || return 1
        recovered_parents+=("$parent")
    done
    for temporary in "$CASE_ARTIFACT_ROOT"/.check.*.tmp \
        "$CASE_ARTIFACT_ROOT"/.bench.*.tmp; do
        [ -e "$temporary" ] || [ -L "$temporary" ] || continue
        rm -f -- "$temporary" || return 1
    done
    fsync_evidence_directory "$CASE_ARTIFACT_ROOT/.staging" || return 1
    for parent in "${recovered_parents[@]}"; do
        [ -d "$parent" ] || continue
        fsync_evidence_directory "$parent" || return 1
    done
    fsync_evidence_directory "$CASE_ARTIFACT_ROOT" || return 1
    fsync_evidence_directory "$PROJECT_SUITE_ARTIFACTS" || return 1
}

evidence_begin_check() {
    local parent
    # A current view is eligibility state, not history. Revoke benchmark first
    # so a crash between the two directory updates cannot expose a benchmark
    # without its still-current correctness prerequisite.
    retire_evidence_view bench || return 1
    retire_evidence_view check || return 1
    if [ -L "$MIRVM_SYSROOT_MARKER" ] || [ ! -f "$MIRVM_SYSROOT_MARKER" ] \
        || [ ! -r "$MIRVM_SYSROOT_MARKER" ]; then
        echo "mirvm_sysroot_marker_unavailable: $MIRVM_SYSROOT_MARKER" >&2
        return 1
    fi
    compute_check_identity || return 1
    mkdir -p "$CASE_ARTIFACT_ROOT/objects/check/$CHECK_ID" || return 1
    for parent in "$CASE_ARTIFACT_ROOT/objects/check/$CHECK_ID" \
        "$CASE_ARTIFACT_ROOT/objects/check" \
        "$CASE_ARTIFACT_ROOT/objects" "$CASE_ARTIFACT_ROOT"; do
        fsync_evidence_directory "$parent" || return 1
    done
    EVIDENCE_STAGE=$(close_workflow_locks; mktemp -d \
        "$CASE_ARTIFACT_ROOT/.staging/check.XXXXXX") || return 1
}

publish_check_evidence() {
    local status=$1 evidence_id final relative temporary
    if ! verify_check_identity_unchanged; then
        echo "FAIL $NAME reason=check_identity_changed" >&2
        return 1
    fi
    evidence_id=$(close_workflow_locks; python_control - "$EVIDENCE_STAGE" "$NAME" "$REPO" "$REV" \
        "$LOCK_SHA256" "$SUBDIR" "$EXPECTED_EXIT" "$TIMEOUT_SECONDS" \
        "$HOST_TARGET" "$CASE_ID" "$CHECK_ID" "$status" \
        "$CARGO_ID_PATH" "$CARGO_ID_SHA" "$RUSTC_ID_PATH" "$RUSTC_ID_SHA" \
        "$RUSTDOC_ID_PATH" "$RUSTDOC_ID_SHA" \
        "$MIRVM_ID_PATH" "$MIRVM_ID_SHA" "$BWRAP_ID_PATH" "$BWRAP_ID_SHA" \
        "$PYTHON_ID_PATH" "$PYTHON_ID_SHA" \
        "$GIT_ID_PATH" "$GIT_ID_SHA" \
        "$PROXY_ID_PATH" "$PROXY_ID_SHA" \
        "$HARNESS_ID_PATH" "$HARNESS_ID_SHA" \
        "$EVIDENCE_HELPER_ID_PATH" "$EVIDENCE_HELPER_ID_SHA" \
        "$MIRVM_SYSROOT_ID_PATH" "$MIRVM_SYSROOT_ID_SHA" \
        "$EVIDENCE_SCHEMA" "$SYSTEM_PATH" "$WORKLOAD_TOOL_COUNT" \
        "$XFAIL_EXIT" "$XFAIL_DIAGNOSTIC" "$ENV_COUNT" "$ARG_COUNT" \
        "${WORKLOAD_TOOL_EVIDENCE_ARGS[@]}" "${CASE_ENV[@]}" "${ARGS[@]}" <<'PY'
import hashlib
import json
import pathlib
import sys

(
    stage_path,
    name,
    repo,
    revision,
    lock_sha256,
    subdir,
    expected_exit,
    timeout_seconds,
    host_target,
    case_id,
    check_id,
    status,
    cargo_path,
    cargo_sha,
    rustc_path,
    rustc_sha,
    rustdoc_path,
    rustdoc_sha,
    mirvm_path,
    mirvm_sha,
    bwrap_path,
    bwrap_sha,
    python_path,
    python_sha,
    git_path,
    git_sha,
    proxy_path,
    proxy_sha,
    harness_path,
    harness_sha,
    evidence_helper_path,
    evidence_helper_sha,
    mirvm_sysroot_marker_path,
    mirvm_sysroot_marker_sha,
    schema_version,
    system_path,
    workload_tool_count,
    xfail_exit,
    xfail_diagnostic,
    env_count,
    arg_count,
    *workload,
) = sys.argv[1:]
schema_version = int(schema_version)
workload_tool_count = int(workload_tool_count)
env_count = int(env_count)
arg_count = int(arg_count)
tool_item_count = workload_tool_count * 3
tool_items = workload[:tool_item_count]
workload = workload[tool_item_count:]
env_items = workload[:env_count]
args = workload[env_count:]
if len(args) != arg_count:
    raise ValueError("check metadata argument count mismatch")
environment = dict(item.split("=", 1) for item in env_items)
workload_tool_map = {
    tool_items[index]: {
        "path": tool_items[index + 1],
        "sha256": tool_items[index + 2],
    }
    for index in range(0, len(tool_items), 3)
}
stage = pathlib.Path(stage_path)
payload = {}
for filename in (
    "native.exit",
    "native.stdout",
    "native.stderr",
    "mirvm.exit",
    "mirvm.stdout",
    "mirvm.stderr",
):
    data = (stage / filename).read_bytes()
    payload[filename] = {
        "bytes": len(data),
        "sha256": hashlib.sha256(data).hexdigest(),
    }
document = {
    "schema_version": schema_version,
    "kind": "check",
    "status": status,
    "identity": {
        "case_id": case_id,
        "check_id": check_id,
    },
    "case": {
        "name": name,
        "repo": repo,
        "revision": revision,
        "lock_sha256": lock_sha256,
        "subdir": subdir,
        "args": args,
        "env": environment,
        "expected_exit": int(expected_exit),
        "timeout_seconds": int(timeout_seconds),
        "stdin": {"kind": "eof"},
        "xfail": None if not xfail_exit else {
            "mirvm_exit": int(xfail_exit),
            "diagnostic": xfail_diagnostic,
        },
    },
    "execution": {
        "host_target": host_target,
        "tools": {
            "cargo": {"path": cargo_path, "sha256": cargo_sha},
            "rustc": {"path": rustc_path, "sha256": rustc_sha},
            "rustdoc": {"path": rustdoc_path, "sha256": rustdoc_sha},
            "mirvm": {"path": mirvm_path, "sha256": mirvm_sha},
            "bwrap": {"path": bwrap_path, "sha256": bwrap_sha},
            "python": {"path": python_path, "sha256": python_sha},
            "git": {"path": git_path, "sha256": git_sha},
            "rustc_proxy": {"path": proxy_path, "sha256": proxy_sha},
            "harness": {"path": harness_path, "sha256": harness_sha},
            "evidence_helper": {
                "path": evidence_helper_path,
                "sha256": evidence_helper_sha,
            },
        },
        "inputs": {
            "mirvm_sysroot_marker": {
                "path": mirvm_sysroot_marker_path,
                "sha256": mirvm_sysroot_marker_sha,
            },
        },
    },
    "payload": payload,
}
if schema_version == 4:
    if not workload_tool_map:
        raise ValueError("schema 4 requires workload tools")
    document["execution"]["workload_tools"] = workload_tool_map
    document["execution"]["inputs"]["system_path"] = system_path
elif schema_version != 3 or workload_tool_map:
    raise ValueError("unsupported check metadata schema")
canonical = json.dumps(document, ensure_ascii=False, sort_keys=True,
                       separators=(",", ":")).encode("utf-8")
evidence_id = hashlib.sha256(
    b"mirvm/project-suite/check-evidence/v1\0" + canonical
).hexdigest()
document["identity"]["evidence_id"] = evidence_id
(stage / "result.json").write_text(
    json.dumps(document, ensure_ascii=False, sort_keys=True,
               separators=(",", ":")) + "\n"
)
print(evidence_id)
PY
    ) || {
        echo "FAIL $NAME reason=check_metadata_failed" >&2
        return 1
    }
    if [[ ! "$evidence_id" =~ ^[0-9a-f]{64}$ ]]; then
        echo "FAIL $NAME reason=check_identity_failed" >&2
        return 1
    fi
    if ! verify_check_identity_unchanged; then
        echo "FAIL $NAME reason=check_identity_changed" >&2
        return 1
    fi
    printf 'committed\n' >"$EVIDENCE_STAGE/COMMITTED" || return 1
    if ! python_control "$EVIDENCE_HELPER" verify-check-stage \
        "$EVIDENCE_STAGE/result.json" "$CASE_ID" "$CHECK_ID" \
        "$evidence_id" "$status"; then
        echo "FAIL $NAME reason=evidence_validation_failed" >&2
        return 1
    fi
    final="$CASE_ARTIFACT_ROOT/objects/check/$CHECK_ID/$evidence_id"
    if [ -e "$final" ]; then
        if ! diff -qr -- "$EVIDENCE_STAGE" "$final" >/dev/null; then
            echo "FAIL $NAME reason=evidence_identity_collision" >&2
            return 1
        fi
        rm -rf "$EVIDENCE_STAGE"
    else
        if ! relocate_evidence_stage \
            "$CASE_ARTIFACT_ROOT/objects/check/$CHECK_ID"; then
            echo "FAIL $NAME reason=evidence_publish_failed" >&2
            return 1
        fi
        if ! seal_evidence_directory "$EVIDENCE_STAGE"; then
            echo "FAIL $NAME reason=evidence_seal_failed" >&2
            return 1
        fi
        mv -T -- "$EVIDENCE_STAGE" "$final" \
            {EVIDENCE_LOCK_FD}>&- {CACHE_LOCK_FD}>&- || {
            echo "FAIL $NAME reason=evidence_publish_failed" >&2
            return 1
        }
    fi
    EVIDENCE_STAGE=
    if ! seal_evidence_directory "$final" \
        || ! fsync_evidence_directory "$CASE_ARTIFACT_ROOT/objects/check/$CHECK_ID" \
        || ! fsync_evidence_directory "$CASE_ARTIFACT_ROOT/objects/check" \
        || ! fsync_evidence_directory "$CASE_ARTIFACT_ROOT/objects" \
        || ! fsync_evidence_directory "$CASE_ARTIFACT_ROOT/.staging" \
        || ! fsync_evidence_directory "$CASE_ARTIFACT_ROOT"; then
        echo "FAIL $NAME reason=evidence_durability_failed" >&2
        return 1
    fi
    if ! python_control "$EVIDENCE_HELPER" verify-check \
        "$final/result.json" "$CASE_ID" "$CHECK_ID" "$evidence_id" "$status"; then
        echo "FAIL $NAME reason=evidence_validation_failed" >&2
        return 1
    fi
    relative="objects/check/$CHECK_ID/$evidence_id"
    temporary="$CASE_ARTIFACT_ROOT/.check.$$.tmp"
    rm -f "$temporary"
    ln -s -- "$relative" "$temporary" \
        && mv -Tf -- "$temporary" "$CASE_ARTIFACT_ROOT/check" || {
        rm -f "$temporary"
        echo "FAIL $NAME reason=evidence_view_publish_failed" >&2
        return 1
    }
    if ! fsync_evidence_directory "$CASE_ARTIFACT_ROOT"; then
        rm -f -- "$CASE_ARTIFACT_ROOT/check"
        fsync_evidence_directory "$CASE_ARTIFACT_ROOT" 2>/dev/null || true
        echo "FAIL $NAME reason=evidence_view_durability_failed" >&2
        return 1
    fi
    CHECK_EVIDENCE=$final
    CHECK_EVIDENCE_ID=$evidence_id
}

compute_bench_identity() {
    BENCH_ID=$(close_workflow_locks; python_control - "$CASE_ID" "$CHECK_ID" "$CHECK_EVIDENCE_ID" \
        "$BENCH_WARMUP" "$BENCH_SAMPLES" <<'PY'
import hashlib
import json
import sys

case_id, check_id, check_evidence_id, warmup, samples = sys.argv[1:]
descriptor = {
    "schema_version": 1,
    "case_id": case_id,
    "check_id": check_id,
    "check_evidence_id": check_evidence_id,
    "schedule": {
        "warmup": int(warmup),
        "samples": int(samples),
        "sample_order": "alternating-native-first",
    },
    "clock": "python-perf-counter-ns",
    "unit": "ns",
}
canonical = json.dumps(
    descriptor, ensure_ascii=False, sort_keys=True, separators=(",", ":")
)
print(hashlib.sha256(
    b"mirvm/project-suite/bench/v1\0" + canonical.encode("utf-8")
).hexdigest())
PY
    ) || return 1
    [[ "$BENCH_ID" =~ ^[0-9a-f]{64}$ ]]
}

evidence_begin_bench() {
    local parent
    python_control "$EVIDENCE_HELPER" verify-check \
        "$CHECK_EVIDENCE/result.json" "$CASE_ID" "$CHECK_ID" \
        "$CHECK_EVIDENCE_ID" pass || return 1
    compute_bench_identity || return 1
    retire_evidence_view bench || return 1
    mkdir -p "$CASE_ARTIFACT_ROOT/objects/bench/$BENCH_ID" || return 1
    for parent in "$CASE_ARTIFACT_ROOT/objects/bench/$BENCH_ID" \
        "$CASE_ARTIFACT_ROOT/objects/bench" \
        "$CASE_ARTIFACT_ROOT/objects" "$CASE_ARTIFACT_ROOT"; do
        fsync_evidence_directory "$parent" || return 1
    done
    EVIDENCE_STAGE=$(close_workflow_locks; mktemp -d \
        "$CASE_ARTIFACT_ROOT/.staging/bench.XXXXXX") || return 1
}

publish_bench_evidence() {
    local evidence_id=$1 final relative temporary
    if ! python_control "$EVIDENCE_HELPER" verify-check \
        "$CHECK_EVIDENCE/result.json" "$CASE_ID" "$CHECK_ID" \
        "$CHECK_EVIDENCE_ID" pass; then
        echo "FAIL $NAME reason=check_evidence_changed" >&2
        return 1
    fi
    if ! verify_check_identity_unchanged; then
        echo "FAIL $NAME reason=check_identity_changed" >&2
        return 1
    fi
    if [[ ! "$evidence_id" =~ ^[0-9a-f]{64}$ ]]; then
        echo "FAIL $NAME reason=benchmark_identity_failed" >&2
        return 1
    fi
    printf 'committed\n' >"$EVIDENCE_STAGE/COMMITTED" || return 1
    if ! python_control "$EVIDENCE_HELPER" verify-bench-stage \
        "$EVIDENCE_STAGE/result.json" "$CHECK_EVIDENCE/result.json" \
        "$CASE_ID" "$CHECK_ID" \
        "$CHECK_EVIDENCE_ID" "$BENCH_ID" "$evidence_id" pass; then
        echo "FAIL $NAME reason=evidence_validation_failed" >&2
        return 1
    fi
    final="$CASE_ARTIFACT_ROOT/objects/bench/$BENCH_ID/$evidence_id"
    if [ -e "$final" ]; then
        if ! diff -qr -- "$EVIDENCE_STAGE" "$final" >/dev/null; then
            echo "FAIL $NAME reason=evidence_identity_collision" >&2
            return 1
        fi
        rm -rf "$EVIDENCE_STAGE"
    else
        if ! relocate_evidence_stage \
            "$CASE_ARTIFACT_ROOT/objects/bench/$BENCH_ID"; then
            echo "FAIL $NAME reason=evidence_publish_failed" >&2
            return 1
        fi
        if ! seal_evidence_directory "$EVIDENCE_STAGE"; then
            echo "FAIL $NAME reason=evidence_seal_failed" >&2
            return 1
        fi
        mv -T -- "$EVIDENCE_STAGE" "$final" \
            {EVIDENCE_LOCK_FD}>&- {CACHE_LOCK_FD}>&- || {
            echo "FAIL $NAME reason=evidence_publish_failed" >&2
            return 1
        }
    fi
    EVIDENCE_STAGE=
    if ! seal_evidence_directory "$final" \
        || ! fsync_evidence_directory "$CASE_ARTIFACT_ROOT/objects/bench/$BENCH_ID" \
        || ! fsync_evidence_directory "$CASE_ARTIFACT_ROOT/objects/bench" \
        || ! fsync_evidence_directory "$CASE_ARTIFACT_ROOT/objects" \
        || ! fsync_evidence_directory "$CASE_ARTIFACT_ROOT/.staging" \
        || ! fsync_evidence_directory "$CASE_ARTIFACT_ROOT"; then
        echo "FAIL $NAME reason=evidence_durability_failed" >&2
        return 1
    fi
    if ! python_control "$EVIDENCE_HELPER" verify-bench \
        "$final/result.json" "$CHECK_EVIDENCE/result.json" \
        "$CASE_ID" "$CHECK_ID" "$CHECK_EVIDENCE_ID" \
        "$BENCH_ID" "$evidence_id" pass; then
        echo "FAIL $NAME reason=evidence_validation_failed" >&2
        return 1
    fi
    relative="objects/bench/$BENCH_ID/$evidence_id"
    temporary="$CASE_ARTIFACT_ROOT/.bench.$$.tmp"
    rm -f "$temporary"
    ln -s -- "$relative" "$temporary" \
        && mv -Tf -- "$temporary" "$CASE_ARTIFACT_ROOT/bench" || {
        rm -f "$temporary"
        echo "FAIL $NAME reason=evidence_view_publish_failed" >&2
        return 1
    }
    if ! fsync_evidence_directory "$CASE_ARTIFACT_ROOT"; then
        rm -f -- "$CASE_ARTIFACT_ROOT/bench"
        fsync_evidence_directory "$CASE_ARTIFACT_ROOT" 2>/dev/null || true
        echo "FAIL $NAME reason=evidence_view_durability_failed" >&2
        return 1
    fi
    BENCH_EVIDENCE=$final
    BENCH_EVIDENCE_ID=$evidence_id
}

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
    actual=$(close_workflow_locks; sha256sum "$checkout/$SUBDIR/Cargo.lock" | cut -d' ' -f1)
    if [ "$actual" != "$LOCK_SHA256" ]; then
        echo "lock_hash_mismatch: $NAME" >&2
        return 1
    fi
}

verify_checkout_unchanged() {
    local checkout=$1 side=$2 head
    head=$(close_workflow_locks; git_control -C "$checkout" rev-parse HEAD 2>/dev/null) || {
        echo "FAIL $NAME reason=source_revision_unreadable side=$side"
        return 1
    }
    if [ "$head" != "$REV" ] \
        || ! git_control -C "$checkout" diff --quiet --ignore-submodules -- \
        || ! git_control -C "$checkout" diff --cached --quiet --ignore-submodules --; then
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
        >"$log" 2>&1 {EVIDENCE_LOCK_FD}>&- {CACHE_LOCK_FD}>&-; then
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
        base=$(close_workflow_locks; basename "$item")
        case "$base" in
            # The project harness never materializes frontmatter scripts. Copying
            # this unrelated cache can be hundreds of MiB per isolated side.
            scripts) continue ;;
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
    "$BWRAP" --die-with-parent --unshare-pid --ro-bind / / \
        --bind "$root" "$root" \
        --bind "$CARGO_CACHE" "$CARGO_CACHE" \
        --dev /dev --proc /proc --chdir "$PWD" \
        /usr/bin/env -i HOME="$root/home" TMPDIR="$root/tmp" \
            XDG_CACHE_HOME="$root/xdg" CARGO_HOME="$CARGO_CACHE" \
            PATH="$SYSTEM_PATH" LANG=C.UTF-8 LC_ALL=C.UTF-8 TZ=UTC TERM=dumb \
            RUSTC="$RUSTC" RUSTDOC="$RUSTDOC" RUSTUP_HOME="$HOST_RUSTUP_HOME" \
            "$@" {EVIDENCE_LOCK_FD}>&- {CACHE_LOCK_FD}>&-
}

prepare_build_env() {
    local root=$1
    shift
    if [ -z "$BWRAP" ]; then
        echo "prepare_sandbox_unavailable: install bwrap" >&2
        return 69
    fi
    "$BWRAP" --die-with-parent --unshare-net --unshare-pid --ro-bind / / \
        --bind "$root" "$root" \
        --ro-bind "$CARGO_CACHE" "$CARGO_CACHE" \
        --dev /dev --proc /proc --chdir "$PWD" \
        /usr/bin/env -i HOME="$root/home" TMPDIR="$root/tmp" \
            XDG_CACHE_HOME="$root/xdg" CARGO_HOME="$CARGO_CACHE" \
            CARGO_NET_OFFLINE=true PATH="$SYSTEM_PATH" \
            LANG=C.UTF-8 LC_ALL=C.UTF-8 TZ=UTC TERM=dumb \
            RUSTC="$RUSTC" RUSTDOC="$RUSTDOC" RUSTUP_HOME="$HOST_RUSTUP_HOME" \
            "${CASE_ENV[@]}" "$@" \
            {EVIDENCE_LOCK_FD}>&- {CACHE_LOCK_FD}>&-
}

prepare_case() {
    local tmp actual manifest
    mkdir -p "$PROJECT_SUITE_ROOT/mirrors/$NAME" \
        "$PROJECT_SUITE_ROOT/prepared/$NAME/$REPO_SHA256" \
        "$PROJECT_SUITE_ROOT/tmp" "$CARGO_CACHE" "$PROJECT_SUITE_MIRVM_CACHE"

    if [ -d "$MIRROR" ]; then
        git_control --git-dir="$MIRROR" remote set-url origin "$REPO"
        git_control --git-dir="$MIRROR" fetch -q --prune origin '+refs/*:refs/*' || {
            echo "prepare_fetch_failed: $NAME" >&2
            return 1
        }
    else
        git_control clone -q --mirror -- "$REPO" "$MIRROR" || {
            echo "prepare_clone_failed: $NAME" >&2
            return 1
        }
    fi
    git_control --git-dir="$MIRROR" cat-file -e "$REV^{commit}" 2>/dev/null || {
        echo "revision_not_found: $NAME $REV" >&2
        return 1
    }

    tmp=$(close_workflow_locks; mktemp -d "$PROJECT_SUITE_ROOT/tmp/prepare.XXXXXX")
    git_control clone -q --no-checkout -- "$MIRROR" "$tmp/source" \
        && git_control -C "$tmp/source" checkout -q --detach "$REV" || {
        rm -rf "$tmp"
        echo "prepare_checkout_failed: $NAME" >&2
        return 1
    }
    if ! verify_lock_file "$tmp/source"; then
        rm -rf "$tmp"
        return 1
    fi
    actual=$(close_workflow_locks; sha256sum "$tmp/source/$(lock_path)" | cut -d' ' -f1)
    if [ "$actual" != "$LOCK_SHA256" ]; then
        rm -rf "$tmp"
        echo "lock_hash_mismatch: $NAME" >&2
        return 1
    fi

    mkdir -p "$tmp/home" "$tmp/tmp" "$tmp/xdg"
    manifest="$tmp/source/$SUBDIR/Cargo.toml"
    if ! (
        close_workflow_locks
        cd "$tmp/source/$SUBDIR" \
            && prepare_fetch_env "$tmp" \
                "$CARGO" fetch --locked --target "$HOST_TARGET" \
                    --manifest-path "$manifest" --quiet
    ); then
        rm -rf "$tmp"
        echo "prepare_cargo_fetch_failed: $NAME" >&2
        return 1
    fi
    if ! (
        close_workflow_locks
        cd "$tmp/source/$SUBDIR" \
            && prepare_build_env "$tmp" \
                "$CARGO" build --locked --target "$HOST_TARGET" \
                    --manifest-path "$manifest" --quiet
    ); then
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
    if [ ! -f "$READY" ] \
        || [ "$(close_workflow_locks; cat "$READY")" != "$LOCK_SHA256" ] \
        || [ ! -d "$MIRROR" ]; then
        echo "case_not_prepared: $NAME" >&2
        return 1
    fi
}

isolated_env() {
    local root=$1 side_root logical_root logical_env_root logical_cwd encoded_rustflags
    side_root=$(close_workflow_locks; dirname "$root")
    # This path exists only in a namespace-private /run tmpfs. It neither trusts
    # a predictable host /tmp entry nor hides a commonly mounted /mnt workspace.
    logical_root=/run/mirvm-project-side
    logical_env_root="$logical_root/${root#"$side_root"/}"
    if [[ $PWD == "$side_root"/* ]] || [ "$PWD" = "$side_root" ]; then
        logical_cwd="$logical_root${PWD#"$side_root"}"
    else
        logical_cwd=$PWD
    fi
    printf -v encoded_rustflags '%s\x1f%s\x1f%s\x1f%s\x1f%s' \
        "--remap-path-prefix=$logical_root=/mirvm-project" \
        "--remap-path-prefix=$logical_env_root/target=/mirvm-project/target" \
        "--remap-path-prefix=$logical_root/source/target/mirvm=/mirvm-project/target" \
        "--remap-path-prefix=$logical_root/source/./target/mirvm=/mirvm-project/target" \
        '--remap-path-scope=diagnostics'
    mkdir -p "$root/home" "$root/tmp" "$root/xdg" "$root/target"
    if [ -z "$BWRAP" ]; then
        echo "network_sandbox_unavailable: install bwrap" >&2
        return 69
    fi
    "$BWRAP" --die-with-parent --unshare-net --unshare-pid --ro-bind / / \
        --bind "$side_root" "$side_root" \
        --tmpfs /run --dir "$logical_root" \
        --bind "$side_root" "$logical_root" \
        --dev /dev --proc /proc --chdir "$logical_cwd" \
        /usr/bin/env -i HOME="$logical_env_root/home" TMPDIR="$logical_env_root/tmp" \
            XDG_CACHE_HOME="$logical_env_root/xdg" \
            CARGO_HOME="$CARGO_CACHE" CARGO_TARGET_DIR="$logical_env_root/target" \
            CARGO_NET_OFFLINE=true PATH="$SYSTEM_PATH" \
            LANG=C.UTF-8 LC_ALL=C.UTF-8 TZ=UTC TERM=dumb \
            RUSTC="$RUSTC_REMAP_PROXY" RUSTDOC="$RUSTDOC" \
            PROJECT_SUITE_RUSTC="$RUSTC" \
            PROJECT_SUITE_ENCODED_RUSTFLAGS_APPEND="$encoded_rustflags" \
            MIRVM_ENCODED_RUSTFLAGS_APPEND="$encoded_rustflags" \
            RUSTUP_HOME="$HOST_RUSTUP_HOME" \
            "${@:2}" {EVIDENCE_LOCK_FD}>&- {CACHE_LOCK_FD}>&-
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
    local env_root=$1 cwd=$2 duration_file=$3 stdout_file=$4 stderr_file=$5 side_root
    local status_file="$3.status" kind code
    shift 5
    side_root=$(close_workflow_locks; dirname "$env_root")
    if [[ $cwd == "$side_root"/* ]] || [ "$cwd" = "$side_root" ]; then
        cwd="/run/mirvm-project-side${cwd#"$side_root"}"
    fi
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
    prepare_mirvm_runtime_cache "$env_root/xdg" || return 1
    measure_command "$env_root" "$source/$SUBDIR" "$duration_file" \
        "$stdout_file" "$stderr_file" timeout "$TIMEOUT_SECONDS" \
        env "${CASE_ENV[@]}" \
        "$CARGO" run --locked --target "$HOST_TARGET" --quiet -- "${ARGS[@]}"
}

measure_mirvm() {
    local env_root=$1 source=$2 duration_file=$3 stdout_file=$4 stderr_file=$5
    local runtime_cache="$1/xdg"
    prepare_mirvm_runtime_cache "$runtime_cache" || return 1
    measure_command "$env_root" "$source/$SUBDIR" "$duration_file" \
        "$stdout_file" "$stderr_file" timeout "$TIMEOUT_SECONDS" \
        env "${CASE_ENV[@]}" \
        MIRVM_CARGO_LOCKED=1 \
        "$MIRVM" run . -- "${ARGS[@]}"
}

matches_correctness_oracle() {
    local code=$1 stdout_file=$2 stderr_file=$3 oracle_code
    local oracle=${CHECK_EVIDENCE:-$CASE_ARTIFACT_ROOT/check}
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
    local native_status mirvm_status native_runtime_cache mirvm_runtime_cache
    local line diagnostic_lines matching_diagnostics
    CHECK_RESULT=fail
    evidence_begin_check || {
        echo "FAIL $NAME reason=evidence_stage_failed" >&2
        return 1
    }
    artifact=$EVIDENCE_STAGE
    require_prepared || return 1
    mkdir -p "$PROJECT_SUITE_ROOT/runs"
    run_root=$(close_workflow_locks; mktemp -d "$PROJECT_SUITE_ROOT/runs/$NAME.XXXXXX")
    native_src="$run_root/native/source"
    mirvm_src="$run_root/mirvm/source"
    mkdir -p "$run_root/native" "$run_root/mirvm"
    git_control clone -q --no-checkout -- "$MIRROR" "$native_src" \
        && git_control -C "$native_src" checkout -q --detach "$REV" \
        && git_control clone -q --no-checkout -- "$MIRROR" "$mirvm_src" \
        && git_control -C "$mirvm_src" checkout -q --detach "$REV" || {
        rm -rf "$run_root"
        echo "check_checkout_failed: $NAME" >&2
        return 1
    }
    verify_lock_file "$native_src" && verify_lock_file "$mirvm_src" || {
        rm -rf "$run_root"
        return 1
    }

    native_status="$run_root/native/status"
    native_runtime_cache="$run_root/native/env/xdg"
    if ! prepare_mirvm_runtime_cache "$native_runtime_cache"; then
        rm -rf "$run_root"
        echo "FAIL $NAME reason=runner_infrastructure_failed side=native"
        return 1
    fi
    (
        close_workflow_locks
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
    mirvm_runtime_cache="$run_root/mirvm/env/xdg"
    if ! prepare_mirvm_runtime_cache "$mirvm_runtime_cache"; then
        rm -rf "$run_root"
        echo "FAIL $NAME reason=runner_infrastructure_failed side=mirvm"
        return 1
    fi
    (
        close_workflow_locks
        cd "$mirvm_src/$SUBDIR" || exit 72
        run_isolated "$run_root/mirvm/env" "$mirvm_status" \
            timeout "$TIMEOUT_SECONDS" \
            env "${CASE_ENV[@]}" \
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
                if ! publish_check_evidence xfail; then
                    CHECK_RESULT=fail
                    return 1
                fi
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
    if ! publish_check_evidence pass; then
        CHECK_RESULT=fail
        return 1
    fi
    echo "PASS $NAME correctness"
}

bench_case() {
    local artifact evidence_file evidence_id
    local run_root native_src mirvm_src native_scratch mirvm_scratch raw i order
    local native_code mirvm_code native_ns mirvm_ns
    local cargo_path rustc_path mirvm_path cargo_sha rustc_sha mirvm_sha
    if ! check_case >&2; then
        echo "FAIL $NAME reason=correctness_not_passed" >&2
        return 1
    fi
    if [ "$CHECK_RESULT" != pass ]; then
        echo "FAIL $NAME reason=expected_red_not_benchmarkable" >&2
        return 1
    fi
    if ! evidence_begin_bench; then
        echo "FAIL $NAME reason=benchmark_evidence_stage_failed" >&2
        return 1
    fi
    artifact=$EVIDENCE_STAGE

    mkdir -p "$PROJECT_SUITE_ROOT/runs"
    run_root=$(close_workflow_locks; mktemp -d "$PROJECT_SUITE_ROOT/runs/$NAME-bench.XXXXXX")
    native_src="$run_root/native/source"
    mirvm_src="$run_root/mirvm/source"
    native_scratch="$run_root/native/measure"
    mirvm_scratch="$run_root/mirvm/measure"
    raw="$run_root/samples.tsv"
    mkdir -p "$run_root/native" "$run_root/mirvm" \
        "$native_scratch" "$mirvm_scratch"
    : >"$raw"
    git_control clone -q --no-checkout -- "$MIRROR" "$native_src" \
        && git_control -C "$native_src" checkout -q --detach "$REV" \
        && git_control clone -q --no-checkout -- "$MIRROR" "$mirvm_src" \
        && git_control -C "$mirvm_src" checkout -q --detach "$REV" || {
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
        native_ns=$(close_workflow_locks; cat "$native_scratch/time")
        mirvm_ns=$(close_workflow_locks; cat "$mirvm_scratch/time")
        printf '%s\t%s\t%s\t%s\n' "$((i + 1))" "$order" "$native_ns" "$mirvm_ns" >>"$raw"
    done

    if [[ "$CARGO" == */* ]]; then
        cargo_path=$(close_workflow_locks; readlink -f -- "$CARGO")
    else
        cargo_path=$(close_workflow_locks; command -v -- "$CARGO")
    fi
    if [[ "$RUSTC" == */* ]]; then
        rustc_path=$(close_workflow_locks; readlink -f -- "$RUSTC")
    else
        rustc_path=$(close_workflow_locks; command -v -- "$RUSTC")
    fi
    if [[ "$MIRVM" == */* ]]; then
        mirvm_path=$(close_workflow_locks; readlink -f -- "$MIRVM")
    else
        mirvm_path=$(close_workflow_locks; command -v -- "$MIRVM")
    fi
    cargo_sha=$(close_workflow_locks; sha256sum -- "$cargo_path" | cut -d' ' -f1) \
        && rustc_sha=$(close_workflow_locks; sha256sum -- "$rustc_path" | cut -d' ' -f1) \
        && mirvm_sha=$(close_workflow_locks; sha256sum -- "$mirvm_path" | cut -d' ' -f1) || {
        rm -rf "$run_root"
        echo "FAIL $NAME reason=benchmark_tool_identity_failed" >&2
        return 1
    }
    if [ "$cargo_sha" != "$CARGO_ID_SHA" ] \
        || [ "$rustc_sha" != "$RUSTC_ID_SHA" ] \
        || [ "$mirvm_sha" != "$MIRVM_ID_SHA" ]; then
        rm -rf "$run_root"
        echo "FAIL $NAME reason=benchmark_tool_identity_changed" >&2
        return 1
    fi

    evidence_file="$run_root/bench-evidence-id"
    if ! python_control - "$raw" "$artifact/samples.jsonl" \
        "$artifact/summary.json" "$NAME" "$REPO" "$REV" "$LOCK_SHA256" \
        "$SUBDIR" "$EXPECTED_EXIT" "$TIMEOUT_SECONDS" "$HOST_TARGET" \
        "$BENCH_WARMUP" "$BENCH_SAMPLES" \
        "$cargo_path" "$cargo_sha" "$rustc_path" "$rustc_sha" \
        "$mirvm_path" "$mirvm_sha" "$CASE_ID" "$CHECK_ID" \
        "$CHECK_EVIDENCE_ID" "$BENCH_ID" "$EVIDENCE_SCHEMA" \
        "$SYSTEM_PATH" "$WORKLOAD_TOOL_COUNT" "$ENV_COUNT" "$ARG_COUNT" \
        "${WORKLOAD_TOOL_EVIDENCE_ARGS[@]}" "${CASE_ENV[@]}" "${ARGS[@]}" <<'PY'
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
    case_id,
    check_id,
    check_evidence_id,
    bench_id,
    schema_version,
    system_path,
    workload_tool_count,
    env_count,
    arg_count,
    *workload,
) = sys.argv[1:]
schema_version = int(schema_version)
workload_tool_count = int(workload_tool_count)
env_count = int(env_count)
arg_count = int(arg_count)
tool_item_count = workload_tool_count * 3
tool_items = workload[:tool_item_count]
workload = workload[tool_item_count:]
env_items = workload[:env_count]
args = workload[env_count:]
if len(args) != arg_count:
    raise ValueError("benchmark metadata argument count mismatch")
environment = dict(item.split("=", 1) for item in env_items)
workload_tool_map = {
    tool_items[index]: {
        "path": tool_items[index + 1],
        "sha256": tool_items[index + 2],
    }
    for index in range(0, len(tool_items), 3)
}
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
    "schema_version": schema_version,
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
    "identity": {
        "case_id": case_id,
        "check_id": check_id,
        "check_evidence_id": check_evidence_id,
        "bench_id": bench_id,
    },
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
if schema_version == 4:
    if not workload_tool_map:
        raise ValueError("schema 4 requires workload tools")
    summary["workload_tools"] = workload_tool_map
    summary["system_path"] = system_path
elif schema_version != 3 or workload_tool_map:
    raise ValueError("unsupported benchmark metadata schema")
samples_text = "".join(
    json.dumps(row, sort_keys=True, separators=(",", ":")) + "\n" for row in rows
)
pathlib.Path(samples_path).write_text(samples_text)
pathlib.Path(summary_path).write_text(
    json.dumps(summary, sort_keys=True, separators=(",", ":")) + "\n"
)
PY
    then
        rm -rf "$run_root"
        echo "FAIL $NAME reason=benchmark_summary_failed" >&2
        return 1
    fi
    if ! python_control "$EVIDENCE_HELPER" verify-check \
        "$CHECK_EVIDENCE/result.json" "$CASE_ID" "$CHECK_ID" \
        "$CHECK_EVIDENCE_ID" pass; then
        rm -rf "$run_root"
        echo "FAIL $NAME reason=check_evidence_changed" >&2
        return 1
    fi
    if ! python_control "$EVIDENCE_HELPER" create-bench-result \
        "$artifact" "$CHECK_EVIDENCE/result.json" \
        "$CASE_ID" "$CHECK_ID" "$CHECK_EVIDENCE_ID" \
        "$BENCH_ID" >"$evidence_file"; then
        rm -rf "$run_root"
        echo "FAIL $NAME reason=benchmark_metadata_failed" >&2
        return 1
    fi
    read -r evidence_id <"$evidence_file" || {
        rm -rf "$run_root"
        echo "FAIL $NAME reason=benchmark_identity_failed" >&2
        return 1
    }
    if ! publish_bench_evidence "$evidence_id"; then
        rm -rf "$run_root"
        return 1
    fi
    rm -rf "$run_root"
    cat "$BENCH_EVIDENCE/summary.json"
}

acquire_evidence_lock
lock_code=$?
if [ "$lock_code" -ne 0 ]; then
    exit "$lock_code"
fi
acquire_cache_lock
lock_code=$?
if [ "$lock_code" -ne 0 ]; then
    exit "$lock_code"
fi
if ! recover_evidence_attempts; then
    echo "evidence_recovery_failed: $NAME" >&2
    exit 69
fi

case "$MODE" in
    prepare) prepare_case ;;
    check) check_case ;;
    bench) bench_case ;;
esac
