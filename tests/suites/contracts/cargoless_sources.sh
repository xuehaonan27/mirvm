#!/usr/bin/env bash
# Cargo 来源合同：固定 Cargo 裁判配置、替代 registry、源替换、patch 与 replace。
set -u
. "$(dirname "${BASH_SOURCE[0]}")/../../support/harness.sh"
test_enter_repo

# 本套件完全使用本机临时 registry；Cargo 只充当行为权威，不访问公网。
MIRVM=${MIRVM:-$(pwd)/target/debug/mirvm}
CARGO=${CARGO:-$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo}
RUSTC=${RUSTC:-$(dirname "$CARGO")/rustc}
STRACE=${STRACE:-$(command -v strace)}
PYTHON=${PYTHON:-$(command -v python3)}
CONTRACT_HOME=${MIRVM_CONTRACT_HOME:-${MIRVM_HOME:-$HOME/.mirvm}}

[ -x "$MIRVM" ] || { echo "cargoless_sources_contract: $MIRVM 不存在" >&2; exit 69; }
[ -x "$CARGO" ] || { echo "cargoless_sources_contract: pinned Cargo 不存在" >&2; exit 69; }
[ -x "$STRACE" ] || { echo "cargoless_sources_contract: strace 不存在" >&2; exit 69; }
[ -x "$PYTHON" ] || { echo "cargoless_sources_contract: python3 不存在" >&2; exit 69; }
case "$($CARGO --version)" in
    "cargo 1.98.0-nightly "*) ;;
    *) echo "ERROR cargoless_sources: Cargo 版本不在合同内" >&2; exit 69 ;;
esac
ensure_test_sysroot "$MIRVM" "$CONTRACT_HOME" "$RUSTC" || exit $?
CONTRACT_SYSROOT=$TEST_SYSROOT

TMP=$(mktemp -d)
SERVER_PID=""
cleanup() {
    if [ -n "$SERVER_PID" ]; then
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
    rm -rf "$TMP"
}
trap cleanup EXIT

REGISTRY="$TMP/registry"
PACKAGE_HOME="$TMP/package-home"
CARGO_HOME_TEST="$TMP/cargo-home"
SELF_HOME="$TMP/self-home"
NO_CARGO="$TMP/no-cargo"
mkdir -p "$REGISTRY/fi/xt" "$REGISTRY/crates/fixture-dep" \
    "$REGISTRY/crates/fixture-parent" "$PACKAGE_HOME" "$CARGO_HOME_TEST" "$NO_CARGO"
printf '#!/bin/sh\n: >"$MIRVM_CARGO_SENTINEL"\nexit 97\n' >"$NO_CARGO/cargo"
chmod +x "$NO_CARGO/cargo"
export MIRVM_CARGO_SENTINEL="$TMP/cargo-was-invoked"

make_library() { # <dir> <name> <version> <manifest tail> <body>
    local dir=$1 name=$2 version=$3 tail=$4 body=$5
    mkdir -p "$dir/src"
    printf '[package]\nname = "%s"\nversion = "%s"\nedition = "2021"\n%s\n' \
        "$name" "$version" "$tail" >"$dir/Cargo.toml"
    printf '%s\n' "$body" >"$dir/src/lib.rs"
}

make_app() { # <dir> <name> <manifest tail> <body>
    local dir=$1 name=$2 tail=$3 body=$4
    mkdir -p "$dir/src"
    printf '[package]\nname = "%s"\nversion = "0.1.0"\nedition = "2021"\n%s\n' \
        "$name" "$tail" >"$dir/Cargo.toml"
    printf '%s\n' "$body" >"$dir/src/main.rs"
}

package_crate() { # <source dir> <name> <version>
    local source=$1 name=$2 version=$3 archive
    CARGO_HOME="$PACKAGE_HOME" CARGO_TARGET_DIR="$TMP/package-target" \
        "$CARGO" package --manifest-path "$source/Cargo.toml" \
        --allow-dirty --no-verify >"$TMP/package-$name.out" 2>"$TMP/package-$name.err" || return 1
    archive="$source/target/package/$name-$version.crate"
    [ -f "$archive" ] || archive="$TMP/package-target/package/$name-$version.crate"
    [ -f "$archive" ] || return 1
    cp "$archive" "$REGISTRY/crates/$name/$version.crate"
    sha256sum "$archive" | awk '{print $1}'
}

DEP_SRC="$TMP/fixture-dep"
PARENT_SRC="$TMP/fixture-parent"
make_library "$DEP_SRC" fixture-dep 1.2.3 "" \
    'pub fn value() -> usize { 123 }'
make_library "$PARENT_SRC" fixture-parent 1.0.0 \
    $'\n[dependencies]\nfixture-dep = { version = "^1", path = "../fixture-dep" }' \
    'pub fn value() -> usize { fixture_dep::value() + 1 }'
DEP_SUM=$(package_crate "$DEP_SRC" fixture-dep 1.2.3) || {
    echo "ERROR 无法制作 fixture-dep .crate" >&2; exit 69;
}
PARENT_STAGE="$TMP/stage/fixture-parent-1.0.0"
mkdir -p "$PARENT_STAGE/src"
cat >"$PARENT_STAGE/Cargo.toml" <<'EOF'
[package]
name = "fixture-parent"
version = "1.0.0"
edition = "2021"

[dependencies]
fixture-dep = "^1"
EOF
cp "$PARENT_SRC/src/lib.rs" "$PARENT_STAGE/src/lib.rs"
tar -C "$TMP/stage" -czf "$REGISTRY/crates/fixture-parent/1.0.0.crate" \
    fixture-parent-1.0.0 || { echo "ERROR 无法制作 fixture-parent .crate" >&2; exit 69; }
PARENT_SUM=$(sha256sum "$REGISTRY/crates/fixture-parent/1.0.0.crate" | awk '{print $1}')
printf '{"name":"fixture-dep","vers":"1.2.3","deps":[],"cksum":"%s","features":{},"yanked":false}\n' \
    "$DEP_SUM" >"$REGISTRY/fi/xt/fixture-dep"
printf '%s\n' \
    "{\"name\":\"fixture-parent\",\"vers\":\"1.0.0\",\"deps\":[{\"name\":\"fixture-dep\",\"req\":\"^1\",\"features\":[],\"optional\":false,\"default_features\":true,\"target\":null,\"kind\":\"normal\",\"registry\":null,\"package\":null}],\"cksum\":\"$PARENT_SUM\",\"features\":{},\"yanked\":false}" \
    >"$REGISTRY/fi/xt/fixture-parent"

PORT_FILE="$TMP/registry.port"
AUTH_LOG="$TMP/registry-auth.log"
"$PYTHON" - "$REGISTRY" "$PORT_FILE" "$AUTH_LOG" <<'PY' &
import http.server
import pathlib
import sys

root = sys.argv[1]
port_file = pathlib.Path(sys.argv[2])
auth_log = pathlib.Path(sys.argv[3])

class Handler(http.server.SimpleHTTPRequestHandler):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, directory=root, **kwargs)

    def do_GET(self):
        token = self.headers.get("Authorization", "")
        auth_log.open("a").write(f"{self.path}|{token}\n")
        if self.path != "/config.json" and token != "secret-token":
            self.send_response(401)
            self.end_headers()
            return
        super().do_GET()

    def log_message(self, *_args):
        pass

server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
port_file.write_text(str(server.server_port))
server.serve_forever()
PY
SERVER_PID=$!
for _ in $(seq 1 100); do
    [ -s "$PORT_FILE" ] && break
    sleep 0.05
done
[ -s "$PORT_FILE" ] || { echo "ERROR 本地 sparse registry 未启动" >&2; exit 69; }
PORT=$(cat "$PORT_FILE")
INDEX="sparse+http://127.0.0.1:$PORT/"
printf '{"dl":"http://127.0.0.1:%s/crates/{crate}/{version}.crate","auth-required":true}\n' \
    "$PORT" >"$REGISTRY/config.json"

PROVIDER="$TMP/credential-provider.py"
cat >"$PROVIDER" <<'PY'
#!/usr/bin/env python3
import json
import os
import sys

print(json.dumps({"v": [1]}), flush=True)
request = json.loads(sys.stdin.readline())
with open(os.environ["PROVIDER_LOG"], "a") as log:
    log.write(json.dumps({"argv": sys.argv[1:], "request": request}) + "\n")
print(json.dumps({"Ok": {"kind": "get", "token": "secret-token", "cache": "session", "operation_independent": True}}), flush=True)
PY
chmod +x "$PROVIDER"
export PROVIDER_LOG="$TMP/provider.log"

write_registry_config() { # <project>
    local project=$1
    mkdir -p "$project/.cargo"
    printf 'include = ["registry.toml"]\n' >"$project/.cargo/config.toml"
    printf '[registries.alt]\nindex = "%s"\ncredential-provider = "fixture-provider"\n
[credential-alias]\nfixture-provider = ["%s", "argument with spaces"]\n' \
        "$INDEX" "$PROVIDER" >"$project/.cargo/registry.toml"
}

run_cargo() { # <project>
    (
        cd "$1" || exit 1
        CARGO_HOME="$CARGO_HOME_TEST" CARGO_TARGET_DIR="$TMP/cargo-target" RUSTC="$RUSTC" \
            "$CARGO" run --quiet
    )
}

run_self() { # <project>
    PATH="$NO_CARGO:$PATH" MIRVM_HOME="$SELF_HOME" MIRVM_SYSROOT="$CONTRACT_SYSROOT" \
        MIRVM_DEPS=self "$MIRVM" run "$1"
}

fresh_self_lock() { # <project> <cargo lock snapshot> <expected output> <label>
    local project=$1 cargo_lock=$2 expected=$3 label=$4 output code=0
    rm -f "$project/Cargo.lock"
    output=$(run_self "$project" 2>"$TMP/$label-self.err") || code=$?
    if [ "$code" -eq 0 ] && [ "$output" = "$expected" ]; then
        ok "$label self fresh 运行"
    else
        bad "$label self fresh：exit=$code output=$output"
        tail -20 "$TMP/$label-self.err"
    fi
    if cmp -s "$cargo_lock" "$project/Cargo.lock"; then
        ok "$label self lock 与 pinned Cargo 逐字节一致"
    else
        bad "$label self lock 与 pinned Cargo 不同"
        diff -u "$cargo_lock" "$project/Cargo.lock" | head -80
    fi
    if (
        cd "$project" || exit 1
        CARGO_HOME="$CARGO_HOME_TEST" CARGO_TARGET_DIR="$TMP/check-target" RUSTC="$RUSTC" \
            "$CARGO" check --locked --offline
    ) >"$TMP/$label-check.out" 2>"$TMP/$label-check.err"; then
        ok "$label self lock 被 pinned Cargo --locked 接受"
    else
        bad "$label self lock 未被 pinned Cargo 接受"
        tail -20 "$TMP/$label-check.err"
    fi
}

# 1. alternate sparse registry + include + credential alias/protocol。
ALT="$TMP/alt-app"
make_app "$ALT" alt-app $'\n[dependencies]\nfixture-dep = { version = "^1", registry = "alt" }' \
    'fn main() { println!("{}", fixture_dep::value()); }'
write_registry_config "$ALT"
alt_cargo=$(run_cargo "$ALT" 2>"$TMP/alt-cargo.err")
if [ "$?" -eq 0 ] && [ "$alt_cargo" = 123 ] \
    && grep -Fq "source = \"$INDEX\"" "$ALT/Cargo.lock"; then
    ok "alternate sparse registry 的 Cargo 权威运行与 lock source"
else
    bad "alternate sparse registry 的 Cargo 权威未成立: output=$alt_cargo"
    tail -20 "$TMP/alt-cargo.err"
fi
cp "$ALT/Cargo.lock" "$TMP/alt-cargo.lock"
fresh_self_lock "$ALT" "$TMP/alt-cargo.lock" 123 alt-registry
if "$PYTHON" - "$PROVIDER_LOG" <<'PY'
import json, sys
rows = [json.loads(line) for line in open(sys.argv[1])]
assert rows
assert all(row["argv"] == ["--cargo-plugin"] for row in rows)
assert all(row["request"]["args"] == ["argument with spaces"] for row in rows)
assert all(row["request"]["registry"]["name"] == "alt" for row in rows)
PY
then
    ok "credential alias 参数只走 JSON，registry 名与 Cargo 协议一致"
else
    bad "credential provider 协议记录不符合 Cargo v1"
fi

# 2. registry source replacement：逻辑 lock source 保持 crates.io，实际从镜像取包。
MIRROR="$TMP/mirror-app"
make_app "$MIRROR" mirror-app $'\n[dependencies]\nfixture-dep = "^1"' \
    'fn main() { println!("{}", fixture_dep::value()); }'
mkdir -p "$MIRROR/.cargo"
printf '[source.crates-io]\nreplace-with = "alt"\n
[registries.alt]\nindex = "%s"\ncredential-provider = "fixture-provider"\n
[credential-alias]\nfixture-provider = ["%s", "argument with spaces"]\n' \
    "$INDEX" "$PROVIDER" >"$MIRROR/.cargo/config.toml"
mirror_cargo=$(run_cargo "$MIRROR" 2>"$TMP/mirror-cargo.err")
if [ "$?" -eq 0 ] && [ "$mirror_cargo" = 123 ] \
    && grep -Fq 'source = "registry+https://github.com/rust-lang/crates.io-index"' "$MIRROR/Cargo.lock"; then
    ok "registry replacement 的 Cargo 权威与逻辑 lock source"
else
    bad "registry replacement 的 Cargo 权威未成立: output=$mirror_cargo"
    tail -20 "$TMP/mirror-cargo.err"
fi
cp "$MIRROR/Cargo.lock" "$TMP/mirror-cargo.lock"
fresh_self_lock "$MIRROR" "$TMP/mirror-cargo.lock" 123 registry-replacement

# 3. local-registry source replacement。
LOCAL_REG="$TMP/local-registry"
mkdir -p "$LOCAL_REG/index/fi/xt"
cp "$REGISTRY/fi/xt/fixture-dep" "$LOCAL_REG/index/fi/xt/fixture-dep"
cp "$REGISTRY/crates/fixture-dep/1.2.3.crate" "$LOCAL_REG/fixture-dep-1.2.3.crate"
printf '{"dl":"unused"}\n' >"$LOCAL_REG/index/config.json"
LOCAL_APP="$TMP/local-app"
make_app "$LOCAL_APP" local-app $'\n[dependencies]\nfixture-dep = "^1"' \
    'fn main() { println!("{}", fixture_dep::value()); }'
mkdir -p "$LOCAL_APP/.cargo"
printf '[source.crates-io]\nreplace-with = "local"\n[source.local]\nlocal-registry = "%s"\n' \
    "$LOCAL_REG" >"$LOCAL_APP/.cargo/config.toml"
local_cargo=$(run_cargo "$LOCAL_APP" 2>"$TMP/local-cargo.err")
if [ "$?" -eq 0 ] && [ "$local_cargo" = 123 ]; then
    ok "local-registry replacement 的 Cargo 权威运行"
else
    bad "local-registry replacement 的 Cargo 权威未成立: output=$local_cargo"
    tail -20 "$TMP/local-cargo.err"
fi
cp "$LOCAL_APP/Cargo.lock" "$TMP/local-cargo.lock"
fresh_self_lock "$LOCAL_APP" "$TMP/local-cargo.lock" 123 local-registry

# 4. cargo vendor directory replacement + 每文件 checksum 拒绝篡改。
VENDOR="$TMP/vendor"
(
    cd "$ALT" || exit 1
    CARGO_HOME="$CARGO_HOME_TEST" RUSTC="$RUSTC" "$CARGO" vendor "$VENDOR"
) >"$TMP/vendor.out" 2>"$TMP/vendor.err"
if [ "$?" -ne 0 ]; then
    echo "ERROR cargo vendor 无法生成权威 fixture" >&2
    tail -20 "$TMP/vendor.err" >&2
    exit 69
fi
DIR_APP="$TMP/directory-app"
make_app "$DIR_APP" directory-app $'\n[dependencies]\nfixture-dep = "^1"' \
    'fn main() { println!("{}", fixture_dep::value()); }'
mkdir -p "$DIR_APP/.cargo"
printf '[source.crates-io]\nreplace-with = "vendored"\n[source.vendored]\ndirectory = "%s"\n' \
    "$VENDOR" >"$DIR_APP/.cargo/config.toml"
dir_cargo=$(run_cargo "$DIR_APP" 2>"$TMP/directory-cargo.err")
if [ "$?" -eq 0 ] && [ "$dir_cargo" = 123 ]; then
    ok "directory replacement 的 Cargo 权威运行"
else
    bad "directory replacement 的 Cargo 权威未成立: output=$dir_cargo"
    tail -20 "$TMP/directory-cargo.err"
fi
cp "$DIR_APP/Cargo.lock" "$TMP/directory-cargo.lock"
fresh_self_lock "$DIR_APP" "$TMP/directory-cargo.lock" 123 directory-replacement
VENDOR_DEP=$(find "$VENDOR" -mindepth 1 -maxdepth 1 -type d -name 'fixture-dep*' -print -quit)
printf 'pub fn value() -> usize { 999 }\n' >"$VENDOR_DEP/src/lib.rs"
if run_self "$DIR_APP" >"$TMP/tamper.out" 2>"$TMP/tamper.err"; then
    bad "directory source 篡改后 self 意外成功"
elif rg -q 'sha256 校验失败' "$TMP/tamper.err"; then
    ok "directory source 每文件 checksum 拒绝篡改"
else
    bad "directory source 篡改诊断不明确"
    tail -20 "$TMP/tamper.err"
fi

# 5. direct/transitive patch + unused patch，证明候选参与求解但未选者不进入构建图。
PATCH_DEP="$TMP/patch-dep"
UNUSED_DEP="$TMP/unused-dep"
make_library "$PATCH_DEP" fixture-dep 1.9.0 "" \
    'pub fn value() -> usize { 1900 }'
make_library "$UNUSED_DEP" fixture-dep 2.0.0 "" \
    'pub fn value() -> usize { 2000 }'
PATCH_APP="$TMP/patch-app"
make_app "$PATCH_APP" patch-app $'\n[dependencies]\nfixture-parent = { version = "^1", registry = "alt" }\nfixture-dep = { version = "^1", registry = "alt" }\n\n[patch.alt]\nfixture-dep = { path = "../patch-dep" }\nfixture-dep-next = { path = "../unused-dep", package = "fixture-dep" }' \
    'fn main() { println!("{} {}", fixture_parent::value(), fixture_dep::value()); }'
write_registry_config "$PATCH_APP"
patch_cargo=$(run_cargo "$PATCH_APP" 2>"$TMP/patch-cargo.err")
if [ "$?" -eq 0 ] && [ "$patch_cargo" = "1901 1900" ] \
    && rg -q '^\[\[patch.unused\]\]$' "$PATCH_APP/Cargo.lock"; then
    ok "direct/transitive/unused patch 的 Cargo 权威运行与 lock"
else
    bad "patch 的 Cargo 权威未成立: output=$patch_cargo"
    tail -20 "$TMP/patch-cargo.err"
fi
cp "$PATCH_APP/Cargo.lock" "$TMP/patch-cargo.lock"
fresh_self_lock "$PATCH_APP" "$TMP/patch-cargo.lock" "1901 1900" patch

# 6. replace 保留原 registry 行，并用同名同版本 path 包执行。
REPLACE_DEP="$TMP/replace-dep"
make_library "$REPLACE_DEP" fixture-dep 1.2.3 "" \
    'pub fn value() -> usize { 8800 }'
REPLACE_APP="$TMP/replace-app"
make_app "$REPLACE_APP" replace-app $'\n[dependencies]\nfixture-dep = "=1.2.3"\n\n[replace]\n"fixture-dep:1.2.3" = { path = "../replace-dep" }' \
    'fn main() { println!("{}", fixture_dep::value()); }'
cp -R "$MIRROR/.cargo" "$REPLACE_APP/.cargo"
replace_cargo=$(run_cargo "$REPLACE_APP" 2>"$TMP/replace-cargo.err")
if [ "$?" -eq 0 ] && [ "$replace_cargo" = 8800 ] \
    && rg -q '^replace = "fixture-dep 1.2.3"$' "$REPLACE_APP/Cargo.lock"; then
    ok "replace 的 Cargo 权威运行与双 package lock"
else
    bad "replace 的 Cargo 权威未成立: output=$replace_cargo"
    tail -20 "$TMP/replace-cargo.err"
fi
cp "$REPLACE_APP/Cargo.lock" "$TMP/replace-cargo.lock"
fresh_self_lock "$REPLACE_APP" "$TMP/replace-cargo.lock" 8800 replace

# 7. replacement 环必须与 Cargo 一样拒绝，不能悄悄退回原源。
LOOP_APP="$TMP/loop-app"
make_app "$LOOP_APP" loop-app $'\n[dependencies]\nfixture-dep = "^1"' 'fn main() {}'
mkdir -p "$LOOP_APP/.cargo"
printf '[source.crates-io]\nreplace-with="one"\n[source.one]\nreplace-with="two"\n[source.two]\nreplace-with="one"\n' \
    >"$LOOP_APP/.cargo/config.toml"
rm -f "$LOOP_APP/Cargo.lock"
if (
    cd "$LOOP_APP" || exit 1
    CARGO_HOME="$CARGO_HOME_TEST" "$CARGO" generate-lockfile
) >"$TMP/loop-cargo.out" 2>"$TMP/loop-cargo.err"; then
    bad "source replacement 环被 pinned Cargo 意外接受"
else
    ok "source replacement 环的 Cargo 权威确实拒绝"
fi
if run_self "$LOOP_APP" >"$TMP/loop-self.out" 2>"$TMP/loop-self.err"; then
    bad "source replacement 环被 self 意外接受"
elif rg -q 'source replacement 形成环' "$TMP/loop-self.err"; then
    ok "source replacement 环被 self 明确拒绝"
else
    bad "source replacement 环的 self 诊断不明确"
    tail -20 "$TMP/loop-self.err"
fi

# 热缓存离线与零 Cargo 进程审计。先停本地 registry，确保不是伪离线。
kill "$SERVER_PID" 2>/dev/null || true
wait "$SERVER_PID" 2>/dev/null || true
SERVER_PID=""
PATH="$NO_CARGO:$PATH" MIRVM_HOME="$SELF_HOME" MIRVM_SYSROOT="$CONTRACT_SYSROOT" \
    MIRVM_OFFLINE=1 MIRVM_DEPS=self "$STRACE" -f -qq -e trace=execve \
    -o "$TMP/self.execve" "$MIRVM" run "$ALT" >"$TMP/offline.out" 2>"$TMP/offline.err"
if [ "$?" -eq 0 ] && [ "$(cat "$TMP/offline.out")" = 123 ]; then
    ok "alternate registry 热缓存离线复跑"
else
    bad "alternate registry 热缓存离线复跑失败"
    tail -20 "$TMP/offline.err"
fi
if [ -e "$MIRVM_CARGO_SENTINEL" ] || rg -q 'execve\("[^"]*/cargo"' "$TMP/self.execve"; then
    bad "来源合同 self 路径启动了 Cargo"
else
    ok "来源合同 self 路径 execve 零 Cargo"
fi

suite_summary contracts.cargoless-sources
