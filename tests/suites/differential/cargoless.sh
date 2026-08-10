#!/usr/bin/env bash
# D15 P2 对拍：MIRVM_DEPS=self（零 cargo 驱动）vs MIRVM_DEPS=cargo
# （三阶段旧路径），stdout/stderr/exit 逐字节一致。六条：frontmatter 脚本
# （切① fresh 求解）、registry 项目（切① 带锁）、path proc-macro 项目
# （切②：proc-macro 真 rustc host 编译）、registry build.rs 脚本（切③：
# libc 的 build.rs 全链）、path build.rs 项目（切③：OUT_DIR/rustc-cfg/
# rustc-env/DEP_* 传播）、serde derive 脚本（切②+③ 全链：proc-macro2 与
# serde_core 的 build.rs + host/target 双侧 + facade 再导出 proc-macro）。
# 零 cargo 进程实证：self 腿以「PATH 只含 mirvm 的临时目录」+ MIRVM_OFFLINE=1
# 跑——cargo 不在 PATH，自路径若偷起 cargo 立刻现形（itoa/memchr/cfg-if 本机
# registry 已有，读穿离线够）。正式 self 腿前的预热跑（正常 PATH、在线）只为
# 灌自有 registry 的 index/src 缓存（sparse index 无 cargo 侧读穿，P1 定案）。
# 脚本腿允许 cargo 腿联网（fresh 求解 cargo 侧可能查 index）。
set -u
. "$(dirname "${BASH_SOURCE[0]}")/../../support/harness.sh"
test_enter_repo
MIRVM=${MIRVM:-$(pwd)/target/debug/mirvm}
[ -x "$MIRVM" ] || { echo "diff_cless: $MIRVM 不存在（先 cargo build）" >&2; exit 69; }
MIRVM=$(realpath "$MIRVM")
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT

# self 腿环境：PATH 只含 mirvm（cargo/rustc 都不在 PATH）
mkdir -p "$TMP/bin"
ln -s "$MIRVM" "$TMP/bin/mirvm"

# <name> <cargo 退出码> <self 退出码>：三维逐字节对拍，失配落明细
check_pair() {
    local name="$1" cc="$2" sc="$3"
    local ok=1 why=""
    if [ "$cc" != "$sc" ]; then ok=0; why="退出码 cargo=$cc self=$sc"
    elif ! diff -q "$TMP/$name.cargo.out" "$TMP/$name.self.out" >/dev/null; then ok=0; why="stdout 不一致"
    elif ! diff -q "$TMP/$name.cargo.err" "$TMP/$name.self.err" >/dev/null; then ok=0; why="stderr 不一致"
    fi
    if [ $ok = 1 ]; then
        echo "PASS $name"; pass=$((pass + 1))
    else
        echo "FAIL $name: $why"
        echo "--- cargo stdout ---"; cat "$TMP/$name.cargo.out"
        echo "--- self stdout ---"; cat "$TMP/$name.self.out"
        echo "--- cargo stderr ---"; cat "$TMP/$name.cargo.err"
        echo "--- self stderr ---"; cat "$TMP/$name.self.err"
        fail=$((fail + 1))
    fi
}

# <name> <target> <预热期望退出码> [cargo 腿附加 env...]
# 预热（灌自有 registry 缓存）→ cargo 腿 → self 腿（受限 PATH + 离线）→ 对拍
diff_cless() {
    local name="$1" target="$2" prime_code="$3"; shift 3
    # 预热：self 腿首跑可能要在/离线拉 index+src 并编译 deps；产物丢弃
    env -u RUST_BACKTRACE MIRVM_DEPS=self "$MIRVM" run "$target" \
        >"$TMP/$name.prime.out" 2>"$TMP/$name.prime.err"
    local pc=$?
    if [ "$pc" != "$prime_code" ]; then
        echo "FAIL $name: self 腿预热失败（exit=$pc，期望 $prime_code = guest 运行码）"
        tail -20 "$TMP/$name.prime.err"
        fail=$((fail + 1))
        return
    fi
    # cargo 腿（脚本腿允许联网；项目腿 --locked）
    env -u RUST_BACKTRACE MIRVM_DEPS=cargo "$@" "$MIRVM" run "$target" \
        >"$TMP/$name.cargo.out" 2>"$TMP/$name.cargo.err"
    local cc=$?
    # self 腿（零 cargo 实证：cargo 不在 PATH + 离线）
    env -u RUST_BACKTRACE PATH="$TMP/bin" MIRVM_OFFLINE=1 MIRVM_DEPS=self \
        "$MIRVM" run "$target" \
        >"$TMP/$name.self.out" 2>"$TMP/$name.self.err"
    local sc=$?
    check_pair "$name" "$cc" "$sc"
}

# 1) frontmatter 脚本夹具（fresh 求解；guest 退出码 4）
diff_cless cless_script tests/fixtures/cless_script.rs 4

# 2) cargo 项目夹具（带锁；先拷一份防污染仓；cargo 腿 --locked；guest 退出码 3）
cp -r tests/fixtures/cless_proj "$TMP/proj"
diff_cless cless_proj "$TMP/proj" 3 MIRVM_CARGO_LOCKED=1

# 3) path proc-macro 项目夹具（切②；带锁；cargo 腿 --locked；guest 退出码 5）
cp -r tests/fixtures/cless_pm "$TMP/pm"
diff_cless cless_pm "$TMP/pm" 5 MIRVM_CARGO_LOCKED=1

# 4) registry build.rs 脚本夹具（切③：libc 的 build.rs 全链；guest 退出码 6）
diff_cless cless_libc tests/fixtures/cless_libc.rs 6

# 5) path build.rs 项目夹具（切③：OUT_DIR/rustc-cfg/rustc-env/DEP_* 传播；
#    带锁；cargo 腿 --locked；guest 退出码 7）
cp -r tests/fixtures/cless_br "$TMP/br"
diff_cless cless_br "$TMP/br" 7 MIRVM_CARGO_LOCKED=1

# 6) serde derive 全链脚本夹具（切②+③：proc-macro2/serde_core 的 build.rs +
#    host/target 双侧 + facade 再导出 proc-macro；guest 退出码 8）
diff_cless cless_serde tests/fixtures/cless_serde.rs 8

# 7) --bin 多目标选择（D15 P4 切⑥b：a2_ws 双 bin + default-run；cargo run
#    --bin 语义双腿逐字节）——diff_cless() 不支持额外 mirvm 旗，专列
cp -r tests/fixtures/a2_ws "$TMP/a2ws"
for leg in cargo self; do
    if [ "$leg" = cargo ]; then
        env -u RUST_BACKTRACE MIRVM_DEPS=cargo MIRVM_CARGO_LOCKED=1 \
            "$MIRVM" run "$TMP/a2ws" --bin a2_two \
            >"$TMP/binsel.$leg.out" 2>"$TMP/binsel.$leg.err"
        eval "${leg}_code=$?"
    else
        env -u RUST_BACKTRACE PATH="$TMP/bin" MIRVM_OFFLINE=1 MIRVM_DEPS=self \
            "$MIRVM" run "$TMP/a2ws" --bin a2_two \
            >"$TMP/binsel.$leg.out" 2>"$TMP/binsel.$leg.err"
        eval "${leg}_code=$?"
    fi
done
check_pair binsel "$cargo_code" "$self_code"

suite_summary differential.cargoless
