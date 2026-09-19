#!/usr/bin/env bash
# cargo-contract: Cargo project rustflags/cwd/fingerprint/wrapper contract.
# fields: none
MODE_FIELDS=""
MODE_REQUIRED=""

mode_run() {
    case_init
    apply_env "$(field env "")"
RUSTC_APPEND_PROXY=${RUSTC_APPEND_PROXY:-$LIB_DIR/helpers/rustc_proxy.sh}
RUSTC_WRAPPER_PROBE=${RUSTC_WRAPPER_PROBE:-$LIB_DIR/helpers/rustc_wrapper_probe.sh}
SCRIPT_CACHE=${SCRIPT_CACHE:-${MIRVM_HOME:-$HOME/.mirvm}/scripts}
# This suite is the dedicated cargo-mode differential track: it always takes the cargo
# three-phase compat path. Even when an outer layer (e.g. a gate DEPS=self full run)
# sets MIRVM_DEPS=self, it must not silently switch to the self path and leave the cargo leg with zero coverage.
export MIRVM_DEPS=cargo
show_diff() {
    local native_out="$1" mirvm_out="$2"
    diff "$native_out" "$mirvm_out" | head -10
}

check_green() {
    local name="$1" native_out="$2" mirvm_out="$3" native_code="$4" mirvm_code="$5"
    local expected_native_code="$6" native_err="${7:-}" mirvm_err="${8:-}"
    if [ "$native_code" != "$expected_native_code" ]; then
        echo "FAIL $name (native baseline exit=$native_code, want=$expected_native_code)"
        [ -z "$native_err" ] || tail -20 "$native_err"
        fail=$((fail+1))
    elif diff -q "$native_out" "$mirvm_out" >/dev/null \
        && diff -q "$native_err" "$mirvm_err" >/dev/null \
        && [ "$native_code" = "$mirvm_code" ]; then
        echo "PASS $name"; pass=$((pass+1))
    else
        echo "FAIL $name (native=$native_code mirvm=$mirvm_code)"
        show_diff "$native_out" "$mirvm_out"
        if ! diff -q "$native_err" "$mirvm_err" >/dev/null; then
            echo "stderr mismatch:"
            show_diff "$native_err" "$mirvm_err"
        fi
        fail=$((fail+1))
    fi
}

# L2 warm rerun consistency: the second run should hit the IR cache, and its stdout/stderr/
# exit code must be byte-identical to the first run (which is then judged against native),
# guarding against a "cache replays stale semantics / damaged snapshot" false green.
# Warning-producing projects stay out of the cache, so their second run replays cold and
# must still match. This dimension gives the runner cache path the gate coverage it lacked.
# Locate the project directory a frontmatter script materialized (match the crate name in Cargo.toml)
# Frontmatter script: the project directory mirvm materialized is handed directly to native cargo
# 2) cargo project mode
PROJ="$TMP/proj"; mkdir -p "$PROJ/.cargo" "$PROJ/src"
cat > "$PROJ/Cargo.toml" <<'EOF'
[package]
name = "diffproj"
version = "0.1.0"
edition = "2024"
[dependencies]
serde_json = "1"
EOF
cat > "$PROJ/.cargo/config.toml" <<'EOF'
[build]
rustflags = [
    "--cfg=diff_cargo_project_config",
    "--check-cfg=cfg(diff_cargo_project_config)",
]
EOF
cat > "$PROJ/src/main.rs" <<'EOF'
#[cfg(not(diff_cargo_project_config))]
compile_error!("project Cargo rustflags were replaced");
#[cfg(not(diff_cargo_harness_append))]
compile_error!("harness-appended rustflags were lost");

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let caller_marker = std::fs::read_to_string("caller-marker.txt")
        .expect("cargo run should preserve its caller's working directory");
    let runtime_value = std::env::var("DIFF_RUNTIME_ENV").unwrap();
    let cargo_pkg_at_runtime = std::env::var_os("CARGO_PKG_NAME").is_some();
    let encoded_flags_at_runtime = std::env::var_os("CARGO_ENCODED_RUSTFLAGS").is_some();
    let v: serde_json::Value = serde_json::json!({"args": args.len()});
    println!(
        "{v} extra={:?} marker={} runtime={} cargo_pkg_at_runtime={cargo_pkg_at_runtime} encoded_flags_at_runtime={encoded_flags_at_runtime}",
        &args[1..], caller_marker.trim(), runtime_value,
    );
    std::process::exit(7);
}
EOF
CALLER="$TMP/caller"; mkdir -p "$CALLER"
printf 'outside-project\n' > "$CALLER/caller-marker.txt"
printf -v project_mirvm_flags '%s\x1f%s' \
    '--cfg=diff_cargo_harness_append' \
    '--check-cfg=cfg(diff_cargo_harness_append)'
printf -v project_mirvm_flags '%s\x1f%s\x1f%s' \
    "$project_mirvm_flags" \
    "--remap-path-prefix=$PROJ=/mirvm-diff-project" \
    '--remap-path-scope=diagnostics'
(cd "$CALLER" && DIFF_RUNTIME_ENV=first \
    MIRVM_ENCODED_RUSTFLAGS_APPEND="$project_mirvm_flags" \
    "$MIRVM" run "$PROJ" -- x y \
    >"$TMP/proj.mirvm" 2>"$TMP/proj.mirvm.err"); mc=$?
(cd "$CALLER" && DIFF_RUNTIME_ENV=second \
    MIRVM_ENCODED_RUSTFLAGS_APPEND="$project_mirvm_flags" \
    "$MIRVM" run "$PROJ" -- x y \
    >"$TMP/proj.mirvm2" 2>"$TMP/proj.mirvm2.err"); mc2=$?
(cd "$CALLER" && DIFF_RUNTIME_ENV=first PROJECT_SUITE_RUSTC="$RUSTC" \
    PROJECT_SUITE_ENCODED_RUSTFLAGS_APPEND="$project_mirvm_flags" \
    RUSTC="$RUSTC_APPEND_PROXY" "$CARGO" run -q \
    --manifest-path "$PROJ/Cargo.toml" --config "$PROJ/.cargo/config.toml" -- x y \
    >"$TMP/proj.native" 2>"$TMP/proj.native.err"); nc=$?
(cd "$CALLER" && DIFF_RUNTIME_ENV=second PROJECT_SUITE_RUSTC="$RUSTC" \
    PROJECT_SUITE_ENCODED_RUSTFLAGS_APPEND="$project_mirvm_flags" \
    RUSTC="$RUSTC_APPEND_PROXY" "$CARGO" run -q \
    --manifest-path "$PROJ/Cargo.toml" --config "$PROJ/.cargo/config.toml" -- x y \
    >"$TMP/proj.native2" 2>"$TMP/proj.native2.err"); nc2=$?
check_green project-cold "$TMP/proj.native" "$TMP/proj.mirvm" "$nc" "$mc" 7 \
    "$TMP/proj.native.err" "$TMP/proj.mirvm.err"
check_green project-warm-runtime-env "$TMP/proj.native2" "$TMP/proj.mirvm2" \
    "$nc2" "$mc2" 7 "$TMP/proj.native2.err" "$TMP/proj.mirvm2.err"

# Cargo fingerprint contract: a change to the rustflags Cargo can see must force a rebuild,
# and flags the compat track appends inside the wrapper must enter Cargo's freshness check equivalently.
FP_PROJ="$TMP/fingerprint"; mkdir -p "$FP_PROJ/src"
cat > "$FP_PROJ/Cargo.toml" <<'EOF'
[package]
name = "fingerprint"
version = "0.1.0"
edition = "2024"
EOF
cat > "$FP_PROJ/src/main.rs" <<'EOF'
#[cfg(all(diff_flag_one, diff_flag_two))]
compile_error!("rustflags from two builds were combined");
#[cfg(not(any(diff_flag_one, diff_flag_two)))]
compile_error!("rustflags were not applied");

fn main() {
    #[cfg(diff_flag_one)]
    println!("one");
    #[cfg(diff_flag_two)]
    println!("two");
}
EOF
printf -v fp_one '%s\x1f%s' '--cfg=diff_flag_one' \
    '--check-cfg=cfg(diff_flag_one,diff_flag_two)'
printf -v fp_two '%s\x1f%s' '--cfg=diff_flag_two' \
    '--check-cfg=cfg(diff_flag_one,diff_flag_two)'
(cd "$FP_PROJ" && MIRVM_ENCODED_RUSTFLAGS_APPEND="$fp_one" \
    "$MIRVM" run "$FP_PROJ" >"$TMP/fp-one.mirvm" 2>"$TMP/fp-one.mirvm.err"); fpm1=$?
(cd "$FP_PROJ" && MIRVM_ENCODED_RUSTFLAGS_APPEND="$fp_two" \
    "$MIRVM" run "$FP_PROJ" >"$TMP/fp-two.mirvm" 2>"$TMP/fp-two.mirvm.err"); fpm2=$?
(cd "$FP_PROJ" && CARGO_ENCODED_RUSTFLAGS="$fp_one" RUSTC="$RUSTC" \
    "$CARGO" run -q >"$TMP/fp-one.native" 2>"$TMP/fp-one.native.err"); fpn1=$?
(cd "$FP_PROJ" && CARGO_ENCODED_RUSTFLAGS="$fp_two" RUSTC="$RUSTC" \
    "$CARGO" run -q >"$TMP/fp-two.native" 2>"$TMP/fp-two.native.err"); fpn2=$?
check_green rustflags-fingerprint-cold "$TMP/fp-one.native" "$TMP/fp-one.mirvm" \
    "$fpn1" "$fpm1" 0 "$TMP/fp-one.native.err" "$TMP/fp-one.mirvm.err"
check_green rustflags-fingerprint-change "$TMP/fp-two.native" "$TMP/fp-two.mirvm" \
    "$fpn2" "$fpm2" 0 "$TMP/fp-two.native.err" "$TMP/fp-two.mirvm.err"

# Cargo wrapper contract: pinned Cargo puts the ordinary wrapper outside and the workspace
# wrapper inside (members only). MIRVM must occupy the innermost compiler position and must
# not reject, swallow, or reorder the wrapper chain Cargo derives from env/config.
WRAP_DEP="$TMP/wrapper-dep"; mkdir -p "$WRAP_DEP/src"
cat > "$WRAP_DEP/Cargo.toml" <<'EOF'
[package]
name = "wrapper_dep"
version = "0.1.0"
edition = "2024"
EOF
cat > "$WRAP_DEP/src/lib.rs" <<'EOF'
pub fn value() -> u32 { 42 }
EOF
WRAP_PROJ="$TMP/wrapper-project"; mkdir -p "$WRAP_PROJ/src" "$WRAP_PROJ/.cargo"
cat > "$WRAP_PROJ/Cargo.toml" <<'EOF'
[package]
name = "wrapper_root"
version = "0.1.0"
edition = "2024"

[dependencies]
wrapper_dep = { path = "../wrapper-dep" }
EOF
cat > "$WRAP_PROJ/src/main.rs" <<'EOF'
fn main() { println!("wrapper={}", wrapper_dep::value()); }
EOF
WRAP_TOOLS="$TMP/wrapper-tools"; mkdir -p "$WRAP_TOOLS"
ln -s "$RUSTC_WRAPPER_PROBE" "$WRAP_TOOLS/ordinary-wrapper"
ln -s "$RUSTC_WRAPPER_PROBE" "$WRAP_TOOLS/workspace-wrapper"

check_wrapper_chain() {
    local name="$1" native_log="$2" mirvm_log="$3"
    local ordinary=ordinary-wrapper workspace=workspace-wrapper
    local bad=0
    for log in "$native_log.$ordinary" "$native_log.$workspace" \
        "$mirvm_log.$ordinary" "$mirvm_log.$workspace"; do
        if [ ! -s "$log" ]; then
            echo "FAIL $name (wrapper did not run: $log)"; bad=1
        fi
    done
    if [ "$bad" = 0 ] \
        && grep -Fq "$ordinary|$WRAP_TOOLS/$workspace|$RUSTC|" "$native_log.$ordinary" \
        && grep -Fq "$ordinary|$RUSTC|--crate-name|wrapper_dep|" "$native_log.$ordinary" \
        && grep -Fq "$workspace|$RUSTC|--crate-name|wrapper_root|" "$native_log.$workspace" \
        && ! grep -Fq '|--crate-name|wrapper_dep|' "$native_log.$workspace" \
        && grep -Fq "$ordinary|$WRAP_TOOLS/$workspace|$MIRVM|" "$mirvm_log.$ordinary" \
        && grep -Fq "$ordinary|$MIRVM|--crate-name|wrapper_dep|" "$mirvm_log.$ordinary" \
        && grep -Fq "$workspace|$MIRVM|--crate-name|wrapper_root|" "$mirvm_log.$workspace" \
        && ! grep -Fq '|--crate-name|wrapper_dep|' "$mirvm_log.$workspace"; then
        echo "PASS $name"; pass=$((pass+1))
    else
        if [ "$bad" = 0 ]; then
            echo "FAIL $name (wrapper order or scope differs from pinned Cargo)"
            tail -20 "$native_log.$ordinary" "$native_log.$workspace" \
                "$mirvm_log.$ordinary" "$mirvm_log.$workspace"
        fi
        fail=$((fail+1))
    fi
}

NATIVE_ENV_LOG="$TMP/wrapper-native-env"
MIRVM_ENV_LOG="$TMP/wrapper-mirvm-env"
(cd "$WRAP_PROJ" && MIRVM_WRAPPER_PROBE_LOG="$NATIVE_ENV_LOG" RUSTC="$RUSTC" \
    RUSTC_WRAPPER="$WRAP_TOOLS/ordinary-wrapper" \
    RUSTC_WORKSPACE_WRAPPER="$WRAP_TOOLS/workspace-wrapper" \
    CARGO_TARGET_DIR="$TMP/wrapper-native-env-target" "$CARGO" run -q \
    >"$TMP/wrapper-native-env.out" 2>"$TMP/wrapper-native-env.err"); wne=$?
(cd "$WRAP_PROJ" && MIRVM_WRAPPER_PROBE_LOG="$MIRVM_ENV_LOG" \
    RUSTC_WRAPPER="$WRAP_TOOLS/ordinary-wrapper" \
    RUSTC_WORKSPACE_WRAPPER="$WRAP_TOOLS/workspace-wrapper" \
    MIRVM_TARGET_DIR="$TMP/wrapper-mirvm-env-target" "$MIRVM" run . \
    >"$TMP/wrapper-mirvm-env.out" 2>"$TMP/wrapper-mirvm-env.err"); wme=$?
check_green cargo-wrapper-env-output "$TMP/wrapper-native-env.out" \
    "$TMP/wrapper-mirvm-env.out" "$wne" "$wme" 0 \
    "$TMP/wrapper-native-env.err" "$TMP/wrapper-mirvm-env.err"
check_wrapper_chain cargo-wrapper-env-chain "$NATIVE_ENV_LOG" "$MIRVM_ENV_LOG"

cat > "$WRAP_PROJ/.cargo/config.toml" <<EOF
[build]
rustc-wrapper = "$WRAP_TOOLS/ordinary-wrapper"
rustc-workspace-wrapper = "$WRAP_TOOLS/workspace-wrapper"
EOF
NATIVE_CONFIG_LOG="$TMP/wrapper-native-config"
MIRVM_CONFIG_LOG="$TMP/wrapper-mirvm-config"
(cd "$WRAP_PROJ" && MIRVM_WRAPPER_PROBE_LOG="$NATIVE_CONFIG_LOG" RUSTC="$RUSTC" \
    CARGO_TARGET_DIR="$TMP/wrapper-native-config-target" "$CARGO" run -q \
    >"$TMP/wrapper-native-config.out" 2>"$TMP/wrapper-native-config.err"); wnc=$?
(cd "$WRAP_PROJ" && MIRVM_WRAPPER_PROBE_LOG="$MIRVM_CONFIG_LOG" \
    MIRVM_TARGET_DIR="$TMP/wrapper-mirvm-config-target" "$MIRVM" run . \
    >"$TMP/wrapper-mirvm-config.out" 2>"$TMP/wrapper-mirvm-config.err"); wmc=$?
check_green cargo-wrapper-config-output "$TMP/wrapper-native-config.out" \
    "$TMP/wrapper-mirvm-config.out" "$wnc" "$wmc" 0 \
    "$TMP/wrapper-native-config.err" "$TMP/wrapper-mirvm-config.err"
check_wrapper_chain cargo-wrapper-config-chain "$NATIVE_CONFIG_LOG" "$MIRVM_CONFIG_LOG"
}
