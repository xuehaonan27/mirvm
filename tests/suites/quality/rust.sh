#!/usr/bin/env bash
# Rust 源码质量：格式、静态检查和单元测试。
# product: no
set -u
. "$(dirname "${BASH_SOURCE[0]}")/../../support/harness.sh"
test_enter_repo

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

run_check "cargo fmt" "${CARGO:-cargo}" fmt --all -- --check
run_check "cargo clippy" "${CARGO:-cargo}" clippy --locked --all-targets --all-features -- -D warnings
run_check "cargo test" "${CARGO:-cargo}" test --locked --all-features

print_section_report
suite_summary quality.rust
