#!/usr/bin/env bash
# repo-quality: the repository's own source checks -- formatting, lints, the Rust unit tests, and
# the rule that a product input name is spelled out exactly once under src/.
# fields: none

MODE_FIELDS=""
MODE_PRODUCT=no

run_check() { # <name> <command...>
    local name=$1
    shift
    section_start "$name"
    if "$@"; then ok "$name"; else bad "$name"; fi
    section_end
}

# A name mirvm reads may be spelled out once; a second spelling is what turns it into a magic
# literal. For every product input that one spelling is the register in src/options.rs, and each
# test fixture lives in the single test that uses it.
check_option_register() {
    local dupes
    dupes=$(grep -rhoE '"MIRVM_[A-Z0-9_]+"' src --include='*.rs' | sort | uniq -d)
    if [ -n "$dupes" ]; then
        echo "MIRVM_* names spelled out more than once under src/:" >&2
        printf '%s\n' "$dupes" >&2
        return 1
    fi
}

mode_run() {
    case_init --no-product
    run_check "cargo fmt" "${CARGO:-cargo}" fmt --all -- --check
    run_check "cargo clippy" "${CARGO:-cargo}" clippy --locked --all-targets --all-features -- -D warnings
    run_check "cargo test" "${CARGO:-cargo}" test --locked --all-features
    run_check "options register" check_option_register
    print_section_report
}
