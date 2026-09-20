//! Pre-lowered std base image: the full lowering product of a synthetic "empty main"
//! session (lang_start chain / panic / fmt / alloc machinery, ~3000 instances), with its
//! frozen region in the BASE_IMAGE_FIXED_ADDR domain and shared across programs.
//!
//! When a program session (delta) lowers, it looks each **v0 symbol_name** up in the base
//! image: a function hit reuses the FuncId (without enqueuing), a static hit reuses the
//! address (materializing a second copy would split `static mut` state and is not an
//! option), and a TLS hit reuses the TlsId. Delta fn/TLS/asm-stub ids start after the base
//! image's counts (**offset merge**): at load time base ++ delta are concatenated into one
//! table, so the interpreter hot path is untouched.
//!
//! Key and invalidation: the file name is fnv(build_id, sysroot stamp), and loading
//! re-checks that build_id and stamp are equal. The **lowering fingerprint** (the three
//! session booleans lower bakes into bytecode: ub/overflow/contract checks) must be
//! verified **within the session** (a cargo runner may pass custom profile flags); on
//! mismatch the base image is dropped and everything is lowered cold (self-heal, silent
//! fallback; same on build failure, with the log in base/build.log).
//! `MIRVM_NO_BASE_IMAGE=1` bypasses the base image entirely (differential/diagnostic use).

use std::path::PathBuf;
use std::process::ExitCode;

use rustc_driver::{Callbacks, Compilation};
use rustc_interface::interface::Compiler;
use rustc_middle::ty::TyCtxt;
use serde::{Deserialize, Serialize};

use crate::vm::ir;

/// Base image file (v1: one postcard blob).
///
/// **Byte-determinism contract** (two consecutive builds must compare equal): `Module`'s
/// exports/fn_addrs/link_fn_addrs are std HashMaps, whose RandomState seed makes iteration order
/// per-process random, so they cannot be written to disk inside the module -- they are
/// extracted into **sorted Vec** fields (and cleared inside the module) and rebuilt by the
/// loader. The remaining fields (funcs/tls/asm_sites/frozen region bytes) are determined
/// by lowering order.
#[derive(Serialize, Deserialize)]
struct BaseFile {
    build_id: String,
    /// sysroot freshness (same stamp source as the sysroot build)
    sysroot_stamp: String,
    /// (ub_checks, overflow_checks, contract_checks): the only session booleans lower bakes in
    lowering_fp: (bool, bool, bool),
    /// exports/fn_addrs/link_fn_addrs are cleared and asm_stub_addrs emptied (see above); rebuilt
    /// from the sorted tables below and rematerialized by the loader
    module: ir::Module,
    /// sym -> FuncId (the sorted form of module.exports)
    export_syms: Vec<(Box<str>, ir::FuncId)>,
    /// fn entry real address -> FuncId (the sorted form of module.fn_addrs)
    fn_addr_pairs: Vec<(u64, ir::FuncId)>,
    /// LinkAddr -> FuncId (the sorted form of module.link_fn_addrs). Sorted by the address: the
    /// keys are unique, so this is a total order and the table is reproducible.
    link_fn_addr_pairs: Vec<(ir::LinkAddr, ir::FuncId)>,
    /// sym -> fn entry real address (only functions whose address was taken have an entry)
    fn_entry_syms: Vec<(Box<str>, u64)>,
    static_syms: Vec<(Box<str>, u64)>,
    tls_syms: Vec<(Box<str>, ir::TlsId)>,
}

/// A loaded base image, ready for a program session to use.
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

fn disabled() -> bool {
    crate::options::get().no_base_image
}

fn base_dir() -> PathBuf {
    crate::options::get().cache_root().join("base")
}

/// (base image path, sysroot stamp). `None` when the stamp is unavailable (sysroot not
/// built yet, etc.), i.e. no base image.
fn locate() -> Option<(PathBuf, String)> {
    let stamp = crate::sysroot::current_stamp_value()?;
    let mut key = String::from(crate::options::build::BUILD_ID);
    key.push('\u{1f}');
    key.push_str(&stamp);
    let h = crate::utils::content::fnv1a(key.as_bytes());
    Some((base_dir().join(format!("{h:016x}.img")), stamp))
}

fn load(path: &std::path::Path, want_stamp: &str) -> Option<BaseImage> {
    let data = std::fs::read(path).ok()?;
    let f: BaseFile = postcard::from_bytes(&data).ok()?; // restores the frozen region at its fixed base; a taken region means a miss
    if f.build_id != crate::options::build::BUILD_ID || f.sysroot_stamp != want_stamp {
        return None;
    }
    // The frozen region must really land in the base-image domain (rejects a swapped file
    // or a taken domain alike)
    let frozen_ok = f.module.frozen.as_ref().is_some_and(|fr| {
        fr.at_fixed_base() && fr.home() == crate::vm::addrlayout::BASE_IMAGE_FIXED_ADDR
    });
    if !frozen_ok {
        return None;
    }
    let fp = f.lowering_fp;
    let mut key = String::from(crate::options::build::BUILD_ID);
    key.push('\u{1f}');
    key.push_str(want_stamp);
    key.push('\u{1f}');
    key.push_str(&format!("fp{}{}{}", fp.0 as u8, fp.1 as u8, fp.2 as u8));
    // Rebuild exports/fn_addrs from the sorted tables (byte-determinism contract, see BaseFile)
    let mut module = f.module;
    module.exports = f.export_syms.iter().cloned().collect();
    module.fn_addrs = f.fn_addr_pairs.iter().copied().collect();
    module.link_fn_addrs = f.link_fn_addr_pairs.iter().copied().collect();
    module.rebuild_load_map();
    module.rebuild_fn_addrs();
    crate::vm::verify::module(&module).ok()?;
    Some(BaseImage {
        fn_by_sym: f.export_syms.into_iter().collect(),
        entry_by_sym: f.fn_entry_syms.into_iter().collect(),
        static_by_sym: f.static_syms.into_iter().collect(),
        tls_by_sym: f.tls_syms.into_iter().collect(),
        lowering_fp: fp,
        key,
        module,
    })
}

/// Load the base image, or build it in a subprocess. Failure yields `None` (full cold
/// lowering, silent self-heal); the build log goes to base/build.log because stderr
/// participates in native differential comparison, so the main path must stay quiet.
fn ensure_base() -> Option<BaseImage> {
    if disabled() {
        return None;
    }
    let (path, stamp) = locate()?;
    if let Some(b) = load(&path, &stamp) {
        return Some(b);
    }
    // Build in a subprocess: this process's rustc-session uniqueness (TRACK_DIAGNOSTIC and
    // the global counters) forbids a second compiler. The exec itself costs ~13ms and
    // happens once.
    let self_exe = std::env::current_exe().ok()?;
    let _ = std::fs::create_dir_all(base_dir());
    let log = std::fs::File::create(base_dir().join("build.log")).ok()?;
    let status = std::process::Command::new(self_exe)
        .arg("__build-base-image")
        .arg(&path)
        .stdout(std::process::Stdio::null())
        .stderr(log)
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }
    load(&path, &stamp)
}

/// Image stack: the base image plus a chain of dependency images, ordered bottom to top in
/// topological order [std base, dep_1, dep_2, ...]. When a program session lowers, it looks
/// each v0 symbol_name up in the **union** and reuses hits; delta fn/TLS/asm ids start after
/// the stack totals, and `absorb` concatenates [stack...][delta] into one table. Each image
/// owns a fixed domain (base at 0x6800, dependency splines at 0x6A00 + k*2^34), so
/// cross-domain absolute addresses are mutually stable. An empty stack (no base image /
/// bypassed) means full cold lowering.
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
    fn from_images(mut images: Vec<BaseImage>) -> Self {
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
        let mut keys = Vec::with_capacity(images.len());
        for img in &images {
            keys.push(img.key.clone());
            for (k, v) in &img.fn_by_sym {
                s.fn_by_sym.entry(k.clone()).or_insert(*v);
            }
            for (k, v) in &img.entry_by_sym {
                s.entry_by_sym.entry(k.clone()).or_insert(*v);
            }
            for (k, v) in &img.static_by_sym {
                s.static_by_sym.entry(k.clone()).or_insert(*v);
            }
            for (k, v) in &img.tls_by_sym {
                s.tls_by_sym.entry(k.clone()).or_insert(*v);
            }
            s.total_fns += img.module.funcs.len();
            s.total_tls += img.module.tls.len();
            s.total_asm += img.module.asm_sites.len();
        }
        s.key = Some(keys.join("\u{1f}"));
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
            Some(k) => {
                k.push('\u{1f}');
                k.push_str(&img.key);
            }
            None => self.key = Some(img.key.clone()),
        }
        self.images.push(img);
    }

    /// Check the session lowering fingerprint (called from cli's after_analysis). A mismatch
    /// discards the whole stack and lowers from scratch. `from_images` already made the
    /// stack's fp uniform, so one comparison covers the whole stack.
    pub fn fp_matches(&self, session_fp: (bool, bool, bool)) -> bool {
        self.is_empty() || self.lowering_fp == session_fp
    }
}

/// Load the image stack: the base image plus its dependency chain. Only the base image is
/// loaded here.
pub fn ensure() -> ImageStack {
    let mut images = Vec::new();
    if let Some(b) = ensure_base() {
        images.push(b);
    }
    ImageStack::from_images(images)
}

/// Offset merge: concatenate [stack...] ++ delta into one table (delta fn/TLS/asm ids
/// already start after the stack totals, and each image's funcs are stored in absolute
/// FuncId order, so sequential concatenation position equals absolute id). After the merge,
/// asm stubs are re-materialized: the stub addresses in an image file are live only in the
/// build process, so they must be idempotently re-materialized here from the recipe (the
/// same contract as a warm L2 load). Each image's frozen region moves into
/// `delta.image_frozens` to keep it alive.
pub fn absorb_stack(delta: &mut ir::Module, stack: ImageStack) {
    let mut funcs = Vec::with_capacity(stack.total_fns);
    let mut function_names = Vec::with_capacity(stack.total_fns + delta.funcs.len());
    let mut tls = Vec::with_capacity(stack.total_tls);
    let mut sites = Vec::with_capacity(stack.total_asm);
    let mut frozens = Vec::with_capacity(stack.images.len());
    for img in stack.images {
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

// ===== Build side (`mirvm __build-base-image <path>` subprocess) =====

struct BaseBuildCallbacks {
    out: PathBuf,
    ok: bool,
}

impl Callbacks for BaseBuildCallbacks {
    fn after_analysis<'tcx>(&mut self, _compiler: &Compiler, tcx: TyCtxt<'tcx>) -> Compilation {
        let (mut module, exports) = crate::lower::lower_for_base_build(tcx);
        // Cacheability criterion (same shape as the L2 store; the base image is shared
        // across programs, so the contract is less negotiable still)
        let frozen_ok = module.frozen.as_ref().is_some_and(|fr| {
            fr.at_fixed_base() && fr.home() == crate::vm::addrlayout::BASE_IMAGE_FIXED_ADDR
        });
        if !frozen_ok {
            eprintln!("base-image: frozen region is not in the base-image fixed domain, giving up");
            return Compilation::Stop;
        }
        // Foreign symbols go through GOT slots (the table travels with the snapshot and is
        // refilled at startup), so they are no longer a barrier to writing the file. The
        // stub code domain must sit at a fixed base: otherwise fn-ptr values are not stable
        // across processes, and the file is rejected.
        if !module.entry_stub_sites.is_empty() && !module.entry_stubs.at_fixed_base() {
            eprintln!("base-image: stub code domain is not in the fixed domain, giving up");
            return Compilation::Stop;
        }
        // "@entry" is the program-entry alias for --vm-stats; the base image is used as a
        // library, so it does not export the synthetic entry
        module.exports.remove("@entry");

        let sess = tcx.sess;
        let Some(sysroot_stamp) = crate::sysroot::current_stamp_value() else {
            eprintln!("base-image: sysroot stamp unavailable, giving up");
            return Compilation::Stop;
        };
        // Byte determinism: extract the HashMaps (RandomState gives a random iteration
        // order) into sorted Vecs for disk, and drop the derived address table.
        //
        // `asm_stub_addrs` holds this process's dlopen addresses for the stub .so, so it is stale
        // in a file by construction; every consumer rematerializes it from `asm_sites` (the cold
        // lowering path, `absorb_stack`, the L2-hit path, package load).
        module.asm_stub_addrs.clear();
        let mut export_syms: Vec<(Box<str>, ir::FuncId)> = module.exports.drain().collect();
        export_syms.sort_unstable();
        let mut fn_addr_pairs: Vec<(u64, ir::FuncId)> = module.fn_addrs.drain().collect();
        fn_addr_pairs.sort_unstable();
        let mut link_fn_addr_pairs: Vec<(ir::LinkAddr, ir::FuncId)> =
            module.link_fn_addrs.drain().collect();
        link_fn_addr_pairs.sort_unstable_by_key(|(addr, _)| addr.0);
        let mut fn_entry_syms = exports.fn_entry_syms;
        fn_entry_syms.sort_unstable();
        let mut static_syms = exports.static_syms;
        static_syms.sort_unstable();
        let mut tls_syms = exports.tls_syms;
        tls_syms.sort_unstable();
        let file = BaseFile {
            build_id: crate::options::build::BUILD_ID.to_string(),
            sysroot_stamp,
            lowering_fp: (
                sess.ub_checks(),
                sess.overflow_checks(),
                sess.contract_checks(),
            ),
            module,
            export_syms,
            fn_addr_pairs,
            link_fn_addr_pairs,
            fn_entry_syms,
            static_syms,
            tls_syms,
        };
        let Ok(bytes) = postcard::to_stdvec(&file) else {
            eprintln!(
                "base-image: serialization failed (frozen region not at a fixed base?), giving up"
            );
            return Compilation::Stop;
        };
        let Some(dir) = self.out.parent() else {
            return Compilation::Stop;
        };
        let _ = std::fs::create_dir_all(dir);
        if crate::store::publish_bytes(&self.out, &bytes).is_err() {
            eprintln!("base-image: writing the file failed, giving up");
            return Compilation::Stop;
        }
        self.ok = true;
        Compilation::Stop
    }
}

/// Subprocess entry point. argv = [<output path>].
pub fn build_main(mut argv: impl Iterator<Item = String>) -> ExitCode {
    let Some(out) = argv.next() else {
        eprintln!("__build-base-image: missing output path");
        return ExitCode::from(2);
    };
    let sysroot = match crate::sysroot::ensure_sysroot() {
        Ok(p) => p.display().to_string(),
        Err(e) => {
            eprintln!("__build-base-image: sysroot unavailable: {e}");
            return ExitCode::from(1);
        }
    };
    // Synthetic empty main: the deterministic base-image seed
    let src_dir = base_dir().join("src");
    if std::fs::create_dir_all(&src_dir).is_err() {
        return ExitCode::from(1);
    }
    let src = src_dir.join("empty_main.rs");
    if std::fs::write(&src, "fn main() {}\n").is_err() {
        return ExitCode::from(1);
    }

    let mut rustc_args = vec![
        "mirvm-base-build".to_string(),
        src.display().to_string(),
        "--edition=2024".to_string(),
        "--crate-type=bin".to_string(),
        "--sysroot".to_string(),
        sysroot,
    ];
    // The base image key is fnv(build_id, sysroot stamp) and never sees these arguments, so the
    // frontend flag can go straight in. Applying it here matters as much as in the runner: a
    // parallel frontend that reordered emitted functions would change the image bytes while the
    // key stayed the same.
    rustc_args.push(crate::cli::parallel_frontend_arg().to_string());
    let mut callbacks = BaseBuildCallbacks {
        out: PathBuf::from(out),
        ok: false,
    };
    let _compiler_session = crate::cli::compiler_session_guard();
    let code = rustc_driver::catch_with_exit_code(|| {
        rustc_driver::run_compiler(&rustc_args, &mut callbacks)
    });
    if code != ExitCode::SUCCESS || !callbacks.ok {
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}
