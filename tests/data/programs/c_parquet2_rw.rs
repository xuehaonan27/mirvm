#!/usr/bin/env mirvm
---
[dependencies]
# parquet2 is pinned at the last release of its line: =0.17.2 is terminal, so the
# dependency cannot drift to a later version.
# The default feature set is snappy+gzip+lz4+zstd+brotli+bloom_filter; the lz4
# feature pulls the C FFI lz4 crate (lz4-sys) and zstd likewise uses C FFI, so
# default-features=false keeps only snappy to hold the compression surface pure Rust.
# parquet SNAPPY encodes and decodes raw snappy blocks (parquet2 uses
# snap::raw::Encoder/Decoder internally) and never enters snap's frame layer, so it
# does not reach frame-level crc32c SSE4.2 runtime dispatch.
# parquet2 has no thrift runtime dependency: metadata goes through the
# parquet-format-safe 0.2 official generated safe bindings (zero mandatory
# dependencies, bundled compact-protocol read/write), so nothing needs trimming.
# The dependency closure is 5 crates, all pure Rust, none compiling C in build.rs:
# parquet2 0.17.2 / parquet-format-safe 0.2.x / seq-macro 0.3 / snap 1.x /
# streaming-decompression 0.1.
parquet2 = { version = "=0.17.2", default-features = false, features = ["snappy"] }
---
// parquet2 0.17.2 three-way differential: write a small table into an in-memory
// Vec<u8> (Cursor) as Uncompressed and Snappy -> byte length + FNV-1a anchor -> read
// back, assert rows/schema/values (nullable def-level) -> print key meta stats.
//
// Coverage:
//   schema  : 4 nullable primitive columns i32_col(Int32) / i64_col(Int64) /
//             f64_col(Double) / utf8_col(ByteArray + logical String), read back
//             from metadata, asserting name/physical type/Repetition/logical_type
//   row groups: 3 row groups with 8/6/7 rows (21 total), one page per column
//   encoding: all Plain encoding + RLE def-level (every column Optional, each with
//             at least one null; specific row groups cover nulls=0 and nulls=2)
//   data    : i32 holds MIN/MAX extremes; i64 holds MIN/MAX and ±1<<40 magnitudes;
//             f64 holds values beyond 2^53 and negative zero alongside zero (assert by
//             to_bits, print the bits); utf8 holds empty/None/repeated strings (compressible)
//   compression: two complete files, Uncompressed and Snappy, printing codec/
//             uncompressed/compressed sizes; both files hold identical logical content,
//             checked twice over (value assertions and statistics-equality assertions)
//   statistics: write_statistics=true -> page-header min/max/null_count (thrift
//             serialized) + FileWriter::end writes column index/offset index; read back,
//             downcast PrimitiveStatistics<i32|i64|f64> / BinaryStatistics, print, and
//             assert equality across both files via PartialEq
//   metadata: fixed created_by string and fixed key_value_metadata kv (read via the
//             thrift callback); format version=1(V1); read_metadata goes through the
//             parquet-format-safe compact protocol (pure Rust)
//
// Determinism: all values are literal constants; no randomness/time/address/HashMap
// order/threads. The compressor is deterministic per input, FNV anchors whole-file bytes;
// floats never print raw, always to_bits. Assertion failures panic (stderr silent when green).
//
// Three-way re-run commands (repo root):
//   A: target/release/mirvm run tests/data/programs/c_parquet2_rw.rs
//   B: cd $(grep -l 'name = "c_parquet2_rw"' ~/.cache/mirvm/scripts/*/Cargo.toml \
//        | head -1 | xargs dirname) && \
//      RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" \
//      "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run tests/data/programs/c_parquet2_rw.rs
//
// FRONTIER: none (expected). thrift compact metadata / RLE bit packing / snap raw block
// compression are all pure Rust with no SIMD surface.
use std::io::Cursor;

use parquet2::compression::CompressionOptions;
use parquet2::deserialize::{
    BinaryPageState, DefLevelsDecoder, HybridDecoderBitmapIter, HybridEncoded, NativePageState,
};
use parquet2::encoding::hybrid_rle::{encode_bool, BitmapIter, HybridRleDecoder};
use parquet2::encoding::Encoding;
use parquet2::error::{Error, Result};
use parquet2::metadata::{
    ColumnChunkMetaData, Descriptor, FileMetaData, KeyValue, SchemaDescriptor,
};
use parquet2::page::{CompressedPage, DataPage, DataPageHeader, DataPageHeaderV1, Page};
use parquet2::read::{get_page_iterator, read_metadata, BasicDecompressor};
use parquet2::schema::types::{ParquetType, PhysicalType, PrimitiveLogicalType};
use parquet2::schema::Repetition;
use parquet2::statistics::{
    serialize_statistics, BinaryStatistics, PrimitiveStatistics, Statistics,
};
use parquet2::types::{ord_binary, NativeType};
use parquet2::write::{
    Compressor, DynIter, DynStreamingIterator, FileWriter, Version, WriteOptions,
};
use parquet2::FallibleStreamingIterator;

// ---------------------------------------------------------------------------
// Fixed data: 3 row groups (8/6/7 rows); 4 nullable columns source both compression modes.
// ---------------------------------------------------------------------------

struct Data {
    i32_rgs: [Vec<Option<i32>>; 3],
    i64_rgs: [Vec<Option<i64>>; 3],
    f64_rgs: [Vec<Option<f64>>; 3],
    utf8_rgs: [Vec<Option<Vec<u8>>>; 3],
}

fn sample_data() -> Data {
    Data {
        i32_rgs: [
            vec![
                Some(0),
                Some(11),
                Some(-7),
                None,
                Some(42),
                Some(42),
                Some(-1000),
                Some(7),
            ],
            vec![Some(1), None, Some(2), Some(3), Some(-3), Some(-2)],
            vec![
                Some(i32::MAX),
                Some(i32::MIN),
                Some(0),
                Some(0),
                None,
                Some(5),
                Some(6),
            ],
        ],
        i64_rgs: [
            vec![
                None,
                Some(1i64 << 40),
                Some(-(1i64 << 40)),
                Some(3),
                Some(3),
                Some(3),
                Some(0),
                Some(i64::MAX),
            ],
            vec![Some(7), Some(7), Some(7), Some(7), Some(7), Some(7)],
            vec![
                Some(-1),
                None,
                Some(i64::MIN),
                Some(123_456_789_012_345),
                Some(-987_654_321),
                Some(64),
                Some(-64),
            ],
        ],
        f64_rgs: [
            vec![
                Some(1.5),
                Some(-0.0),
                Some(3.25),
                None,
                Some(1e300),
                Some(-2.5e-10),
                Some(0.0),
                Some(123_456.789),
            ],
            vec![Some(0.1), Some(0.1), Some(0.1), Some(0.1), Some(0.1), Some(0.1)],
            vec![None, None, Some(2.0), Some(-2.0), Some(1e-3), Some(4.5), Some(-4.5)],
        ],
        utf8_rgs: [
            vec![
                Some(b"alpha".to_vec()),
                None,
                Some(b"beta beta beta".to_vec()),
                Some(Vec::new()),
                Some(b"mirvm-corpus".to_vec()),
                Some(b"zz".to_vec()),
                Some(b"hello world".to_vec()),
                Some(b"42".to_vec()),
            ],
            vec![
                Some(b"row-01".to_vec()),
                Some(b"row-02".to_vec()),
                Some(b"row-03".to_vec()),
                Some(b"row-01".to_vec()),
                Some(b"row-01".to_vec()),
                Some(b"row-01".to_vec()),
            ],
            vec![
                None,
                Some(b"tail".to_vec()),
                Some(b"a".to_vec()),
                Some(b"b".to_vec()),
                Some(b"ab".to_vec()),
                Some(b"ba".to_vec()),
                Some(b"tail".to_vec()),
            ],
        ],
    }
}

const ROWS_PER_RG: [usize; 3] = [8, 6, 7];
const TOTAL_ROWS: usize = 21;
const FIELD_NAMES: [&str; 4] = ["i32_col", "i64_col", "f64_col", "utf8_col"];
const PHYSICALS: [PhysicalType; 4] = [
    PhysicalType::Int32,
    PhysicalType::Int64,
    PhysicalType::Double,
    PhysicalType::ByteArray,
];

// ---------------------------------------------------------------------------
// Write side: schema -> Plain-encoded pages (V1 header + RLE def-level + page stats) -> FileWriter.
// ---------------------------------------------------------------------------

/// 4 primitive columns, all Optional; utf8_col carries logical String.
fn build_schema() -> Result<SchemaDescriptor> {
    Ok(SchemaDescriptor::new(
        "schema".to_string(),
        vec![
            ParquetType::from_physical(FIELD_NAMES[0].to_string(), PhysicalType::Int32),
            ParquetType::from_physical(FIELD_NAMES[1].to_string(), PhysicalType::Int64),
            ParquetType::from_physical(FIELD_NAMES[2].to_string(), PhysicalType::Double),
            ParquetType::try_from_primitive(
                FIELD_NAMES[3].to_string(),
                PhysicalType::ByteArray,
                Repetition::Optional,
                None,
                Some(PrimitiveLogicalType::String),
                None,
            )?,
        ],
    ))
}

/// Fixed sequence -> (values as LE bytes, 4-byte length header + RLE def-level bitmap).
fn unzip_option<T: NativeType>(array: &[Option<T>]) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut validity = std::io::Cursor::new(vec![0; 4]);
    validity.set_position(4);

    let mut values = vec![];
    let iter = array.iter().map(|value| {
        if let Some(item) = value {
            values.extend_from_slice(item.to_le_bytes().as_ref());
            true
        } else {
            false
        }
    });
    encode_bool(&mut validity, iter)?;

    let mut validity = validity.into_inner();
    let length = (validity.len() - 4).to_le_bytes();
    validity[0..4].copy_from_slice(&length[0..4]);
    Ok((values, validity))
}

fn unzip_option_binary(array: &[Option<Vec<u8>>]) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut validity = std::io::Cursor::new(vec![0; 4]);
    validity.set_position(4);

    let mut values = vec![];
    let iter = array.iter().map(|value| {
        if let Some(item) = value {
            values.extend_from_slice(&(item.len() as i32).to_le_bytes());
            values.extend_from_slice(item.as_ref());
            true
        } else {
            false
        }
    });
    encode_bool(&mut validity, iter)?;

    let mut validity = validity.into_inner();
    let length = (validity.len() - 4).to_le_bytes();
    validity[0..4].copy_from_slice(&length[0..4]);
    Ok((values, validity))
}

/// Primitive fixed array -> V1 data page (Plain value area + RLE def-level + min/max/null_count).
fn primitive_page<T: NativeType>(
    array: &[Option<T>],
    options: &WriteOptions,
    descriptor: &Descriptor,
) -> Result<Page> {
    let (values, mut buffer) = unzip_option(array)?;
    buffer.extend_from_slice(&values);

    let statistics = if options.write_statistics {
        let statistics = &PrimitiveStatistics {
            primitive_type: descriptor.primitive_type.clone(),
            null_count: Some((array.len() - array.iter().flatten().count()) as i64),
            distinct_count: None,
            max_value: array.iter().flatten().max_by(|x, y| x.ord(y)).copied(),
            min_value: array.iter().flatten().min_by(|x, y| x.ord(y)).copied(),
        } as &dyn Statistics;
        Some(serialize_statistics(statistics))
    } else {
        None
    };

    let header = DataPageHeaderV1 {
        num_values: array.len() as i32,
        encoding: Encoding::Plain.into(),
        definition_level_encoding: Encoding::Rle.into(),
        repetition_level_encoding: Encoding::Rle.into(),
        statistics,
    };

    Ok(Page::Data(DataPage::new(
        DataPageHeader::V1(header),
        buffer,
        descriptor.clone(),
        Some(array.len()),
    )))
}

/// Binary fixed array -> V1 data page (Plain "len LE + bytes" value area + statistics).
fn binary_page(
    array: &[Option<Vec<u8>>],
    options: &WriteOptions,
    descriptor: &Descriptor,
) -> Result<Page> {
    let (values, mut buffer) = unzip_option_binary(array)?;
    buffer.extend_from_slice(&values);

    let statistics = if options.write_statistics {
        let statistics = &BinaryStatistics {
            primitive_type: descriptor.primitive_type.clone(),
            null_count: Some((array.len() - array.iter().flatten().count()) as i64),
            distinct_count: None,
            max_value: array
                .iter()
                .flatten()
                .max_by(|x, y| ord_binary(x, y))
                .cloned(),
            min_value: array
                .iter()
                .flatten()
                .min_by(|x, y| ord_binary(x, y))
                .cloned(),
        } as &dyn Statistics;
        Some(serialize_statistics(statistics))
    } else {
        None
    };

    let header = DataPageHeaderV1 {
        num_values: array.len() as i32,
        encoding: Encoding::Plain.into(),
        definition_level_encoding: Encoding::Rle.into(),
        repetition_level_encoding: Encoding::Rle.into(),
        statistics,
    };

    Ok(Page::Data(DataPage::new(
        DataPageHeader::V1(header),
        buffer,
        descriptor.clone(),
        Some(array.len()),
    )))
}

/// Write the whole table into an in-memory Vec: 3 row groups, fixed created_by and kv metadata.
fn write_table(data: &Data, compression: CompressionOptions) -> Result<Vec<u8>> {
    let options = WriteOptions {
        write_statistics: true,
        version: Version::V1,
    };
    let schema = build_schema()?;
    let descs: Vec<Descriptor> = schema
        .columns()
        .iter()
        .map(|c| c.descriptor.clone())
        .collect();

    let mut writer = FileWriter::new(
        Cursor::new(Vec::new()),
        schema,
        options,
        Some("mirvm-parquet2-rw 0.17".to_string()),
    );

    for rg in 0..3 {
        let pages: Vec<Result<DynStreamingIterator<'_, CompressedPage, Error>>> = vec![
            Ok(DynStreamingIterator::new(Compressor::new_from_vec(
                DynIter::new(std::iter::once(primitive_page(
                    &data.i32_rgs[rg],
                    &options,
                    &descs[0],
                ))),
                compression,
                vec![],
            ))),
            Ok(DynStreamingIterator::new(Compressor::new_from_vec(
                DynIter::new(std::iter::once(primitive_page(
                    &data.i64_rgs[rg],
                    &options,
                    &descs[1],
                ))),
                compression,
                vec![],
            ))),
            Ok(DynStreamingIterator::new(Compressor::new_from_vec(
                DynIter::new(std::iter::once(primitive_page(
                    &data.f64_rgs[rg],
                    &options,
                    &descs[2],
                ))),
                compression,
                vec![],
            ))),
            Ok(DynStreamingIterator::new(Compressor::new_from_vec(
                DynIter::new(std::iter::once(binary_page(
                    &data.utf8_rgs[rg],
                    &options,
                    &descs[3],
                ))),
                compression,
                vec![],
            ))),
        ];
        writer.write(DynIter::new(pages.into_iter()))?;
    }
    writer.end(Some(vec![KeyValue {
        key: "app".to_string(),
        value: Some("mirvm corpus parquet2 rw".to_string()),
    }]))?;

    Ok(writer.into_inner().into_inner())
}

// ---------------------------------------------------------------------------
// Read: metadata -> row groups/column chunks -> decompress -> decode V1 pages (Plain + def-level).
// ---------------------------------------------------------------------------

/// def-level driven decode: the legal value stream (copied from parquet2's it/read/utils.rs).
fn deserialize_optional<C: Clone, I: Iterator<Item = Result<C>>>(
    validity: DefLevelsDecoder,
    values: I,
) -> Result<Vec<Option<C>>> {
    match validity {
        DefLevelsDecoder::Bitmap(bitmap) => deserialize_bitmap(bitmap, values),
        DefLevelsDecoder::Levels(levels, max_level) => {
            deserialize_levels(levels, max_level, values)
        }
    }
}

fn deserialize_bitmap<C: Clone, I: Iterator<Item = Result<C>>>(
    mut validity: HybridDecoderBitmapIter,
    mut values: I,
) -> Result<Vec<Option<C>>> {
    let mut deserialized = Vec::with_capacity(validity.len());

    validity.try_for_each(|run| match run? {
        HybridEncoded::Bitmap(bitmap, length) => BitmapIter::new(bitmap, 0, length)
            .into_iter()
            .try_for_each(|x| {
                if x {
                    deserialized.push(values.next().transpose()?);
                } else {
                    deserialized.push(None);
                }
                std::result::Result::<_, Error>::Ok(())
            }),
        HybridEncoded::Repeated(is_set, length) => {
            if is_set {
                deserialized.reserve(length);
                for x in values.by_ref().take(length) {
                    deserialized.push(Some(x?))
                }
            } else {
                deserialized.extend(std::iter::repeat(None).take(length))
            }
            Ok(())
        }
    })?;
    Ok(deserialized)
}

fn deserialize_levels<C: Clone, I: Iterator<Item = Result<C>>>(
    levels: HybridRleDecoder,
    max: u32,
    mut values: I,
) -> Result<Vec<Option<C>>> {
    levels
        .into_iter()
        .map(|x| {
            if x? == max {
                values.next().transpose()
            } else {
                Ok(None)
            }
        })
        .collect()
}

/// Plain encoding only: no dictionary page appears; P is a placeholder, so that arm is unreachable.
enum NoDict {}

fn primitive_page_to_vec<T: NativeType>(page: &DataPage) -> Result<Vec<Option<T>>> {
    assert_eq!(page.descriptor.max_rep_level, 0);
    match NativePageState::<T, NoDict>::try_new(page, None)? {
        NativePageState::Optional(validity, values) => {
            deserialize_optional(validity, values.map(Ok))
        }
        NativePageState::Required(values) => Ok(values.map(Some).collect()),
        _ => Err(Error::OutOfSpec("unexpected dictionary encoding".to_string())),
    }
}

fn binary_page_to_vec(page: &DataPage) -> Result<Vec<Option<Vec<u8>>>> {
    assert_eq!(page.descriptor.max_rep_level, 0);
    match BinaryPageState::<NoDict>::try_new(page, None)? {
        BinaryPageState::Optional(validity, values) => {
            deserialize_optional(validity, values.map(|x| x.map(|x| x.to_vec())))
        }
        BinaryPageState::Required(values) => values
            .map(|x| x.map(|x| x.to_vec()))
            .map(Some)
            .map(|x| x.transpose())
            .collect(),
        _ => Err(Error::OutOfSpec("unexpected dictionary encoding".to_string())),
    }
}

fn read_primitive_column<T: NativeType>(
    reader: &mut Cursor<Vec<u8>>,
    col: &ColumnChunkMetaData,
) -> Result<Vec<Option<T>>> {
    let pages = get_page_iterator(col, reader, None, vec![], usize::MAX)?;
    let mut it = BasicDecompressor::new(pages, vec![]);
    let mut out = vec![];
    while let Some(page) = it.next()? {
        match page {
            Page::Data(dp) => out.extend(primitive_page_to_vec::<T>(dp)?),
            Page::Dict(_) => {
                return Err(Error::OutOfSpec("dict page in plain driver".to_string()))
            }
        }
    }
    Ok(out)
}

fn read_binary_column(
    reader: &mut Cursor<Vec<u8>>,
    col: &ColumnChunkMetaData,
) -> Result<Vec<Option<Vec<u8>>>> {
    let pages = get_page_iterator(col, reader, None, vec![], usize::MAX)?;
    let mut it = BasicDecompressor::new(pages, vec![]);
    let mut out = vec![];
    while let Some(page) = it.next()? {
        match page {
            Page::Data(dp) => out.extend(binary_page_to_vec(dp)?),
            Page::Dict(_) => {
                return Err(Error::OutOfSpec("dict page in plain driver".to_string()))
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Printing and assertions
// ---------------------------------------------------------------------------

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn b64(x: f64) -> String {
    format!("{:016x}", x.to_bits())
}

fn stats_line(st: &dyn Statistics, col: &ColumnChunkMetaData) -> String {
    let detail = match col.physical_type() {
        PhysicalType::Int32 => {
            let s = st.as_any().downcast_ref::<PrimitiveStatistics<i32>>().unwrap();
            format!(
                "nulls={:?} min={:?} max={:?}",
                s.null_count, s.min_value, s.max_value
            )
        }
        PhysicalType::Int64 => {
            let s = st.as_any().downcast_ref::<PrimitiveStatistics<i64>>().unwrap();
            format!(
                "nulls={:?} min={:?} max={:?}",
                s.null_count, s.min_value, s.max_value
            )
        }
        PhysicalType::Double => {
            let s = st.as_any().downcast_ref::<PrimitiveStatistics<f64>>().unwrap();
            format!(
                "nulls={:?} min={:?} max={:?}",
                s.null_count,
                s.min_value.map(b64),
                s.max_value.map(b64)
            )
        }
        PhysicalType::ByteArray => {
            let s = st.as_any().downcast_ref::<BinaryStatistics>().unwrap();
            let s8 = |v: &Option<Vec<u8>>| {
                v.as_ref()
                    .map(|v| String::from_utf8_lossy(v).into_owned())
            };
            format!(
                "nulls={:?} min={:?} max={:?}",
                s.null_count,
                s8(&s.min_value),
                s8(&s.max_value)
            )
        }
        other => unreachable!("driver only writes 4 columns, got {other:?}"),
    };
    format!(
        "codec={:?} nv={} u={} c={} {detail}",
        col.compression(),
        col.num_values(),
        col.uncompressed_size(),
        col.compressed_size()
    )
}

/// Assert metadata and schema: row-group/column counts, field names, physical types,
/// nullability, logical types; full=true also prints row-group sizes and 12 stats lines.
fn inspect_meta(md: &FileMetaData, label: &str, full: bool) {
    assert_eq!(md.num_rows, TOTAL_ROWS);
    assert_eq!(md.row_groups.len(), 3);
    assert_eq!(md.schema_descr.columns().len(), 4);
    println!(
        "{label} meta rows={} cols={} rgs={} version={}",
        md.num_rows,
        md.schema_descr.columns().len(),
        md.row_groups.len(),
        md.version
    );
    if !full {
        return;
    }
    println!("created_by = {:?}", md.created_by);
    for kv in md.key_value_metadata.iter().flatten() {
        println!("kv {} = {:?}", kv.key, kv.value);
    }
    for (i, field) in md.schema_descr.fields().iter().enumerate() {
        match field {
            ParquetType::PrimitiveType(pt) => {
                assert_eq!(pt.field_info.name, FIELD_NAMES[i]);
                assert_eq!(pt.physical_type, PHYSICALS[i]);
                assert_eq!(pt.field_info.repetition, Repetition::Optional);
                if i == 3 {
                    assert_eq!(pt.logical_type, Some(PrimitiveLogicalType::String));
                } else {
                    assert_eq!(pt.logical_type, None);
                }
                println!(
                    "field {i} = {} {:?} {:?} logical={:?}",
                    pt.field_info.name, pt.physical_type, pt.field_info.repetition, pt.logical_type
                );
            }
            ParquetType::GroupType { .. } => unreachable!("flat schema expected"),
        }
    }
    for (rg, group) in md.row_groups.iter().enumerate() {
        assert_eq!(group.num_rows(), ROWS_PER_RG[rg]);
        assert_eq!(group.columns().len(), 4);
        println!(
            "rg {rg}: rows={} bytes={}",
            group.num_rows(),
            group.total_byte_size()
        );
    }
    for (rg, group) in md.row_groups.iter().enumerate() {
        for col in group.columns() {
            let name = &col.descriptor().path_in_schema[0];
            let st = col.statistics().unwrap().unwrap();
            println!("stats rg{rg} {name}: {}", stats_line(st.as_ref(), col));
        }
    }
}

/// Read back the four columns per row group and value-assert the fixed source (f64 by to_bits).
fn inspect_values(bytes: &[u8], data: &Data, label: &str, md: &FileMetaData) {
    let mut reader = Cursor::new(bytes.to_vec());
    for (rg, group) in md.row_groups.iter().enumerate() {
        let cols = group.columns();
        let d_i32 = read_primitive_column::<i32>(&mut reader, &cols[0]).unwrap();
        let d_i64 = read_primitive_column::<i64>(&mut reader, &cols[1]).unwrap();
        let d_f64 = read_primitive_column::<f64>(&mut reader, &cols[2]).unwrap();
        let d_utf8 = read_binary_column(&mut reader, &cols[3]).unwrap();
        assert_eq!(d_i32, data.i32_rgs[rg]);
        assert_eq!(d_i64, data.i64_rgs[rg]);
        let to_bits = |v: &[Option<f64>]| v.iter().map(|x| x.map(f64::to_bits)).collect::<Vec<_>>();
        assert_eq!(to_bits(&d_f64), to_bits(&data.f64_rgs[rg]));
        assert_eq!(d_utf8, data.utf8_rgs[rg]);
        println!("{label} rg {rg} decode ok rows={}", d_i32.len());
    }
}

fn main() {
    let data = sample_data();

    println!("== write ==");
    let plain = write_table(&data, CompressionOptions::Uncompressed).unwrap();
    println!("plain bytes = {}", plain.len());
    println!("plain fnv = {:016x}", fnv1a(&plain));
    let snappy = write_table(&data, CompressionOptions::Snappy).unwrap();
    println!("snappy bytes = {}", snappy.len());
    println!("snappy fnv = {:016x}", fnv1a(&snappy));
    println!("snappy smaller = {}", snappy.len() < plain.len());

    println!("== meta ==");
    let md_plain = read_metadata(&mut Cursor::new(plain.clone())).unwrap();
    inspect_meta(&md_plain, "plain", true);
    let md_snappy = read_metadata(&mut Cursor::new(snappy.clone())).unwrap();
    inspect_meta(&md_snappy, "snappy", false);

    // Both files must hold identical logical content: 1) column-chunk statistics compare
    // equal one by one (dyn Statistics PartialEq via downcast) -> print bool; 2) values next.
    let mut stats_same = true;
    for rg in 0..3 {
        for c in 0..4 {
            let a = md_plain.row_groups[rg].columns()[c]
                .statistics()
                .unwrap()
                .unwrap();
            let b = md_snappy.row_groups[rg].columns()[c]
                .statistics()
                .unwrap()
                .unwrap();
            stats_same &= a.as_ref() == b.as_ref();
            assert_eq!(a.as_ref(), b.as_ref());
        }
    }
    println!("stats same as plain = {stats_same}");

    println!("== decode ==");
    inspect_values(&plain, &data, "plain", &md_plain);
    inspect_values(&snappy, &data, "snappy", &md_snappy);

    println!("roundtrip ok");
}
