#!/usr/bin/env bash
# tests/corpus_deps_pair.sh —— D15 P2 闭合契约对拍轴：corpus 指定层每条目跑
# 两腿（MIRVM_DEPS=cargo 三阶段旧路径 vs MIRVM_DEPS=self 零 cargo 自有调度），
# stdout/stderr/exit 逐字节一致才判 PASS。
#
# 用法：bash tests/corpus_deps_pair.sh [--tier smoke|full|all] [name ...]
# 环境同 corpus.sh（MIRVM / MIRVM_DISK_MIN_GB / MIRVM_TARGET_BUDGET_GB 等）。
# 磁盘纪律：两腿各跑一遍 = 两遍开销；cache 清理口径与 corpus_run 相同
# （deps/ir 每跑清，cargoless/cargo 两 target store 保留复用）。
set -u
cd "$(dirname "$0")/.."
MIRVM=${MIRVM:-$(pwd)/target/release/mirvm}
[ -x "$MIRVM" ] || { echo "corpus_deps_pair: $MIRVM 不存在（先 cargo build --release）" >&2; exit 69; }
. tests/lib.sh
CORPUS_TIMINGS_FILE=$(mktemp); export CORPUS_TIMINGS_FILE
trap 'rm -f "$CORPUS_TIMINGS_FILE"' EXIT

tier=smoke
names=()
while [ $# -gt 0 ]; do
    case "$1" in
        --tier) tier=$2; shift 2 ;;
        --tier=*) tier=${1#--tier=}; shift ;;
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
else
    rows=$(manifest_rows "$tier") || exit 2
fi

OUT_C=$(mktemp -d /tmp/corpair-cargo-XXXXXX)
OUT_S=$(mktemp -d /tmp/corpair-self-XXXXXX)
trap 'rm -rf "$OUT_C" "$OUT_S" "$CORPUS_TIMINGS_FILE"' EXIT

pass=0 fail=0 skip_count=0
while IFS='|' read -r name _tier tmo mode envv needs args _xfail; do
    [ -n "$name" ] || continue
    argv=()
    [ -n "$args" ] && parse_args "$args" argv

    MIRVM_DEPS=cargo corpus_run "$OUT_C" "$name" "$tmo" "$envv" "$needs" ${argv[@]+"${argv[@]}"}
    cc=$?
    MIRVM_DEPS=self corpus_run "$OUT_S" "$name" "$tmo" "$envv" "$needs" ${argv[@]+"${argv[@]}"}
    sc=$?

    if [ "$cc" -eq 77 ] || [ "$sc" -eq 77 ]; then
        echo "SKIP  $name（needs 缺席：$needs）"
        skip_count=$((skip_count + 1))
        continue
    fi
    ok=1 why=""
    if [ "$cc" != "$sc" ]; then ok=0; why="退出码 cargo=$cc self=$sc"
    elif ! diff -q "$OUT_C/$name.out" "$OUT_S/$name.out" >/dev/null; then ok=0; why="stdout 不一致"
    elif ! diff -q "$OUT_C/$name.err" "$OUT_S/$name.err" >/dev/null; then ok=0; why="stderr 不一致"
    fi
    if [ "$ok" = 1 ]; then
        echo "PASS  $name"
        pass=$((pass + 1))
    else
        echo "FAIL  $name: $why"
        echo "--- cargo stderr 尾部 ---"; tail -5 "$OUT_C/$name.err" 2>/dev/null
        echo "--- self stderr 尾部 ---"; tail -5 "$OUT_S/$name.err" 2>/dev/null
        fail=$((fail + 1))
    fi
done <<< "$rows"

echo "---"
if [ "$skip_count" -gt 0 ]; then
    echo "deps_pair: $pass pass, $skip_count skip, $fail fail"
else
    echo "deps_pair: $pass pass, $fail fail"
fi
target_budget_check
[ "$fail" -eq 0 ]
