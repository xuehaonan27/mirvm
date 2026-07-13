#!/usr/bin/env python3
"""Validate content-addressed evidence produced by real_projects.sh."""

from __future__ import annotations

import copy
import hashlib
import json
import math
import os
import pathlib
import stat
import sys
from typing import NoReturn


CHECK_PAYLOAD = (
    "native.exit",
    "native.stdout",
    "native.stderr",
    "mirvm.exit",
    "mirvm.stdout",
    "mirvm.stderr",
)
BENCH_PAYLOAD = (
    "samples.jsonl",
    "summary.json",
)
CURRENT_SCHEMA_VERSION = 3
LEGACY_SCHEMA_VERSION = 2
WORKLOAD_TOOLS_SCHEMA_VERSION = 4
CHECK_TOOLS_V2 = (
    "cargo",
    "rustc",
    "rustdoc",
    "mirvm",
    "bwrap",
    "python",
    "rustc_proxy",
    "harness",
    "evidence_helper",
)
CHECK_TOOLS_V3 = (
    "cargo",
    "rustc",
    "rustdoc",
    "mirvm",
    "bwrap",
    "python",
    "git",
    "rustc_proxy",
    "harness",
    "evidence_helper",
)


class EvidenceCorrupt(Exception):
    pass


def reject(label: str) -> NoReturn:
    raise EvidenceCorrupt(label)


def canonical_json(value: object) -> bytes:
    return json.dumps(
        value,
        ensure_ascii=False,
        sort_keys=True,
        separators=(",", ":"),
    ).encode("utf-8")


def require_keys(value: object, keys: set[str], label: str) -> dict[str, object]:
    if not isinstance(value, dict) or set(value) != keys:
        reject(label)
    return value


def require_nonnegative_int(value: object, label: str) -> int:
    if type(value) is not int or value < 0:
        reject(label)
    return value


def require_digest(value: object, label: str) -> str:
    if (
        not isinstance(value, str)
        or len(value) != 64
        or any(character not in "0123456789abcdef" for character in value)
    ):
        reject(label)
    return value


def require_tool_digest(value: object, label: str) -> str:
    if value == "unavailable":
        return value
    return require_digest(value, label)


def derive_case_id_from_case(case: object) -> str:
    case = require_keys(
        case,
        {
            "name",
            "repo",
            "revision",
            "lock_sha256",
            "subdir",
            "args",
            "env",
            "expected_exit",
            "timeout_seconds",
            "stdin",
            "xfail",
        },
        "case",
    )
    descriptor = {
        "schema_version": 1,
        "repo": case["repo"],
        "revision": case["revision"],
        "lock_sha256": case["lock_sha256"],
        "subdir": case["subdir"],
        "args": case["args"],
        "env": case["env"],
        "expected_exit": case["expected_exit"],
        "timeout_seconds": case["timeout_seconds"],
        "normalizers": [],
        "stdin": case["stdin"],
        "xfail": case["xfail"],
    }
    return hashlib.sha256(
        b"mirvm/project-suite/case/v1\0" + canonical_json(descriptor)
    ).hexdigest()


def derive_case_id_from_summary(summary: dict[str, object]) -> str:
    keys = {
        "schema_version",
        "case",
        "repo",
        "revision",
        "lock_sha256",
        "subdir",
        "args",
        "env",
        "expected_exit",
        "timeout_seconds",
        "host_target",
        "benchmark",
        "identity",
        "tools",
        "samples",
        "unit",
        "native",
        "mirvm",
    }
    if isinstance(summary, dict) \
        and summary.get("schema_version") == WORKLOAD_TOOLS_SCHEMA_VERSION:
        keys.update({"workload_tools", "system_path"})
    summary = require_keys(
        summary, keys, "summary"
    )
    descriptor = {
        "schema_version": 1,
        "repo": summary.get("repo"),
        "revision": summary.get("revision"),
        "lock_sha256": summary.get("lock_sha256"),
        "subdir": summary.get("subdir"),
        "args": summary.get("args"),
        "env": summary.get("env"),
        "expected_exit": summary.get("expected_exit"),
        "timeout_seconds": summary.get("timeout_seconds"),
        "normalizers": [],
        "stdin": {"kind": "eof"},
        "xfail": None,
    }
    return hashlib.sha256(
        b"mirvm/project-suite/case/v1\0" + canonical_json(descriptor)
    ).hexdigest()


def derive_check_id(document: dict[str, object], case_id: str) -> str:
    schema_version = document.get("schema_version")
    execution_keys = {"host_target", "tools", "inputs"}
    if schema_version == WORKLOAD_TOOLS_SCHEMA_VERSION:
        execution_keys.add("workload_tools")
    execution = require_keys(document.get("execution"), execution_keys, "execution")
    raw_tools = execution["tools"]
    if not isinstance(raw_tools, dict):
        reject("execution.tools")
    tool_keys = set(raw_tools)
    if schema_version == LEGACY_SCHEMA_VERSION and tool_keys == set(CHECK_TOOLS_V2):
        tool_names = CHECK_TOOLS_V2
    elif schema_version in {
        LEGACY_SCHEMA_VERSION,
        CURRENT_SCHEMA_VERSION,
        WORKLOAD_TOOLS_SCHEMA_VERSION,
    } \
        and tool_keys == set(CHECK_TOOLS_V3):
        # schema 2 + Git was emitted briefly during the local migration. Keep
        # it verifiable as history; only schema 3 is emitted as current.
        tool_names = CHECK_TOOLS_V3
    else:
        reject("execution.tools")
    tools = require_keys(raw_tools, set(tool_names), "execution.tools")
    input_keys = {"mirvm_sysroot_marker"}
    if schema_version == WORKLOAD_TOOLS_SCHEMA_VERSION:
        input_keys.add("system_path")
    inputs = require_keys(execution["inputs"], input_keys, "execution.inputs")

    def content_hash(value: object, label: str) -> object:
        descriptor = require_keys(value, {"path", "sha256"}, label)
        if not isinstance(descriptor["path"], str):
            reject(f"{label}.path")
        return require_tool_digest(descriptor["sha256"], f"{label}.sha256")

    descriptor = {
        "schema_version": schema_version,
        "case_id": case_id,
        "host_target": execution["host_target"],
        "tools": {
            name: content_hash(tools[name], f"execution.tools.{name}")
            for name in tool_names
        },
        "inputs": {
            "mirvm_sysroot_marker": content_hash(
                inputs["mirvm_sysroot_marker"],
                "execution.inputs.mirvm_sysroot_marker",
            )
        },
    }
    if schema_version == WORKLOAD_TOOLS_SCHEMA_VERSION:
        system_path = inputs["system_path"]
        if not isinstance(system_path, str) or not system_path or "\0" in system_path:
            reject("execution.inputs.system_path")
        raw_workload_tools = execution["workload_tools"]
        if not isinstance(raw_workload_tools, dict) or not raw_workload_tools:
            reject("execution.workload_tools")
        workload_tools: dict[str, dict[str, str]] = {}
        for name, value in raw_workload_tools.items():
            if not isinstance(name, str) or not name \
                or "/" in name or name in {".", ".."}:
                reject("execution.workload_tools")
            tool = require_keys(
                value, {"path", "sha256"}, f"execution.workload_tools.{name}"
            )
            path = tool["path"]
            if not isinstance(path, str) or not pathlib.PurePath(path).is_absolute() \
                or "\0" in path:
                reject(f"execution.workload_tools.{name}.path")
            workload_tools[name] = {
                "path": path,
                "sha256": require_digest(
                    tool["sha256"], f"execution.workload_tools.{name}.sha256"
                ),
            }
        descriptor["workload_tools"] = workload_tools
        descriptor["inputs"]["system_path"] = system_path
    return hashlib.sha256(
        b"mirvm/project-suite/check/v1\0" + canonical_json(descriptor)
    ).hexdigest()


def derive_bench_id(summary: dict[str, object]) -> str:
    identity = require_keys(
        summary.get("identity"),
        {"case_id", "check_id", "check_evidence_id", "bench_id"},
        "summary.identity",
    )
    benchmark = require_keys(
        summary.get("benchmark"), {"warmup", "samples"}, "summary.benchmark"
    )
    descriptor = {
        "schema_version": 1,
        "case_id": identity["case_id"],
        "check_id": identity["check_id"],
        "check_evidence_id": identity["check_evidence_id"],
        "schedule": {
            "warmup": benchmark["warmup"],
            "samples": benchmark["samples"],
            "sample_order": "alternating-native-first",
        },
        "clock": "python-perf-counter-ns",
        "unit": "ns",
    }
    return hashlib.sha256(
        b"mirvm/project-suite/bench/v1\0" + canonical_json(descriptor)
    ).hexdigest()


def verify_benchmark_summary(root: pathlib.Path, summary: dict[str, object]) -> None:
    benchmark = require_keys(
        summary.get("benchmark"), {"warmup", "samples"}, "summary.benchmark"
    )
    require_nonnegative_int(benchmark["warmup"], "summary.benchmark.warmup")
    configured_samples = require_nonnegative_int(
        benchmark["samples"], "summary.benchmark.samples"
    )
    if configured_samples == 0:
        reject("summary.benchmark.samples")
    if summary.get("samples") != configured_samples:
        reject("summary.samples")
    if summary.get("unit") != "ns" or not isinstance(summary.get("case"), str):
        reject("summary.unit")

    tools = require_keys(
        summary.get("tools"), {"cargo", "rustc", "mirvm"}, "summary.tools"
    )
    for name, value in tools.items():
        descriptor = require_keys(value, {"path", "sha256"}, f"summary.tools.{name}")
        if not isinstance(descriptor["path"], str):
            reject(f"summary.tools.{name}.path")
        require_digest(descriptor["sha256"], f"summary.tools.{name}.sha256")

    if summary.get("schema_version") == WORKLOAD_TOOLS_SCHEMA_VERSION:
        system_path = summary.get("system_path")
        if not isinstance(system_path, str) or not system_path or "\0" in system_path:
            reject("summary.system_path")
        workload_tools = summary.get("workload_tools")
        if not isinstance(workload_tools, dict) or not workload_tools:
            reject("summary.workload_tools")
        for name, value in workload_tools.items():
            if not isinstance(name, str) or not name \
                or "/" in name or name in {".", ".."}:
                reject("summary.workload_tools")
            descriptor = require_keys(
                value, {"path", "sha256"}, f"summary.workload_tools.{name}"
            )
            path = descriptor["path"]
            if not isinstance(path, str) or not pathlib.PurePath(path).is_absolute() \
                or "\0" in path:
                reject(f"summary.workload_tools.{name}.path")
            require_digest(
                descriptor["sha256"], f"summary.workload_tools.{name}.sha256"
            )

    samples_path = root / "samples.jsonl"
    try:
        lines = samples_path.read_text().splitlines()
    except (OSError, UnicodeError):
        reject("samples.jsonl")
    if len(lines) != configured_samples:
        reject("samples.count")
    native_values: list[int] = []
    mirvm_values: list[int] = []
    for expected_index, line in enumerate(lines, start=1):
        try:
            row = json.loads(line)
        except json.JSONDecodeError:
            reject("samples.jsonl")
        row = require_keys(
            row,
            {"case", "index", "order", "native_ns", "mirvm_ns"},
            f"samples.{expected_index}",
        )
        expected_order = "native-first" if expected_index % 2 else "mirvm-first"
        if row["case"] != summary["case"]:
            reject(f"samples.{expected_index}.case")
        if row["index"] != expected_index:
            reject(f"samples.{expected_index}.index")
        if row["order"] != expected_order:
            reject(f"samples.{expected_index}.order")
        native_values.append(
            require_nonnegative_int(row["native_ns"], f"samples.{expected_index}.native_ns")
        )
        mirvm_values.append(
            require_nonnegative_int(row["mirvm_ns"], f"samples.{expected_index}.mirvm_ns")
        )

    def median(values: list[int]) -> int:
        ordered = sorted(values)
        middle = len(ordered) // 2
        if len(ordered) % 2:
            return ordered[middle]
        return (ordered[middle - 1] + ordered[middle]) // 2

    def p95(values: list[int]) -> int:
        ordered = sorted(values)
        return ordered[max(0, math.ceil(len(ordered) * 0.95) - 1)]

    expected_native = {
        "median_ns": median(native_values),
        "p95_ns": p95(native_values),
    }
    expected_mirvm = {
        "median_ns": median(mirvm_values),
        "p95_ns": p95(mirvm_values),
    }
    if summary.get("native") != expected_native:
        reject("summary.native")
    if summary.get("mirvm") != expected_mirvm:
        reject("summary.mirvm")


def resolve_result_path(result_path: pathlib.Path) -> pathlib.Path:
    if result_path.name != "result.json" or result_path.is_symlink():
        reject("result.path")
    try:
        resolved = result_path.resolve(strict=True)
    except OSError:
        reject("result.path")
    if resolved.name != "result.json" or resolved.is_symlink():
        reject("result.path")
    return resolved


def payload_descriptors(root: pathlib.Path, filenames: tuple[str, ...]) -> dict[str, dict[str, object]]:
    payload: dict[str, dict[str, object]] = {}
    for filename in filenames:
        path = root / filename
        if path.is_symlink() or not path.is_file():
            reject(f"payload.{filename}.path")
        try:
            data = path.read_bytes()
        except OSError:
            reject(f"payload.{filename}.path")
        payload[filename] = {
            "bytes": len(data),
            "sha256": hashlib.sha256(data).hexdigest(),
        }
    return payload


def verify_payload(
    root: pathlib.Path,
    payload: object,
    filenames: tuple[str, ...],
) -> None:
    if not isinstance(payload, dict) or set(payload) != set(filenames):
        reject("payload")
    actual = payload_descriptors(root, filenames)
    for filename in filenames:
        descriptor = payload.get(filename)
        if not isinstance(descriptor, dict) or set(descriptor) != {"bytes", "sha256"}:
            reject(f"payload.{filename}")
        if descriptor.get("sha256") != actual[filename]["sha256"]:
            reject(f"payload.{filename}.sha256")
        if descriptor.get("bytes") != actual[filename]["bytes"]:
            reject(f"payload.{filename}.bytes")


def verify_directory_entries(root: pathlib.Path, payload: tuple[str, ...]) -> None:
    try:
        actual = {entry.name for entry in root.iterdir()}
    except OSError:
        reject("directory.entries")
    expected = set(payload) | {"result.json", "COMMITTED"}
    if actual != expected:
        reject("directory.entries")


def parse_exit(root: pathlib.Path, filename: str) -> int:
    try:
        data = (root / filename).read_bytes()
    except OSError:
        reject(f"oracle.{filename}")
    try:
        text = data.decode("ascii")
    except UnicodeDecodeError:
        reject(f"oracle.{filename}")
    if not text.endswith("\n") or text.count("\n") != 1 or not text[:-1].isdigit():
        reject(f"oracle.{filename}")
    value = int(text[:-1])
    if not 0 <= value <= 255:
        reject(f"oracle.{filename}")
    return value


def verify_check_oracle(root: pathlib.Path, document: dict[str, object]) -> None:
    case = document["case"]
    if not isinstance(case, dict):
        reject("case")
    for field in ("name", "repo", "revision", "subdir"):
        if not isinstance(case.get(field), str):
            reject(f"case.{field}")
    revision = case["revision"]
    if len(revision) != 40 or any(character not in "0123456789abcdef" for character in revision):
        reject("case.revision")
    require_digest(case.get("lock_sha256"), "case.lock_sha256")
    if not isinstance(case.get("args"), list) \
        or not all(isinstance(argument, str) for argument in case["args"]):
        reject("case.args")
    environment = case.get("env")
    if not isinstance(environment, dict) \
        or not all(isinstance(key, str) and isinstance(value, str)
                   for key, value in environment.items()):
        reject("case.env")
    expected_exit = case.get("expected_exit")
    if type(expected_exit) is not int or not 0 <= expected_exit <= 255:
        reject("case.expected_exit")
    timeout_seconds = case.get("timeout_seconds")
    if type(timeout_seconds) is not int or timeout_seconds <= 0:
        reject("case.timeout_seconds")
    if case.get("stdin") != {"kind": "eof"}:
        reject("case.stdin")

    native_exit = parse_exit(root, "native.exit")
    mirvm_exit = parse_exit(root, "mirvm.exit")
    if native_exit != expected_exit:
        reject("oracle.native_exit")
    status = document.get("status")
    if status == "pass":
        if case.get("xfail") is not None:
            reject("case.xfail")
        if mirvm_exit != native_exit:
            reject("oracle.exit")
        if (root / "native.stdout").read_bytes() != (root / "mirvm.stdout").read_bytes():
            reject("oracle.stdout")
        if (root / "native.stderr").read_bytes() != (root / "mirvm.stderr").read_bytes():
            reject("oracle.stderr")
        return
    if status != "xfail":
        reject("status")

    xfail = require_keys(
        case.get("xfail"), {"mirvm_exit", "diagnostic"}, "case.xfail"
    )
    xfail_exit = xfail["mirvm_exit"]
    diagnostic = xfail["diagnostic"]
    if type(xfail_exit) is not int or not 0 <= xfail_exit <= 255:
        reject("case.xfail.mirvm_exit")
    if not isinstance(diagnostic, str) or not diagnostic.startswith("mirvm") \
        or any(character in diagnostic for character in ("\0", "\r", "\n")):
        reject("case.xfail.diagnostic")
    if mirvm_exit != xfail_exit:
        reject("oracle.xfail_exit")
    try:
        mirvm_stderr = (root / "mirvm.stderr").read_text()
    except (OSError, UnicodeError):
        reject("oracle.xfail_diagnostic")
    diagnostic_lines = [line for line in mirvm_stderr.splitlines() if line.startswith("mirvm")]
    if diagnostic_lines != [diagnostic]:
        reject("oracle.xfail_diagnostic")
    if mirvm_exit == native_exit \
        and (root / "native.stdout").read_bytes() == (root / "mirvm.stdout").read_bytes() \
        and (root / "native.stderr").read_bytes() == (root / "mirvm.stderr").read_bytes():
        reject("oracle.xpass")


def verify_summary_matches_check(
    summary: dict[str, object], check: dict[str, object]
) -> None:
    if summary.get("schema_version") != check.get("schema_version"):
        reject("summary.schema_version")
    case = check["case"]
    execution = check["execution"]
    if not isinstance(case, dict) or not isinstance(execution, dict):
        reject("check.metadata")
    field_map = {
        "case": "name",
        "repo": "repo",
        "revision": "revision",
        "lock_sha256": "lock_sha256",
        "subdir": "subdir",
        "args": "args",
        "env": "env",
        "expected_exit": "expected_exit",
        "timeout_seconds": "timeout_seconds",
    }
    for summary_field, case_field in field_map.items():
        if summary.get(summary_field) != case.get(case_field):
            reject(f"summary.{summary_field}")
    if summary.get("host_target") != execution.get("host_target"):
        reject("summary.host_target")
    summary_tools = summary.get("tools")
    check_tools = execution.get("tools")
    if not isinstance(summary_tools, dict) or not isinstance(check_tools, dict):
        reject("summary.tools")
    for name in ("cargo", "rustc", "mirvm"):
        if summary_tools.get(name) != check_tools.get(name):
            reject(f"summary.tools.{name}")
    if summary.get("schema_version") == WORKLOAD_TOOLS_SCHEMA_VERSION:
        if summary.get("workload_tools") != execution.get("workload_tools"):
            reject("summary.workload_tools")
        inputs = execution.get("inputs")
        if not isinstance(inputs, dict) \
            or summary.get("system_path") != inputs.get("system_path"):
            reject("summary.system_path")


def verify_check(
    result_path: pathlib.Path,
    case_id: str,
    check_id: str,
    evidence_id: str,
    status: str,
    enforce_path: bool = True,
) -> dict[str, object]:
    result_path = resolve_result_path(result_path)
    try:
        document = json.loads(result_path.read_text())
    except (OSError, UnicodeError, json.JSONDecodeError):
        reject("result.json")
    if not isinstance(document, dict) \
        or document.get("schema_version") not in {
            LEGACY_SCHEMA_VERSION,
            CURRENT_SCHEMA_VERSION,
            WORKLOAD_TOOLS_SCHEMA_VERSION,
        }:
        reject("schema_version")
    if set(document) != {
        "schema_version",
        "kind",
        "status",
        "identity",
        "case",
        "execution",
        "payload",
    }:
        reject("result")
    if document.get("kind") != "check":
        reject("kind")
    if document.get("status") != status:
        reject("status")
    if status not in {"pass", "xfail"}:
        reject("status")
    require_digest(case_id, "identity.case_id")
    require_digest(check_id, "identity.check_id")
    require_digest(evidence_id, "identity.evidence_id")
    expected_identity = {
        "case_id": case_id,
        "check_id": check_id,
        "evidence_id": evidence_id,
    }
    if document.get("identity") != expected_identity:
        reject("identity")
    derived_case_id = derive_case_id_from_case(document.get("case"))
    if derived_case_id != case_id:
        reject("identity.case_id")
    if derive_check_id(document, derived_case_id) != check_id:
        reject("identity.check_id")
    if enforce_path:
        if result_path.parent.name != evidence_id:
            reject("path.evidence_id")
        if result_path.parent.parent.name != check_id:
            reject("path.check_id")

    verify_directory_entries(result_path.parent, CHECK_PAYLOAD)
    verify_payload(result_path.parent, document.get("payload"), CHECK_PAYLOAD)
    verify_check_oracle(result_path.parent, document)

    hashed_document = copy.deepcopy(document)
    identity = hashed_document.get("identity")
    if not isinstance(identity, dict):
        reject("identity")
    identity.pop("evidence_id", None)
    actual_evidence_id = hashlib.sha256(
        b"mirvm/project-suite/check-evidence/v1\0"
        + canonical_json(hashed_document)
    ).hexdigest()
    if actual_evidence_id != evidence_id:
        reject("identity.evidence_id")
    committed = result_path.parent / "COMMITTED"
    if committed.is_symlink():
        reject("committed.path")
    try:
        marker = committed.read_text()
    except (OSError, UnicodeError):
        reject("committed.path")
    if marker != "committed\n":
        reject("committed")
    return document


def create_bench_result(
    stage: pathlib.Path,
    check_result_path: pathlib.Path,
    case_id: str,
    check_id: str,
    check_evidence_id: str,
    bench_id: str,
) -> str:
    if stage.is_symlink() or not stage.is_dir():
        reject("stage.path")
    summary_path = stage / "summary.json"
    try:
        summary = json.loads(summary_path.read_text())
    except (OSError, UnicodeError, json.JSONDecodeError):
        reject("summary.json")
    expected_summary_identity = {
        "case_id": case_id,
        "check_id": check_id,
        "check_evidence_id": check_evidence_id,
        "bench_id": bench_id,
    }
    if not isinstance(summary, dict) \
        or summary.get("schema_version") not in {
            CURRENT_SCHEMA_VERSION,
            WORKLOAD_TOOLS_SCHEMA_VERSION,
        }:
        reject("summary.schema_version")
    if summary.get("identity") != expected_summary_identity:
        reject("summary.identity")
    if derive_case_id_from_summary(summary) != case_id:
        reject("identity.case_id")
    if derive_bench_id(summary) != bench_id:
        reject("identity.bench_id")
    verify_benchmark_summary(stage, summary)
    check = verify_check(
        check_result_path, case_id, check_id, check_evidence_id, "pass"
    )
    verify_summary_matches_check(summary, check)
    result_path = stage / "result.json"
    if result_path.exists() or result_path.is_symlink():
        reject("result.exists")
    document = {
        "schema_version": summary["schema_version"],
        "kind": "bench",
        "status": "pass",
        "identity": expected_summary_identity.copy(),
        "payload": payload_descriptors(stage, BENCH_PAYLOAD),
    }
    evidence_id = hashlib.sha256(
        b"mirvm/project-suite/bench-evidence/v1\0" + canonical_json(document)
    ).hexdigest()
    document["identity"]["evidence_id"] = evidence_id
    try:
        result_path.write_bytes(canonical_json(document) + b"\n")
    except OSError:
        reject("result.write")
    return evidence_id


def verify_bench(
    result_path: pathlib.Path,
    check_result_path: pathlib.Path,
    case_id: str,
    check_id: str,
    check_evidence_id: str,
    bench_id: str,
    evidence_id: str,
    status: str,
    enforce_path: bool = True,
) -> None:
    result_path = resolve_result_path(result_path)
    try:
        document = json.loads(result_path.read_text())
    except (OSError, UnicodeError, json.JSONDecodeError):
        reject("result.json")
    if not isinstance(document, dict) or set(document) != {
        "schema_version",
        "kind",
        "status",
        "identity",
        "payload",
    }:
        reject("result")
    if document.get("schema_version") not in {
        LEGACY_SCHEMA_VERSION,
        CURRENT_SCHEMA_VERSION,
        WORKLOAD_TOOLS_SCHEMA_VERSION,
    }:
        reject("schema_version")
    if document.get("kind") != "bench":
        reject("kind")
    if document.get("status") != status:
        reject("status")
    if status != "pass":
        reject("status")
    for label, value in (
        ("case_id", case_id),
        ("check_id", check_id),
        ("check_evidence_id", check_evidence_id),
        ("bench_id", bench_id),
        ("evidence_id", evidence_id),
    ):
        require_digest(value, f"identity.{label}")
    expected_identity = {
        "case_id": case_id,
        "check_id": check_id,
        "check_evidence_id": check_evidence_id,
        "bench_id": bench_id,
        "evidence_id": evidence_id,
    }
    if document.get("identity") != expected_identity:
        reject("identity")
    if enforce_path:
        if result_path.parent.name != evidence_id:
            reject("path.evidence_id")
        if result_path.parent.parent.name != bench_id:
            reject("path.bench_id")
    verify_directory_entries(result_path.parent, BENCH_PAYLOAD)
    verify_payload(result_path.parent, document.get("payload"), BENCH_PAYLOAD)
    try:
        summary = json.loads((result_path.parent / "summary.json").read_text())
    except (OSError, UnicodeError, json.JSONDecodeError):
        reject("summary.json")
    if not isinstance(summary, dict) \
        or summary.get("schema_version") != document.get("schema_version"):
        reject("summary.schema_version")
    summary_identity = require_keys(
        summary.get("identity"),
        {"case_id", "check_id", "check_evidence_id", "bench_id"},
        "summary.identity",
    )
    if summary_identity.get("case_id") != case_id \
        or derive_case_id_from_summary(summary) != case_id:
        reject("identity.case_id")
    if summary_identity.get("check_id") != check_id:
        reject("identity.check_id")
    if summary_identity.get("check_evidence_id") != check_evidence_id:
        reject("identity.check_evidence_id")
    if summary_identity.get("bench_id") != bench_id \
        or derive_bench_id(summary) != bench_id:
        reject("identity.bench_id")
    verify_benchmark_summary(result_path.parent, summary)
    check = verify_check(
        check_result_path, case_id, check_id, check_evidence_id, "pass"
    )
    verify_summary_matches_check(summary, check)

    hashed_document = copy.deepcopy(document)
    hashed_identity = hashed_document.get("identity")
    if not isinstance(hashed_identity, dict):
        reject("identity")
    hashed_identity.pop("evidence_id", None)
    actual_evidence_id = hashlib.sha256(
        b"mirvm/project-suite/bench-evidence/v1\0"
        + canonical_json(hashed_document)
    ).hexdigest()
    if actual_evidence_id != evidence_id:
        reject("identity.evidence_id")
    committed = result_path.parent / "COMMITTED"
    if committed.is_symlink():
        reject("committed.path")
    try:
        marker = committed.read_text()
    except (OSError, UnicodeError):
        reject("committed.path")
    if marker != "committed\n":
        reject("committed")


def fsync_regular_file(path: pathlib.Path) -> None:
    flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0)
    try:
        descriptor = os.open(path, flags)
    except OSError:
        reject(f"fsync.file.{path.name}")
    try:
        if not stat.S_ISREG(os.fstat(descriptor).st_mode):
            reject(f"fsync.file.{path.name}")
        os.fsync(descriptor)
    except OSError:
        reject(f"fsync.file.{path.name}")
    finally:
        os.close(descriptor)


def fsync_directory(path: pathlib.Path) -> None:
    if path.is_symlink() or not path.is_dir():
        reject("fsync.directory.path")
    flags = os.O_RDONLY | getattr(os, "O_DIRECTORY", 0)
    try:
        descriptor = os.open(path, flags)
    except OSError:
        reject("fsync.directory.open")
    try:
        os.fsync(descriptor)
    except OSError:
        reject("fsync.directory.fsync")
    finally:
        os.close(descriptor)


def sync_directory_files(path: pathlib.Path) -> None:
    if path.is_symlink() or not path.is_dir():
        reject("seal.path")
    try:
        entries = sorted(path.iterdir())
    except OSError:
        reject("seal.entries")
    if not entries:
        reject("seal.empty")
    for entry in entries:
        if entry.is_symlink() or not entry.is_file():
            reject(f"seal.entry.{entry.name}")
        fsync_regular_file(entry)
    for entry in entries:
        try:
            entry.chmod(0o444)
        except OSError:
            reject(f"seal.chmod.{entry.name}")
        fsync_regular_file(entry)
    fsync_directory(path)


def seal_directory(path: pathlib.Path) -> None:
    sync_directory_files(path)
    try:
        path.chmod(0o555)
    except OSError:
        reject("seal.chmod.directory")
    fsync_directory(path)


def main(argv: list[str]) -> int:
    try:
        if len(argv) == 7 and argv[1] in {"verify-check", "verify-check-stage"}:
            verify_check(
                pathlib.Path(argv[2]),
                argv[3],
                argv[4],
                argv[5],
                argv[6],
                enforce_path=argv[1] == "verify-check",
            )
        elif len(argv) == 8 and argv[1] == "create-bench-result":
            evidence_id = create_bench_result(
                pathlib.Path(argv[2]),
                pathlib.Path(argv[3]),
                argv[4],
                argv[5],
                argv[6],
                argv[7],
            )
            print(evidence_id)
        elif len(argv) == 10 and argv[1] in {"verify-bench", "verify-bench-stage"}:
            verify_bench(
                pathlib.Path(argv[2]),
                pathlib.Path(argv[3]),
                argv[4],
                argv[5],
                argv[6],
                argv[7],
                argv[8],
                argv[9],
                enforce_path=argv[1] == "verify-bench",
            )
        elif len(argv) == 3 and argv[1] == "seal-directory":
            seal_directory(pathlib.Path(argv[2]))
        elif len(argv) == 3 and argv[1] == "sync-directory-files":
            sync_directory_files(pathlib.Path(argv[2]))
        elif len(argv) == 3 and argv[1] == "fsync-directory":
            fsync_directory(pathlib.Path(argv[2]))
        else:
            print(
                "usage: project_suite_evidence.py "
                "verify-check[-stage] RESULT CASE_ID CHECK_ID EVIDENCE_ID STATUS | "
                "create-bench-result STAGE CHECK_RESULT CASE_ID CHECK_ID "
                "CHECK_EVIDENCE_ID BENCH_ID | "
                "verify-bench[-stage] RESULT CHECK_RESULT CASE_ID CHECK_ID CHECK_EVIDENCE_ID "
                "BENCH_ID EVIDENCE_ID STATUS | "
                "sync-directory-files DIRECTORY | seal-directory DIRECTORY | "
                "fsync-directory DIRECTORY",
                file=sys.stderr,
            )
            return 64
    except EvidenceCorrupt as error:
        print(f"evidence_corrupt: {error}", file=sys.stderr)
        return 69
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
