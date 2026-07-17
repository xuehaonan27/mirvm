#!/usr/bin/env mirvm
---
[dependencies]
# tantivy 钉 exact 0.26.1：sparse index 实测 crates.io 当前最新即 0.26.1
# （任务书「pin 最新 0.22/0.24」系旧情报，0.24/0.25/0.26 相继已发，按
# 「pin 最新」本义取 0.26.1）。特性保持 crate default（全量：
# mmap/stopwords/lz4-compression/columnar-zstd-compression/stemmer）——
# 任务明示 default，且 mmap（经 memmap2 走 MmapDirectory）正是本批有意
# 施压面；columnar-zstd-compression 带 zstd 0.13 C FFI（docstore 默认
# 压缩器，c_zstd_stream/c_zstd_long 已实证的 native-archive 绿家族）。
tantivy = "=0.26.1"
---
// tantivy 0.26.1 真搜索引擎三维差分（批8 波1 重 FFI/C 条目）。
//
// 测试面：三字段 schema（title 文本+STORED / body 文本 / num u64
// INDEXED+FAST+STORED）→ 程序生成 20 份定值文档 add_document → commit →
// drop Index 后 Index::open_in_dir 整载（重读 meta + mmap 段文件）→ 重建
// reader → 三类查询各打印命中标题 BTree 序 + 计数：
//   ① term(body:cherry)    —— 倒排 TermQuery（WithFreqs）
//   ② range(num∈[12,30))   —— InvertedIndexRangeQuery 倒排版（绕行理由见头注尾部 FRONTIER 条）
//   ③ phrase(title:"hello world") —— 位置索引 PhraseQuery
// 再 delete_term(doc07) + commit（update = delete+commit 形态）→ 重开 reader
// 检查 segment 状态后 num_docs(19) 与 term/range 重查计数；收尾显式 schema
// 元数据断言（字段名集合、每字段 field_type/is_indexed/is_stored）。
//
// 索引落盘走系统 tmp 固定子目录 mirvm_c_tantivy_idx（路径不打印；首清尾删，
// 复跑必为冷态）。全程 MmapDirectory——读侧 mmap 是本条目有意施压面（写侧
// 目录 atomic_write = tmp+rename；meta.json 内含 uuid v4 与时间戳，均不进
// 输出）。writer 按 available_parallelism 开多索引线程（本机 ≤3 worker），
// 段内语义结果与线程数无关。
//
// 确定性：文档内容由下标完全决定（title=docNN+四短语轮转、
// body=3水果×2甜点轮转、num=3i）；只打印语义结果——命中文档标题集合
// （BTree 序）、命中计数、断言通过行；BM25 score(f32)/DocAddress/段布局/
// 线程数/绝对路径一律不进输出。native 侧三连跑逐字节一致、stderr 真空。
//
// 成本声明：冷构建 ~120 crate 图 + zstd-sys 的 vendored C 构建，B 维实测
// ~61s（源已在本机 registry 缓存）；单维运行秒级。
//
// 三维复跑（A 首跑后 B 才有脚本目录）：
//   A: target/release/mirvm run corpus/c_tantivy.rs
//   B: cd "$(dirname "$(grep -l 'name = "c_tantivy"' ~/.cache/mirvm/scripts/*/Cargo.toml)")" && \
//      RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" \
//      "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run corpus/c_tantivy.rs
//
// FRONTIER 绕行：fastfield 列值读取（FAST range / 聚合面）经
// tantivy-columnar → bitpacking-0.9.3 bitpacker8x AVX2 的
// `_mm256_lddqu_si256`（llvm.x86.avx.ldu.dq.256，LDDQU 族）——mirvm 未内建，
// M5.x intrinsic 欠账队列新面孔（12 行无依赖探针 native ok / mirvm TRAP
// exit 70 同文实录；128 位兄弟 llvm.x86.sse3.ldu.dq 同族同缺）。本 driver
// 的 range 查询显式改 InvertedIndexRangeQuery（tantivy 公开的倒排等价
// 变体）绕行；num 的 FAST 声明保留（schema/列物化/元数据面不降级）。
// 欠账内建后可将查询 2/5 还原为 RangeQuery（走 FastFieldRangeWeight）。
use std::collections::BTreeSet;
use std::fs;
use std::ops::Bound;
use std::path::PathBuf;

use tantivy::collector::TopDocs;
use tantivy::query::{InvertedIndexRangeQuery, PhraseQuery, Query, TermQuery};
use tantivy::schema::{
    FAST, Field, FieldType, INDEXED, IndexRecordOption, STORED, Schema, TEXT, Value,
};
use tantivy::{Index, IndexWriter, Searcher, TantivyDocument, Term, doc};

const PHRASES: [&str; 4] = [
    "hello world",
    "quick brown fox",
    "lazy dog sleeps",
    "hello fox runs",
];
const FRUITS: [&str; 3] = ["apple", "cherry", "banana"];
const DESSERTS: [&str; 2] = ["tart", "split"];

/// 第 i 份文档的标题：`%4` 轮转短语，前缀 docNN 保证标题字典序 == 写入序。
fn title_of(i: usize) -> String {
    format!("doc{:02} {}", i, PHRASES[i % PHRASES.len()])
}

/// 跑出全部命中（limit 远大于文档总数），标题去重进 BTreeSet 后按字典序返回。
/// 只取语义结果（标题集合）；score/DocAddress/段布局一律不进输出。
fn hit_titles(
    searcher: &Searcher,
    query: &dyn Query,
    title_field: Field,
) -> tantivy::Result<Vec<String>> {
    let hits = searcher.search(query, &TopDocs::with_limit(1000).order_by_score())?;
    let mut titles = BTreeSet::new();
    for (_score, addr) in hits {
        let doc: TantivyDocument = searcher.doc(addr)?;
        let t = doc
            .get_first(title_field)
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string();
        titles.insert(t);
    }
    Ok(titles.into_iter().collect())
}

fn main() -> tantivy::Result<()> {
    // 索引落在系统 tmp 的固定子目录（路径不打印）；先清空保证可复跑。
    let dir: PathBuf = std::env::temp_dir().join("mirvm_c_tantivy_idx");
    if dir.exists() {
        fs::remove_dir_all(&dir).expect("清空旧索引目录失败");
    }
    fs::create_dir_all(&dir).expect("创建索引目录失败");

    // 三字段 schema：title(文本,存)/body(文本,不存)/num(u64,索引+快列+存)。
    let mut sb = Schema::builder();
    let title = sb.add_text_field("title", TEXT | STORED);
    let body = sb.add_text_field("body", TEXT);
    let num = sb.add_u64_field("num", INDEXED | FAST | STORED);
    let schema = sb.build();

    let index = Index::create_in_dir(&dir, schema.clone())?;
    let mut writer: IndexWriter<TantivyDocument> = index.writer(50_000_000)?;
    for i in 0..20 {
        writer.add_document(doc!(
            title => title_of(i),
            body => format!("{} {} pie", FRUITS[i % FRUITS.len()], DESSERTS[i % DESSERTS.len()]),
            num => (i as u64) * 3
        ))?;
    }
    writer.commit()?;
    drop(writer);
    println!("[c_tantivy] wrote 20 docs, committed");

    // 重开：先 drop 旧 Index，再从目录整载（重读 meta + mmap 段），重建 reader。
    drop(index);
    let index = Index::open_in_dir(&dir)?;
    let reader = index.reader()?;
    let searcher = reader.searcher();
    assert_eq!(searcher.num_docs(), 20);
    println!("[c_tantivy] reopened, num_docs={}", searcher.num_docs());

    // 查询 1：term（body:cherry → i%3==1，期望 {01,04,07,10,13,16,19}）。
    let q1 = TermQuery::new(
        Term::from_field_text(body, "cherry"),
        IndexRecordOption::WithFreqs,
    );
    let h1 = hit_titles(&searcher, &q1, title)?;
    assert_eq!(h1.len(), 7);
    println!(
        "[c_tantivy] term(body:cherry) -> {} hits: {}",
        h1.len(),
        h1.join(" ")
    );

    // 查询 2：range（num∈[12,30) → i∈[4,10)，期望 {04..09}）。
    // FRONTIER 绕行：num 字段虽声明 FAST，但查询显式用
    // InvertedIndexRangeQuery（tantivy 公开的倒排版 range，语义与
    // RangeQuery 等价而只走倒排）——FAST 列值读取会经
    // tantivy-columnar → bitpacking-0.9.3 bitpacker8x 的 AVX2 实现，
    // 其 `_mm256_lddqu_si256`（llvm.x86.avx.ldu.dq.256）mirvm 未内建
    // （M5.x intrinsic 欠账新面孔，LDDQU 族；无 tantivy 的 12 行探针
    // 实测同 trap 同 exit 70，128 位兄弟 llvm.x86.sse3.ldu.dq 同缺）。
    // FAST 声明保留在 schema/列物化/元数据断言（构建侧对小列不触发 256
    // 块压缩，已实证 commit 安然）；fastfield 值读取面待 intrinsic
    // 内建后转正。
    let q2 = InvertedIndexRangeQuery::new(
        Bound::Included(Term::from_field_u64(num, 12)),
        Bound::Excluded(Term::from_field_u64(num, 30)),
    );
    let h2 = hit_titles(&searcher, &q2, title)?;
    assert_eq!(h2.len(), 6);
    println!(
        "[c_tantivy] range(num:[12,30)) -> {} hits: {}",
        h2.len(),
        h2.join(" ")
    );

    // 查询 3：phrase（title:"hello world" → i%4==0，期望 {00,04,08,12,16}）。
    let q3 = PhraseQuery::new(vec![
        Term::from_field_text(title, "hello"),
        Term::from_field_text(title, "world"),
    ]);
    let h3 = hit_titles(&searcher, &q3, title)?;
    assert_eq!(h3.len(), 5);
    println!(
        "[c_tantivy] phrase(title:\"hello world\") -> {} hits: {}",
        h3.len(),
        h3.join(" ")
    );

    // update：delete_term(doc07) + commit；重开 reader 检查合并后计数。
    let mut writer: IndexWriter<TantivyDocument> = index.writer(15_000_000)?;
    writer.delete_term(Term::from_field_text(title, "doc07"));
    writer.commit()?;
    drop(writer);
    drop(reader);
    let reader = index.reader()?;
    let searcher = reader.searcher();
    assert_eq!(searcher.num_docs(), 19);
    println!(
        "[c_tantivy] delete doc07 + commit: num_docs={}",
        searcher.num_docs()
    );

    // 删除应同时作用于两类查询：term 去 07 → 6 命中；range 去 07 → 5 命中。
    let h4 = hit_titles(&searcher, &q1, title)?;
    assert_eq!(h4.len(), 6);
    println!(
        "[c_tantivy] term again -> {} hits: {}",
        h4.len(),
        h4.join(" ")
    );
    let h5 = hit_titles(&searcher, &q2, title)?;
    assert_eq!(h5.len(), 5);
    println!(
        "[c_tantivy] range again -> {} hits: {}",
        h5.len(),
        h5.join(" ")
    );

    // schema 元数据断言：字段名集合 + 每字段类型/索引/存储标志。
    let meta = index.schema();
    let names: BTreeSet<String> = meta.fields().map(|(_, e)| e.name().to_string()).collect();
    let expect: BTreeSet<String> = ["body", "num", "title"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(names, expect);
    let title_entry = meta.get_field_entry(meta.get_field("title")?);
    assert!(title_entry.is_indexed() && title_entry.is_stored());
    assert!(matches!(title_entry.field_type(), FieldType::Str(_)));
    let num_entry = meta.get_field_entry(meta.get_field("num")?);
    assert!(num_entry.is_indexed() && num_entry.is_stored());
    assert!(matches!(num_entry.field_type(), FieldType::U64(_)));
    println!("[c_tantivy] schema metadata ok (3 fields)");

    // 清理：不留索引，保证下次冷启动状态一致。
    drop(reader);
    drop(index);
    fs::remove_dir_all(&dir).expect("最终清理失败");
    println!("[c_tantivy] all green");
    Ok(())
}
