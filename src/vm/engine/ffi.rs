//! Foreign direct call: dlsym + libffi, invoked directly under a frozen signature.
//!
//! A direct benefit of the real-address model: a guest pointer *is* a host pointer, so
//! marshalling is zero -- arguments are plain u64 bits truncated by FfiKind, and a native
//! write to guest memory writes real memory, visible by construction.
//!
//! Library-loading discipline: a materialized archive is a required library (RTLD_NOW;
//! failure aborts carrying the dlerror detail); a plain `-l` name is an optional candidate
//! (best-effort). Resolution order: the archive hidden-symbol `.symtab` fallback table
//! (link-time binding always beats the global scope) -> RTLD_DEFAULT -> each handle (see the
//! `archive_fallbacks` field note). Variadic functions use `Cif::new_variadic`; the trailing
//! argument classes are frozen at the call site, and libffi handles x86_64 AL.
//!
//! A simplified, provenance-free version of tier-0's native.rs.

use std::collections::HashMap;
use std::ffi::{CString, c_void};
use std::mem::MaybeUninit;

use libffi::middle::{Arg, Cif, CodePtr, Ret, Type as FfiType};

use super::ir::{FfiKind, ForeignSig};

// libffi-sys declares ffi_call as plain C, so Rust does not allow an exception to cross that
// call. The same native symbol is declared again as C-unwind, used only when
// ForeignSig.unwind is true.
unsafe extern "C-unwind" {
    #[link_name = "ffi_call"]
    fn ffi_call_unwind(
        cif: *mut libffi::raw::ffi_cif,
        fun: Option<unsafe extern "C" fn()>,
        rvalue: *mut c_void,
        avalue: *mut *mut c_void,
    );
}

/// Per-thread FFI state (dlsym result cache + dlopen handles). dlsym is idempotent, so an
/// independent cache per thread is harmless.
#[derive(Default)]
pub struct FfiState {
    syms: HashMap<Box<str>, usize>,
    /// dlopen handles of required archive libraries, in `required_native_libs` order (which
    /// mirrors link order). Their `.dynsym`-visible symbols resolve **before RTLD_DEFAULT**:
    /// native link-time binding means an object the guest linked in itself always beats a
    /// same-named library in the host process. Optional-library handles are listed separately
    /// and queried after the global scope.
    required_handles: Vec<usize>,
    handles: Vec<usize>,
    /// Hidden-symbol fallback tables of required archive libraries: (load bias, symbol ->
    /// st_value). They hold only symbols absent from `.dynsym` (from `-fvisibility=hidden`
    /// archives such as the ring/zstd-sys family). Resolution order follows
    /// `required_native_libs` (mirroring link order) and runs **before the global dlsym
    /// scope**: once a static archive member is linked into the guest, its definition beats
    /// the global namespace (native link-time binding). Otherwise a same-named library
    /// embedded in the host (e.g. ZSTD_* inside libLLVM) silently intercepts the call.
    archive_fallbacks: Vec<(u64, HashMap<Box<str>, u64>)>,
    libs_loaded: bool,
}

impl FfiState {
    /// Resolve a symbol's real address (cached, including misses). `None` means no search
    /// scope had it.
    ///
    /// After obtaining an address the caller must end its mutable borrow of `FfiState` before
    /// entering native code: a native function may synchronously call back into the guest, and
    /// that guest callback may resolve and call foreign symbols again.
    pub(crate) fn resolve(
        &mut self,
        name: &str,
        optional_libs: &[Box<str>],
        required_libs: &[Box<str>],
        native_images: &[super::native_instance::NativeImage],
        mc_images: &[super::mcload::McImage],
    ) -> Result<Option<usize>, String> {
        if let Some(&p) = self.syms.get(name) {
            return Ok((p != 0).then_some(p));
        }
        self.ensure_libs(optional_libs, required_libs, native_images)?;
        let Ok(cname) = CString::new(name) else {
            return Ok(None);
        };
        // ① Archive hidden-symbol fallback table (before the global scope): native link-time
        // binding -- an archive definition beats the global namespace. Resolving via dlsym
        // first would silently bind the guest's ZSTD_* to the copy embedded in the host's
        // libLLVM (same ABI, different policy setting -> valid but wrong bytes).
        let mut p = 0usize;
        for (bias, syms) in &self.archive_fallbacks {
            if let Some(&v) = syms.get(name) {
                p = (bias + v) as usize;
                break;
            }
        }
        // ①' MC images (self-loaded, guest-produced global_asm/dep_asm from the package; same
        // semantic slot as ② -- a guest-produced object beats a same-named host library)
        if p == 0
            && let Some(addr) = super::mcload::resolve(mc_images, name)
        {
            p = addr;
        }
        // ② dlsym on required archive handles (link order): native link-time binding of
        // `.dynsym`-visible archive symbols -- the guest's own object beats a same-named host
        // library. Handle resolution is independent of load order and reproducible (same for
        // ①'s hidden symbols). Residual: a symbol that collides across archive-internal
        // references still goes through the global order; known and not observed in the corpus.
        if p == 0 {
            for &h in &self.required_handles {
                p = crate::os::dll::sym(h, &cname);
                if p != 0 {
                    break;
                }
            }
        }
        // ③ global dlsym (real system libraries). `.dynsym`-visible archive symbols loaded
        // with RTLD_GLOBAL also hit here, but when a same-named host library exists ② already
        // hit the archive, so there is no ambiguity.
        if p == 0 {
            p = crate::os::dll::sym(0, &cname);
        }
        if p == 0 {
            for &h in &self.handles {
                p = crate::os::dll::sym(h, &cname);
                if p != 0 {
                    break;
                }
            }
        }
        self.syms.insert(name.into(), p);
        Ok((p != 0).then_some(p))
    }

    pub(crate) fn ensure_libs(
        &mut self,
        optional_libs: &[Box<str>],
        required_libs: &[Box<str>],
        native_images: &[super::native_instance::NativeImage],
    ) -> Result<(), String> {
        if self.libs_loaded {
            return Ok(());
        }

        if !native_images.is_empty() {
            if native_images.len() != required_libs.len() {
                return Err("native image/path count mismatch".into());
            }
            for image in native_images {
                self.required_handles.push(image.handle());
                self.archive_fallbacks
                    .push((image.bias(), image.hidden_symbol_values().clone()));
            }
        } else {
            // Direct FfiState probes may still supply raw paths. Product Engine
            // startup always prepares staged NativeImage objects before this point.
            for cand in required_libs {
                let cpath = CString::new(&**cand)
                    .map_err(|_| format!("[native library path must contains NUL]: `{cand}`"))?;
                let h =
                    crate::os::dll::open(&cpath, crate::os::dll::Mode::Now).map_err(|detail| {
                        format!("[dlopen needs native library] `{cand}` failure: {detail}")
                    })?;
                self.required_handles.push(h);
                if let Some(bias) = crate::os::dll::load_bias(h)
                    && let Ok(syms) = crate::elfsym::hidden_symtab_values(cand)
                {
                    self.archive_fallbacks.push((bias as u64, syms));
                }
            }
        }
        for cand in optional_libs {
            let Ok(cpath) = CString::new(&**cand) else {
                continue;
            };
            if let Ok(h) = crate::os::dll::open(&cpath, crate::os::dll::Mode::Lazy) {
                self.handles.push(h);
            }
        }
        self.libs_loaded = true;
        Ok(())
    }
}

/// Startup GOT refill: resolve every foreign symbol with the same resolution order used by
/// runtime foreign calls, then write `resolved + addend` at each fixup point. Cold and warm
/// share one path -- the cold result must equal the address lowering filled in (idempotent),
/// while the warm path (L2/image replay) uses it to replace a previous process's stale host
/// addresses with this process's real ones. A non-weak miss is an Err, and loudly so: a stale
/// address is a silent source of SIGSEGV-level wrong values. A weak miss writes 0, matching
/// the absent-`extern weak` semantics.
pub(crate) fn resolve_got_fixups(module: &mut super::ir::Module) -> Result<(), String> {
    if module.got_fixups.is_empty() {
        return Ok(());
    }
    let mut ffi = FfiState::default();
    ffi.ensure_libs(
        &module.native_libs,
        &module.required_native_libs,
        &module.native_images,
    )?;
    let mut resolved: Vec<u64> = Vec::with_capacity(module.foreign_syms.len());
    for s in &module.foreign_syms {
        match (
            ffi.resolve(
                &s.name,
                &module.native_libs,
                &module.required_native_libs,
                &module.native_images,
                &module.mc_images,
            )?,
            s.weak,
        ) {
            (Some(p), _) => resolved.push(p as u64),
            (None, true) => resolved.push(0),
            (None, false) => {
                return Err(format!(
                    "foreign symbol `{}` unresolved at startup GOT refill \
                     (absent from the archive fallback tables and the global dlsym scope)",
                    s.name
                ));
            }
        }
    }
    for f in &module.got_fixups {
        // A fixup addr always points at an 8-byte cell inside the frozen region (a lowering
        // registration rule); the frozen-region mapping is RW for its whole lifetime.
        let addr = module.resolve_link_addr(f.addr);
        unsafe { *(addr as *mut u64) = resolved[f.sym as usize].wrapping_add(f.addend) };
    }
    Ok(())
}

pub(super) fn ffi_type(k: &FfiKind) -> FfiType {
    match k {
        FfiKind::I8 => FfiType::i8(),
        FfiKind::I16 => FfiType::i16(),
        FfiKind::I32 => FfiType::i32(),
        FfiKind::I64 => FfiType::i64(),
        FfiKind::U8 => FfiType::u8(),
        FfiKind::U16 => FfiType::u16(),
        FfiKind::U32 => FfiType::u32(),
        FfiKind::U64 => FfiType::u64(),
        FfiKind::F32 => FfiType::f32(),
        FfiKind::F64 => FfiType::f64(),
        FfiKind::Ptr => FfiType::pointer(),
        FfiKind::Void => FfiType::void(),
        FfiKind::Agg(agg) => ffi_type_agg(agg),
    }
}

/// Frozen aggregate -> libffi struct type (recursively nested; libffi computes size/align
/// from the fields).
fn ffi_type_agg(agg: &super::ir::FfiAgg) -> FfiType {
    let fields: Vec<FfiType> = agg
        .fields
        .iter()
        .map(|f| match &f.leaf {
            super::ir::FfiLeaf::Scalar(k) => ffi_type(k),
            super::ir::FfiLeaf::Agg(inner) => ffi_type_agg(inner),
        })
        .collect();
    FfiType::structure(fields)
}

/// Amplify a guest thread's stack: on pthread_create with an explicit stacksize
/// (`std::thread` always sets one), temporarily grow the attr. An interpreted frame costs
/// tens of times the host stack of a native frame, so the original size would blow the host
/// stack at a guest depth far shallower than native. When it returns `Some((attr, original
/// size))` the caller restores the attr after create (the guest may reuse it). A
/// guest-supplied stack (pthread_attr_setstack with a non-null addr) is untouched, as is a
/// null attr (glibc default): that form only appears for threads native code creates itself,
/// where the stack_floor real-stack guard covers thunk re-entry. The enlarged size is a
/// virtual reservation, committed on demand.
pub fn amplify_pthread_stack(sym: &str, av: &[u64]) -> Option<(*mut std::ffi::c_void, usize)> {
    /// Conservative upper bound on the interpreted/native host-stack cost ratio (~2KB vs ~64B)
    const AMPLIFY: usize = 32;
    const FLOOR: usize = 64 << 20;
    if sym != "pthread_create" || av.len() < 4 {
        return None;
    }
    let attr = av[1] as *mut std::ffi::c_void;
    if attr.is_null() {
        return None;
    }
    let (lo, size) = crate::os::thread::attr_stack_bounds(attr)?;
    // An attr with no stacksize (glibc's fake-address form is detected in os::thread) is
    // untouched, as is a guest-supplied stack (setstack with addr inside the user address
    // range).
    if !crate::os::thread::stack_addr_is_unset(lo) {
        return None;
    }
    let want = size.saturating_mul(AMPLIFY).max(FLOOR);
    if want <= size || !crate::os::thread::attr_set_stack_size(attr, want) {
        return None;
    }
    Some((attr, size))
}

/// Call directly at a real code address: the shared tail of CallForeign, and the native
/// fn-pointer channel used when a CallIndirect reverse lookup misses (real code the guest
/// obtained from dlsym at runtime).
pub fn call_addr(fnptr: usize, sig: &ForeignSig, args: &[u64], ret_dst: Option<u64>) -> u64 {
    // Args and signature must be the same length; zip silently truncating once hid the
    // types of a variadic call's real trailing arguments.
    if args.len() != sig.args.len() {
        crate::vm::engine::interp::engine_abort(&format!(
            "FFI argument/signature length mismatch (args {} / signature {}; \
             signature drift or a variadic freeze gap)",
            args.len(),
            sig.args.len()
        ));
    }
    let types: Vec<FfiType> = sig.args.iter().map(ffi_type).collect();
    let cif = match sig.fixed {
        Some(nfixed) => Cif::new_variadic(types, nfixed, ffi_type(&sig.ret)),
        None => Cif::new(types, ffi_type(&sig.ret)),
    };

    // One 8-byte little-endian buffer per scalar argument (libffi reads the prefix at the
    // type's width); an aggregate argument's avalue points straight at the aggregate bytes the
    // guest evaluated (zero copy).
    let bufs: Vec<[u8; 8]> = args.iter().map(|a| a.to_le_bytes()).collect();
    let ffi_args: Vec<Arg<'_>> = args
        .iter()
        .zip(sig.args.iter())
        .zip(bufs.iter())
        .map(|((&v, k), buf)| match k {
            FfiKind::Agg(agg) => {
                Arg::new(unsafe { std::slice::from_raw_parts(v as *const u8, agg.size as usize) })
            }
            _ => Arg::new(buf),
        })
        .collect();
    let mut raw_args = sig.unwind.then(|| {
        args.iter()
            .zip(sig.args.iter())
            .zip(bufs.iter())
            .map(|((&v, k), buf)| match k {
                FfiKind::Agg(_) => v as usize as *mut c_void,
                _ => buf.as_ptr().cast_mut().cast(),
            })
            .collect::<Vec<*mut c_void>>()
    });

    if let FfiKind::Agg(agg) = &sig.ret {
        // Aggregate returned by value: the result buffer is allocated in 8-byte-aligned
        // buckets (align > 8 is rejected at the freeze boundary), and after the call `size`
        // bytes are copied to the caller's destination. libffi interprets rtype from the
        // struct type itself, covering both the register-pair and the sret form.
        let dst = ret_dst
            .expect("caller destination for an aggregate-by-value return (engine invariant)");
        let mut rbuf: Vec<u64> = vec![0; (agg.size as usize).div_ceil(8)];
        unsafe {
            call_return_into(
                &cif,
                fnptr,
                sig.unwind,
                &ffi_args,
                raw_args.as_deref_mut(),
                &mut rbuf[..],
            );
            std::ptr::copy_nonoverlapping(
                rbuf.as_ptr() as *const u8,
                dst as *mut u8,
                agg.size as usize,
            );
        }
        return 0;
    }
    let mut ret = [0u8; 8];
    // SAFETY: the address comes from dlsym or a real code pointer the guest holds; the
    // signature is frozen from the rustc fn sig layout; the guest buffer is a host buffer.
    // Fast-mode stance: correctness of a native call is the guest program's responsibility.
    unsafe {
        call_return_into(
            &cif,
            fnptr,
            sig.unwind,
            &ffi_args,
            raw_args.as_deref_mut(),
            &mut ret[..],
        );
    }
    u64::from_le_bytes(ret)
}

/// Choose plain C or C-unwind ffi_call per the frozen ABI. The C path keeps the libffi
/// crate's original declaration; the unwind path changes only the boundary attribute Rust
/// sees, not the CIF or the argument layout.
unsafe fn call_return_into<T: ?Sized>(
    cif: &Cif,
    fnptr: usize,
    unwind: bool,
    ffi_args: &[Arg<'_>],
    raw_args: Option<&mut [*mut c_void]>,
    ret: &mut T,
) {
    if !unwind {
        unsafe {
            cif.call_return_into(CodePtr(fnptr as *mut _), ffi_args, Ret::new(ret));
        }
        return;
    }

    let raw_args = raw_args.expect("C-unwind ffi_call requires the raw argument array");
    assert_eq!(
        unsafe { (*cif.as_raw_ptr()).nargs as usize },
        raw_args.len(),
        "C-unwind ffi_call argument count differs from the CIF"
    );
    unsafe {
        call_return_into_unwind(
            cif.as_raw_ptr(),
            fnptr,
            raw_args.as_mut_ptr(),
            (ret as *mut T).cast(),
        );
    }
}

/// C-unwind equivalent of libffi::low::call_return_into. For a small integer return libffi
/// writes a full register, so the value is first collected into a usize and only the real
/// width is copied, avoiding overwriting the caller's buffer.
unsafe fn call_return_into_unwind(
    cif: *mut libffi::raw::ffi_cif,
    fnptr: usize,
    args: *mut *mut c_void,
    ret: *mut c_void,
) {
    let rtype = unsafe { (*cif).rtype };
    let return_size = unsafe { (*rtype).size };
    let return_kind = unsafe { (*rtype).type_ };
    let fun: unsafe extern "C" fn() = unsafe { std::mem::transmute(fnptr) };

    if return_size >= std::mem::size_of::<usize>()
        || return_kind == libffi::raw::FFI_TYPE_FLOAT
        || return_kind == libffi::raw::FFI_TYPE_STRUCT
        || return_kind == libffi::raw::FFI_TYPE_VOID
    {
        unsafe { ffi_call_unwind(cif, Some(fun), ret, args) };
        return;
    }

    let mut register = MaybeUninit::<usize>::uninit();
    unsafe {
        ffi_call_unwind(cif, Some(fun), register.as_mut_ptr().cast(), args);
    }
    let register = unsafe { register.assume_init() };
    let src = if cfg!(target_endian = "big") {
        (&register as *const usize)
            .cast::<u8>()
            .wrapping_add(std::mem::size_of::<usize>() - return_size)
    } else {
        (&register as *const usize).cast::<u8>()
    };
    unsafe { std::ptr::copy_nonoverlapping(src, ret.cast(), return_size) };
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{FfiState, call_addr};
    use crate::vm::engine::ctx::{Engine, Shared};
    use crate::vm::engine::interp::{RunOutcome, run_export};
    use crate::vm::engine::ir::{
        Block, FfiAgg, FfiField, FfiKind, FfiLeaf, ForeignSig, FuncBody, MemOrd, Module, Operand,
        ParamAbi, RetAbi, RetDest, Rvalue, ScalarPlace, Slot, Stmt, Terminator, UnwindAction,
        Width,
    };

    static REENTRANT_FOREIGN_LEN: AtomicU64 = AtomicU64::new(0);

    #[repr(C)]
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct AggregateProbe {
        wide: u64,
        narrow: u32,
    }

    fn sig(args: Vec<FfiKind>, ret: FfiKind, fixed: Option<usize>, unwind: bool) -> ForeignSig {
        ForeignSig {
            args,
            ret,
            fixed,
            thunk_args: vec![],
            unwind,
        }
    }

    unsafe extern "C" fn plain_add_one(v: u64) -> u64 {
        v + 1
    }

    unsafe extern "C-unwind" fn unwind_panics() {
        std::panic::panic_any(0x18_u32);
    }

    unsafe extern "C-unwind" fn unwind_i8() -> i8 {
        -7
    }

    unsafe extern "C-unwind" fn unwind_u16() -> u16 {
        0xabcd
    }

    unsafe extern "C-unwind" fn unwind_aggregate(value: AggregateProbe) -> AggregateProbe {
        AggregateProbe {
            wide: value.wide + 1,
            narrow: value.narrow + 2,
        }
    }

    fn aggregate_probe_kind() -> FfiKind {
        FfiKind::Agg(FfiAgg {
            size: std::mem::size_of::<AggregateProbe>() as u32,
            align: std::mem::align_of::<AggregateProbe>() as u32,
            fields: vec![
                FfiField {
                    off: 0,
                    leaf: FfiLeaf::Scalar(FfiKind::U64),
                },
                FfiField {
                    off: 8,
                    leaf: FfiLeaf::Scalar(FfiKind::U32),
                },
            ],
        })
    }

    fn reentrant_foreign_module(data: *mut u64) -> Module {
        const CALLBACK_ADDR: u64 = 0xf11f_1f11;
        let callback_sig = ForeignSig {
            args: vec![FfiKind::Ptr, FfiKind::Ptr],
            ret: FfiKind::I32,
            fixed: None,
            thunk_args: Vec::new(),
            unwind: true,
        };
        let outer_ret = Slot {
            off: 0,
            width: Width::W64,
        };
        let len = Slot {
            off: 8,
            width: Width::W64,
        };
        let callback_ret = Slot {
            off: 0,
            width: Width::W32,
        };
        let callback = FuncBody {
            frame_size: 32,
            frame_align: 8,
            ret: RetAbi::Scalar(callback_ret),
            params: vec![
                ParamAbi::Scalar(Slot {
                    off: 16,
                    width: Width::W64,
                }),
                ParamAbi::Scalar(Slot {
                    off: 24,
                    width: Width::W64,
                }),
            ],
            caller_loc_off: None,
            blocks: vec![
                Block {
                    stmts: Vec::new(),
                    term: Terminator::CallForeign {
                        sym: "strlen".into(),
                        sig: sig(vec![FfiKind::Ptr], FfiKind::U64, None, true),
                        args: vec![Operand::Imm {
                            bits: c"nested".as_ptr() as u64,
                            width: Width::W64,
                        }],
                        ret: RetDest::Scalar(ScalarPlace::Slot(len)),
                        target: 1,
                        unwind: UnwindAction::Continue,
                    },
                },
                Block {
                    stmts: vec![
                        Stmt::AtomicStore {
                            addr: Operand::Imm {
                                bits: REENTRANT_FOREIGN_LEN.as_ptr() as u64,
                                width: Width::W64,
                            },
                            val: Operand::Slot(len),
                            order: MemOrd::SeqCst,
                        },
                        Stmt::Assign {
                            dst: ScalarPlace::Slot(callback_ret),
                            rv: Rvalue::Use(Operand::Imm {
                                bits: 0,
                                width: Width::W32,
                            }),
                        },
                    ],
                    term: Terminator::Return,
                },
            ],
            name: "qsort_guest_callback_calls_strlen".into(),
        };
        let outer = FuncBody {
            frame_size: 8,
            frame_align: 8,
            ret: RetAbi::Scalar(outer_ret),
            params: Vec::new(),
            caller_loc_off: None,
            blocks: vec![
                Block {
                    stmts: Vec::new(),
                    term: Terminator::CallForeign {
                        sym: "qsort".into(),
                        sig: ForeignSig {
                            args: vec![FfiKind::Ptr, FfiKind::U64, FfiKind::U64, FfiKind::Ptr],
                            ret: FfiKind::Void,
                            fixed: None,
                            thunk_args: vec![(3, callback_sig)],
                            unwind: true,
                        },
                        args: vec![
                            Operand::Imm {
                                bits: data as u64,
                                width: Width::W64,
                            },
                            Operand::Imm {
                                bits: 2,
                                width: Width::W64,
                            },
                            Operand::Imm {
                                bits: std::mem::size_of::<u64>() as u64,
                                width: Width::W64,
                            },
                            Operand::Imm {
                                bits: CALLBACK_ADDR,
                                width: Width::W64,
                            },
                        ],
                        ret: RetDest::Ignore,
                        target: 1,
                        unwind: UnwindAction::Continue,
                    },
                },
                Block {
                    stmts: vec![Stmt::Assign {
                        dst: ScalarPlace::Slot(outer_ret),
                        rv: Rvalue::Use(Operand::Imm {
                            bits: 0x51_51,
                            width: Width::W64,
                        }),
                    }],
                    term: Terminator::Return,
                },
            ],
            name: "qsort_synchronously_calls_guest".into(),
        };
        let mut module = Module {
            funcs: vec![outer, callback].into(),
            ..Module::default()
        };
        module.exports.insert("probe".into(), 0);
        module.fn_addrs.insert(CALLBACK_ADDR, 1);
        module
    }

    #[test]
    fn native_callback_can_reenter_guest_and_make_another_foreign_call() {
        #[allow(unused_mut)]
        let mut modes = vec![("interp", false)];
        #[cfg(feature = "cranelift")]
        modes.push(("jit", true));

        for (mode, jit) in modes {
            REENTRANT_FOREIGN_LEN.store(0, Ordering::SeqCst);
            let mut data = [2_u64, 1];
            let module = reentrant_foreign_module(data.as_mut_ptr());
            crate::vm::engine::verify::module(&module)
                .unwrap_or_else(|error| panic!("{mode}: invalid reentry probe: {error}"));
            let mut shared = Shared::new(module);
            shared.jit.enabled = jit;
            if jit {
                shared.jit.threshold = 1;
                shared.jit.sync = true;
            }
            let engine = Engine::new(shared);

            let result = unsafe { run_export(&engine, "probe", &[]) };
            assert!(
                matches!(result, Ok(RunOutcome::Returned(value)) if value.lo == 0x51_51),
                "{mode}: synchronous native callback did not return through the guest: {result:?}"
            );
            assert_eq!(
                REENTRANT_FOREIGN_LEN.load(Ordering::SeqCst),
                6,
                "{mode}: guest callback did not finish its nested strlen foreign call"
            );
            if jit {
                assert!(
                    engine
                        .shared()
                        .jit
                        .slots
                        .iter()
                        .take(2)
                        .all(|slot| slot.load(Ordering::Acquire) != 0),
                    "{mode}: forced synchronous JIT did not publish both guest functions"
                );
            }
            engine.wait_closed().unwrap();
        }
    }

    #[test]
    fn plain_c_and_c_unwind_use_separate_call_boundaries() {
        let plain = sig(vec![FfiKind::U64], FfiKind::U64, None, false);
        assert_eq!(
            call_addr(plain_add_one as *const () as usize, &plain, &[41], None),
            42
        );

        let unwind = sig(vec![], FfiKind::Void, None, true);
        let panic = std::panic::catch_unwind(|| {
            call_addr(unwind_panics as *const () as usize, &unwind, &[], None)
        })
        .expect_err("C-unwind ffi_call must let the panic return to Rust");
        assert_eq!(panic.downcast_ref::<u32>(), Some(&0x18));
    }

    #[test]
    fn c_unwind_path_preserves_small_integer_returns() {
        let i8_sig = sig(vec![], FfiKind::I8, None, true);
        assert_eq!(
            call_addr(unwind_i8 as *const () as usize, &i8_sig, &[], None),
            0xf9
        );

        let u16_sig = sig(vec![], FfiKind::U16, None, true);
        assert_eq!(
            call_addr(unwind_u16 as *const () as usize, &u16_sig, &[], None),
            0xabcd
        );
    }

    #[test]
    fn c_unwind_path_preserves_aggregate_arguments_and_returns() {
        let aggregate = aggregate_probe_kind();
        let signature = sig(vec![aggregate.clone()], aggregate, None, true);
        let input = AggregateProbe {
            wide: 0x1020_3040_5060_7080,
            narrow: 40,
        };
        let mut output = std::mem::MaybeUninit::<AggregateProbe>::uninit();

        assert_eq!(
            call_addr(
                unwind_aggregate as *const () as usize,
                &signature,
                &[std::ptr::from_ref(&input) as u64],
                Some(output.as_mut_ptr() as u64),
            ),
            0
        );
        assert_eq!(
            unsafe { output.assume_init() },
            AggregateProbe {
                wide: 0x1020_3040_5060_7081,
                narrow: 42,
            }
        );
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn c_unwind_path_keeps_variadic_cif_rules() {
        let mut out = [0u8; 8];
        let format = c"%d";
        let variadic = sig(
            vec![FfiKind::Ptr, FfiKind::U64, FfiKind::Ptr, FfiKind::I32],
            FfiKind::I32,
            Some(3),
            true,
        );
        let ret = call_addr(
            libc::snprintf as *const () as usize,
            &variadic,
            &[
                out.as_mut_ptr() as u64,
                out.len() as u64,
                format.as_ptr() as u64,
                42,
            ],
            None,
        );
        assert_eq!(ret, 2);
        assert_eq!(std::ffi::CStr::from_bytes_until_nul(&out).unwrap(), c"42");
    }

    fn missing_library() -> Box<str> {
        format!(
            "/tmp/mirvm-definitely-missing-native-library-{}.so",
            std::process::id()
        )
        .into()
    }

    #[test]
    fn missing_optional_candidate_still_allows_rtld_default_resolution() {
        let mut state = FfiState::default();
        let address = state
            .resolve("malloc", &[missing_library()], &[], &[], &[])
            .expect("optional dlopen failure must stay optional");
        assert!(address.is_some(), "malloc should resolve from RTLD_DEFAULT");
    }

    #[test]
    fn missing_required_library_fails_before_same_named_rtld_default_symbol() {
        let missing = missing_library();
        let mut state = FfiState::default();
        let error = state
            .resolve("malloc", &[], std::slice::from_ref(&missing), &[], &[])
            .unwrap_err();

        assert!(
            error.contains(&*missing),
            "required path missing from diagnostic: {error}"
        );
        assert!(
            error.contains("[dlopen needs native library]"),
            "unexpected diagnostic: {error}"
        );
        assert!(
            !error.contains("[dlerror without value]"),
            "dlerror detail was lost: {error}"
        );
    }

    /// An archive hidden symbol resolves before RTLD_DEFAULT (native link-time binding: an
    /// archive definition beats a global same-name -- the fix for the host libLLVM's embedded
    /// ZSTD_* silently intercepting). Probe: a hidden `malloc` must resolve to the archive
    /// definition, even though the process global scope always has libc's.
    #[test]
    fn hidden_archive_symbol_wins_over_rtld_default() {
        let dir = std::env::temp_dir().join(format!("mirvm-ffi-order-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (c, o, a, so) = (
            dir.join("p.c"),
            dir.join("p.o"),
            dir.join("libp.a"),
            dir.join("libp.so"),
        );
        std::fs::write(
            &c,
            "__attribute__((visibility(\"hidden\"))) void *malloc(unsigned long size) { (void)size; return (void *)0x2aUL; }\n",
        )
        .unwrap();
        use std::process::Command;
        assert!(
            Command::new("cc")
                .args(["-fPIC", "-c"])
                .arg(&c)
                .arg("-o")
                .arg(&o)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("ar")
                .args(["crs"])
                .arg(&a)
                .arg(&o)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("cc")
                .args(["-shared", "-Wl,-z,defs", "-Wl,--whole-archive"])
                .arg(&a)
                .args(["-Wl,--no-whole-archive", "-o"])
                .arg(&so)
                .status()
                .unwrap()
                .success()
        );
        // Precondition: hidden malloc is not in .dynsym, so the process global scope only has
        // libc's.
        let libc_malloc = crate::os::dll::sym(0, c"malloc");
        assert!(libc_malloc != 0);
        let mut state = FfiState::default();
        let required: Box<str> = so.display().to_string().into();
        let resolved = state
            .resolve("malloc", &[], std::slice::from_ref(&required), &[], &[])
            .expect("required lib loads")
            .expect("malloc resolves");
        assert_ne!(
            resolved, libc_malloc,
            "archive hidden malloc must beat RTLD_DEFAULT's libc malloc"
        );
        let f: unsafe extern "C" fn(u64) -> *mut std::ffi::c_void =
            unsafe { std::mem::transmute(resolved) };
        assert_eq!(unsafe { f(0) }, 0x2a as *mut std::ffi::c_void);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
