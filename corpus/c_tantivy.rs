#!/usr/bin/env mirvm
---
[dependencies]
# tantivy pinned to exact 0.26.1: at the time of writing 0.26.1 is the newest release on
# crates.io (0.24, 0.25 and 0.26 have all shipped, so "pin the newest" means 0.26.1).
# Features keep the crate defaults (mmap/stopwords/lz4-compression/
# columnar-zstd-compression/stemmer): defaults are deliberate, and mmap (memmap2 through
# MmapDirectory) is exactly the surface this fixture means to stress;
# columnar-zstd-compression pulls in the zstd 0.13 C FFI (the docstore's default
# compressor, already proven by c_zstd_stream/c_zstd_long).
tantivy = "=0.26.1"
---
// tantivy 0.26.1 differential over a real search engine, heavy on C FFI.
//
// Test surface: a three-field schema (title text + STORED / body text / num u64
// INDEXED+FAST+STORED); 20 fixed documents added via add_document; commit; drop the Index
// and reload the whole directory with Index::open_in_dir (re-reading meta and mmap'ing the
// segment files); rebuild the reader; then three queries, each printing its hit titles in
// BTree order plus a count:
//   ① term(body:cherry)             -- inverted TermQuery (WithFreqs)
//   ② range(num ∈ [12,30))          -- InvertedIndexRangeQuery (see the known limitation below)
//   ③ phrase(title:"hello world")   -- positional PhraseQuery
// Then delete_term(doc07) + commit (the update = delete + commit shape); reopen the reader,
// check the segment state and num_docs(19), re-run the term/range counts; finish with
// explicit schema metadata assertions (field-name set, and each field's
// field_type/is_indexed/is_stored).
//
// The index lives in the fixed system tmp subdirectory mirvm_c_tantivy_idx (the path is
// never printed; it is cleared first and deleted last, so every run starts cold). All IO
// goes through MmapDirectory -- read-side mmap is a surface this fixture means to stress
// (the write side uses atomic_write = tmp + rename; meta.json contains a uuid v4 and a
// timestamp, neither of which reaches the output). The writer opens one indexing thread per
// available_parallelism (≤3 workers here), and the per-segment semantic results do not
// depend on the thread count.
//
// Deterministic: document contents are fully determined by the index (title = docNN plus a
// rotation of four phrases, body = a rotation over 3 fruits × 2 desserts, num = 3i). Only
// semantic results are printed: hit title sets (BTree order), hit counts and assertion
// lines. BM25 score (f32), DocAddress, segment layout, thread count and absolute paths
// never reach the output. Three consecutive native runs are byte-identical and stderr is
// empty.
//
// Cost: a cold build of ~120 crates plus zstd-sys' vendored C build; one dimension takes
// seconds to run once built.
//
// Known limitation: fast-field column reads (the FAST range / aggregation surface) go
// through tantivy-columnar -> bitpacking-0.9.3 bitpacker8x AVX2, which uses
// `_mm256_lddqu_si256` (llvm.x86.avx.ldu.dq.256, the LDDQU family) -- mirvm has not built
// that in (a 12-line dependency-free probe reproduces it: native ok, mirvm TRAP exit 70;
// its 128-bit sibling llvm.x86.sse3.ldu.dq is missing too). This driver therefore uses the
// inverted InvertedIndexRangeQuery variant for the range query, while keeping the num FAST
// declaration intact (so the schema, column materialization and metadata surfaces are not
// downgraded). Once the intrinsic exists, the range query can go back to RangeQuery
// (FastFieldRangeWeight).
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

/// Title of document i: a rotating phrase (%4), with the docNN prefix making title order == write order.
fn title_of(i: usize) -> String {
    format!("doc{:02} {}", i, PHRASES[i % PHRASES.len()])
}

/// Runs the query with a limit far above the document count and returns the deduplicated
/// titles in lexicographic order. Only the semantic result (the title set) reaches the output.
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
    // The index goes in a fixed system-tmp subdirectory (path never printed); clear it first for repeatability.
    let dir: PathBuf = std::env::temp_dir().join("mirvm_c_tantivy_idx");
    if dir.exists() {
        fs::remove_dir_all(&dir).expect("failed to clear the old index directory");
    }
    fs::create_dir_all(&dir).expect("failed to create the index directory");

    // Three-field schema: title (text, stored) / body (text, not stored) / num (u64, indexed + fast + stored).
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

    // Reopen: drop the old Index, reload the whole directory (re-read meta, mmap segments), rebuild the reader.
    drop(index);
    let index = Index::open_in_dir(&dir)?;
    let reader = index.reader()?;
    let searcher = reader.searcher();
    assert_eq!(searcher.num_docs(), 20);
    println!("[c_tantivy] reopened, num_docs={}", searcher.num_docs());

    // Query 1: term (body:cherry -> i%3==1, expected {01,04,07,10,13,16,19}).
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

    // Query 2: range (num ∈ [12,30) -> i ∈ [4,10), expected {04..09}).
    // Known limitation: num is declared FAST, but the query deliberately uses
    // InvertedIndexRangeQuery (tantivy's public inverted equivalent, same semantics as
    // RangeQuery but touching only the inverted index) because a FAST column read goes
    // through tantivy-columnar -> bitpacking-0.9.3 bitpacker8x AVX2, whose
    // `_mm256_lddqu_si256` (llvm.x86.avx.ldu.dq.256) mirvm has not built in (a 12-line probe
    // without tantivy reproduces the same trap and exit 70; its 128-bit sibling
    // llvm.x86.sse3.ldu.dq is missing too). The FAST declaration stays for the schema, column
    // materialization and metadata assertions (the build side does not trigger 256-block
    // compression for a small column, so commit is safe); the fast-field read surface can be
    // restored once the intrinsic exists.
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

    // Query 3: phrase (title:"hello world" -> i%4==0, expected {00,04,08,12,16}).
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

    // Update: delete_term(doc07) + commit; reopen the reader and check the post-merge counts.
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

    // The delete must affect both query kinds: term loses 07 -> 6 hits; range loses 07 -> 5 hits.
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

    // Schema metadata assertions: field-name set + each field's type/indexed/stored flags.
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

    // Cleanup: leave no index behind so the next cold start sees the same state.
    drop(reader);
    drop(index);
    fs::remove_dir_all(&dir).expect("final cleanup failed");
    println!("[c_tantivy] all green");
    Ok(())
}
