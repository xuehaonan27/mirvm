#!/usr/bin/env bash
# corpus 跑批：逐个 mirvm run，记录 exit code / 耗时 / 首个错误。
# 用途是"发现真实 crate 对抽象机/VM 边界的要求"，不是给 tier-0 刷通过率。
set -u
cd "$(dirname "$0")/.."
MIRVM=${MIRVM:-$(pwd)/target/release/mirvm}
OUT=${OUT:-/tmp/corpus-out}
mkdir -p "$OUT"

# 可传程序名跑子集：bash tests/corpus.sh tempfile walkdir ...；不传 = 全量
progs=("$@")
if [ ${#progs[@]} -eq 0 ]; then
    progs=(itertools anyhow rayon chrono indexmap clap csv crossbeam tokio blake3 \
           tempfile walkdir numbigint smallvec bytes sha2 petgraph \
           net_tcp net_udp volatile \
           serde_json serde_yaml rand_det flate2 brotli argon2 p256 syn_parse hickory \
           unicode_tables wasmi boa_js tiny_skia zip_arch rust_decimal rustfft roaring \
           bitvec compact_str nom_parse comrak_md fst_build spade_delaunay jieba_cut \
           smoltcp_tcp statrs_stats rkyv_zero qr_round fatfs_img geo_ops rhai_script \
           redb_kv zstd_stream \
           crc32fast chacha_poly k256_ecdsa libflate_zlib rustls_cert \
           lyon_tess midly_midi jaq_jq kdl_doc ds_obscure qoi_img)
fi
pass=0 fail=0

for p in "${progs[@]}"; do
    src="corpus/c_$p.rs"
    [ -f "$src" ] || { echo "SKIP $p (no src)"; continue; }
    start=$(date +%s)
    timeout 600 "$MIRVM" run "$src" >"$OUT/$p.out" 2>"$OUT/$p.err"
    code=$?
    end=$(date +%s)
    dur=$((end - start))
    if [ "$code" -eq 0 ]; then
        echo "PASS  $p  (${dur}s)"
        pass=$((pass + 1))
    else
        first_err=$(grep -m1 -iE 'error|panic|unsupported|unimplemented|not (yet )?(implemented|supported)|no (shim|intrinsic)|abort' "$OUT/$p.err" | head -c 200)
        [ -z "$first_err" ] && first_err=$(tail -1 "$OUT/$p.err" | head -c 200)
        echo "FAIL  $p  (${dur}s, exit=$code)  ::  $first_err"
        fail=$((fail + 1))
    fi
done

echo "---"
echo "corpus: $pass pass, $fail fail"
[ "$fail" -eq 0 ]
