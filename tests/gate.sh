#!/usr/bin/env bash
# tests/gate.sh —— 战役收尾全量门禁（2026-07-23 起取代 m4_gate5 + m5_gate6）。
#
# 段序：
#   ① corpus 全量防线（tests/corpus.manifest 的 smoke+full 层；判绿口径按 mode 列：
#     exit / oracle:<名> / diff 三维对拍；xfail= 列锁欠账原因，XPASS 响亮）
#   ② diff_cargo 5/5（cargo 形态差分）
#   ③ 性能与资源（tests/perf.sh：时间硬门 + 空间/cache 计量；SKIP_PERF=1 只跳时序门）
#   ④ 全量回归（diff.sh 默认 / 底座旁路 / 逢调即编 JIT SYNC / JIT-off 四态）
#   ⑤ a2_deps_image 全链路冒烟
#   ⑥ x86 特性探针（tests/probes.sh；SKIP 不得冒充 PASS，x86_vectors 双子能力分别记账）
#   ⑦ 运行时语义门（tests/runtime_gates.sh：纯函数/值内存/unwind/线程/TSan）
#   ⑧ JIT 助手频度统计冒烟（原 m5_gate6 增量；MIRVM_JIT_STATS dump 在 + c2i 桶非零）
#   （m5_gate6 的两条文档 grep 时点检查已化石退役——落档纪律由评审承担，不由门 grep）
#
# 用法：bash tests/gate.sh                 # 全量（cargo compat 轨：corpus ① 走 cargo 三阶段）
#       MIRVM_DEPS=self bash tests/gate.sh # D15 P3 双轨轴：corpus ① 全量走零 cargo
#                                          #   自有调度（② diff_cargo 恒为 cargo compat 轨，
#                                          #   不受本开关影响——cargo 腿覆盖不缺席）
#       SKIP_TSAN=1 bash tests/gate.sh     # 跳过 TSan（CI 已单列 spike4 时用）
#       SKIP_PERF=1 bash tests/gate.sh     # 跳过时序硬门，语义门照常
#       CORPUS_PROGS="a b c" bash tests/gate.sh  # 只跑指定 corpus 条目（gate_truth/调试用）
# 磁盘纪律：每驱动后清 deps/ir（MIRVM_GATE_KEEP_CACHE=1 旁路）；
#   MIRVM_DISK_MIN_GB（默认 8G）见底自动升级清理仍不足则响亮中止；
#   target 超 MIRVM_TARGET_BUDGET_GB（默认 24G）自动 purge --target。
set -u
cd "$(dirname "$0")/.."
MIRVM=${MIRVM:-$(pwd)/target/release/mirvm}
CARGO=${CARGO:-$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo}
RUSTC=${RUSTC:-$(dirname "$CARGO")/rustc}
. tests/lib.sh
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

# ---- ① corpus：manifest 全量防线 ----
echo "== corpus =="
section_start "corpus(smoke+full)"
if [ -n "${CORPUS_PROGS:-}" ]; then
    corpus_rows=$(
        for p in $CORPUS_PROGS; do
            manifest_lookup "$p" || { echo "gate: CORPUS_PROGS 条目 $p 不在 tests/corpus.manifest" >&2; exit 2; }
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
        xfail_code=${xfail_spec%%:*} xfail_pat=${xfail_spec#*:}
        if [ "$code" -eq 0 ]; then
            bad "c_$name XPASS（已转绿：请摘 manifest 的 xfail= 并转正）"
        elif [ "$code" -eq "$xfail_code" ] && grep -Eq "$xfail_pat" "$TMP/$name.err"; then
            red "c_$name（$xfail_pat）"
        else
            bad "c_$name（预期 xfail $xfail_code/'$xfail_pat'，实 exit=$code）: $(tail -1 "$TMP/$name.err" | head -c 100)"
        fi
    elif [ "$code" -ne 0 ]; then
        # D15 P5 边界单列（corpus_deps_pair / deps audit 同款纪律）：self 轨
        # 遇「归 P5」响亮拒绝是设计好的不闭合面（git 源/alt registry 等，
        # 设计档 §5 P5）——单列 p5 不算失败，不冒充闭合
        if [ "${MIRVM_DEPS:-}" = "self" ] && grep -q '归 P5' "$TMP/$name.err"; then
            p5 "c_$name（P5 边界响亮拒绝在案）"
        else
            bad "c_$name (exit=$code): $(tail -1 "$TMP/$name.err" | head -c 100)"
        fi
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

# ---- ② diff_cargo：五个 cargo 形态均与 native 一致 ----
echo "== diff_cargo =="
section_start "diff_cargo"
if MIRVM="$MIRVM" bash tests/diff_cargo.sh >"$TMP/diff_cargo.out" 2>&1 \
    && grep -q "PASS ecosystem" "$TMP/diff_cargo.out" \
    && grep -q "PASS ffi_zlib" "$TMP/diff_cargo.out" \
    && grep -q "PASS ripgrep_regex" "$TMP/diff_cargo.out" \
    && grep -q "PASS warning_return" "$TMP/diff_cargo.out" \
    && grep -q "PASS project" "$TMP/diff_cargo.out" \
    && grep -q "== 5 passed, 0 expected-red, 0 failed ==" "$TMP/diff_cargo.out"; then
    ok "diff_cargo 5/5"
else
    bad "diff_cargo"
    grep -E "PASS|XFAIL|XPASS|FAIL" "$TMP/diff_cargo.out"
fi
section_end

# ---- ③ 性能与资源（perf.sh；SKIP_PERF 只跳时序门）----
echo "== 性能与资源 =="
section_start "perf"
if [ -n "${SKIP_PERF:-}" ]; then
    skip "性能上限（SKIP_PERF=1；语义 gate 仍照常执行）"
    MIRVM="$MIRVM" bash tests/perf.sh --metrics-only || bad "perf 资源计量"
else
    if MIRVM="$MIRVM" bash tests/perf.sh; then
        ok "perf（时间硬门 + 资源计量）"
    else
        bad "perf"
    fi
fi
section_end

# ---- ④ 全量回归（diff.sh 四态）----
echo "== 全量回归 =="
section_start "diff 四态"
if MIRVM="$MIRVM" bash tests/diff.sh >"$TMP/diff.out" 2>&1 \
    && grep -Eq "== [0-9]+ passed, 0 failed ==" "$TMP/diff.out"; then
    diff_summary=$(grep -Eo '[0-9]+ passed, 0 failed' "$TMP/diff.out" | tail -1)
    ok "diff.sh $diff_summary"
else
    bad "diff.sh 回归"
fi
# S4 底座旁路冒烟：MIRVM_NO_BASE_IMAGE 逃生门必须始终可用（单例即够：两路径共享
# 全部降低代码，只差底座查找）。
if MIRVM_NO_BASE_IMAGE=1 MIRVM_NO_IR_CACHE=1 ONLY=fib MIRVM="$MIRVM" \
    bash tests/diff.sh >"$TMP/diff-nobase.out" 2>&1 \
    && grep -Eq "== [0-9]+ passed, 0 failed ==" "$TMP/diff-nobase.out"; then
    ok "diff.sh 底座旁路冒烟（ONLY=fib）"
else
    bad "diff.sh 底座旁路冒烟"
fi
# JIT 双跑：①逢调即编全量——阈值=1 + MIRVM_JIT_SYNC 同步发布（可准入函数首调即
# 同步编译并真跑机器码，编译失败响亮 RED）；与 native 全量差分 = 翻译器误编译的
# 第一显形处。②JIT-off 冒烟——纯解释逃生门不被施工弄坏。
if MIRVM_JIT_SYNC=1 MIRVM_JIT_THRESHOLD=1 MIRVM="$MIRVM" bash tests/diff.sh >"$TMP/diff-jit1.out" 2>&1 \
    && grep -Eq "== [0-9]+ passed, 0 failed ==" "$TMP/diff-jit1.out"; then
    jit_summary=$(grep -Eo '[0-9]+ passed, 0 failed' "$TMP/diff-jit1.out" | tail -1)
    ok "diff.sh 逢调即编（阈值=1+SYNC 同步发布）$jit_summary"
else
    bad "diff.sh 逢调即编（阈值=1+SYNC）回归"
fi
if MIRVM_JIT=off MIRVM_NO_IR_CACHE=1 ONLY=fib MIRVM="$MIRVM" \
    bash tests/diff.sh >"$TMP/diff-jitoff.out" 2>&1 \
    && grep -Eq "== [0-9]+ passed, 0 failed ==" "$TMP/diff-jitoff.out"; then
    ok "diff.sh JIT-off 冒烟（ONLY=fib）"
else
    bad "diff.sh JIT-off 冒烟"
fi
section_end

# ---- ⑤ A2 deps-image 全链路冒烟 ----
echo "== a2_deps_image =="
section_start "a2_deps_image"
if MIRVM="$MIRVM" bash tests/a2_deps_image.sh >"$TMP/a2.out" 2>&1 \
    && grep -Eq "^PASS a2_deps_image$" "$TMP/a2.out"; then
    ok "a2_deps_image（冷写/热读/编辑/S3′c）"
else
    bad "a2_deps_image"
    tail -5 "$TMP/a2.out"
fi
section_end

# ---- ⑥ x86 特性探针 ----
echo "== x86 特性探针 =="
section_start "probes"
probe_code=0
MIRVM="$MIRVM" bash tests/probes.sh >"$TMP/probes.out" 2>&1 || probe_code=$?
if [ "$probe_code" -ne 0 ]; then
    bad "probes"
    tail -5 "$TMP/probes.out"
else
    # 每个探针的 PASS/SKIP 状态必须唯一；SKIP 独立记账不得冒充 PASS。
    for probe in addcarry xgetbv simd_insert simd_shift vzeroupper; do
        status_count=$(grep -Ec "^(PASS|SKIP) m51_$probe(:|$)" "$TMP/probes.out")
        if [ "$status_count" -ne 1 ]; then
            bad "m51_$probe（PASS/SKIP 状态不唯一）"
            tail -5 "$TMP/probes.out"
        elif grep -Eq "^SKIP m51_$probe(:|$)" "$TMP/probes.out"; then
            status_line=$(grep -E "^SKIP m51_$probe(:|$)" "$TMP/probes.out")
            skip "${status_line#SKIP }"
        else
            ok "m51_$probe"
        fi
    done
    # 同一 vector fixture 含 SSSE3 pshufb 与 SHA-NI 两个独立 CPU 能力；
    # 宿主可能只支持其一，必须分别 PASS/SKIP，不得以总 PASS 掩盖 SHA 未执行。
    for feature in pshufb sha; do
        status_count=$(grep -Ec "^(PASS|SKIP) m51_x86_vectors/$feature(:|$)" "$TMP/probes.out")
        if [ "$status_count" -ne 1 ]; then
            bad "m51_x86_vectors/$feature（PASS/SKIP 状态不唯一）"
        elif grep -Eq "^SKIP m51_x86_vectors/$feature(:|$)" "$TMP/probes.out"; then
            status_line=$(grep -E "^SKIP m51_x86_vectors/$feature(:|$)" "$TMP/probes.out")
            skip "${status_line#SKIP }"
        else
            ok "m51_x86_vectors/$feature"
        fi
    done
fi
section_end

# ---- ⑦ 运行时语义门 ----
echo "== 运行时语义门 =="
section_start "runtime_gates"
rg_code=0
MIRVM="$MIRVM" bash tests/runtime_gates.sh >"$TMP/rg.out" 2>&1 || rg_code=$?
if [ "$rg_code" -ne 0 ]; then
    bad "runtime_gates"
    tail -5 "$TMP/rg.out"
else
    tsan_status_count=$(grep -Ec '^(PASS|SKIP) TSan' "$TMP/rg.out")
    if [ "$tsan_status_count" -ne 1 ]; then
        bad "runtime_gates（TSan PASS/SKIP 状态不唯一）"
        tail -5 "$TMP/rg.out"
    elif grep -q '^SKIP TSan' "$TMP/rg.out"; then
        tsan_status=$(grep '^SKIP TSan' "$TMP/rg.out")
        skip "runtime_gates（纯函数/值内存/unwind/线程子门通过；${tsan_status#SKIP }）"
    else
        ok "runtime_gates（纯函数/值内存/unwind/线程/TSan）"
    fi
fi
section_end

# ---- ⑧ JIT 助手频度统计冒烟（原 m5_gate6 增量）----
echo "== JIT 统计冒烟 =="
section_start "jit-stats"
MIRVM_JIT_STATS=1 "$MIRVM" run demo/jit_unwind_probe.rs >"$TMP/uw.out" 2>"$TMP/uw.err"
if grep -q "^mirvm-jit-stats:" "$TMP/uw.err" && grep -q "c2i=[1-9]" "$TMP/uw.err"; then
    ok "JIT 助手频度统计冒烟（atexit dump + 桶计数非零）"
else
    bad "JIT 助手频度统计（见下）"
    tail -3 "$TMP/uw.err"
fi
section_end

# ---- 收尾：预算闸 + 计量总表 ----
target_budget_check
cache_snapshot "gate 收尾后"
print_slowest 10
print_section_report
echo "---"
if [ "$p5" -gt 0 ]; then
    echo "gate: $pass pass, $xfail expected-red, $skip_count skip, $p5 p5, $fail fail"
else
    echo "gate: $pass pass, $xfail expected-red, $skip_count skip, $fail fail"
fi
[ $fail -eq 0 ]
