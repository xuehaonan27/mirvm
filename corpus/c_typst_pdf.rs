#!/usr/bin/env mirvm
---
[dependencies]
typst = "=0.15.1"
typst-layout = "=0.15.1"
typst-pdf = "=0.15.1"
typst-assets = { version = "=0.15.1", features = ["fonts"] }
---
// typst 0.15.1 compiles a fixed little document to PDF; the oracle compares every
// byte with native. Status: expected-red. Native is green and byte-stable across
// two runs, while mirvm default and JIT both TRAP with exit 70 at typst::compile;
// the driver itself is verified good.
// Document surface: multiple paragraphs (8 x #lorem with the fixed word list),
// three ATX heading levels (= and ==), unordered and ordered lists, a 3-column
// table (header + 4 rows), an A6 page small enough to force several page breaks,
// page numbering via "1 / 1" (counter(page) plus total-page introspection) and
// fully computed justified line breaking and glyph layout.
// Pipeline surface: a minimal World (embedded FontBook/Source plus a fixed today)
// -> typst::compile::<PagedDocument> (page count printed) -> typst_pdf::pdf with
// PdfOptions::default -> PDF byte length plus an FNV-1a anchor.
// Font surface: every face of the typst-assets embedded fonts is loaded through
// Font::iter and FontBook (17 faces); no system font is touched and the face
// count is printed.
// Determinism (what makes the PDF bytes identical on both sides): PdfOptions
// defaults are ident:Auto / creator:Auto / timestamp:None / page_ranges:None /
// standards default / tagged:true / pretty:false. With document.date=auto and
// timestamp None the exporter writes no /CreationDate, so no wall clock enters;
// ident:Auto hashes title+author and both are unset here; creator:Auto is the
// fixed "Typst 0.15.1" string. Compilation is single-threaded (comemo, no rayon)
// with no real randomness and no HashMap-order output; today() is pinned to
// 2020-01-01; the document is pure ASCII over a fixed word list, so there are no
// font fallback warnings (warnings = 0 is an anchor). No float is printed; every
// coordinate the PDF contains is covered by the byte FNV.
// Version pin: typst, typst-layout, typst-pdf and typst-assets are all pinned to
// =0.15.1 because they are published as one matching sequence. Since 0.15 the PDF
// exporter is its own typst-pdf crate (typst itself has no pdf feature and
// PagedDocument is named by typst-layout), so this is upstream structure rather
// than a workaround. typst-assets 0.15 made fonts an optional feature that is off
// by default (docs.rs: "returns an empty iterator if the fonts feature is
// disabled"), hence features=["fonts"]; without it the run loads 0 fonts.
// The TRAP comes from portable-atomic 1.14.0: on x86_64 with the SSE baseline its
// atomic128 load selects a vmovdqa inline-asm path whose output is an
// std::arch::x86_64::__m128i place. typst-utils 0.15.1 uses HashLock(AtomicU128)
// on the main compile path, so the first compile step that touches the lock
// reaches that instruction. mirvm only accepts scalar places, so it aborts
// loudly. The interpreter and the JIT share that place-check entry point, so both
// fail on the same instruction. No official switch disables atomic128 (the
// fallback features cover aarch64/riscv outline atomics only), cargo-script
// frontmatter cannot inject RUSTFLAGS and typst-utils cannot be removed.
// Wiring: red_code=70; red_pattern="TRAP: 非标量 place（ty=std::arch::x86_64::__m128i".
//
//
//
//
//
//
//
//
//
//
//
//
//
//
//
//
//
//
//
//
//
//
//
//
//
use typst::diag::{FileError, FileResult};
use typst::foundations::{Bytes, Datetime, Duration};
use typst::syntax::{FileId, Source};
use typst::text::{Font, FontBook};
use typst::utils::LazyHash;
use typst::{Library, LibraryExt, World};
use typst_layout::PagedDocument;

/// Inline FNV-1a anchoring binary content (the raw bytes are never printed).
fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// The fixed little document: paragraphs, three heading levels, lists, a table and page numbering.
/// An A6 page forces page breaks; pure ASCII keeps the embedded fonts complete, warnings zero.
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

/// Minimal World: an embedded font book, one detached source file and a fixed date.
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

    // Compile: PagedDocument (layout produces frames, one per page).
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

    // Export: default PdfOptions (no timestamp, no external id); the whole PDF is anchored.
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
