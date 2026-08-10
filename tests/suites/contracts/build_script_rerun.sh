#!/usr/bin/env bash
# build.rs rerun-if 精细增量合同。
# （MIRVM_DEBUG_BLDRS=1 观测 `bldrs run|skip <pkg> <原因>` 行）。
# 场景（夹具 tests/fixtures/cless_br：根 build.rs 读 DEP_MYLINKS_FOO + 发
# rerun-if-env-changed=BR_TOGGLE；bdep 带 links=mylinks 发 metadata）：
#   ① 第一遍：全 run（no-record——cp -r 刷 mtime ⇒ fp 全新 ⇒ 存档缺席）
#   ② 第二遍：bdep 与根包全 skip（default 面树快照一致 + env 未变），
#      guest stdout 与 ① 逐字节同（跳过执行不改变任何可观察输出）
#   ③ touch bdep/build.rs：源 stamp 进 fp ⇒ bdep fp 变 ⇒ 存档缺席 run；
#      dep fp 传递 ⇒ 根 fp 变 ⇒ 根 run（links 传递的等价结果由 fp 传递
#      先行覆盖——单测 links_dep_rerun_propagates 覆盖判定本体）
#   ④ BR_TOGGLE=xyz：env 不进 fp ⇒ 存档命中 ⇒ 根 run(env:BR_TOGGLE) 且
#      输出变（toggle=xyz）；bdep skip
#   ⑤ BR_TOGGLE=xyz 不变：根 skip，输出与 ④ 逐字节同（存档回放等价）
#   ⑥ registry 面（cless_libc 脚本夹具，libc 有 build.rs）：第二遍必
#      `bldrs skip libc`（registry 源不可变 ⇒ 默认面永不重跑；第一遍
#      run/skip 皆可——全局 build 缓存可能已有存档，不钉）
set -u
. "$(dirname "${BASH_SOURCE[0]}")/../../support/harness.sh"
test_enter_repo
MIRVM=${MIRVM:-$(pwd)/target/debug/mirvm}
[ -x "$MIRVM" ] || { echo "bldrs_rerun: $MIRVM 不存在（先 cargo build）" >&2; exit 69; }
MIRVM=$(realpath "$MIRVM")
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
cp -r tests/fixtures/cless_br "$TMP/br"

# <描述> <模式> <文件>：grep -F 定点断言
expect() {
    local desc="$1" pat="$2" file="$3"
    if grep -qF -- "$pat" "$file"; then
        echo "PASS $desc"; pass=$((pass + 1))
    else
        echo "FAIL $desc：缺 [$pat]（$file）"
        echo "--- $file ---"; cat "$file"
        fail=$((fail + 1))
    fi
}
# <期望退出码> <标签> [env...]：MIRVM_DEPS=self 跑 mirvm run 存 <标签>.out/.err
run_br() {
    local want="$1" tag="$2"; shift 2
    env -u RUST_BACKTRACE MIRVM_DEPS=self MIRVM_DEBUG_BLDRS=1 "$@" "$MIRVM" run "$TMP/br" \
        >"$TMP/$tag.out" 2>"$TMP/$tag.err"
    local code=$?
    if [ "$code" = "$want" ]; then
        echo "PASS run $tag 退出码 $code"; pass=$((pass + 1))
    else
        echo "FAIL run $tag：退出码 $code（期望 $want）"
        tail -20 "$TMP/$tag.err"; fail=$((fail + 1))
    fi
}
# <描述> <文件A> <文件B> same|diff
expect_cmp() {
    local desc="$1" a="$2" b="$3" want="$4"
    local eq=1; diff -q "$a" "$b" >/dev/null || eq=0
    if { [ "$want" = same ] && [ $eq = 1 ]; } || { [ "$want" = diff ] && [ $eq = 0 ]; }; then
        echo "PASS $desc"; pass=$((pass + 1))
    else
        echo "FAIL $desc（$a vs $b，期望 $want）"
        echo "--- A ---"; cat "$a"; echo "--- B ---"; cat "$b"
        fail=$((fail + 1))
    fi
}

# ① 第一遍：全 run（no-record）
run_br 7 first
expect "① bdep run" "bldrs run bdep no-record" "$TMP/first.err"
expect "① 根包 run" "bldrs run cless_br no-record" "$TMP/first.err"

# ② 第二遍：全 skip + 输出逐字节同
run_br 7 second
expect "② bdep skip（default 面树一致）" "bldrs skip bdep default-tree-intact" "$TMP/second.err"
expect "② 根包 skip（default 面树一致 + env 未变）" "bldrs skip cless_br default-tree-intact" "$TMP/second.err"
expect_cmp "② 输出与①逐字节同" "$TMP/first.out" "$TMP/second.out" same

# ③ touch bdep/build.rs ⇒ fp 变 ⇒ bdep/根包均 run（no-record）
touch "$TMP/br/bdep/build.rs"
run_br 7 third
expect "③ bdep run" "bldrs run bdep no-record" "$TMP/third.err"
expect "③ 根包 run（dep fp 传递）" "bldrs run cless_br no-record" "$TMP/third.err"
expect_cmp "③ 输出仍与②逐字节同" "$TMP/second.out" "$TMP/third.out" same

# ④ BR_TOGGLE=xyz ⇒ 根 run(env:BR_TOGGLE) 且输出变；bdep skip
run_br 7 fourth BR_TOGGLE=xyz
expect "④ 根包 run env:BR_TOGGLE" "bldrs run cless_br env:BR_TOGGLE" "$TMP/fourth.err"
expect "④ bdep skip（env 与树都未动）" "bldrs skip bdep default-tree-intact" "$TMP/fourth.err"
expect "④ 输出含 toggle=xyz" "toggle=xyz" "$TMP/fourth.out"
expect_cmp "④ 输出与③不同（env 进输出）" "$TMP/third.out" "$TMP/fourth.out" diff

# ⑤ BR_TOGGLE=xyz 不变 ⇒ 根 skip，输出与④逐字节同
run_br 7 fifth BR_TOGGLE=xyz
expect "⑤ 根包 skip" "bldrs skip cless_br default-tree-intact" "$TMP/fifth.err"
expect_cmp "⑤ 输出与④逐字节同（存档回放等价）" "$TMP/fourth.out" "$TMP/fifth.out" same

# ⑥ registry 面：cless_libc 第二遍必 skip libc（全局缓存或已有存档，
#    第一遍 run/skip 不钉；env -u 清掉 BR_TOGGLE 防串面）
for i in 1 2; do
    env -u RUST_BACKTRACE -u BR_TOGGLE MIRVM_DEPS=self MIRVM_DEBUG_BLDRS=1 "$MIRVM" run \
        tests/fixtures/cless_libc.rs >"$TMP/libc$i.out" 2>"$TMP/libc$i.err"
    code=$?
    if [ "$code" != 6 ]; then
        echo "FAIL libc 第 $i 遍：退出码 $code（期望 6）"; tail -20 "$TMP/libc$i.err"
        fail=$((fail + 1))
    fi
done
expect "⑥ registry libc 第二遍 skip" "bldrs skip libc" "$TMP/libc2.err"
expect_cmp "⑥ libc 两遍输出逐字节同" "$TMP/libc1.out" "$TMP/libc2.out" same

suite_summary contracts.build-script-rerun
