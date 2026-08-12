#!/bin/sh
# Record Cargo's wrapper chain, then behave as a transparent rustc wrapper.
set -eu

: "${MIRVM_WRAPPER_PROBE_LOG:?wrapper probe log prefix is missing}"
label=$(basename "$0")
{
    printf '%s' "$label"
    for arg in "$@"; do
        printf '|%s' "$arg"
    done
    printf '\n'
} >> "${MIRVM_WRAPPER_PROBE_LOG}.${label}"
exec "$@"
