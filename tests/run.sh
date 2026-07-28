#!/usr/bin/env bash
# tests/run.sh —— 测试套件统一入口（2026-07-23 测试管线整顿）。
#
# 档位（由小到大）：
#   fast    逢提交级：cargo fmt/clippy/test + diff.sh 双态 + diff_cargo + diff_cless
#           + bldrs_rerun + gate_truth
#   smoke   战役批次级：fast + corpus smoke 层（tests/corpus.manifest）+ probes + runtime_gates
#   gate    战役收尾级：静态/单元/gate_truth + tests/gate.sh 全量（其内部已含
#           corpus 全防线 + diff 四态 + diff_cargo + perf + a2 + probes + runtime_gates）
#   corpus  手工跑批：bash tests/run.sh corpus [--tier T|名字...]（转 tests/corpus.sh）
#   perf    性能与资源计量（tests/perf.sh）
#
# 环境：MIRVM（默认 target/release/mirvm，fast/smoke/gate 统一导出）；
#   SKIP_TSAN=1 / SKIP_PERF=1（语义同各叶脚本）；磁盘护栏见 tests/lib.sh。
# 测试资产归属：demo/*.rs = diff.sh 差分用例（demo/m4/ = runtime_gates 的
#   vm-call 夹具）；corpus/c_*.rs = 真实 crate 最小驱动（manifest 登记）；
#   corpus/projects/<名>/ = 真 cargo 项目对拍（manifest mode=diff）；
#   bench/ = 性能基准素材（perf.sh 体系消费）；tests/parked/ = 休眠基建存档。
set -u
cd "$(dirname "$0")/.."
MIRVM=${MIRVM:-$(pwd)/target/release/mirvm}
export MIRVM
. tests/lib.sh

usage() {
    sed -n '2,20p' tests/run.sh
    exit 64
}

run_step() {  # <标题> <命令...>：跑一步，失败打尾 10 行并记账
    local title=$1; shift
    section_start "$title"
    if "$@" >"$TMPDIR_RUN/$title.log" 2>&1; then
        ok "$title"
    else
        bad "$title"
        tail -10 "$TMPDIR_RUN/$title.log"
    fi
    section_end
}

run_quality() {  # 静态检查与单元测试层
    run_step "cargo fmt" cargo fmt --all -- --check
    run_step "cargo clippy" cargo clippy --locked --all-targets --all-features -- -D warnings
    run_step "cargo test" cargo test --locked --all-features
}

run_diff_family() {  # 差分家族（diff.sh 双态 + cargo 形态 + cargoless 对拍 + bldrs 增量 + 门禁自回归）
    run_step "diff.sh" bash tests/diff.sh
    run_step "diff.sh(JIT=1+SYNC)" env MIRVM_JIT_SYNC=1 MIRVM_JIT_THRESHOLD=1 bash tests/diff.sh
    run_step "diff_cargo" bash tests/diff_cargo.sh
    run_step "diff_cless" bash tests/diff_cless.sh
    run_step "bldrs_rerun" bash tests/bldrs_rerun.sh
    run_step "gate_truth" bash tests/gate_truth_regression.sh
}

cmd=${1:-}
[ $# -ge 1 ] && shift
case "$cmd" in
    fast|smoke|gate)
        [ -x "$MIRVM" ] || { echo "run.sh: $MIRVM 不存在（先 cargo build --release）" >&2; exit 69; }
        TMPDIR_RUN=$(mktemp -d); trap 'rm -rf "$TMPDIR_RUN"' EXIT
        cache_snapshot "run $cmd 起跑前"
        disk_guard
        run_quality
        if [ "$cmd" = gate ]; then
            run_step "gate_truth" bash tests/gate_truth_regression.sh
            run_step "gate.sh" bash tests/gate.sh
        else
            run_diff_family
            if [ "$cmd" = smoke ]; then
                run_step "corpus smoke" bash tests/corpus.sh --tier smoke
                run_step "probes" bash tests/probes.sh
                run_step "runtime_gates" bash tests/runtime_gates.sh
            fi
        fi
        target_budget_check
        cache_snapshot "run $cmd 收尾后"
        print_section_report
        echo "---"
        echo "run $cmd: $pass pass, $fail fail"
        [ "$fail" -eq 0 ]
        ;;
    corpus)
        exec bash tests/corpus.sh "$@"
        ;;
    perf)
        exec bash tests/perf.sh "$@"
        ;;
    *)
        usage
        ;;
esac
