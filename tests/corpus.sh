#!/usr/bin/env bash
# tests/corpus.sh —— corpus 手工跑批（tests/corpus.manifest 唯一真源驱动）。
# 用途是"发现真实 crate 对抽象机/VM 边界的要求"，不是刷通过率；
# 判绿口径 = 退出码冒烟（严格判绿——oracle/diff/xfail——归 tests/gate.sh）。
#
# 用法：
#   bash tests/corpus.sh                  # 全量（smoke+full+manual 三层）
#   bash tests/corpus.sh --tier smoke     # 只跑某层（smoke|full|manual|all）
#   bash tests/corpus.sh tempfile walkdir # 按名跑子集（须在 manifest 登记）
# 环境：MIRVM（默认 target/release/mirvm）、OUT（默认 /tmp/corpus-out）、
#   MIRVM_GATE_KEEP_CACHE=1（逐驱动清 cache 的调试旁路）、
#   MIRVM_DISK_MIN_GB / MIRVM_TARGET_BUDGET_GB（磁盘护栏，见 tests/lib.sh）。
set -u
cd "$(dirname "$0")/.."
MIRVM=${MIRVM:-$(pwd)/target/release/mirvm}
OUT=${OUT:-/tmp/corpus-out}
mkdir -p "$OUT"
. tests/lib.sh
CORPUS_TIMINGS_FILE=$(mktemp)
export CORPUS_TIMINGS_FILE
trap 'rm -f "$CORPUS_TIMINGS_FILE"' EXIT

tier=all
names=()
while [ $# -gt 0 ]; do
    case "$1" in
        --tier) tier=$2; shift 2 ;;
        --tier=*) tier=${1#--tier=}; shift ;;
        *) names+=("$1"); shift ;;
    esac
done
case "$tier" in smoke|full|manual|all) ;; *)
    echo "corpus.sh: 非法 tier '$tier'（smoke|full|manual|all）" >&2; exit 64 ;; esac

if [ ${#names[@]} -gt 0 ]; then
    rows=$(
        for n in "${names[@]}"; do
            manifest_lookup "$n" || { echo "corpus.sh: $n 未在 tests/corpus.manifest 登记" >&2; exit 2; }
        done
    ) || exit 2
else
    rows=$(manifest_rows "$tier") || exit 2
fi

cache_snapshot "corpus 起跑前"
while IFS='|' read -r name _tier tmo mode envv needs args _xfail; do
    [ -n "$name" ] || continue
    argv=()
    [ -n "$args" ] && parse_args "$args" argv
    code=0
    corpus_run "$OUT" "$name" "$tmo" "$envv" "$needs" ${argv[@]+"${argv[@]}"} || code=$?
    dur=$(awk -v n="$name" '$2==n{s=$1} END{print s+0}' "$CORPUS_TIMINGS_FILE")
    if [ "$code" -eq 77 ]; then
        echo "SKIP  $name（needs 缺席：$needs）"
        skip_count=$((skip_count + 1))
    elif [ "$code" -eq 2 ]; then
        echo "FAIL  $name（manifest 有登记但无 driver 文件）"
        fail=$((fail + 1))
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

echo "---"
if [ "$skip_count" -gt 0 ]; then
    echo "corpus: $pass pass, $skip_count skip, $fail fail"
else
    echo "corpus: $pass pass, $fail fail"
fi
print_slowest 10
target_budget_check
cache_snapshot "corpus 收尾后"
[ "$fail" -eq 0 ]
