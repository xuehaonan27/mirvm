#!/bin/bash
set -u

manifest=Cargo.toml
args=("$@")
for ((i = 0; i < ${#args[@]}; i++)); do
    case "${args[$i]}" in
        --manifest-path)
            manifest=${args[$((i + 1))]:-Cargo.toml}
            ;;
        --manifest-path=*)
            manifest=${args[$i]#--manifest-path=}
            ;;
    esac
done

name=$(sed -n 's/^name = "\([^"]*\)"/\1/p' "$manifest" | head -1)
case "$name" in
    ecosystem)
        if [ "${SCENARIO:-xfail}" = false_positive ]; then
            exit 101
        fi
        echo 'ecosystem-ok'
        ;;
    ffi_zlib)
        echo 'ffi-ok'
        ;;
    real_ripgrep_regex)
        echo 'regex-ok'
        ;;
    cargo_warning_return)
        echo 'warning-return-ok'
        ;;
    diffproj)
        echo 'project-ok'
        exit 7
        ;;
    fingerprint)
        case "${CARGO_ENCODED_RUSTFLAGS:-}" in
            *diff_flag_one*) echo one ;;
            *diff_flag_two*) echo two ;;
            *)
                echo 'fake cargo fingerprint flags missing' >&2
                exit 99
                ;;
        esac
        ;;
    *)
        echo "unexpected fake cargo package: $name" >&2
        exit 99
        ;;
esac
