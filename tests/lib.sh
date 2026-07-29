# shellcheck shell=bash
# tests/lib.sh —— 测试套件共享库（只被 source，不直接执行）。
#
# 提供四组能力：
#   1) PASS/FAIL/SKIP/XFAIL 统一记账（ok/bad/skip/red + summary 尾行）
#   2) corpus.manifest 解析（manifest_rows：唯一真源 → 规范化行）
#   3) corpus driver 执行器（corpus_run：env/needs/timeout/计时/磁盘护栏一体）
#   4) 磁盘与 cache 管理（disk_guard / target_budget_check / cache_snapshot）
#
# 约定：source 本文件的脚本自己 set -u；计数器在 source 时初始化为 0。
# 汇总尾行统一以 "$skip_count skip, $fail fail" 收尾（gate_truth 锁此形状）。

# ---- ① 记账 ----
pass=0 fail=0 skip_count=0 xfail=0 p5=0
ok()   { pass=$((pass + 1)); echo "PASS $*"; }
bad()  { fail=$((fail + 1)); echo "FAIL $*"; }
skip() { skip_count=$((skip_count + 1)); echo "SKIP $*"; }
red()  { xfail=$((xfail + 1)); echo "XFAIL $*"; }
p5()   { p5=$((p5 + 1)); echo "P5 $*"; }

# ---- ② 计时 ----
now_ms() { date +%s%N; }
SECTION_REPORT=()
_section_t0=0 _section_name=""
section_start() { _section_name="$1"; _section_t0=$(now_ms); }
section_end() {
    local ms=$(( ($(now_ms) - _section_t0) / 1000000 ))
    SECTION_REPORT+=("$(printf '%-28s %8dms' "$_section_name" "$ms")")
    echo "[计时] $_section_name ${ms}ms"
}
print_section_report() {
    [ ${#SECTION_REPORT[@]} -eq 0 ] && return 0
    echo "== 分段耗时 =="
    printf '%s\n' "${SECTION_REPORT[@]}"
}

# ---- ③ corpus.manifest 解析 ----
# 行格式（# 起注释，空白分隔）：
#   name  tier(smoke|full|manual)  timeout秒  mode(exit|oracle:<名>|diff)
#         [env=K=V;K=V] [needs=<路径>] [args=a;b;c] [xfail=<code>:<grep 模式>]
#         [group=<组>[,<组>...]]（分组键，如 group=heavy；无 group= 的条目属
#         隐含 light 组——corpus.sh/corpus_deps_pair.sh 的 --group 按它过滤）
# 输出（管道分隔，域内无 |）：
#   name|tier|tmo|mode|env|needs|args|xfail|groups
# 用法：manifest_rows <tiers 逗号|all>            —— 按层过滤
# 非法字段/非法枚举值 → stderr 报错并以非零退出（登记错误必须响亮）。
manifest_rows() {
    local tiers="$1"
    awk -v tiers="$tiers" '
        /^[[:space:]]*(#|$)/ { next }
        {
            name=$1; tier=$2; tmo=$3; mode=$4
            if (tier !~ /^(smoke|full|manual)$/) {
                printf "manifest: %s 非法 tier %s\n", name, tier > "/dev/stderr"; bad=1; next
            }
            if (mode !~ /^(exit|diff|oracle:.+)$/) {
                printf "manifest: %s 非法 mode %s\n", name, mode > "/dev/stderr"; bad=1; next
            }
            if (tmo !~ /^[0-9]+$/) {
                printf "manifest: %s 非法 timeout %s\n", name, tmo > "/dev/stderr"; bad=1; next
            }
            envv=""; needs=""; args=""; xfail=""; groups=""
            for (i = 5; i <= NF; i++) {
                if ($i ~ /^env=/)       envv  = substr($i, 5)
                else if ($i ~ /^needs=/) needs = substr($i, 7)
                else if ($i ~ /^args=/)  args  = substr($i, 6)
                else if ($i ~ /^xfail=/) xfail = substr($i, 7)
                else if ($i ~ /^group=/) groups = substr($i, 7)
                else {
                    printf "manifest: %s 未知字段 %s\n", name, $i > "/dev/stderr"; bad=1
                }
            }
            if (tiers != "all" && index("," tiers ",", "," tier ",") == 0) next
            printf "%s|%s|%s|%s|%s|%s|%s|%s|%s\n", name, tier, tmo, mode, envv, needs, args, xfail, groups
        }
        END { exit bad }
    ' "$(dirname "${BASH_SOURCE[0]}")/corpus.manifest"
}

# manifest_group_rows <group> [tiers 逗号|all] —— 按组过滤（组键在输出第 9 列；
# 无 group= 的条目只在 group=light 时命中）
manifest_group_rows() {
    local group="$1" tiers="${2:-all}"
    manifest_rows "$tiers" | awk -F'|' -v g="$group" '
        { inlist = ("," $9 ",") ~ ("," g ","); if (g == "light") inlist = ($9 == "") || inlist
          if (inlist) print }
    '
}

# manifest_lookup <name> —— 单条查询（手工跑批按名过滤用）；查无此行 → 非零
manifest_lookup() {
    local name="$1" row
    row=$(manifest_rows all | grep -F "|" | awk -F'|' -v n="$name" '$1 == n { print; found=1 } END { exit !found }') \
        || return 1
    printf '%s\n' "$row"
}

# ---- ④ corpus driver 执行器 ----
# corpus_run <outdir> <name> <tmo> <env> <needs> [args...]
# 行为：
#   - driver 定位：corpus/c_<name>.rs（script）或 corpus/projects/<name>/（project，
#     mirvm run <目录>）；两者皆无 → 返回 2（登记错误，调用方记 FAIL）
#   - needs 缺席 → 返回 77（调用方记 SKIP）
#   - env 串 K=V;K=V 逐项 export，跑完 unset
#   - timeout <tmo> 包住 mirvm run；stdout/stderr 落 <outdir>/<name>.{out,err}
#   - 逐驱动磁盘纪律：deps/ir image 跑完即无复读者，默认 purge（MIRVM_GATE_KEEP_CACHE=1 旁路）；
#     每驱动前 disk_guard（可用空间见底自动升级清理，仍不足响亮中止）
#   - 墙钟秒数追加到 ${CORPUS_TIMINGS_FILE:-/dev/null}（"秒 名" 行，供最慢榜）
# 返回：mirvm/timeout 的退出码（needs 缺席 = 77，driver 缺席 = 2）
corpus_run() {
    local outdir="$1" name="$2" tmo="$3" envv="$4" needs="$5"
    shift 5
    local mirvm=${MIRVM:-$(pwd)/target/release/mirvm}
    local src="corpus/c_$name.rs" proj="corpus/projects/$name"
    local target=""
    if [ -f "$src" ]; then
        target="$src"
    elif [ -d "$proj" ]; then
        target="$proj"
    else
        echo "corpus_run: $name 在 manifest 有登记但 corpus/ 下无 driver" >&2
        return 2
    fi
    if [ -n "$needs" ] && [ ! -e "$needs" ]; then
        return 77
    fi

    disk_guard

    local -a env_names=()
    if [ -n "$envv" ]; then
        local pair k
        local oldifs=$IFS; IFS=';'
        for pair in $envv; do
            [ -n "$pair" ] || continue
            k=${pair%%=*}
            # %20 解码为空格（manifest 字段空白分隔，值内空格须编码）
            pair=${pair//%20/ }
            export "$pair"
            env_names+=("$k")
        done
        IFS=$oldifs
    fi

    local t0 code
    t0=$(date +%s)
    if [ $# -gt 0 ]; then
        timeout "$tmo" "$mirvm" run "$target" -- "$@" \
            >"$outdir/$name.out" 2>"$outdir/$name.err"
    else
        timeout "$tmo" "$mirvm" run "$target" \
            >"$outdir/$name.out" 2>"$outdir/$name.err"
    fi
    code=$?
    local dur=$(( $(date +%s) - t0 ))
    printf '%s %s\n' "$dur" "$name" >>"${CORPUS_TIMINGS_FILE:-/dev/null}" 2>/dev/null || true

    local k
    for k in "${env_names[@]}"; do unset "$k"; done
    # 常规环境噪音也清掉（与旧 gate5 同款防御）
    unset CARGO_CFG_CURVE25519_DALEK_BACKEND RUSTFLAGS CFLAGS 2>/dev/null || true

    if [ -z "${MIRVM_GATE_KEEP_CACHE:-}" ]; then
        "$mirvm" cache purge --deps --ir >/dev/null 2>&1 || true
    fi
    return "$code"
}

# print_slowest [N] —— 最慢榜（corpus 跑批末尾调用）
print_slowest() {
    local n=${1:-10} f=${CORPUS_TIMINGS_FILE:-}
    [ -n "$f" ] && [ -s "$f" ] || return 0
    echo "== 最慢 $n 个 corpus 驱动（秒）=="
    sort -rn "$f" | head -"$n" | awk '{ printf "  %6ds  %s\n", $1, $2 }'
}

# parse_args <分号串> <数组名>：拆分 manifest args 串进数组；
# {ROOT} 占位替换为仓库根绝对路径——项目对拍两侧 cwd 不同（mirvm 项目模式
# guest cwd=项目目录，native cargo run cwd=项目目录或调用处），相对路径无法
# 同指一文件；绝对路径与 cwd 无关，两边解析恒一致。
parse_args() {
    local _root; _root=$(pwd)
    local -n _out=$2
    local _a _oldifs=$IFS; IFS=';'
    for _a in $1; do
        [ -n "$_a" ] && _out+=("${_a//\{ROOT\}/$_root}")
    done
    IFS=$_oldifs
}

# ---- ⑤ 磁盘与 cache 管理 ----
_mirvm_home() { printf '%s' "${MIRVM_HOME:-$HOME/.mirvm}"; }

_avail_gb() {  # <path> → 可用 GiB（整数）
    df -Pk "$1" 2>/dev/null | awk 'NR==2{print int($4/1024/1024)}'
}

# disk_guard：可用空间低于 MIRVM_DISK_MIN_GB（默认 8G）时逐级升级清理：
#   ① 清陈代+deps/ir（下次冷构建代价小） ② --target（共享依赖存储，代价大）
#   ③ 仍不足 → 响亮 exit 3（不许把磁盘打爆还继续跑）
disk_guard() {
    local min_gb=${MIRVM_DISK_MIN_GB:-8} home_dir
    home_dir=$(_mirvm_home)
    [ -d "$home_dir" ] || return 0
    local mirvm=${MIRVM:-$(pwd)/target/release/mirvm}
    local avail
    avail=$(_avail_gb "$home_dir")
    [ -n "$avail" ] || return 0
    [ "$avail" -ge "$min_gb" ] && return 0
    echo "disk-guard: 可用 ${avail}G < 下限 ${min_gb}G，清陈代+deps/ir"
    "$mirvm" cache purge >/dev/null 2>&1 || true
    "$mirvm" cache purge --deps --ir >/dev/null 2>&1 || true
    avail=$(_avail_gb "$home_dir")
    if [ "$avail" -lt "$min_gb" ]; then
        echo "disk-guard: 仍 ${avail}G，追加 --target（共享依赖存储重建代价大，迫不得已）"
        "$mirvm" cache purge --target >/dev/null 2>&1 || true
        avail=$(_avail_gb "$home_dir")
    fi
    if [ "$avail" -lt "$min_gb" ]; then
        echo "disk-guard: 两级清理后仍 ${avail}G < ${min_gb}G，响亮中止（先手工腾磁盘）" >&2
        exit 3
    fi
}

# target_budget_check：统一 target dir 超 MIRVM_TARGET_BUDGET_GB（默认 24G）→
# 自动 purge --target 并报告。这是"防 cache 把盘养爆"的预算闸；
# 代价是下一轮全冷重建，所以只在超预算时触发。
target_budget_check() {
    local budget=${MIRVM_TARGET_BUDGET_GB:-24} home_dir t mb
    home_dir=$(_mirvm_home)
    t="$home_dir/target"
    [ -d "$t" ] || return 0
    mb=$(du -sm "$t" 2>/dev/null | cut -f1)
    [ -n "$mb" ] || return 0
    if [ "$mb" -gt $((budget * 1024)) ]; then
        local mirvm=${MIRVM:-$(pwd)/target/release/mirvm}
        echo "target-budget: $t 达 ${mb}MiB > 预算 ${budget}G，执行 cache purge --target"
        "$mirvm" cache purge --target >/dev/null 2>&1 || true
        echo "target-budget: 清理后 $(du -sm "$t" 2>/dev/null | cut -f1)MiB"
    fi
}

# cache_snapshot <标签>：打印 cache 各分部体量与磁盘可用（跑前跑后各一次）
cache_snapshot() {
    local home_dir
    home_dir=$(_mirvm_home)
    [ -d "$home_dir" ] || { echo "[cache $1] $home_dir 不存在"; return 0; }
    echo "[cache $1] 合计 $(du -sm "$home_dir" 2>/dev/null | cut -f1)MiB；磁盘可用 $(_avail_gb "$home_dir")G；分部（MiB）："
    du -sm "$home_dir"/* 2>/dev/null | sort -rn | head -8 | sed "s|$home_dir/||" | awk '{ printf "  %8d  %s\n", $1, $2 }'
}
