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
    wrapper_root)
        ordinary=${RUSTC_WRAPPER:-}
        workspace=${RUSTC_WORKSPACE_WRAPPER:-}
        if [ -z "$ordinary" ] && [ -f .cargo/config.toml ]; then
            ordinary=$(sed -n 's/^rustc-wrapper = "\(.*\)"/\1/p' .cargo/config.toml)
            workspace=$(sed -n 's/^rustc-workspace-wrapper = "\(.*\)"/\1/p' .cargo/config.toml)
        fi
        ordinary_name=$(basename "$ordinary")
        workspace_name=$(basename "$workspace")
        printf '%s|%s|%s|-vV\n' "$ordinary_name" "$workspace" "$RUSTC" \
            >>"$MIRVM_WRAPPER_PROBE_LOG.$ordinary_name"
        printf '%s|%s|--crate-name|wrapper_dep|\n' "$ordinary_name" "$RUSTC" \
            >>"$MIRVM_WRAPPER_PROBE_LOG.$ordinary_name"
        printf '%s|%s|--crate-name|wrapper_root|\n' "$workspace_name" "$RUSTC" \
            >>"$MIRVM_WRAPPER_PROBE_LOG.$workspace_name"
        echo 'wrapper=42'
        ;;
    *)
        echo "unexpected fake cargo package: $name" >&2
        exit 99
        ;;
esac
