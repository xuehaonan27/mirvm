#!/usr/bin/env bash
# corpus 探索跑批（suites/corpus/cases.manifest 唯一真源驱动）。
# 用途是"发现真实 crate 对抽象机/VM 边界的要求"，不是刷通过率；
# 判绿口径是登记的退出码；已登记 XFAIL 还会锁定退出码和诊断。oracle/diff 的
# 逐字节判定归 corpus.contract。
#
# 用法：
#   ./tests/run.sh suite corpus.run --tier smoke
#   ./tests/run.sh suite corpus.run --group heavy
#   ./tests/run.sh suite corpus.run tempfile walkdir
#   --tier 与 --group 可叠加（交集）；按名跑忽略组过滤
# 环境：MIRVM（默认 target/release/mirvm）、OUT（默认 /tmp/corpus-out）、
#   MIRVM_GATE_KEEP_CACHE=1（逐驱动清 cache 的调试旁路）、
#   MIRVM_DISK_MIN_GB / MIRVM_TARGET_BUDGET_GB（磁盘护栏见共享 harness）。
set -u
. "$(dirname "${BASH_SOURCE[0]}")/../../support/harness.sh"
test_enter_repo
MIRVM=${MIRVM:-$(pwd)/target/release/mirvm}
OUT=${OUT:-/tmp/corpus-out}
mkdir -p "$OUT"
CORPUS_TIMINGS_FILE=$(mktemp)
export CORPUS_TIMINGS_FILE
trap 'rm -f "$CORPUS_TIMINGS_FILE"' EXIT

tier=all
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
    echo "corpus.run: 非法 tier '$tier'（smoke|full|manual|all）" >&2; exit 64 ;; esac

if [ ${#names[@]} -gt 0 ]; then
    rows=$(
        for n in "${names[@]}"; do
            manifest_lookup "$n" || { echo "corpus.run: $n 未在 cases.manifest 登记" >&2; exit 2; }
        done
    ) || exit 2
elif [ -n "$group" ]; then
    rows=$(manifest_group_rows "$group" "$tier") || exit 2
    [ -n "$rows" ] || { echo "corpus.run: 组 '$group'（tier=$tier）无条目" >&2; exit 64; }
else
    rows=$(manifest_rows "$tier") || exit 2
fi

cache_snapshot "corpus 起跑前"
while IFS='|' read -r name _tier tmo mode envv needs args xfail_spec _groups; do
    [ -n "$name" ] || continue
    argv=()
    [ -n "$args" ] && parse_args "$args" argv
    code=0
    corpus_run "$OUT" "$name" "$tmo" "$envv" "$needs" ${argv[@]+"${argv[@]}"} || code=$?
    dur=$(awk -v n="$name" '$2==n{s=$1} END{print s+0}' "$CORPUS_TIMINGS_FILE")
    if [ "$code" -eq 77 ]; then
        skip "$name（needs 缺席：$needs）"
    elif [ "$code" -eq 2 ]; then
        echo "FAIL  $name（manifest 有登记但无 driver 文件）"
        fail=$((fail + 1))
    elif [ -n "$xfail_spec" ]; then
        record_expected_failure "$name" "$code" "$xfail_spec" "$OUT/$name.err"
    elif [ "$code" -eq 0 ]; then
        echo "PASS  $name  (${dur}s)"
        pass=$((pass + 1))
    else
        first_err=$(grep -m1 -iE 'error|panic|unsupported|unimplemented|not (yet )?(implemented|supported)|no (shim|intrinsic)|abort' "$OUT/$name.err" | head -c 200)
        [ -z "$first_err" ] && first_err=$(tail -1 "$OUT/$name.err" | head -c 200)
        echo "FAIL  $name  (${dur}s, exit=$code)  ::  $first_err"
        fail=$((fail + 1))
    fi
done <<< "$rows"

print_slowest 10
target_budget_check
cache_snapshot "corpus 收尾后"
suite_summary corpus.run
