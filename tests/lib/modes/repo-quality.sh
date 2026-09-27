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

# Each store family directory is spelled once, in the register in src/store/mod.rs. A
# store-relative path anywhere else means a writer rebuilt what `store::CONST.dir()` hands it.
check_store_families() {
    local dirs bad
    dirs=$(sed -n '/^families! {/,/^}/p' src/store/mod.rs | grep -oE '"[a-z][a-z-]*"' | tr -d '"' | sort -u)
    bad=$(for dir in $dirs; do
        grep -rnE "\"(cache|data|build|run)/$dir\"" src --include='*.rs' \
            | grep -v '^src/store/mod.rs:'
    done)
    if [ -n "$bad" ]; then
        echo "store-relative paths spelled outside src/store/mod.rs:" >&2
        printf '%s\n' "$bad" >&2
        return 1
    fi
}

# A family's directory is derived where the register says it may be. The `owners:` clause above each
# line names the module(s) allowed to name `store::CONST` — the writer, plus a reader with a reason to
# relocate the path — and every other consumer goes through a `Family` it was handed or through the
# module that owns it. A family line with no clause is itself a failure: the ownership claim cannot be
# dropped silently.
check_store_naming() {
    local bad
    bad=$(
        awk '
            /^families! \{/ { inside = 1; next }
            /^\}/ { inside = 0 }
            /^[ \t]*\/\/ owners:/ {
                line = $0
                sub(/^[ \t]*\/\/ owners:[ \t]*/, "", line)
                sub(/[ \t]+—.*$/, "", line)
                owners = line
            }
            inside && /^[ \t]*(Cache|Data|Build|Run)[ \t]+[A-Z_]+[ \t]/ {
                print $2 "\t" owners
                owners = ""
            }
        ' src/store/mod.rs \
        | while IFS=$'\t' read -r family owners; do
            if [ -z "$owners" ]; then
                printf '%s: the register line above it names no owner\n' "$family"
                continue
            fi
            grep -rn "store::$family\b" src --include='*.rs' | grep -v '^src/store/' \
            | while IFS=: read -r file line _; do
                allowed=no
                for owner in $(printf '%s' "$owners" | tr ',' ' '); do
                    prefix="src/$(printf '%s' "$owner" | sed 's|::|/|g')"
                    if [ "$file" = "$prefix.rs" ] || [ "${file#"$prefix"/}" != "$file" ]; then
                        allowed=yes
                    fi
                done
                [ "$allowed" = yes ] \
                    || printf '%s:%s names store::%s (owners: %s)\n' "$file" "$line" "$family" "$owners"
            done
        done
    )
    if [ -n "$bad" ]; then
        echo "store families named outside the module the register gives them to:" >&2
        printf '%s\n' "$bad" >&2
        return 1
    fi
}

# The pinned toolchain's layout has one owner. `src/options.rs` declares the constant and
# `src/sysroot.rs` derives the paths from it; a consumer names `sysroot::toolchain::…` rather than
# joining `bin/rustc` again, which is how one layout came to be spelled in eight modules.
check_toolchain_layout() {
    local bad
    bad=$(grep -rnE 'DEFAULT_SYSROOT|"bin/(rustc|cargo|rustdoc)"' src --include='*.rs' \
        | grep -v '^src/options.rs:' | grep -v '^src/sysroot.rs:' \
        | grep -vE '^src/[^:]*tests?\.rs:|^src/[^:]*/tests/')
    if [ -n "$bad" ]; then
        echo "the pinned toolchain's layout spelled outside src/{options,sysroot}.rs:" >&2
        printf '%s\n' "$bad" >&2
        return 1
    fi
}

# A register row is the whole read surface: it declares how an option is read and generates the one
# accessor, so nothing outside `src/options.rs` names the resolved values or the raw environment.
# The writing half (`export_to_process`, `note_cli`) and the parent-to-child half (`protocol`) are
# that module's own, which is why they may be named from anywhere.
check_option_access() {
    local bad
    bad=$(grep -rnE 'options::(get|raw|flag|nonempty)\(' src --include='*.rs' \
        | grep -v '^src/options.rs:')
    if [ -n "$bad" ]; then
        echo "options reads outside src/options.rs (name the accessor its row generates):" >&2
        printf '%s\n' "$bad" >&2
        return 1
    fi
}

# An option the command line settles after startup is exported into the variable its child processes
# inherit, so this process has to read that option live as well — or say `child_only`, meaning the
# export is for the child alone and nothing here reads it again. A protocol row declares no reader
# (its typed accessors are the `protocol` module's), so it is not part of this.
check_option_readers() {
    local bad
    bad=$(
        grep -rhoE '(export_to_process|export_os_to_process|note_cli)\("[a-z_0-9]+"' src --include='*.rs' \
            | grep -oE '"[a-z_0-9]+"' | tr -d '"' | sort -u \
        | while read -r field; do
            row=$(awk -v field="$field" '
                /^entries! \{/ { inside = 1; next }
                /^\}/ { inside = 0 }
                inside && $2 == field { collecting = 1 }
                collecting { print; if ($0 ~ /;/) exit }
            ' src/options.rs)
            [ -n "$row" ] || continue
            if printf '%s' "$row" | grep -qE 'live\(|child_only'; then
                continue
            fi
            if printf '%s' "$row" | grep -q 'reads('; then
                printf '%s\n' "$field"
            fi
        done
    )
    if [ -n "$bad" ]; then
        echo "options the CLI exports are read as snapshots (make the row live, or mark it child_only):" >&2
        printf '%s\n' "$bad" >&2
        return 1
    fi
}

# anyhow is banned outright: every failure class is a typed thiserror enum, because the loss of a
# failure's identity is exactly what an unstructured error string costs a machine consumer. The
# pattern matches uses, not the word: a lockfile fixture that merely names the crate is fine.
check_no_anyhow() {
    local bad
    bad=$(
        grep -rnE 'anyhow(::|[[:space:]]*=[[:space:]]*")' src build.rs --include='*.rs'
        grep -nE '^anyhow[[:space:]]*=' Cargo.toml
    )
    if [ -n "$bad" ]; then
        echo "anyhow is banned; declare a thiserror variant instead:" >&2
        printf '%s\n' "$bad" >&2
        return 1
    fi
}

# Every failure code is registered once, in the `diag_codes!` block of the enum that owns it, and a
# code is what a structured consumer matches on: two variants sharing one identity would make that
# match ambiguous while still looking registered.
check_error_codes() {
    local codes dupes
    codes=$(grep -rh -A100 'diag_codes! {' src --include='*.rs' \
        | grep -oE '\=> "[a-z0-9_.]+"' | sed 's/=> "//; s/"//' | sort)
    dupes=$(printf '%s\n' "$codes" | uniq -d)
    if [ -n "$dupes" ]; then
        echo "error codes registered more than once under src/:" >&2
        printf '%s\n' "$dupes" >&2
        return 1
    fi
}

# src/diag is the vocabulary every tree emits through, and the TSan harness compiles it
# source-for-source next to the engine: it must not acquire a dependency. `diag_codes!` is the one
# exception and it needs none in this module — the macro expands to `serde_json` in the caller, and
# the harness build is what proves a fully-qualified use could not have slipped in.
check_diag_purity() {
    local bad
    bad=$(grep -rnE '^[[:space:]]*(pub(\([a-z]+\))? )?use (serde|serde_json|thiserror|toml|semver|postcard|libc|blake3|memmap2|libffi|pubgrub|ureq|flate2|tar|log)[^a-zA-Z_]' \
        src/diag --include='*.rs')
    if [ -n "$bad" ]; then
        echo "src/diag must stay std-only (the TSan harness shares it source-for-source):" >&2
        printf '%s\n' "$bad" >&2
        return 1
    fi
}

# The platform layers are the only place a platform item may be named. `src/arch` owns the CPU,
# `src/os` owns the kernel, and `src/os_arch` owns the two together; anywhere else a raw name is
# either a constant whose value belongs to the kernel, a function whose protocol one of those layers
# already wraps, or an `asm!` site that a second architecture would have to find by search.
#
# Three limits, stated rather than implied. (1) The scan reads a file's product half: lines from the
# top, stopping where an inline `#[cfg(test)]` module begins — a `#[cfg(test)] mod tests;`
# declaration only points at a sibling file, so the product half continues past it — and skipping
# files under a `tests` directory or named `tests.rs`/`test_driver.rs`, because a test that drives
# the raw ABI is evidence rather than debt. (2) A `#[cfg(not(...))]` branch is not matched: it
# guards code for a host this crate refuses to build on at all, so it cannot carry a live second
# spelling. (3) `std::os::fd` is not a platform path — std exposes it on every host — so only
# `std::os::unix`/`windows` are listed. Every platform name counts, including a type alias such as
# `libc::c_int` and a mention inside a doc comment: both are second spellings of what the axis
# modules exist to spell once.
check_platform_boundary() {
    local bad
    bad=$(
        for file in $(grep -rlE 'libc::|[[:space:]]asm!\(|global_asm!\(|std::os::(unix|windows)|#\[cfg\([^)]*(target_arch|target_os|unix|windows)|target_arch[[:space:]]*=|target_os[[:space:]]*=|:[0-9]+:[[:space:]]*(unix|windows)[[:space:],)]*$' \
            src --include='*.rs' \
            | grep -v '^src/arch/' | grep -v '^src/os/' | grep -v '^src/os_arch/' \
            | grep -vE '(^|/)tests?\.rs$|/tests/|/embed_tests/|/test_driver\.rs$'); do
            awk '/^#\[cfg\((all\()?test/ { getline following; if (following ~ /^[[:space:]]*(pub )?mod [A-Za-z_0-9]+[[:space:]]*\{/) exit; print FILENAME ":" FNR ":" following; next } { print FILENAME ":" FNR ":" $0 }' "$file"
        done \
        | grep -E 'libc::|[[:space:]]asm!\(|global_asm!\(|std::os::(unix|windows)|#\[cfg\([^)]*(target_arch|target_os|unix|windows)|target_arch[[:space:]]*=|target_os[[:space:]]*=|:[0-9]+:[[:space:]]*(unix|windows)[[:space:],)]*$'
    )
    if [ -n "$bad" ]; then
        echo "platform items named outside src/{arch,os,os_arch}:" >&2
        printf '%s\n' "$bad" >&2
        return 1
    fi
}

mode_run() {
    case_init --no-product
    run_check "cargo fmt" "${CARGO:-cargo}" fmt --all -- --check
    run_check "cargo clippy" "${CARGO:-cargo}" clippy --locked --all-targets --all-features -- -D warnings
    run_check "cargo test" "${CARGO:-cargo}" test --locked --all-features
    run_check "options register" check_option_register
    run_check "option access" check_option_access
    run_check "option readers" check_option_readers
    run_check "toolchain layout" check_toolchain_layout
    run_check "store families" check_store_families
    run_check "store naming" check_store_naming
    run_check "no anyhow" check_no_anyhow
    run_check "error codes" check_error_codes
    run_check "diag purity" check_diag_purity
    run_check "platform boundary" check_platform_boundary
    print_section_report
    # A section that failed must fail the case: the report is the record, not the verdict, and a mode
    # that ends with a printing helper otherwise reports PASS for a red check.
    [ "$fail" -eq 0 ] || return 1
}
