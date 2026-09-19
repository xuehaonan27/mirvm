#!/usr/bin/env mirvm
---
[dependencies]
syn = { version = "2", features = ["full", "parsing", "printing", "visit", "visit-mut", "extra-traits"] }
quote = "1"
proc-macro2 = { version = "1", features = ["span-locations"] }
---
// syn 2 at runtime (not proc-macro expansion): parse_file over an embedded realistic source
// (fn/struct/enum/impl/trait/generics/where/async/const/static/macro_rules!/attributes/doc
// comments); counts Item kinds and Visit hits (calls/lifetimes/unsafe blocks); re-prints via
// ToTokens, rewrites integer literals with visit-mut, checks error positions; deep AST, heavy drop.
use quote::ToTokens;
use std::collections::BTreeMap;
use syn::visit::{self, Visit};
use syn::visit_mut::{self, VisitMut};

const SRC: &str = r##"//! A small in-memory event store with typed projections.
//! Fixture text for the syn parse driver.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fmt::{self, Display};
use std::marker::PhantomData;

/// Maximum number of events kept per stream.
pub const MAX_EVENTS_PER_STREAM: usize = 4096;

/// Global toggle, flipped by the tracing layer.
pub static mut TRACING_ENABLED: bool = false;

static STREAM_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Identifier for a stream of events.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StreamId(String);

impl StreamId {
    /// Create a new id, rejecting empty names.
    pub fn new(name: &str) -> Result<StreamId, StoreError> {
        if name.is_empty() {
            Err(StoreError::InvalidStream("empty name".to_string()))
        } else {
            Ok(StreamId(name.to_ascii_lowercase()))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for StreamId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "stream:{}", self.0)
    }
}

/// Errors surfaced by the store.
#[derive(Debug)]
pub enum StoreError {
    InvalidStream(String),
    Conflict { expected: u64, actual: u64 },
    ProjectorFailed { name: &'static str, source: Option<Box<StoreError>> },
}

/// A recorded event with a monotonically increasing position.
#[derive(Debug, Clone)]
pub struct Event<T> {
    pub position: u64,
    pub payload: T,
    pub metadata: BTreeMap<String, String>,
}

/// Fixed-size scratch buffer used by the compressors.
#[derive(Debug, Clone, Copy)]
pub struct Scratch<const N: usize> {
    pub data: [u8; N],
    pub len: usize,
}

/// Something that can fold events into a projection.
pub trait Projector<'a, E>
where
    E: 'a + Clone,
{
    type State: Default;
    type Output;

    fn name(&self) -> &'static str;
    fn fold(&self, state: &mut Self::State, event: &'a E) -> Result<(), StoreError>;
    fn finish(&self, state: Self::State) -> Self::Output;
}

pub trait Snapshotter: Send + Sync {
    fn snapshot(&self) -> Option<Vec<u8>> {
        None
    }
}

/// In-memory store, generic over the payload type.
pub struct EventStore<T, S = ()> {
    streams: BTreeMap<StreamId, Vec<Event<T>>>,
    _marker: PhantomData<S>,
}

impl<T, S> Default for EventStore<T, S> {
    fn default() -> Self {
        EventStore { streams: BTreeMap::new(), _marker: PhantomData }
    }
}

impl<T, S> EventStore<T, S>
where
    T: Clone,
{
    /// Append `payload` to `stream` at the next position.
    pub fn append(&mut self, stream: StreamId, payload: T) -> Result<u64, StoreError> {
        let events = self.streams.entry(stream).or_default();
        if events.len() >= MAX_EVENTS_PER_STREAM {
            return Err(StoreError::InvalidStream("stream full".into()));
        }
        let position = events.len() as u64;
        events.push(Event { position, payload, metadata: BTreeMap::new() });
        Ok(position)
    }

    /// Append without checking the per-stream bound.
    pub unsafe fn append_unchecked(&mut self, stream: StreamId, payload: T) -> u64 {
        let events = self.streams.entry(stream).or_insert_with(Vec::new);
        let position = events.len() as u64;
        events.push(Event { position, payload, metadata: BTreeMap::new() });
        position
    }

    /// Run a projector across every event in `stream`.
    pub fn project<'a, P>(&'a self, stream: &StreamId, projector: &P) -> Result<P::Output, StoreError>
    where
        P: Projector<'a, Event<T>>,
    {
        let mut state = P::State::default();
        if let Some(events) = self.streams.get(stream) {
            for event in events {
                projector.fold(&mut state, event)?;
            }
        }
        Ok(projector.finish(state))
    }
}

impl<T, S> EventStore<T, S> {
    pub fn streams_mut(&mut self) -> &mut BTreeMap<StreamId, Vec<Event<T>>> {
        &mut self.streams
    }
}

/// Fetch events from a remote peer (sketch).
pub async fn replicate<T: Clone>(store: &mut EventStore<T>, peer: &str) -> Result<usize, StoreError> {
    let fetched = unsafe { fetch_remote_unchecked(peer).await };
    let mut applied = 0usize;
    for (id, payload) in fetched {
        store.append(id, payload)?;
        applied += 1;
    }
    Ok(applied)
}

async unsafe fn fetch_remote_unchecked<T>(_peer: &str) -> Vec<(StreamId, T)> {
    Vec::new()
}

/// Copy `len` bytes from `src` into a fresh `Vec<u8>`.
pub unsafe fn leak_bytes(src: *const u8, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    unsafe {
        std::ptr::copy_nonoverlapping(src, out.as_mut_ptr(), len);
        out.set_len(len);
    }
    out
}

#[cfg(feature = "compression")]
mod compressed {
    //! Compression support, compiled in optionally.

    use super::EventStore;

    /// Compress the streams in `store` in place.
    pub fn shrink<T: Clone>(store: &mut EventStore<T>) -> usize {
        store.streams_mut().len()
    }
}

macro_rules! define_counter_projector {
    ($name:ident, $out:ty) => {
        /// Counts events, generated by `define_counter_projector!`.
        pub struct $name;

        impl<'a, E> Projector<'a, E> for $name
        where
            E: 'a + Clone,
        {
            type State = u64;
            type Output = $out;

            fn name(&self) -> &'static str {
                stringify!($name)
            }

            fn fold(&self, state: &mut Self::State, _event: &'a E) -> Result<(), StoreError> {
                *state += 1;
                Ok(())
            }

            fn finish(&self, state: Self::State) -> Self::Output {
                state as $out
            }
        }
    };
}

define_counter_projector!(CountAll, u64);
define_counter_projector!(CountWide, u128);

#[doc(hidden)]
pub type Result<T> = std::result::Result<T, StoreError>;

pub union RawOrOwned {
    pub raw: *const u8,
    pub owned: std::mem::ManuallyDrop<Vec<u8>>,
}

#[repr(C)]
pub struct Header {
    pub magic: u32,
    pub version: u16,
    pub flags: u16,
}

impl Header {
    /// Read a header from a raw pointer without validation.
    pub unsafe fn from_raw(ptr: *const u8) -> Header {
        unsafe { std::ptr::read(ptr as *const Header) }
    }
}

extern crate alloc as rust_alloc;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_then_project() {
        let mut store: EventStore<String> = EventStore::default();
        let id = StreamId::new("orders").unwrap();
        store.append(id.clone(), "created".to_string()).unwrap();
        let count = store.project(&id, &CountAll).unwrap();
        assert_eq!(count, 1u64);
    }

    #[test]
    fn rejects_empty_stream() {
        assert!(StreamId::new("").is_err());
    }
}
"##;

const BAD_SRC: &str = "pub fn total(pairs: &[(String, u64)]) -> u64 {\n    let mut acc = 0u64;\n    for (name, value) in pairs {\n        acc += value * ;\n    }\n    acc\n}";

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

// Hand-written hex: avoids `{:x}` (LowerHex goes through an indirect fn-ptr in core::fmt)
fn hex64(v: u64) -> String {
    let mut s = String::with_capacity(16);
    for i in (0..16).rev() {
        let nib = ((v >> (i * 4)) & 0xf) as u32;
        s.push(char::from_digit(nib, 16).unwrap());
    }
    s
}

#[derive(Default)]
struct Stats {
    call_exprs: usize,
    method_calls: usize,
    lifetimes: usize,
    unsafe_blocks: usize,
    attributes: usize,
    doc_attributes: usize,
    macros: usize,
}

impl<'ast> Visit<'ast> for Stats {
    fn visit_expr_call(&mut self, node: &'ast syn::ExprCall) {
        self.call_exprs += 1;
        visit::visit_expr_call(self, node);
    }
    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        self.method_calls += 1;
        visit::visit_expr_method_call(self, node);
    }
    fn visit_lifetime(&mut self, node: &'ast syn::Lifetime) {
        self.lifetimes += 1;
        visit::visit_lifetime(self, node);
    }
    fn visit_expr_unsafe(&mut self, node: &'ast syn::ExprUnsafe) {
        self.unsafe_blocks += 1;
        visit::visit_expr_unsafe(self, node);
    }
    fn visit_attribute(&mut self, node: &'ast syn::Attribute) {
        self.attributes += 1;
        if node.path().is_ident("doc") {
            self.doc_attributes += 1;
        }
        visit::visit_attribute(self, node);
    }
    fn visit_macro(&mut self, node: &'ast syn::Macro) {
        self.macros += 1;
        visit::visit_macro(self, node);
    }
}

struct BumpInts(usize);

impl VisitMut for BumpInts {
    fn visit_lit_int_mut(&mut self, node: &mut syn::LitInt) {
        if let Ok(v) = node.base10_parse::<u64>() {
            *node = syn::LitInt::new(&(v + 1).to_string(), node.span());
            self.0 += 1;
        }
        visit_mut::visit_lit_int_mut(self, node);
    }
}

fn main() {
    // ① parse_file + Item kind counts (BTreeMap keeps order)
    let file = syn::parse_file(SRC).expect("fixture parses");
    let mut kinds: BTreeMap<&'static str, usize> = BTreeMap::new();
    for item in &file.items {
        let kind = match item {
            syn::Item::Const(_) => "const",
            syn::Item::Enum(_) => "enum",
            syn::Item::ExternCrate(_) => "extern_crate",
            syn::Item::Fn(_) => "fn",
            syn::Item::ForeignMod(_) => "foreign_mod",
            syn::Item::Impl(_) => "impl",
            syn::Item::Macro(_) => "macro",
            syn::Item::Mod(_) => "mod",
            syn::Item::Static(_) => "static",
            syn::Item::Struct(_) => "struct",
            syn::Item::Trait(_) => "trait",
            syn::Item::TraitAlias(_) => "trait_alias",
            syn::Item::Type(_) => "type",
            syn::Item::Union(_) => "union",
            syn::Item::Use(_) => "use",
            syn::Item::Verbatim(_) => "verbatim",
            _ => "other",
        };
        *kinds.entry(kind).or_default() += 1;
    }
    println!("== items ==");
    for (k, v) in &kinds {
        println!("item {k} = {v}");
    }
    println!("items total = {}", file.items.len());
    println!("file attrs = {}", file.attrs.len());

    // ② Visit: call expressions / method calls / lifetimes / unsafe blocks / attributes / macros
    let mut stats = Stats::default();
    stats.visit_file(&file);
    println!("== visit ==");
    println!("call exprs = {}", stats.call_exprs);
    println!("method calls = {}", stats.method_calls);
    println!("lifetimes = {}", stats.lifetimes);
    println!("unsafe blocks = {}", stats.unsafe_blocks);
    println!("attributes = {}", stats.attributes);
    println!("doc attributes = {}", stats.doc_attributes);
    println!("macros = {}", stats.macros);

    // ③ ToTokens: whole-file re-print + roundtrip; one item re-printed for length and first 80 chars
    let whole = file.to_token_stream().to_string();
    println!("== tokens ==");
    println!("whole tokens len = {}", whole.len());
    println!("whole tokens fnv1a64 = 0x{}", hex64(fnv1a64(whole.as_bytes())));
    let reparsed = syn::parse_file(&whole).expect("roundtrip reparses");
    println!("roundtrip items = {}", reparsed.items.len());

    let projector = file
        .items
        .iter()
        .find_map(|i| match i {
            syn::Item::Trait(t) if t.ident == "Projector" => Some(t),
            _ => None,
        })
        .expect("Projector trait present");
    let pts = projector.to_token_stream().to_string();
    println!("Projector tokens len = {}", pts.len());
    let head: String = pts.chars().take(80).collect();
    println!("Projector head80 = {head:?}");

    // ④ visit-mut: bump every integer literal by 1, then re-print and fingerprint
    let mut mutated = file.clone();
    let mut bump = BumpInts(0);
    bump.visit_file_mut(&mut mutated);
    let mts = mutated.to_token_stream().to_string();
    println!("== visit-mut ==");
    println!("ints bumped = {}", bump.0);
    println!("mutated tokens len = {}", mts.len());
    println!("mutated fnv1a64 = 0x{}", hex64(fnv1a64(mts.as_bytes())));

    // ⑤ extra-traits Debug + deliberately broken source: error line/column
    let op: syn::BinOp = syn::parse_str("+").expect("binop parses");
    println!("== extra ==");
    println!("binop debug = {op:?}");
    let err = syn::parse_file(BAD_SRC).expect_err("bad src fails");
    let sp = err.span().start();
    println!("== errors ==");
    println!("bad err at line {} col {}", sp.line, sp.column);
    println!("bad err msg = {err}");
}
