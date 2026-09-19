#!/usr/bin/env bash
# Cargo uses this as RUSTC so project rustflags and configured wrappers remain
# authoritative. Only harness-owned diagnostic path remaps are appended.
set -u

: "${PROJECT_SUITE_RUSTC:?project-suite rustc path is missing}"
: "${PROJECT_SUITE_ENCODED_RUSTFLAGS_APPEND:?project-suite rustc flags are missing}"

IFS=$'\x1f' read -r -a project_suite_extra_rustflags \
    <<<"$PROJECT_SUITE_ENCODED_RUSTFLAGS_APPEND"
exec "$PROJECT_SUITE_RUSTC" "$@" "${project_suite_extra_rustflags[@]}"
