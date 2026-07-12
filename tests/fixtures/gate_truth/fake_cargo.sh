#!/bin/bash
set -u

name=$(sed -n 's/^name = "\([^"]*\)"/\1/p' Cargo.toml | head -1)
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
    diffproj)
        echo 'project-ok'
        exit 7
        ;;
    *)
        echo "unexpected fake cargo package: $name" >&2
        exit 99
        ;;
esac
