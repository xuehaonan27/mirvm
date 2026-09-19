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

# An MIRVM_* name may be spelled out only in src/options.rs. Everywhere else it is a register field
# name passed to an accessor, so a rename cannot leave a stale copy behind and no call site can
# introduce a variable the register does not know about.
check_option_register() {
    local offenders
    offenders=$(grep -rnE '"MIRVM_[A-Z0-9_]+"' src --include='*.rs' --exclude='options.rs')
    if [ -n "$offenders" ]; then
        echo "MIRVM_* names must be declared in src/options.rs, not spelled out here:" >&2
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
