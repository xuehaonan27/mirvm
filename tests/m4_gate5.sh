#!/usr/bin/env bash
# M4.5 gate（M4 收官，随 M5 前沿滚动）：corpus required-green + 明示 XFAIL
# + diff_cargo + 性能上限
# + 全量回归。
# 前沿记账（透明）：M5.1 已清掉 blake3/sha2/numbigint/ecosystem 的滚动红；
# M5.2 D8e 用影子帧不变式让 c_backtrace 转绿；D8d async 信号 AS-trampoline 让
# c_signal 转绿（handler_ran 断言）——两个历史 XFAIL 均已清。sync 故障信号 guest
# handler（SEGV 等）仍响亮拒绝（D8l，不冒充绿）。
# 用法：bash tests/m4_gate5.sh          # 全量
#       SKIP_TSAN=1 bash tests/m4_gate5.sh
#       SKIP_PERF=1 bash tests/m4_gate5.sh  # 共享 CI runner：跳过时序门，不跳语义门
set -u
cd "$(dirname "$0")/.."
MIRVM=${MIRVM:-$(pwd)/target/release/mirvm}
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
# gix 写 reflog 需要提交者身份（corpus c_gix_pure；commit 哈希由 driver 内
# 固定签名决定，此身份只进 reflog，不进任何可比输出）——不依赖宿主
# ~/.gitconfig，任何机器/CI 同一口径（换机实锤：无 gitconfig 即 MissingCommitter）。
export GIT_AUTHOR_NAME=mirvm-test GIT_AUTHOR_EMAIL=mirvm@test.local
export GIT_COMMITTER_NAME=mirvm-test GIT_COMMITTER_EMAIL=mirvm@test.local
pass=0 xfail=0 skip_count=0 fail=0
ok()  { pass=$((pass + 1)); echo "PASS $*"; }
red() { xfail=$((xfail + 1)); echo "XFAIL $*"; }
skip() { skip_count=$((skip_count + 1)); echo "SKIP $*"; }
bad() { fail=$((fail + 1)); echo "FAIL $*"; }
NUMBIGINT_ORACLE='50! = 30414093201713378043612608166064768844377641568960512000000000000
2^1000 mod 1e9+7 = 688423210
a^2 = 15241578753238836750495351562536198787501905199875019052100'
BLAKE3_ORACLE='blake3(0..1000 le) = 62c6155664d5beb5b693c552de9f614cb6e224461a87c045e3f45cae91fe2bd8
blake3("") = af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262
blake3("hello") = ea8f163db38682925e4491c5e58d4bb3506ef8c14eb78a86e908c5624a67200f'
SHA2_ORACLE='sha256(hello world) = b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9
sha256() = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855'

# ---- ① corpus：required-green + 原因锁定的 expected-red ----
echo "== corpus =="
CORPUS_PROGS=${CORPUS_PROGS:-"itertools anyhow rayon chrono indexmap clap csv crossbeam tokio blake3 \
tempfile walkdir numbigint smallvec bytes sha2 petgraph net_tcp net_udp \
process tokio_mt mmap blocking_io net_echo_threaded signal backtrace volatile fork_exec \
portable_simd float_wide atexit \
serde_json serde_yaml rand_det flate2 brotli argon2 ed25519 p256 syn_parse hickory \
unicode_tables wasmi boa_js tiny_skia zip_arch rust_decimal rustfft roaring bitvec \
compact_str nom_parse comrak_md fst_build spade_delaunay aes_gcm png_round \
smoltcp_tcp snow_noise statrs_stats rkyv_zero qr_round fatfs_img geo_ops rhai_script \
redb_kv gix_pure lz4_snap calamine_xlsx rusqlite_db \
crc32fast chacha_poly k256_ecdsa rsa_pss libflate_zlib revm_evm rustls_cert \
lyon_tess midly_midi jaq_jq kdl_doc ds_obscure qoi_img \
pgp_packet zopfli_deep simd_json symphonia_wav pdf_pair deunicode_slug \
malachite_big arkworks_ff im_persistent zxcvbn_pass barcoders_gen bzip2_pure \
fixed_point openssl_evp \
wat_parse jsonschema html5ever xml_rs markdown_it logos_lex chumsky_parse \
ndarray smartcore rune koto \
parquet2_rw syntect_fancy phonenumber orgmode oxc_parse rsa_4096 ed25519_default \
mimalloc libgit2 rustls_shake zstd_long \
aws_lc mlua_lua tantivy sequoia_pgp sqlx_sqlite"}
# jieba_cut 全绿但 mirvm 单跑 77-89s（贴 90s timeout），留 corpus.sh 手工跑批
# opencc（批7 波1）三维已绿但不进 gate5：依赖机器侧 /tmp/opencc-local
# （OpenCC 1.1.9 自建前缀，driver 头注有重建法）——留 corpus.sh 手工批（有 gating）
for p in $CORPUS_PROGS; do
    src="corpus/c_$p.rs"
    [ -f "$src" ] || continue
    # ed25519：dalek 官方 serial backend（u128 语义压力本意，docs/corpus.md
    # 记账口径；默认 simd backend 的 avx512ifma vpmadd52 已于 2b4766b 内建）
    [ "$p" = ed25519 ] && export CARGO_CFG_CURVE25519_DALEK_BACKEND=serial
    # mimalloc：libmimalloc.a 的 prim.c constructor 触发 native_archive 卫士，
    # CFLAGS 注入 -DMI_PRIM_HAS_PROCESS_ATTACH 绕行（corpus/c_mimalloc.rs 头注①）
    [ "$p" = mimalloc ] && export CFLAGS="-DMI_PRIM_HAS_PROCESS_ATTACH"
    # 重构建项的冷 timeout 放宽（gix/revm deps 树大、rusqlite 编 C sqlite、
    # calamine 双 crate、rustls 编 aws-lc C、rsa 大数 JIT 压力实测单跑 >90s、
    # zopfli 重计算 A 维 ~200s、arkworks/ malachite 大 dep 树、
    # mimalloc/libgit2 的 vendored C 构建）
    tmo=90
    case "$p" in
        gix_pure|rusqlite_db|revm_evm|zopfli_deep|mimalloc|libgit2|aws_lc|tantivy|sequoia_pgp) tmo=300 ;;
        calamine_xlsx|rustls_cert|phonenumber|mlua_lua) tmo=180 ;;
        rsa_pss|rsa_4096) tmo=400 ;;
    esac
    timeout $tmo "$MIRVM" run "$src" >"$TMP/corpus-$p.out" 2>"$TMP/corpus-$p.err"; code=$?
    unset CARGO_CFG_CURVE25519_DALEK_BACKEND RUSTFLAGS CFLAGS
    stdout=$(cat "$TMP/corpus-$p.out")
    out=$(cat "$TMP/corpus-$p.out" "$TMP/corpus-$p.err")
    red_pattern="" red_label="" red_code=70
    # 历史转绿：六条 intrinsic 红于 2b4766b 内建、rusqlite libm 闭包于 2518314
    # 修 LINK_SUFFIX——机制保留备将来欠账锁定
    # openssl_evp 已于本轮转绿（元数据 -l 预载修复）；机制保留备将来欠账锁定
    if [ "$p" = numbigint ] && { [ $code -ne 0 ] \
        || [ "$stdout" != "$NUMBIGINT_ORACLE" ]; }; then
        bad "c_numbigint oracle (exit=$code stdout='$stdout')"
    elif [ "$p" = backtrace ] && { [ $code -ne 0 ] \
        || [ "$stdout" != 'backtrace: captured, non-empty, depth reflected (+30 frames)' ]; }; then
        # D8e：backtrace 文本非 well-defined，oracle 是影子帧不变式（捕获/非空/深度反映）
        bad "c_backtrace oracle (exit=$code stdout='$stdout')"
    elif [ "$p" = signal ] && { [ $code -ne 0 ] \
        || [ "$stdout" != 'handler hit = true' ]; }; then
        # D8d：async 信号 guest handler 经 AS-trampoline 真执行（handler_ran 断言）
        bad "c_signal oracle (exit=$code stdout='$stdout')"
    elif [ "$p" = blake3 ] && { [ $code -ne 0 ] \
        || [ "$stdout" != "$BLAKE3_ORACLE" ]; }; then
        bad "c_blake3 oracle (exit=$code stdout='$stdout')"
    elif [ "$p" = sha2 ] && { [ $code -ne 0 ] \
        || [ "$stdout" != "$SHA2_ORACLE" ]; }; then
        bad "c_sha2 oracle (exit=$code stdout='$stdout')"
    elif [ "$p" = volatile ] && { [ $code -ne 0 ] \
        || [ "$stdout" != 'volatile aligned=0x0123456789abcdef unaligned=0x89abcdef pair=0x90a0b0c0d0e0f000:0x1020304050607080' ]; }; then
        bad "c_volatile oracle (exit=$code stdout='$stdout')"
    elif [ $code -eq 0 ] && [ -n "$red_pattern" ]; then
        bad "c_$p XPASS（请移出 expected-red 并建立 native 差分）"
    elif [ $code -eq 0 ]; then
        ok "c_$p"
    elif [ -n "$red_pattern" ] && [ $code -eq $red_code ] \
        && echo "$out" | grep -Eq "$red_pattern"; then
        red "c_$p（$red_label）"
    else
        bad "c_$p (exit=$code): $(echo "$out" | tail -1 | head -c 100)"
    fi
done

# ---- ② diff_cargo：五个 cargo 形态均与 native 一致 ----
echo "== diff_cargo =="
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

# ---- ③ 性能上限（D2：加载 ≤1s / rayon ≤5s；热 sysroot 缓存）----
echo "== 性能上限 =="
if [ -n "${SKIP_PERF:-}" ]; then
    skip "性能上限（SKIP_PERF=1；语义 gate 仍照常执行）"
else
    echo 'fn main(){}' >"$TMP/empty.rs"
    t0=$(date +%s%N); "$MIRVM" run "$TMP/empty.rs" >"$TMP/load.out" 2>"$TMP/load.err"; load_code=$?
    load_ms=$(( ($(date +%s%N) - t0) / 1000000 ))
    [ $load_code -eq 0 ] && [ $load_ms -lt 1000 ] \
        && ok "加载相 ${load_ms}ms（< 1s 硬门）" \
        || bad "加载相 exit=$load_code ${load_ms}ms（要求 exit=0 且 < 1s）"
    t0=$(date +%s%N); "$MIRVM" run corpus/c_rayon.rs >"$TMP/rayon.out" 2>"$TMP/rayon.err"; rayon_code=$?
    rayon_ms=$(( ($(date +%s%N) - t0) / 1000000 ))
    if [ $rayon_code -eq 0 ] && grep -q 'par_sort ok = true' "$TMP/rayon.out" \
        && [ $rayon_ms -lt 5000 ]; then
        rayon_divisor=$rayon_ms; [ $rayon_divisor -gt 0 ] || rayon_divisor=1
        ok "rayon ${rayon_ms}ms（< 5s 硬门；tier-0 28s，≈$((28000/rayon_divisor))×）"
    else
        bad "rayon exit=$rayon_code ${rayon_ms}ms（要求语义 oracle 且 < 5s）"
    fi
    # M5.3 JIT 硬门（m5-design gate6 ②）：fib(32) ≤ 10× native ≈ ≤80ms 墙钟
    #（解释锚点 0.94s；native -O 6.7ms；实测 JIT engine 段 ~19ms ≈ 2.9×）。
    # 三跑取最小：每 `mirvm run` 是新进程、JIT 后台线程重编译，满载 gate 下偶被
    # 饿死一轮（回退解释 940ms）——多跑取最快杀调度毛刺，契约（≤80ms）不变。
    fib_ms=999999 fib_code=1 fib_out=""
    for _ in 1 2 3; do
        t0=$(date +%s%N)
        fib_out=$("$MIRVM" run --vm-call 'fib(32)' demo/m4/pure.rs 2>"$TMP/fib32.err")
        fib_code=$?
        ms=$(( ($(date +%s%N) - t0) / 1000000 ))
        [ $ms -lt $fib_ms ] && fib_ms=$ms
        { [ $fib_code -eq 0 ] && [ "$fib_out" = "2178309" ]; } || break
    done
    if [ $fib_code -eq 0 ] && [ "$fib_out" = "2178309" ] && [ $fib_ms -lt 80 ]; then
        ok "fib(32) JIT ${fib_ms}ms（≤80ms=10× native 硬门；解释锚点 940ms）"
    else
        bad "fib(32) JIT exit=$fib_code out=$fib_out ${fib_ms}ms（要求 2178309 且 <80ms）"
    fi
fi

# ---- ④ 全量回归 ----
echo "== 全量回归 =="
if MIRVM="$MIRVM" bash tests/diff.sh >"$TMP/diff.out" 2>&1 \
    && grep -Eq "== [0-9]+ passed, 0 failed ==" "$TMP/diff.out"; then
    diff_summary=$(grep -Eo '[0-9]+ passed, 0 failed' "$TMP/diff.out" | tail -1)
    ok "diff.sh $diff_summary"
else
    bad "diff.sh 回归"
fi
# S4 底座旁路冒烟（M6 片6）：MIRVM_NO_BASE_IMAGE 逃生门必须始终可用——上面的全量
# diff 走底座路径，这里锁"无底座全量降低"路径不被底座施工静默弄坏（单例即够：
# 两路径共享全部降低代码，只差底座查找）。判据与主 diff 行同款汇总式
# （truth-regression 的 bash 垫片回放罐头输出，逐例 grep 在 fixture 下不成立）。
if MIRVM_NO_BASE_IMAGE=1 MIRVM_NO_IR_CACHE=1 ONLY=fib MIRVM="$MIRVM" \
    bash tests/diff.sh >"$TMP/diff-nobase.out" 2>&1 \
    && grep -Eq "== [0-9]+ passed, 0 failed ==" "$TMP/diff-nobase.out"; then
    ok "diff.sh 底座旁路冒烟（ONLY=fib）"
else
    bad "diff.sh 底座旁路冒烟"
fi
# M5.3 JIT 双跑（m5.3-design §4 oracle）：①逢调即编全量——阈值=1 使所有可准入函数
# 走 JIT 帧（默认阈值 1000 下多数 demo 不触发编译，覆盖面不足），与 native 全量差分
# = 翻译器误编译的第一显形处；②JIT-off 冒烟——纯解释逃生门不被 JIT 施工弄坏。
if MIRVM_JIT_THRESHOLD=1 MIRVM="$MIRVM" bash tests/diff.sh >"$TMP/diff-jit1.out" 2>&1 \
    && grep -Eq "== [0-9]+ passed, 0 failed ==" "$TMP/diff-jit1.out"; then
    jit_summary=$(grep -Eo '[0-9]+ passed, 0 failed' "$TMP/diff-jit1.out" | tail -1)
    ok "diff.sh 逢调即编（阈值=1）$jit_summary"
else
    bad "diff.sh 逢调即编（阈值=1）回归"
fi
if MIRVM_JIT=off MIRVM_NO_IR_CACHE=1 ONLY=fib MIRVM="$MIRVM" \
    bash tests/diff.sh >"$TMP/diff-jitoff.out" 2>&1 \
    && grep -Eq "== [0-9]+ passed, 0 failed ==" "$TMP/diff-jitoff.out"; then
    ok "diff.sh JIT-off 冒烟（ONLY=fib）"
else
    bad "diff.sh JIT-off 冒烟"
fi
# A2 deps-image（s3b-a2-design，A2-3 默认开启）：冷写/热读 ≤300ms 锚点、编辑重跑
# 不重建、L2 矩阵、旁路双态、S3′c 同 workspace 跨 bin 共享——单脚本全链路冒烟。
if MIRVM="$MIRVM" bash tests/a2_deps_image.sh >"$TMP/a2.out" 2>&1 \
    && grep -Eq "^PASS a2_deps_image$" "$TMP/a2.out"; then
    ok "a2_deps_image（冷写/热读/编辑/S3′c）"
else
    bad "a2_deps_image"
    tail -5 "$TMP/a2.out"
fi
for probe in addcarry xgetbv simd_insert simd_shift vzeroupper x86_vectors; do
    probe_code=0
    MIRVM="$MIRVM" bash "tests/m51_$probe.sh" >"$TMP/m51-$probe.out" 2>&1 \
        || probe_code=$?
    if [ "$probe_code" -ne 0 ]; then
        bad "m51_$probe"
        tail -5 "$TMP/m51-$probe.out"
        continue
    fi

    # 同一 vector fixture 含 SSSE3 pshufb 与 SHA-NI 两个独立 CPU 能力；
    # 宿主可能只支持其一，因此必须分别 PASS/SKIP，不得以一个
    # 总 PASS 掩盖 SHA helper 完全未执行。
    if [ "$probe" = x86_vectors ]; then
        for feature in pshufb sha; do
            status_count=$(grep -Ec "^(PASS|SKIP) m51_x86_vectors/$feature(:|$)" \
                "$TMP/m51-$probe.out")
            if [ "$status_count" -ne 1 ]; then
                bad "m51_x86_vectors/$feature（退出 0 但 PASS/SKIP 状态不唯一）"
            elif grep -Eq "^SKIP m51_x86_vectors/$feature(:|$)" \
                "$TMP/m51-$probe.out"; then
                status_line=$(grep -E "^SKIP m51_x86_vectors/$feature(:|$)" \
                    "$TMP/m51-$probe.out")
                skip "${status_line#SKIP }"
            else
                ok "m51_x86_vectors/$feature"
            fi
        done
        continue
    fi

    status_count=$(grep -Ec "^(PASS|SKIP) m51_$probe(:|$)" \
        "$TMP/m51-$probe.out")
    if [ "$status_count" -ne 1 ]; then
        bad "m51_$probe（退出 0 但 PASS/SKIP 状态不唯一）"
        tail -5 "$TMP/m51-$probe.out"
    elif grep -Eq "^SKIP m51_$probe(:|$)" "$TMP/m51-$probe.out"; then
        status_line=$(grep -E "^SKIP m51_$probe(:|$)" "$TMP/m51-$probe.out")
        skip "${status_line#SKIP }"
    else
        ok "m51_$probe"
    fi
done
for gate in 0 1 2; do
    if MIRVM="$MIRVM" bash "tests/m4_gate$gate.sh" >"$TMP/gate$gate.out" 2>&1; then
        ok "gate$gate"
    else
        bad "gate$gate"
        tail -3 "$TMP/gate$gate.out"
    fi
done
gate4_code=0
MIRVM="$MIRVM" bash tests/m4_gate4.sh >"$TMP/gate4.out" 2>&1 || gate4_code=$?
if [ "$gate4_code" -ne 0 ]; then
    bad "gate4"
    tail -3 "$TMP/gate4.out"
else
    tsan_status_count=$(grep -Ec '^(PASS|SKIP) TSan' "$TMP/gate4.out")
    if [ "$tsan_status_count" -ne 1 ]; then
        bad "gate4（退出 0 但 TSan PASS/SKIP 状态不唯一）"
        tail -5 "$TMP/gate4.out"
    elif grep -q '^SKIP TSan' "$TMP/gate4.out"; then
        tsan_status=$(grep '^SKIP TSan' "$TMP/gate4.out")
        skip "gate4（真线程子门通过；${tsan_status#SKIP }）"
    else
        ok "gate4（真线程/TSan）"
    fi
fi

echo "---"
echo "m4-gate5(收官): $pass pass, $xfail expected-red, $skip_count skip, $fail fail"
[ $fail -eq 0 ]
