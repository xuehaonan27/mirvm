//! The layered image stack: the pre-lowered layers a program session reuses, and the offset merge
//! that folds them into one module before the program runs.
//!
//! A stack is ordered bottom to top in topological order `[std base, dep_1, dep_2, ...]`. When a
//! program session lowers, it looks each **v0 symbol_name** up in the union and reuses hits: a
//! function hit reuses the FuncId (without enqueuing), a static hit reuses the address (materializing
//! a second copy would split `static mut` state and is not an option), and a TLS hit reuses the
//! TlsId. Delta fn/TLS/asm ids start after the stack totals (**offset merge**), so the interpreter
//! hot path is untouched.
//!
//! Each layer owns a fixed domain — the base at `BASE_IMAGE_FIXED_ADDR`, dependency images on the
//! spline `image_addr(k)` — so cross-domain absolute addresses are mutually stable and a layer file
//! stays valid in another process. An empty stack (no base image / bypassed) means full cold
//! lowering.

use crate::vm::ir;

/// A loaded layer, ready for a program session to use.
pub struct BaseImage {
    pub module: ir::Module,
    pub fn_by_sym: std::collections::HashMap<Box<str>, ir::FuncId>,
    pub entry_by_sym: std::collections::HashMap<Box<str>, u64>,
    pub static_by_sym: std::collections::HashMap<Box<str>, u64>,
    pub tls_by_sym: std::collections::HashMap<Box<str>, ir::TlsId>,
    pub lowering_fp: (bool, bool, bool),
    /// Layered cache key (referenced by ircache delta entries; includes the lowering fingerprint)
    pub key: String,
}

/// The base image plus a chain of dependency images.
pub struct ImageStack {
    images: Vec<BaseImage>,
    /// Union lookups (sym -> absolute id/address; image id domains are disjoint, so the union is unambiguous)
    fn_by_sym: std::collections::HashMap<Box<str>, ir::FuncId>,
    entry_by_sym: std::collections::HashMap<Box<str>, u64>,
    static_by_sym: std::collections::HashMap<Box<str>, u64>,
    tls_by_sym: std::collections::HashMap<Box<str>, ir::TlsId>,
    /// Delta id offset = the stack's cumulative counts
    total_fns: usize,
    total_tls: usize,
    total_asm: usize,
    /// Lowering fingerprint (uniform across the stack; construction truncates at the first divergence, self-healing)
    lowering_fp: (bool, bool, bool),
    /// Layered cache key chain (each image key joined by \x1f; `None` for an empty stack)
    key: Option<String>,
}

impl ImageStack {
    pub fn empty() -> Self {
        ImageStack {
            images: Vec::new(),
            fn_by_sym: Default::default(),
            entry_by_sym: Default::default(),
            static_by_sym: Default::default(),
            tls_by_sym: Default::default(),
            total_fns: 0,
            total_tls: 0,
            total_asm: 0,
            lowering_fp: (false, false, false),
            key: None,
        }
    }

    /// Ordered image list -> stack. A lowering-fingerprint divergence triggers **prefix
    /// truncation**: the diverging image and everything above it fall back to delta. This
    /// is self-healing and guarantees an image is never reused under the wrong fingerprint.
    pub(crate) fn from_images(mut images: Vec<BaseImage>) -> Self {
        if images.is_empty() {
            return Self::empty();
        }
        let fp = images[0].lowering_fp;
        if let Some(cut) = images.iter().position(|i| i.lowering_fp != fp) {
            images.truncate(cut);
        }
        if images.is_empty() {
            return Self::empty();
        }
        let mut s = ImageStack::empty();
        s.lowering_fp = fp;
        for img in &images {
            s.fold_image(img);
        }
        s.images = images;
        s
    }

    pub fn is_empty(&self) -> bool {
        self.images.is_empty()
    }

    /// Bottom-most base image (used for the below-key/lowering-fp layered check when
    /// loading dependency images; `None` for an empty stack)
    pub fn base_image(&self) -> Option<&BaseImage> {
        self.images.first()
    }
    pub fn total_fns(&self) -> usize {
        self.total_fns
    }
    pub fn total_tls(&self) -> usize {
        self.total_tls
    }
    pub fn total_asm(&self) -> usize {
        self.total_asm
    }
    pub fn fn_by_sym(&self) -> &std::collections::HashMap<Box<str>, ir::FuncId> {
        &self.fn_by_sym
    }
    pub fn entry_by_sym(&self) -> &std::collections::HashMap<Box<str>, u64> {
        &self.entry_by_sym
    }
    pub fn static_by_sym(&self) -> &std::collections::HashMap<Box<str>, u64> {
        &self.static_by_sym
    }
    pub fn tls_by_sym(&self) -> &std::collections::HashMap<Box<str>, ir::TlsId> {
        &self.tls_by_sym
    }
    pub fn key(&self) -> Option<&str> {
        self.key.as_deref()
    }

    /// Append one image (an in-memory split product): incrementally updates the union
    /// lookups, cumulative offsets and key chain. The caller guarantees fp matches the stack
    /// (built in the same session, so no truncation is needed).
    pub fn push(&mut self, img: BaseImage) {
        if self.images.is_empty() {
            self.lowering_fp = img.lowering_fp;
        }
        self.fold_image(&img);
        self.images.push(img);
    }

    /// Fold one image into the union lookups, the cumulative offsets and the key chain. Bottom
    /// layers win a name collision (`or_insert`), which is what makes the union unambiguous: image
    /// id domains are disjoint, so a repeated symbol can only come from a layer that already had it.
    fn fold_image(&mut self, img: &BaseImage) {
        for (k, v) in &img.fn_by_sym {
            self.fn_by_sym.entry(k.clone()).or_insert(*v);
        }
        for (k, v) in &img.entry_by_sym {
            self.entry_by_sym.entry(k.clone()).or_insert(*v);
        }
        for (k, v) in &img.static_by_sym {
            self.static_by_sym.entry(k.clone()).or_insert(*v);
        }
        for (k, v) in &img.tls_by_sym {
            self.tls_by_sym.entry(k.clone()).or_insert(*v);
        }
        self.total_fns += img.module.funcs.len();
        self.total_tls += img.module.tls.len();
        self.total_asm += img.module.asm_sites.len();
        match &mut self.key {
            Some(key) => {
                key.push('\u{1f}');
                key.push_str(&img.key);
            }
            None => self.key = Some(img.key.clone()),
        }
    }

    /// Check the session lowering fingerprint (called from cli's after_analysis). A mismatch
    /// discards the whole stack and lowers from scratch. `from_images` already made the
    /// stack's fp uniform, so one comparison covers the whole stack.
    pub fn fp_matches(&self, session_fp: (bool, bool, bool)) -> bool {
        self.is_empty() || self.lowering_fp == session_fp
    }

    /// Offset merge: concatenate `[stack...] ++ delta` into one table (delta fn/TLS/asm ids already
    /// start after the stack totals, and each image's funcs are stored in absolute FuncId order, so
    /// sequential concatenation position equals absolute id). After the merge, asm stubs are
    /// re-materialized: the stub addresses in an image file are live only in the build process, so
    /// they must be idempotently re-materialized here from the recipe (the same contract as a warm L2
    /// load). Each image's frozen region moves into `delta.image_frozens` to keep it alive.
    pub fn absorb_into(self, delta: &mut ir::Module) {
        let mut funcs = Vec::with_capacity(self.total_fns);
        let mut function_names = Vec::with_capacity(self.total_fns + delta.funcs.len());
        let mut tls = Vec::with_capacity(self.total_tls);
        let mut sites = Vec::with_capacity(self.total_asm);
        let mut frozens = Vec::with_capacity(self.images.len());
        for img in self.images {
            let mut m = img.module;
            function_names.append(&mut m.function_names);
            m.funcs.drain_into(&mut funcs);
            tls.append(&mut m.tls);
            sites.append(&mut m.asm_sites);
            for (a, f) in m.fn_addrs {
                delta.fn_addrs.entry(a).or_insert(f);
            }
            for (a, f) in m.link_fn_addrs {
                delta.link_fn_addrs.entry(a).or_insert(f);
            }
            for (s, f) in m.exports {
                delta.exports.entry(s).or_insert(f);
            }
            for l in m.native_libs {
                if !delta.native_libs.contains(&l) {
                    delta.native_libs.push(l);
                }
            }
            for l in m.required_native_libs {
                if !delta.required_native_libs.contains(&l) {
                    delta.required_native_libs.push(l);
                }
            }
            // The GOT merges with the image: syms are deduplicated by name and fixup indices
            // are renumbered. Image spline-domain addresses are fixed-base stable, so they
            // still point at the same frozen slot after the merge.
            delta.absorb_got(m.foreign_syms, m.got_fixups);
            delta.frozen_relocs.append(&mut m.frozen_relocs);
            // Entry stubs merge with the image: the recipe is attached per code domain and
            // rebuilt per domain at startup.
            if !m.entry_stub_sites.is_empty() || m.entry_stubs.is_mapped() {
                let home = m
                    .frozen
                    .as_ref()
                    .and_then(|f| crate::vm::addrlayout::code_home_for_frozen(f.home()))
                    .expect("image frozen region is invalid; stub code domain cannot be derived");
                delta.image_entry_stubs.push((
                    home,
                    std::mem::take(&mut m.entry_stub_sites),
                    std::mem::take(&mut m.entry_stubs),
                ));
            }
            if let Some(fr) = m.frozen {
                frozens.push(fr);
            }
        }
        delta.funcs.drain_into(&mut funcs);
        function_names.append(&mut delta.function_names);
        delta.funcs = funcs.into();
        delta.function_names = function_names;
        delta.ensure_function_names();
        tls.append(&mut delta.tls);
        delta.tls = tls;
        sites.append(&mut delta.asm_sites);
        delta.asm_sites = sites;
        delta.asm_stub_addrs = crate::lower::asm::materialize(&delta.asm_sites);
        // entry: delta is authoritative
        delta.image_frozens = frozens;
        delta.rebuild_load_map();
        delta.rebuild_fn_addrs();
    }
}
