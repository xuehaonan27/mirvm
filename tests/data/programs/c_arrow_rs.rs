#!/usr/bin/env mirvm
---
[dependencies]
arrow = { version = "=59.1.0", default-features = false, features = ["ipc"] }
---
// c_arrow_rs -- Apache Arrow current stable: a small-payload, wide-type differential driver.
//
// Coverage:
//   1) Hand-built RecordBatch with five column types: Int64 / Utf8 / Float64 / List<i32> /
//      Struct{Int32, Utf8}; nullability covers Int64/Float64 slots, whole List slots, and
//      Struct sub-columns.
//   2) compute kernels: filter_record_batch (boolean mask), take (UInt32 indices),
//      sort_to_indices (Float64 with nulls), sum/min/max aggregates (one each for Int64 and Float64).
//   3) IPC roundtrip: StreamWriter into an in-memory buffer -> byte FNV-1a digest -> StreamReader
//      reads it back and compares RecordBatch == and per-column == against the original.
//   4) Printed: schema summary (field order/name/type Debug/nullable), all raw column values,
//      all kernel results, IPC byte length and FNV-1a, and the read-back equality bool.
//
// Determinism: every f64 prints via to_bits(), never float to_string; no randomness, wall clock,
//   HashMap iteration order, or raw addresses; output order = explicit row order, field order,
//   fixed take/filter indices; stderr is empty.
//
// Version pin and bypass notes:
//   arrow = 59.1.0, pinned exactly to the crates.io max_stable version as of 2026-07-17.
//   default-features = false with only "ipc": the default set is ["csv","ipc","json"]; this driver
//   uses no csv/json surface, so they are dropped to shrink the tree. "ipc" omits ipc_compression
//   and arrow-ipc's default features are empty, so the dependency tree is pure Rust (no -sys C lib).
//   No engine bypass switch or force-soft env var is used.

use std::io::Cursor;
use std::sync::Arc;

use arrow::array::{
    Array, ArrayRef, BooleanArray, Float64Array, Int32Array, Int64Array, ListArray,
    RecordBatch, StringArray, StructArray, UInt32Array,
};
use arrow::compute::{filter_record_batch, max, min, sort_to_indices, sum, take};
use arrow::datatypes::{DataType, Field, Fields, Int32Type, Schema};
use arrow::ipc::reader::StreamReader;
use arrow::ipc::writer::StreamWriter;

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn opt_i64(v: Option<i64>) -> String {
    v.map(|x| x.to_string()).unwrap_or_else(|| "null".to_string())
}

fn opt_bits(v: Option<f64>) -> String {
    v.map(|x| x.to_bits().to_string())
        .unwrap_or_else(|| "null".to_string())
}

fn i64_cell(a: &Int64Array, i: usize) -> String {
    if a.is_null(i) {
        "null".to_string()
    } else {
        a.value(i).to_string()
    }
}

fn build_batch() -> (Arc<Schema>, RecordBatch) {
    let id = Int64Array::from(vec![
        Some(7),
        None,
        Some(3),
        Some(-2),
        Some(11),
        Some(0),
        Some(5),
        Some(-8),
    ]);
    let name = StringArray::from(vec![
        Some("delta"),
        Some("alpha"),
        None,
        Some("echo"),
        Some("bravo"),
        Some("golf"),
        Some("foxtrot"),
        Some("charlie"),
    ]);
    let score = Float64Array::from(vec![
        Some(2.5),
        Some(-1.25),
        Some(0.0),
        None,
        Some(3.75),
        Some(-0.5),
        Some(1.5),
        Some(2.0),
    ]);
    let primes = ListArray::from_iter_primitive::<Int32Type, _, _>(vec![
        Some(vec![Some(2), Some(3), Some(5)]),
        Some(vec![]),
        None,
        Some(vec![Some(7)]),
        Some(vec![Some(11), Some(13)]),
        Some(vec![Some(-2)]),
        None,
        Some(vec![Some(17), Some(19), Some(23), Some(29)]),
    ]);
    let meta_fields = Fields::from(vec![
        Field::new("k", DataType::Int32, true),
        Field::new("v", DataType::Utf8, true),
    ]);
    let meta = StructArray::new(
        meta_fields.clone(),
        vec![
            Arc::new(Int32Array::from(vec![
                Some(1),
                Some(2),
                None,
                Some(4),
                Some(5),
                Some(6),
                Some(7),
                Some(8),
            ])) as ArrayRef,
            Arc::new(StringArray::from(vec![
                Some("x"),
                Some("y"),
                Some("z"),
                None,
                Some("w"),
                Some("v"),
                Some("u"),
                Some("t"),
            ])) as ArrayRef,
        ],
        None,
    );
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, true),
        Field::new("name", DataType::Utf8, true),
        Field::new("score", DataType::Float64, true),
        Field::new(
            "primes",
            DataType::List(Arc::new(Field::new("item", DataType::Int32, true))),
            true,
        ),
        Field::new("meta", DataType::Struct(meta_fields), true),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(id) as ArrayRef,
            Arc::new(name) as ArrayRef,
            Arc::new(score) as ArrayRef,
            Arc::new(primes) as ArrayRef,
            Arc::new(meta) as ArrayRef,
        ],
    )
    .unwrap();
    (schema, batch)
}

fn main() {
    let (schema, batch) = build_batch();
    let n_rows = batch.num_rows();

    println!("== schema ==");
    println!("fields={} rows={}", schema.fields().len(), n_rows);
    for (i, f) in schema.fields().iter().enumerate() {
        println!(
            "f{i} name={} ty={:?} nullable={}",
            f.name(),
            f.data_type(),
            f.is_nullable()
        );
    }

    println!("== values ==");
    let id = batch.column(0).as_any().downcast_ref::<Int64Array>().unwrap();
    let name = batch
        .column(1)
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    let score = batch
        .column(2)
        .as_any()
        .downcast_ref::<Float64Array>()
        .unwrap();
    let primes = batch
        .column(3)
        .as_any()
        .downcast_ref::<ListArray>()
        .unwrap();
    let meta = batch
        .column(4)
        .as_any()
        .downcast_ref::<StructArray>()
        .unwrap();

    let id_s: Vec<String> = (0..n_rows).map(|i| i64_cell(id, i)).collect();
    println!("id=[{}]", id_s.join(","));
    let name_s: Vec<String> = (0..n_rows)
        .map(|i| {
            if name.is_null(i) {
                "null".to_string()
            } else {
                name.value(i).to_string()
            }
        })
        .collect();
    println!("name=[{}]", name_s.join(","));
    let score_s: Vec<String> = (0..n_rows)
        .map(|i| {
            if score.is_null(i) {
                "null".to_string()
            } else {
                score.value(i).to_bits().to_string()
            }
        })
        .collect();
    println!("score_bits=[{}]", score_s.join(","));
    let mut prime_s = Vec::new();
    for i in 0..n_rows {
        if primes.is_null(i) {
            prime_s.push("null".to_string());
        } else {
            let v = primes.value(i);
            let vi = v.as_any().downcast_ref::<Int32Array>().unwrap();
            let inner: Vec<String> = (0..vi.len())
                .map(|j| {
                    if vi.is_null(j) {
                        "null".to_string()
                    } else {
                        vi.value(j).to_string()
                    }
                })
                .collect();
            prime_s.push(format!("[{}]", inner.join(",")));
        }
    }
    println!("primes=[{}]", prime_s.join(","));
    let mk = meta
        .column(0)
        .as_any()
        .downcast_ref::<Int32Array>()
        .unwrap();
    let mv = meta
        .column(1)
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    let meta_s: Vec<String> = (0..n_rows)
        .map(|i| {
            let k = if mk.is_null(i) {
                "null".to_string()
            } else {
                mk.value(i).to_string()
            };
            let v = if mv.is_null(i) {
                "null".to_string()
            } else {
                mv.value(i).to_string()
            };
            format!("({k},{v})")
        })
        .collect();
    println!("meta=[{}]", meta_s.join(","));

    println!("== kernels ==");
    let mask = BooleanArray::from(vec![true, false, true, true, false, true, false, true]);
    let fb = filter_record_batch(&batch, &mask).unwrap();
    let fid = fb.column(0).as_any().downcast_ref::<Int64Array>().unwrap();
    let fid_s: Vec<String> = (0..fb.num_rows()).map(|i| i64_cell(fid, i)).collect();
    println!("filter rows={} id=[{}]", fb.num_rows(), fid_s.join(","));

    let idx = UInt32Array::from(vec![5u32, 0, 7, 2]);
    let tid_a = take(id as &dyn Array, &idx, None).unwrap();
    let tid = tid_a.as_any().downcast_ref::<Int64Array>().unwrap();
    let tid_s: Vec<String> = (0..tid.len()).map(|i| i64_cell(tid, i)).collect();
    println!("take idx=[5,0,7,2] id=[{}]", tid_s.join(","));

    let sidx = sort_to_indices(score as &dyn Array, None, None).unwrap();
    let sidx_s: Vec<String> = (0..sidx.len()).map(|i| sidx.value(i).to_string()).collect();
    println!("sort score idx=[{}]", sidx_s.join(","));

    println!(
        "sum_id={} min_id={} max_id={}",
        opt_i64(sum(id)),
        opt_i64(min(id)),
        opt_i64(max(id))
    );
    println!(
        "sum_score_bits={} min_score_bits={} max_score_bits={}",
        opt_bits(sum(score)),
        opt_bits(min(score)),
        opt_bits(max(score))
    );

    println!("== ipc ==");
    let mut buf: Vec<u8> = Vec::new();
    {
        let mut w = StreamWriter::try_new(&mut buf, &schema).unwrap();
        w.write(&batch).unwrap();
        w.finish().unwrap();
    }
    println!("ipc bytes={} fnv={:016x}", buf.len(), fnv1a(&buf));

    let cursor = Cursor::new(buf);
    let mut reader = StreamReader::try_new(cursor, None).unwrap();
    let mut n_batches = 0usize;
    let mut total_rows = 0usize;
    let mut all_eq = true;
    while let Some(rr) = reader.next() {
        let rb = rr.unwrap();
        n_batches += 1;
        total_rows += rb.num_rows();
        let eq = rb == batch;
        all_eq &= eq;
        let col_eq: Vec<String> = (0..batch.num_columns())
            .map(|i| (batch.column(i) == rb.column(i)).to_string())
            .collect();
        println!(
            "read batch{n_batches} rows={} eq={eq} col_eq=[{}]",
            rb.num_rows(),
            col_eq.join(",")
        );
    }
    println!("read batches={n_batches} rows={total_rows} batch_eq={all_eq}");
}
