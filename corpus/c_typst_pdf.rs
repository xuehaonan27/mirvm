#!/usr/bin/env mirvm
---
[dependencies]
typst = "=0.15.1"
typst-layout = "=0.15.1"
typst-pdf = "=0.15.1"
typst-assets = { version = "=0.15.1", features = ["fonts"] }
---
// 【状态：expected-red（引擎红③）】typst 0.15.1（crates.io 2026-07-18
// max_stable_version 实勘）编译定值小文档为 PDF 的三维差分。
// B（native）绿且两连跑逐字节一致；A（mirvm 默认）/C（逢调即编 JIT）同型
// TRAP 红（exit 70），诊断链见末节。driver 本体经 native 双重验证确定无虞。
//
// 覆盖清单：
//   ① 文档面：多段落（#lorem 定值词库 ×8 段）/ ATX 标题三级（= ==）/
//      无序+有序列表 / 3 列表格（表头+4 行）/ A6 小页多页断页 /
//      页码计数（numbering "1 / 1"：counter(page) 与总页数内省）/
//      justify 段落换行与字形排版全计算。
//   ② 管线面：最小 World（内嵌 FontBook/Source/定值 today）→
//      typst::compile::<PagedDocument>（页数打印）→ typst_pdf::pdf
//      （PdfOptions::default）→ PDF 字节 len + FNV-1a 锚定。
//   ③ 字体面：typst-assets 内嵌字体全 face 装载（Font::iter + FontBook，
//      17 face），不触系统字体；face 计数打印。
//
// 确定性说明（PDF 字节两侧逐字节一致的前提，源码出处逐条可核）：
//   · PdfOptions::default() = ident:Auto / creator:Auto / timestamp:None /
//     page_ranges:None / standards:默认 / tagged:true / pretty:false。
//     typst-pdf/src/metadata.rs：document.date=auto 且 options.timestamp=None
//     → 不写 /CreationDate（无壁钟）；ident:Auto → title+author 哈希，
//     本文档两者均未设 → 定值；creator:Auto → "Typst 0.15.1" 定值串。
//   · 单线程编译（comemo 无并行）；无 rayon/真随机/HashMap 迭代序出口。
//   · today() 钉 2020-01-01（本文档未调 datetime，防御性固定）。
//   · 文档纯 ASCII + #lorem 定值词库 → 无字体回退警告（warnings=0 锚定）。
//   · 不打印浮点；PDF 内部全部坐标计算结果由字节 FNV 覆盖。
//
// 绕行/钉版本记录：
//   · 四 crate 同钉 =0.15.1（typst/typst-layout/typst-pdf/typst-assets 同
//     发布序列配对）。自 0.15 起 PDF 导出为独立 crate typst-pdf（typst 本体
//     无 pdf feature，PagedDocument 由 typst-layout 具名）——非绕行，上游
//     结构如此。
//   · typst-assets 0.15 起 fonts 为可选 feature 且默认关（docs.rs 注明
//     "returns an empty iterator if the fonts feature is disabled"）——
//     显式开 features=["fonts"]，否则 0 字体（首跑实测 faces=0 实证）。
//   · 无官方开关/force-soft 类绕行；无规格裁剪。
//
// 红因诊断链（红③：引擎语义缺口——M4.1 非标量 SIMD place）：
//   1) B native 两连跑：stdout 五行逐字节一致、stderr 真空、exit 0——
//      driver 自身确定性成立。期望锚定：
//        doc bytes = 471 fnv = e0fe23f76e9cd0e9
//        embedded font faces = 17
//        warnings = 0
//        pages = 3
//        pdf bytes = 24770 fnv = a07292af73881d72
//   2) A mirvm 默认：stdout 前两行与 native 逐字节一致（doc fnv、faces=17），
//      进入 typst::compile 即 TRAP（exit 70，冷构建+跑 2m51s/热跑 52s）：
//        mirvm[m4-engine]: TRAP: 非标量 place（ty=std::arch::x86_64::__m128i，
//        M4.1）（fn portable_atomic::imp::atomic128::x86_64::
//        __atomic_load_vmovdqa）
//   3) C 逢调即编 JIT：同型同码 TRAP（exit 70，11s 即落雷）——place 校验在
//      解释/JIT 共用入口层，两维同栈。
//   4) 依赖链：typst-utils 0.15.1 src/hash.rs `HashLock(AtomicU128)`
//      （LazyHash 128 位哈希锁，typst 编译主路径必经）→ portable-atomic
//      1.14.0 x86_64 atomic128：SSE 基线下 load 静态选 vmovdqa 内联汇编
//      （out xmm_reg → __m128i 类型 place）；M4.1 只收标量 place → 响亮
//      TRAP。与 open-issues R17「SIMD 向量按值」同族但位于解释器核心
//      place 层，非 FFI 边界。
//   5) 最小复现（已单独触雷）：单依赖 portable-atomic =1.14.0 的探针做
//      AtomicU128::new/load/store——native 三行定值 exit 0；mirvm 同型
//      TRAP exit 70（探针体见批10汇报，/tmp/m128i_probe.rs 即抛即弃）。
//   6) 绕行排查（无合法解）：portable-atomic 无禁用 atomic128 的官方
//      feature/cfg（其 fallback feature 只管 aarch64/riscv outline
//      atomics；x86_64 由 cmpxchg16b 探测+SSE 基线静态选定）；cargo-script
//      frontmatter 无法注入 RUSTFLAGS；typst-utils 为 typst 全栈硬依赖
//      不可摘。
// 接线建议：red_code=70；red_pattern="TRAP: 非标量 place（ty=std::arch::x86_64::__m128i"。
use typst::diag::{FileError, FileResult};
use typst::foundations::{Bytes, Datetime, Duration};
use typst::syntax::{FileId, Source};
use typst::text::{Font, FontBook};
use typst::utils::LazyHash;
use typst::{Library, LibraryExt, World};
use typst_layout::PagedDocument;

/// 内联 FNV-1a（二进制内容锚定，不打印原始字节）。
fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// 定值小文档：多段落 + 三级标题 + 列表 + 表格 + 页码计数（"1 / 总页数"）。
/// A6 小页强制多页断页；纯 ASCII 保内嵌字体全覆盖、warnings 为零。
const DOC: &str = r#"#set page(paper: "a6", margin: (x: 12mm, y: 14mm), numbering: "1 / 1")
#set par(justify: true)

= Overview
#lorem(24)

== Scope
#lorem(36)

- apple pie
- banana split
- cherry tart

+ gather data
+ run the probe
+ compare bytes

#lorem(45)

#table(
  columns: 3,
  [name], [qty], [price],
  [apple], [3], [1.50],
  [banana], [6], [0.75],
  [cherry], [12], [0.25],
  [plum], [7], [2.00],
)

#lorem(50)

= Details
#lorem(40)

== Numbers
#lorem(30)

= Conclusion
#lorem(25)
"#;

/// 最小 World：内嵌字体书 + 单 detached 源文件 + 定值日期。
struct MiniWorld {
    library: LazyHash<Library>,
    book: LazyHash<FontBook>,
    fonts: Vec<Font>,
    source: Source,
}

impl MiniWorld {
    fn new(text: &str) -> Self {
        let mut book = FontBook::new();
        let fonts: Vec<Font> = typst_assets::fonts()
            .flat_map(|data| Font::iter(Bytes::new(data.to_vec())))
            .inspect(|font| book.push(font.info().clone()))
            .collect();
        Self {
            library: LazyHash::new(Library::default()),
            book: LazyHash::new(book),
            fonts,
            source: Source::detached(text.to_string()),
        }
    }

    fn missing(id: FileId) -> FileError {
        FileError::NotFound(std::path::PathBuf::from(id.vpath().get_without_slash()))
    }
}

impl World for MiniWorld {
    fn library(&self) -> &LazyHash<Library> {
        &self.library
    }

    fn book(&self) -> &LazyHash<FontBook> {
        &self.book
    }

    fn main(&self) -> FileId {
        self.source.id()
    }

    fn source(&self, id: FileId) -> FileResult<Source> {
        if id == self.source.id() {
            Ok(self.source.clone())
        } else {
            Err(Self::missing(id))
        }
    }

    fn file(&self, id: FileId) -> FileResult<Bytes> {
        if id == self.source.id() {
            Ok(Bytes::new(self.source.text().as_bytes().to_vec()))
        } else {
            Err(Self::missing(id))
        }
    }

    fn font(&self, index: usize) -> Option<Font> {
        self.fonts.get(index).cloned()
    }

    fn today(&self, _offset: Option<Duration>) -> Option<Datetime> {
        Datetime::from_ymd(2020, 1, 1)
    }
}

fn main() {
    let world = MiniWorld::new(DOC);
    println!(
        "doc bytes = {} fnv = {:016x}",
        DOC.len(),
        fnv1a(DOC.as_bytes())
    );
    println!("embedded font faces = {}", world.fonts.len());

    // 编译：PagedDocument（排版出帧，一页一帧）。
    let warned = typst::compile::<PagedDocument>(&world);
    println!("warnings = {}", warned.warnings.len());
    for w in warned.warnings.iter() {
        println!("warning: {}", w.message);
    }
    let doc = match warned.output {
        Ok(doc) => doc,
        Err(errors) => {
            println!("errors = {}", errors.len());
            for e in errors.iter() {
                println!("error: {}", e.message);
            }
            std::process::exit(2);
        }
    };
    println!("pages = {}", doc.pages().len());

    // 导出：默认 PdfOptions（无时间戳/无外部标识），PDF 字节整体锚定。
    let pdf = match typst_pdf::pdf(&doc, &typst_pdf::PdfOptions::default()) {
        Ok(bytes) => bytes,
        Err(errors) => {
            println!("pdf errors = {}", errors.len());
            for e in errors.iter() {
                println!("pdf error: {}", e.message);
            }
            std::process::exit(3);
        }
    };
    println!("pdf bytes = {} fnv = {:016x}", pdf.len(), fnv1a(&pdf));
}
