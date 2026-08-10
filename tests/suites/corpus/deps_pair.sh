#!/usr/bin/env bash
# corpus 依赖路径对拍：指定层的每个条目各跑 Cargo 与 cargoless 两条路径，
# 两腿（MIRVM_DEPS=cargo 三阶段旧路径 vs MIRVM_DEPS=self 零 cargo 自有调度），
# stdout/stderr/exit 逐字节一致才判 PASS。
#
# 用法：./tests/run.sh suite corpus.deps-pair [--tier smoke|full|all] [--group 组] [name ...]
#   --group heavy 只跑 manifest group=heavy 的条目（light = 无 group= 键条目）；
#   --tier 与 --group 可叠加（交集）；按名跑忽略组过滤
# 环境同 corpus.run（MIRVM / MIRVM_DISK_MIN_GB / MIRVM_TARGET_BUDGET_GB 等）。
# 磁盘纪律：两腿各跑一遍 = 两遍开销；cache 清理口径与 corpus_run 相同
# （deps/ir 每跑清，cargoless/cargo 两 target store 保留复用）。
set -u
. "$(dirname "${BASH_SOURCE[0]}")/../../support/harness.sh"
test_enter_repo
MIRVM=${MIRVM:-$(pwd)/target/release/mirvm}
[ -x "$MIRVM" ] || { echo "corpus_deps_pair: $MIRVM 不存在（先 cargo build --release）" >&2; exit 69; }
CORPUS_TIMINGS_FILE=$(mktemp); export CORPUS_TIMINGS_FILE
trap 'rm -f "$CORPUS_TIMINGS_FILE"' EXIT

tier=smoke
group=""
names=()
while [ $# -gt 0 ]; do
    case "$1" in
        --tier) tier=$2; shift 2 ;;
        --tier=*) tier=${1#--tier=}; shift ;;
        --group) group=$2; shift 2 ;;
        --group=*) group=${1#--group=}; shift ;;
        *) names+=("$1"); shift ;;
    esac
done
case "$tier" in smoke|full|manual|all) ;; *)
    echo "corpus_deps_pair: 非法 tier '$tier'" >&2; exit 64 ;; esac

if [ ${#names[@]} -gt 0 ]; then
    rows=$(
        for n in "${names[@]}"; do
            manifest_lookup "$n" || { echo "corpus_deps_pair: $n 未登记" >&2; exit 2; }
        done
    ) || exit 2
elif [ -n "$group" ]; then
    rows=$(manifest_group_rows "$group" "$tier") || exit 2
    [ -n "$rows" ] || { echo "corpus_deps_pair: 组 '$group'（tier=$tier）无条目" >&2; exit 64; }
else
    rows=$(manifest_rows "$tier") || exit 2
fi

OUT_C=$(mktemp -d /tmp/corpair-cargo-XXXXXX)
OUT_S=$(mktemp -d /tmp/corpair-self-XXXXXX)
# KEEP_OUT=1：保留两腿输出现场（分诊用）；缺省退出即清。
if [ "${KEEP_OUT:-0}" = 1 ]; then
    trap 'rm -f "$CORPUS_TIMINGS_FILE"; echo "corpair 现场保留: $OUT_C $OUT_S" >&2' EXIT
else
    trap 'rm -rf "$OUT_C" "$OUT_S" "$CORPUS_TIMINGS_FILE"' EXIT
fi

while IFS='|' read -r name _tier tmo mode envv needs args _xfail _groups; do
    [ -n "$name" ] || continue
    argv=()
    [ -n "$args" ] && parse_args "$args" argv

    MIRVM_DEPS=cargo corpus_run "$OUT_C" "$name" "$tmo" "$envv" "$needs" ${argv[@]+"${argv[@]}"}
    cc=$?
    MIRVM_DEPS=self corpus_run "$OUT_S" "$name" "$tmo" "$envv" "$needs" ${argv[@]+"${argv[@]}"}
    sc=$?

    if [ "$cc" -eq 77 ] || [ "$sc" -eq 77 ]; then
        skip "$name（needs 缺席：$needs）"
        continue
    fi
    ok=1 why=""
    # stderr 对比前规范化 panic 头的线程名/TID（程序差分套件已有同款先例：TID 随
    # 进程漂移天然不可逐字节；只规范化本来就不稳定的部分，其余差异照红）。
    sed -E "s/thread '[^']*' \([0-9]+\)/thread 'T'/" "$OUT_C/$name.err" >"$OUT_C/$name.err.n"
    sed -E "s/thread '[^']*' \([0-9]+\)/thread 'T'/" "$OUT_S/$name.err" >"$OUT_S/$name.err.n"
    if [ "$cc" != "$sc" ]; then ok=0; why="退出码 cargo=$cc self=$sc"
    elif ! diff -q "$OUT_C/$name.out" "$OUT_S/$name.out" >/dev/null; then ok=0; why="stdout 不一致"
    elif ! diff -q "$OUT_C/$name.err.n" "$OUT_S/$name.err.n" >/dev/null; then ok=0; why="stderr 不一致"
    fi
    if [ "$ok" = 1 ]; then
        echo "PASS  $name"
        pass=$((pass + 1))
    elif [ "$cc" -eq 0 ] && [ "$sc" -ne 0 ] \
        && grep -q "归 P5" "$OUT_S/$name.err" 2>/dev/null; then
        red "$name（已知边界：归 P5）"
    else
        echo "FAIL  $name: $why"
        echo "--- cargo stderr 尾部 ---"; tail -5 "$OUT_C/$name.err" 2>/dev/null
        echo "--- self stderr 尾部 ---"; tail -5 "$OUT_S/$name.err" 2>/dev/null
        fail=$((fail + 1))
    fi
done <<< "$rows"

target_budget_check
suite_summary corpus.deps-pair
