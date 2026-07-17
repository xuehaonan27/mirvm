#!/usr/bin/env mirvm
---
[dependencies]
# parquet2 0.17.2（0.17.x 最新 patch 也是 parquet2 全线的最后发布，
# 2023-04-13 发布于 jorgecarleitao/parquet2，此后生态由 arrow2 系接棒，
# 故 =0.17.2 钉死即到顶，无后续漂移风险）。
# feature 钉选理由：默认 default = snappy+gzip+lz4+zstd+brotli+bloom_filter，
# 其中 lz4 feature 拉的是 C FFI 的 lz4/1.24 crate（lz4-sys）、zstd 同理 C FFI，
# 任务口径要求纯 Rust 压缩面 → default-features=false 只留 snappy。
# parquet SNAPPY 编解码是 raw snappy 块（parquet2 内部走 snap::raw::Encoder/
# Decoder），不经 snap 的 frame 层 → 不触 c_lz4_snap 记录过的 frame 层掩码
# crc32c SSE4.2 运行期派发（且该 intrinsic 族 2026-07-15 已内建，双保险）。
# thrift 系核查结论：parquet2 全系无 thrift runtime 依赖——元数据走
# parquet-format-safe 0.2（官方 generated safe bindings，零 mandatory 依赖、
# 自带 compact protocol 读写），无需裁剪。
# 闭包清单（5 crate、全纯 Rust、无 build.rs C 编译）：parquet2 0.17.2 /
# parquet-format-safe 0.2.x / seq-macro 0.3 / snap 1.x / streaming-decompression 0.1。
parquet2 = { version = "=0.17.2", default-features = false, features = ["snappy"] }
---
// parquet2 0.17.2 三维差分：向内存 Vec<u8>（Cursor）写一张小表、按
// Uncompressed 与 Snappy 两档各写一遍 → 字节长度 + FNV-1a 锚定 → 内存读回
// 断言行数/schema/逐值（含 nullable def-level 解码）→ 打印 meta 关键统计。
//
// 覆盖测试面：
//   schema  ：4 个 nullable 基元列 i32_col(Int32) / i64_col(Int64) /
//             f64_col(Double) / utf8_col(ByteArray + logical String)，
//             由元数据读回并逐列断言名字/物理类型/Repetition/logical_type
//   行组    ：3 个 row group，行数 8/6/7 不等（总 21），每列单页
//   编码    ：全 Plain 编码 + RLE def-level（全部 Optional 列，每列至少
//             一个 null；特定行组 nulls=0 / nulls=2 两档覆盖）
//   数据    ：i32 含 MIN/MAX 极值；i64 含 MIN/MAX、±1<<40 大数；
//             f64 含 2^53 外大数/负零与零并存（f64 值断言按 to_bits 比，
//             打印按 bits 锁位型）；utf8 含空串/None/重复串（可压缩）
//   压缩    ：Uncompressed 与 Snappy 两份完整文件，印刷 codec/未压缩/
//             压缩尺寸，两文件逻辑内容完全相同（值断言与统计相等断言
//             双路复核）
//   统计    ：write_statistics=true → 页头 min/max/null_count（thrift
//             序列化）+ FileWriter::end 写 column index/offset index；
//             读回 downcast PrimitiveStatistics<i32|i64|f64> /
//             BinaryStatistics 打印并按 PartialEq 跨两份文件断言相等
//   元数据  ：created_by 固定串与 key_value_metadata 固定 kv（thrift 回调
//             读）；format version=1(V1)；read_metadata 走
//             parquet-format-safe compact protocol（纯 Rust）
//
// 确定性：全部值为字面量常量，无随机/时间/地址/HashMap 序/线程；压缩器对
// 同输入逐字节确定，FNV 锚定全文件字节；浮点永不直打，一律 to_bits。
// 断言失败走 panic（stderr 正常路径真空）。
//
// 三维复跑命令（仓库根）：
//   A: target/release/mirvm run corpus/c_parquet2_rw.rs
//   B: cd $(grep -l 'name = "c_parquet2_rw"' ~/.cache/mirvm/scripts/*/Cargo.toml \
//        | head -1 | xargs dirname) && \
//      RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" \
//      "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run corpus/c_parquet2_rw.rs
//
// FRONTIER：无（预期）。thrift compact 元数据/RLE 位打包/snap raw 块压缩
// 均为纯 Rust 无 SIMD 面。
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
// 定值数据：3 行组（8/6/7 行），4 nullable 列同源供两档压缩各写一次。
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
// 写侧：schema → Plain 编码页（V1 头 + RLE def-level + 页统计）→ FileWriter。
// ---------------------------------------------------------------------------

/// 4 个全部 Optional 的基元列；utf8_col 挂 logical String。
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

/// 定值序列 → (values LE 字节, 4B 长度头 + RLE def-level 位图)。
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

/// 基元列定值数组 → V1 数据页（Plain 值区 + RLE def-level + min/max/null_count）。
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

/// 二进制列定值数组 → V1 数据页（Plain「len LE + bytes」值区 + 统计）。
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

/// 整表写进内存 Vec：3 个 row group，固定 created_by 与 kv 元数据。
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
// 读侧：元数据 → 行组/列块 → 解压 → 解码 V1 页（Plain + def-level）。
// ---------------------------------------------------------------------------

/// def-level 驱动解码：合法的巷值流（复制自 parquet2 官方 it/read/utils.rs）。
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

/// 本 driver 只写 Plain 编码，字典页永不出险；P 取占位类型，字典臂不可达。
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
// 打印与断言
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

/// 断言元数据与 schema（行组数/列数/字段名/物理类型/可空性/逻辑类型），
/// full=true 时额外打行组尺寸与 12 条列块统计行。
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

/// 逐行组读回四列并与定值源数组逐值断言（f64 经 to_bits 比位型）。
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

    // 两文件逻辑内容须完全相同：1) 列块统计逐一相等（按 dyn Statistics 的
    // PartialEq 走 downcast 结构比）→ 打印布尔；2) 逐值断言在下一节。
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
