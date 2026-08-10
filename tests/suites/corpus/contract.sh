#!/usr/bin/env bash
# 严格 corpus 合同：运行 smoke+full 条目并按 manifest 的判定方式验收。
#
# 判定方式：exit、固定输出 oracle、与 native 的 stdout/stderr/退出码三维对拍。
# xfail 必须锁定退出码与诊断；意外转绿按失败处理，要求更新合同。
# 用法统一经 ./tests/run.sh suite corpus.contract [名字...]。
# 磁盘纪律：每驱动后清 deps/ir（MIRVM_GATE_KEEP_CACHE=1 旁路）；
#   MIRVM_DISK_MIN_GB（默认 8G）见底自动升级清理仍不足则响亮中止；
#   target 超 MIRVM_TARGET_BUDGET_GB（默认 24G）自动 purge --target。
set -u
. "$(dirname "${BASH_SOURCE[0]}")/../../support/harness.sh"
test_enter_repo
MIRVM=${MIRVM:-$(pwd)/target/release/mirvm}
CARGO=${CARGO:-$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo}
RUSTC=${RUSTC:-$(dirname "$CARGO")/rustc}
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
export CORPUS_TIMINGS_FILE="$TMP/corpus-timings"
# gix 写 reflog 需要提交者身份（corpus c_gix_pure；commit 哈希由 driver 内
# 固定签名决定，此身份只进 reflog，不进任何可比输出）——不依赖宿主
# ~/.gitconfig，任何机器/CI 同一口径（换机实锤：无 gitconfig 即 MissingCommitter）。
export GIT_AUTHOR_NAME=mirvm-test GIT_AUTHOR_EMAIL=mirvm@test.local
export GIT_COMMITTER_NAME=mirvm-test GIT_COMMITTER_EMAIL=mirvm@test.local

cache_snapshot "gate 起跑前"
target_budget_check

# ---- corpus：manifest 全量防线 ----
echo "== corpus =="
section_start "corpus(smoke+full)"
if [ $# -gt 0 ]; then
    corpus_rows=$(
        for p in "$@"; do
            manifest_lookup "$p" || { echo "corpus.contract: $p 不在 cases.manifest" >&2; exit 2; }
        done
    ) || exit 2
elif [ -n "${CORPUS_PROGS:-}" ]; then
    corpus_rows=$(
        for p in $CORPUS_PROGS; do
            manifest_lookup "$p" || { echo "corpus.contract: $p 不在 cases.manifest" >&2; exit 2; }
        done
    ) || exit 2
else
    corpus_rows=$(manifest_rows "smoke,full") || exit 2
fi
while IFS='|' read -r name _tier tmo mode envv needs args xfail_spec _groups; do
    [ -n "$name" ] || continue
    argv=()
    [ -n "$args" ] && parse_args "$args" argv
    code=0
    if [ "$mode" = diff ]; then
        # 项目三维对拍：冷跑（建 cache + 仅断言 exit 0；冷跑 stderr 含依赖构建
        # 告警噪音，属构建事件非程序行为）→ warm 复跑（cargo 静默，stderr 纯程序
        # 输出）→ native 基线。判绿 = warm 三维 == native 三维 + warm stdout ==
        # cold stdout（防缓存回放旧语义）。
        MIRVM_CORPUS_NO_PURGE=1 corpus_run "$TMP" "$name" "$tmo" "$envv" "$needs" ${argv[@]+"${argv[@]}"} || code=$?
        if [ "$code" -eq 0 ]; then
            cp "$TMP/$name.out" "$TMP/$name.cold.out"
            warm_code=0
            corpus_run "$TMP" "$name" "$tmo" "$envv" "$needs" ${argv[@]+"${argv[@]}"} || warm_code=$?
            if [ "$warm_code" -ne 0 ] \
                || ! diff -q "$TMP/$name.cold.out" "$TMP/$name.out" >/dev/null; then
                bad "c_$name（L2 warm 复跑不一致 cold=$code warm=$warm_code）"
                continue
            fi
            proj="corpus/projects/$name"
            ncode=0
            # 两侧 cwd 统一 = 项目目录（mirvm 项目模式 guest cwd 语义 = cd proj &&
            # cargo run）；夹具路径经 {ROOT} 绝对化，与 cwd 无关。--cap-lints
            # 让上游告警静默（stderr 三维对拍只承载程序自身输出，不含编译噪音）。
            # RUSTC 必须显式钉：rustup 代理按【每次调用的 cwd】解析——registry
            # 依赖的编译 cwd 在仓外（~/.cargo/registry），会落到 rustup
            # default（stable）造成仓内 nightly/仓外 stable 混合工具链
            # （E0514 实锤，换新机 default 漂移后冷重建必现）。
            (cd "$proj" && CARGO_TARGET_DIR="${MIRVM_HOME:-$HOME/.mirvm}/target/native" \
                RUSTFLAGS="--cap-lints allow" \
                RUSTC="$RUSTC" \
                timeout "$tmo" "$CARGO" run -q --locked -- ${argv[@]+"${argv[@]}"} \
                >"$TMP/$name.native.out" 2>"$TMP/$name.native.err") || ncode=$?
            if [ "$ncode" -ne 0 ]; then
                bad "c_$name（native 基线 exit=$ncode，双方同败不是 PASS）"
                tail -5 "$TMP/$name.native.err"
            elif diff -q "$TMP/$name.native.out" "$TMP/$name.out" >/dev/null \
                && diff -q "$TMP/$name.native.err" "$TMP/$name.err" >/dev/null; then
                ok "c_$name（三维对拍 == native）"
            else
                bad "c_$name（与 native 三维不一致）"
                diff "$TMP/$name.native.out" "$TMP/$name.out" | head -5
                diff "$TMP/$name.native.err" "$TMP/$name.err" | head -5
            fi
        elif [ "$code" -eq 77 ]; then
            skip "c_$name（needs 缺席：$needs）"
        else
            bad "c_$name（mirvm exit=$code）: $(tail -1 "$TMP/$name.err" | head -c 100)"
        fi
        continue
    fi
    corpus_run "$TMP" "$name" "$tmo" "$envv" "$needs" ${argv[@]+"${argv[@]}"} || code=$?
    stdout=$(cat "$TMP/$name.out" 2>/dev/null)
    if [ "$code" -eq 77 ]; then
        skip "c_$name（needs 缺席：$needs）"
    elif [ "$code" -eq 2 ]; then
        bad "c_$name（manifest 有登记但无 driver 文件）"
    elif [ -n "$xfail_spec" ]; then
        record_expected_failure "c_$name" "$code" "$xfail_spec" "$TMP/$name.err"
    elif [ "$code" -ne 0 ]; then
        bad "c_$name (exit=$code): $(tail -1 "$TMP/$name.err" | head -c 100)"
    elif [[ "$mode" == oracle:* ]]; then
        oname=${mode#oracle:}
        oracle=$(cat "tests/fixtures/oracles/$oname.txt")
        if [ "$stdout" = "$oracle" ]; then
            ok "c_$name"
        else
            bad "c_$name oracle 不一致（见 tests/fixtures/oracles/$oname.txt）"
        fi
    else
        ok "c_$name"
    fi
done <<< "$corpus_rows"
section_end
target_budget_check
cache_snapshot "gate 收尾后"
print_slowest 10
print_section_report
suite_summary corpus.contract
