#!/usr/bin/env bash
# prepare: the preparation/execution split. `mirvm run` is quiet on success — a guest build's own
# compiler diagnostics are preparation, not program output — while `mirvm prepare` does the same
# work, reports it, and stops before the guest starts. A build that failed still speaks on both.
# fields: fixture(required)

MODE_FIELDS="fixture"
MODE_REQUIRED="fixture"

mode_run() {
    case_init
    case_fixtures
    apply_env "$(field env "")"

    local clean=${FIXTURES[0]} warning=${FIXTURES[1]}
    local home="$TMP/home"
    mkdir -p "$home"
    ensure_test_sysroot "$MIRVM" "$home" "$RUSTC" || exit $?

    local pass=0 fail=0

    # <description> <exit> <tag> [env...] -- <args...>
    run_mirvm() {
        local desc="$1" want="$2" tag="$3"
        shift 3
        local code=0
        env -u RUST_BACKTRACE MIRVM_HOME="$home" MIRVM_SYSROOT="$TEST_SYSROOT" "$@" \
            >"$TMP/$tag.out" 2>"$TMP/$tag.err" || code=$?
        if [ "$code" = "$want" ]; then
            echo "PASS $desc (exit $code)"; pass=$((pass + 1))
        else
            echo "FAIL $desc: exit $code want $want"
            echo "--- stdout ---"; head -20 "$TMP/$tag.out"
            echo "--- stderr ---"; head -20 "$TMP/$tag.err"
            fail=$((fail + 1))
        fi
    }

    # <description> <pattern> <tag>: the captured stderr contains the pattern
    expect_stderr() {
        local desc="$1" pattern="$2" tag="$3"
        if grep -qF -- "$pattern" "$TMP/$tag.err"; then
            echo "PASS $desc"; pass=$((pass + 1))
        else
            echo "FAIL $desc: stderr lacks [$pattern]"
            head -20 "$TMP/$tag.err"
            fail=$((fail + 1))
        fi
    }

    # <description> <tag>: the captured stderr is empty — the whole point of the quiet run
    expect_silent() {
        local desc="$1" tag="$2"
        if [ ! -s "$TMP/$tag.err" ]; then
            echo "PASS $desc"; pass=$((pass + 1))
        else
            echo "FAIL $desc: stderr is not empty"
            head -20 "$TMP/$tag.err"
            fail=$((fail + 1))
        fi
    }

    expect_output() {
        local desc="$1" want="$2" tag="$3"
        if [ "$(cat "$TMP/$tag.out")" = "$want" ]; then
            echo "PASS $desc"; pass=$((pass + 1))
        else
            echo "FAIL $desc: stdout differs"
            head -20 "$TMP/$tag.out"; fail=$((fail + 1))
        fi
    }

    expect_bound_hit() {
        local desc="$1" tag="$2"
        expect_stderr "$desc reports a cache load" "cache-load=" "$tag"
        if grep -qE 'frontend=|lower=|layer-load=|base-load=' "$TMP/$tag.err"; then
            echo "FAIL $desc entered the preparation pipeline"
            head -20 "$TMP/$tag.err"; fail=$((fail + 1))
        else
            echo "PASS $desc loaded the bound image directly"; pass=$((pass + 1))
        fi
    }

    # ① prepare does the whole build, reports the phases, and does not run the guest.
    run_mirvm "prepare builds and stops" 0 prep "$MIRVM" prepare "$clean"
    expect_stderr "prepare reports the compiler frontend" "frontend=" prep
    if [ ! -s "$TMP/prep.out" ]; then
        echo "PASS prepare ran no guest"; pass=$((pass + 1))
    else
        echo "FAIL prepare ran the guest"; cat "$TMP/prep.out"; fail=$((fail + 1))
    fi

    # ② what prepare built is what the next run loads: no frontend phase, just the cache.
    run_mirvm "run after prepare" 0 warm env MIRVM_TIMING=1 "$MIRVM" run "$clean"
    expect_bound_hit "run after prepare" warm

    # ③ the guest's own compiler warning is preparation detail: held back by default.
    run_mirvm "plain run of a warning guest" 0 quiet "$MIRVM" run "$warning"
    expect_silent "plain run holds build diagnostics back" quiet
    if [ "$(cat "$TMP/quiet.out")" = "prepare-warning-ran" ]; then
        echo "PASS plain run still ran the guest"; pass=$((pass + 1))
    else
        echo "FAIL plain run did not run the guest"; cat "$TMP/quiet.out"; fail=$((fail + 1))
    fi

    # ④ the two spellings that ask for that detail: prepare, and -v.
    run_mirvm "prepare shows the warning" 0 prep-warning "$MIRVM" prepare "$warning"
    expect_stderr "prepare releases the warning" "never_called_by_the_guest" prep-warning
    run_mirvm "-v shows the warning" 0 verbose "$MIRVM" run -v "$warning"
    expect_stderr "-v releases the warning" "never_called_by_the_guest" verbose
    run_mirvm "MIRVM_LOG=debug shows the warning" 0 verbose-env \
        env MIRVM_LOG=debug "$MIRVM" run "$warning"
    expect_stderr "MIRVM_LOG=debug releases the warning" "never_called_by_the_guest" verbose-env

    # ⑤ an explicit threshold outranks the command's own verbosity, and an unknown one is refused
    # rather than read as the default.
    run_mirvm "prepare under MIRVM_LOG=error" 0 prep-quiet \
        env MIRVM_LOG=error "$MIRVM" prepare "$warning"
    expect_silent "MIRVM_LOG=error silences prepare" prep-quiet
    run_mirvm "an unknown severity is a usage error" 2 bad-level \
        env MIRVM_LOG=loud "$MIRVM" run "$clean"
    expect_stderr "the rejection names the vocabulary" \
        "error, warning, note, info or debug" bad-level

    # ⑥ a build that failed is not preparation detail: the user has to see why.
    cat >"$TMP/broken.rs" <<'EOF'
fn main() {
    let x: i32 = "not an integer";
    println!("{x}");
}
EOF
    run_mirvm "a failed build still speaks" 1 broken "$MIRVM" run "$TMP/broken.rs"
    expect_stderr "failed build replays the compiler error" "error[E0308]" broken

    # ⑦ prepare rejects what only execution can honour, loudly, and names itself when it has no input.
    run_mirvm "prepare rejects an execution-only flag" 2 reject \
        "$MIRVM" prepare --vm-call main "$clean"
    expect_stderr "prepare names the flag it rejects" "--vm-call" reject
    run_mirvm "prepare without an input" 2 no-input "$MIRVM" prepare
    expect_stderr "the missing input is prepare's, not run's" '`prepare` needs an input' no-input
    run_mirvm "install without an input" 2 install-no-input "$MIRVM" install
    expect_stderr "the missing input names install" '`install` needs an input' install-no-input
    run_mirvm "install rejects guest arguments" 2 install-args \
        "$MIRVM" install "$clean" -- guest-arg
    expect_stderr "install names the execution boundary" '`install` stops short of' install-args

    # ⑧ the dependency track does not change what prepare means for a single file: a plain .rs never
    # takes the Cargo track, so this must behave exactly like the default-track prepare above. The
    # earlier prepare already warmed the cache, so the ledger reports a load rather than a build.
    run_mirvm "prepare under MIRVM_DEPS=cargo" 0 prep-cargo \
        env MIRVM_DEPS=cargo "$MIRVM" prepare "$clean"
    expect_stderr "prepare under MIRVM_DEPS=cargo reports its phases" "mirvm-timing:" prep-cargo

    # Both dependency tracks must prepare the selected root, including its generic dependency,
    # before reporting success. Cargo must also invoke its runner when the launcher is already fresh.
    local track project tag
    for track in self cargo; do
        project="$TMP/project-$track"
        mkdir -p "$project/src/bin" "$project/dep/src"
        cat >"$project/Cargo.toml" <<'EOF'
[package]
name = "prepared-app"
version = "0.1.0"
edition = "2024"
default-run = "selected"

[dependencies]
prepared-dep = { path = "dep" }
EOF
        cat >"$project/dep/Cargo.toml" <<'EOF'
[package]
name = "prepared-dep"
version = "0.1.0"
edition = "2024"
EOF
        cat >"$project/dep/src/lib.rs" <<'EOF'
pub fn identity<T>(value: T) -> T { value }
pub fn concrete(value: u64) -> u64 { value + 1 }
pub fn subtract(value: i64) -> i64 { value - 1 }
pub fn multiply(value: i32) -> i32 { value * 2 }
pub struct Bundle<T> { pub tag: u8, pub value: T }
pub fn latent<T>(value: T) -> Bundle<T> { Bundle { tag: 13, value } }
EOF
        printf 'fn main() {}\n' >"$project/src/main.rs"
        printf 'fn main() {}\n' >"$project/src/bin/selected.rs"
        tag="library-$track"
        run_mirvm "install a library without a consumer on $track" 0 "$tag" \
            env MIRVM_DEPS="$track" "$MIRVM" install -v "$project/dep"
        expect_output "library installation invoked no guest on $track" "" "$tag"
        expect_stderr "library installation prepares a generic definition on $track" \
            "generic template exported: latent" "$tag"
        expect_stderr "library installation prepares a non-generic definition on $track" \
            "generic template exported: concrete" "$tag"
        if grep -qF 'generic instance specialized:' "$TMP/$tag.err"; then
            echo "FAIL library installation specialized an application instance"
            fail=$((fail + 1))
        else
            echo "PASS library installation requires no application instantiation"
            pass=$((pass + 1))
        fi
        cat >"$project/src/main.rs" <<'EOF'
fn main() { println!("unselected guest ran"); }
EOF
        cat >"$project/src/bin/selected.rs" <<'EOF'
fn main() {
    assert_eq!(prepared_dep::subtract(8), 7);
    assert_eq!(prepared_dep::multiply(-3), -6);
    std::panic::set_hook(Box::new(|_| {}));
    assert!(std::panic::catch_unwind(|| prepared_dep::concrete(std::hint::black_box(u64::MAX))).is_err());
    assert!(std::panic::catch_unwind(|| prepared_dep::subtract(std::hint::black_box(i64::MIN))).is_err());
    assert!(std::panic::catch_unwind(|| prepared_dep::multiply(std::hint::black_box(i32::MAX))).is_err());
    let number = prepared_dep::concrete(prepared_dep::identity(41u64));
    let label = prepared_dep::identity(include_str!("message.txt").trim());
    println!("{label}:{number}");
}
EOF
        printf 'installed\n' >"$project/src/bin/message.txt"
        tag="install-$track"
        run_mirvm "install on $track" 0 "$tag" \
            env MIRVM_DEPS="$track" "$MIRVM" install -v "$project"
        expect_output "install on $track invoked no guest" "" "$tag"
        expect_stderr "install on $track lowered the root" "frontend=" "$tag"
        expect_stderr "install on $track reuses the dependency's prepared template" \
            "generic template reused: prepared_dep::identity" "$tag"
        expect_stderr "install on $track reuses prepared non-generic bytecode" \
            "generic template reused: prepared_dep::concrete" "$tag"
        run_mirvm "first installed run on $track" 0 "$tag-run" \
            env MIRVM_DEPS="$track" MIRVM_TIMING=1 \
            "$MIRVM" run "$project"
        expect_output "installed default target on $track" "installed:42" "$tag-run"
        expect_bound_hit "first installed run on $track" "$tag-run"

        run_mirvm "clear installed images on $track" 0 "$tag-purge" "$MIRVM" cache purge --ir
        run_mirvm "prepare fresh dependency artifacts on $track" 0 "$tag-prepare" \
            env MIRVM_DEPS="$track" "$MIRVM" prepare "$project"
        expect_stderr "prepare on $track still lowered its fresh launcher" "frontend=" "$tag-prepare"
        expect_output "prepare on $track invoked no guest" "" "$tag-prepare"
        run_mirvm "first prepared run on $track" 0 "$tag-prepared-run" \
            env MIRVM_DEPS="$track" MIRVM_TIMING=1 \
            "$MIRVM" run "$project"
        expect_bound_hit "first prepared run on $track" "$tag-prepared-run"

        printf 'edited\n' >"$project/src/bin/message.txt"
        run_mirvm "changed source input on $track" 0 "$tag-edited" \
            env MIRVM_DEPS="$track" MIRVM_TIMING=1 \
            "$MIRVM" run "$project"
        expect_stderr "changed input invalidates $track's image" "frontend=" "$tag-edited"
        expect_output "changed input is observed on $track" "edited:42" "$tag-edited"

        # Changing only the consumer to use a previously uninstantiated definition must reuse
        # the template exported by dependency preparation, with current concrete type bindings.
        cat >"$project/src/bin/selected.rs" <<'EOF'
fn main() {
    let value = prepared_dep::latent([7u64; 9]);
    assert_eq!(value.tag, 13);
    println!("latent:{}", value.value.iter().sum::<u64>());
}
EOF
        run_mirvm "edited consumer reuses an unused dependency template on $track" 0 "$tag-latent" \
            env MIRVM_DEPS="$track" MIRVM_LOG=debug \
            "$MIRVM" run "$project"
        expect_output "new concrete binding is correct on $track" "latent:63" "$tag-latent"
        expect_stderr "unused dependency template is reused on $track" \
            "generic template reused: prepared_dep::latent" "$tag-latent"
        local other="$TMP/second-$track"
        mkdir -p "$other/src"
        cat >"$other/Cargo.toml" <<EOF
[package]
name = "second-consumer"
version = "0.1.0"
edition = "2024"
[dependencies]
prepared-dep = { path = "$project/dep" }
EOF
        cat >"$other/src/main.rs" <<'EOF'
fn main() {
    let value = prepared_dep::latent([3u16; 5]);
    assert_eq!(value.tag, 13);
    println!("second:{}", value.value.iter().sum::<u16>());
}
EOF
        run_mirvm "another consumer reuses installed templates on $track" 0 "$tag-second" \
            env MIRVM_DEPS="$track" MIRVM_LOG=debug "$MIRVM" run "$other"
        expect_output "second consumer binds its own type on $track" "second:15" "$tag-second"
        expect_stderr "second consumer reuses the installed generic on $track" \
            "generic template reused: prepared_dep::latent" "$tag-second"
    done

    local deferred
    for track in self cargo; do
        deferred="$TMP/deferred-$track"
        mkdir -p "$deferred/src"
        cat >"$deferred/Cargo.toml" <<'EOF'
[package]
name = "deferred-library"
version = "0.1.0"
edition = "2024"
EOF
        printf 'pub fn deferred<T>(value: T) -> T { value }\n' >"$deferred/src/lib.rs"
        run_mirvm "installation can skip prelowering on $track" 0 "no-prelower-$track" \
            env MIRVM_DEPS="$track" "$MIRVM" install --no-prelower -v "$deferred"
        if grep -qF 'generic template exported: deferred' "$TMP/no-prelower-$track.err"; then
            echo "FAIL --no-prelower exported templates"; fail=$((fail + 1))
        else
            echo "PASS --no-prelower leaves metadata only"; pass=$((pass + 1))
        fi
        run_mirvm "installation repairs metadata-only dependencies on $track" 0 "finish-prelower-$track" \
            env MIRVM_DEPS="$track" "$MIRVM" install -v "$deferred"
        expect_stderr "fresh metadata does not suppress prelowering on $track" \
            'generic template exported: deferred' "finish-prelower-$track"
        run_mirvm "clear dependency products on $track" 0 "purge-templates-$track" \
            "$MIRVM" cache purge --units
        run_mirvm "installation repairs missing templates on $track" 0 "repair-templates-$track" \
            env MIRVM_DEPS="$track" "$MIRVM" install -v "$deferred"
        expect_stderr "missing templates are exported again on $track" \
            'generic template exported: deferred' "repair-templates-$track"
    done

    local missing="$TMP/missing-dependency"
    mkdir -p "$missing/src"
    cat >"$missing/Cargo.toml" <<'EOF'
[package]
name = "missing-dependency"
version = "0.1.0"
edition = "2024"
[dependencies]
mirvm_missing_install_probe = "=99.0.0"
EOF
    printf 'fn main() { println!("must not run"); }\n' >"$missing/src/main.rs"
    run_mirvm "run refuses undownloaded dependencies even when online" 1 missing \
        env -u MIRVM_OFFLINE "$MIRVM" run "$missing"
    expect_stderr "missing dependency names the installation boundary" \
        'run `mirvm install <input>` first' missing
    expect_output "missing dependency never enters the guest" "" missing

    local unavailable="$TMP/unavailable-templates"
    mkdir -p "$unavailable/src" "$unavailable/dep/src"
    cat >"$unavailable/Cargo.toml" <<'EOF'
[package]
name = "unavailable-templates"
version = "0.1.0"
edition = "2024"
[dependencies]
uncached = { path = "dep" }
EOF
    cat >"$unavailable/dep/Cargo.toml" <<'EOF'
[package]
name = "uncached"
version = "0.1.0"
edition = "2024"
EOF
    printf 'pub fn value<T>(value: T) -> T { value }\n' >"$unavailable/dep/src/lib.rs"
    printf 'fn main() { println!("{}", uncached::value(42u64)); }\n' >"$unavailable/src/main.rs"
    mv "$home/cache/generic-templates" "$TMP/saved-templates"
    touch "$home/cache/generic-templates"
    run_mirvm "install requires dependency bytecode publication" 1 failed-templates \
        "$MIRVM" install "$unavailable"
    expect_stderr "install reports dependency preparation failure" \
        'cannot prepare dependency bytecode' failed-templates
    run_mirvm "run tolerates unavailable optional dependency bytecode storage" 0 optional-templates \
        "$MIRVM" run "$unavailable"
    expect_output "optional template publication preserves execution" 42 optional-templates
    expect_silent "optional template publication preserves quiet runs" optional-templates
    rm "$home/cache/generic-templates"
    mv "$TMP/saved-templates" "$home/cache/generic-templates"

    # A failed optional delta cannot damage the live module or prevent complete image publication.
    mv "$home/cache/ir" "$TMP/saved-ir"
    touch "$home/cache/ir"
    cp "$clean" "$TMP/optional-delta.rs"
    run_mirvm "install survives an optional delta publication failure" 0 optional-delta \
        "$MIRVM" install "$TMP/optional-delta.rs"
    expect_output "optional publication failure ran no guest" "" optional-delta
    run_mirvm "run with an unavailable delta store" 0 optional-run \
        env MIRVM_TIMING=1 "$MIRVM" run "$TMP/optional-delta.rs"
    expect_output "optional publication preserved the program" "prepare-clean-ran" optional-run
    expect_bound_hit "run after optional delta failure" optional-run
    rm "$home/cache/ir"
    mv "$TMP/saved-ir" "$home/cache/ir"

    # A required bound-image publication failure must fail installation without invoking the guest.
    mv "$home/cache/execution" "$TMP/saved-execution"
    touch "$home/cache/execution"
    run_mirvm "install reports required publication failure" 1 failed-install \
        "$MIRVM" install "$clean"
    expect_stderr "install identifies required publication failure" \
        "cannot publish the prepared execution image" failed-install
    expect_output "failed install invoked no guest" "" failed-install
    run_mirvm "metadata-only installation needs no execution store" 0 skip-image \
        "$MIRVM" install --no-prelower "$clean"
    expect_output "metadata-only installation invoked no guest" "" skip-image
    run_mirvm "ordinary run survives publication failure" 0 failed-run "$MIRVM" run "$clean"
    expect_output "ordinary run preserves the guest result" "prepare-clean-ran" failed-run
    rm "$home/cache/execution"
    mv "$TMP/saved-execution" "$home/cache/execution"

    echo "prepare: $pass passed, $fail failed"
    [ "$fail" -eq 0 ]
}
