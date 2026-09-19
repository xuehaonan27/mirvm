#!/usr/bin/env bash
# Rust source quality: formatting, static checks, and unit tests.
# product: no
set -u
. "$(dirname "${BASH_SOURCE[0]}")/../../support/harness.sh"
suite_init --no-product

run_check() {
    local name=$1
    shift
    section_start "$name"
    if "$@"; then
        ok "$name"
    else
        bad "$name"
    fi
    section_end
}

# Every external input mirvm defines is registered in src/options.rs. Product code must read an
# option through its accessor rather than naming the variable itself, or the register stops being a
# source of truth. Test modules are out of scope: their child-process fixtures select a branch in
# the test binary and are not product inputs.
check_option_register() {
    local offenders
    offenders=$(
        while IFS= read -r file; do
            grep -HnE 'env::(var|var_os|set_var|remove_var)\(|\.env(_remove)?\(' "$file" \
                | grep -E '"MIRVM_[A-Z0-9_]+"' \
                | sed "s|^|${file}:|"
        done < <(find src -name '*.rs' \
            ! -name 'options.rs' \
            ! -name 'tests.rs' \
            ! -path '*/tests/*' \
            ! -path '*/embed_tests/*')
    )
    if [ -n "$offenders" ]; then
        echo "unregistered MIRVM_* access outside src/options.rs:" >&2
        printf '%s\n' "$offenders" >&2
        return 1
    fi
}

run_check "cargo fmt" "${CARGO:-cargo}" fmt --all -- --check
run_check "cargo clippy" "${CARGO:-cargo}" clippy --locked --all-targets --all-features -- -D warnings
run_check "cargo test" "${CARGO:-cargo}" test --locked --all-features
run_check "options register" check_option_register

print_section_report
suite_summary quality.rust
