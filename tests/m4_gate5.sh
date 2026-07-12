#!/usr/bin/env bash
# M4.5 gate（M4 收官，随 M5 前沿滚动）：corpus 全绿 − 预期红 + diff_cargo + 性能上限
# + 全量回归。
# 前沿记账（透明）：M4.5 时 asm 四项（cpuid/syscall/div 三面孔）记预期红；
# **M5.0 asm-stub 工厂清掉 asm 面**——tempfile 转绿（必须 PASS，锁住工厂不回归），
# blake3/sha2/numbigint 推进到下一层 `llvm.x86.*` intrinsic（M5.1 D7 归宿）记预期红。
# 用法：bash tests/m4_gate5.sh          # 全量
#       SKIP_TSAN=1 bash tests/m4_gate5.sh
set -u
cd "$(dirname "$0")/.."
MIRVM=${MIRVM:-$(pwd)/target/release/mirvm}
pass=0 fail=0
ok()  { pass=$((pass + 1)); echo "PASS $*"; }
bad() { fail=$((fail + 1)); echo "FAIL $*"; }

# 预期红清单（M5.1 归宿）——红且诊断含 llvm.x86 才算 gate 通过；诊断变形即报 FAIL
INTRINSIC_RED="blake3 sha2 numbigint"

# ---- ① corpus 全量：全绿 − intrinsic 三项预期红（tempfile 等 asm 面已清必须绿）----
echo "== corpus =="
CORPUS_PROGS="itertools anyhow rayon chrono indexmap clap csv crossbeam tokio blake3 \
tempfile walkdir numbigint smallvec bytes sha2 petgraph net_tcp net_udp \
process tokio_mt mmap blocking_io net_echo_threaded signal"
for p in $CORPUS_PROGS; do
    src="corpus/c_$p.rs"
    [ -f "$src" ] || continue
    out=$(timeout 90 "$MIRVM" run "$src" 2>&1); code=$?
    is_red=0; for a in $INTRINSIC_RED; do [ "$p" = "$a" ] && is_red=1; done
    if [ $code -eq 0 ]; then
        ok "c_$p"
    elif [ $is_red -eq 1 ] && echo "$out" | grep -q 'llvm\.x86'; then
        ok "c_$p（llvm.x86 intrinsic 预期红，M5.1）"
    else
        bad "c_$p (exit=$code): $(echo "$out" | tail -1 | head -c 100)"
    fi
done

# ---- ② diff_cargo：ffi_zlib + project 绿；ecosystem = llvm.x86 预期红（M5.1）----
echo "== diff_cargo =="
MIRVM="$MIRVM" bash tests/diff_cargo.sh >/tmp/gate5_cargo.out 2>&1
if grep -q "PASS ffi_zlib" /tmp/gate5_cargo.out && grep -q "PASS project" /tmp/gate5_cargo.out; then
    ok "diff_cargo（ffi_zlib + project 绿；ecosystem = llvm.x86 预期红，M5.1）"
else
    bad "diff_cargo"; grep -E "PASS|FAIL" /tmp/gate5_cargo.out
fi

# ---- ③ 性能上限（D2：加载 ≤1s / rayon ≤5s；热 sysroot 缓存）----
echo "== 性能上限 =="
echo 'fn main(){}' >/tmp/gate5_empty.rs
t0=$(date +%s%N); "$MIRVM" run /tmp/gate5_empty.rs >/dev/null 2>&1
load_ms=$(( ($(date +%s%N) - t0) / 1000000 ))
[ $load_ms -lt 1000 ] && ok "加载相 ${load_ms}ms（< 1s 硬门）" || bad "加载相 ${load_ms}ms ≥ 1s"
t0=$(date +%s%N); "$MIRVM" run corpus/c_rayon.rs >/dev/null 2>&1
rayon_ms=$(( ($(date +%s%N) - t0) / 1000000 ))
[ $rayon_ms -lt 5000 ] && ok "rayon ${rayon_ms}ms（< 5s 硬门；tier-0 28s，≈$((28000/rayon_ms))×）" \
    || bad "rayon ${rayon_ms}ms ≥ 5s"

# ---- ④ 全量回归 ----
echo "== 全量回归 =="
MIRVM="$MIRVM" bash tests/diff.sh 2>&1 | grep -q "17 passed, 0 failed" \
    && ok "diff.sh 17/17" || bad "diff.sh 回归"
bash tests/m4_gate2.sh >/dev/null 2>&1 && ok "gate0-2（值/unwind/债务清零）" || bad "gate0-2"
bash tests/m4_gate4.sh ${SKIP_TSAN:+} >/tmp/gate5_g4.out 2>&1 \
    && ok "gate4（真线程/TSan）" || { bad "gate4"; tail -3 /tmp/gate5_g4.out; }

echo "---"
echo "m4-gate5(收官): $pass pass, $fail fail"
[ $fail -eq 0 ]
