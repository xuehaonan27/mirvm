//! Minimal validation of the LSDA pipeline, mirroring cg_clif's GccExceptTable.
//!
//! The chain under test -- if any link fails, the LSDA approach must be re-evaluated:
//! try_call (tag 0 = cleanup, the exception pointer arrives via `BlockArg::TryCallExn(0)`)
//! -> build a GccExceptTable by hand from `buffer.call_sites()` (a one-byte call-site entry
//! at ret_addr - 1) -> CIE (rust_eh_personality, absptr) plus FDE.lsda -> `__register_frame`
//! -> host panic payload (resume_unwind) -> the cleanup pad runs -> `_Unwind_Resume(exn)`
//! continues unwinding into the host catch_unwind.

use cranelift_codegen::ir::{
    AbiParam, BlockArg, BlockCall, ExceptionTableData, ExceptionTableItem, ExceptionTag,
    InstBuilder, types,
};
use cranelift_codegen::isa::TargetIsa;
use cranelift_codegen::isa::unwind::UnwindInfo;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{Linkage, Module as ClifModule};
use gimli::RunTimeEndian;
use gimli::write::{Address, EhFrame, EndianVec, FrameTable};
use std::sync::atomic::{AtomicU64, Ordering};

/// Pad-execution marker for host-side assertions (0 = not reached, 1 = pad ran,
/// 2 = normal return).
static PAD_MARK: AtomicU64 = AtomicU64::new(0);

/// Host panic source: `resume_unwind` carrying a Rust payload.
extern "C-unwind" fn probe_raise() {
    std::panic::resume_unwind(Box::new(0x2a_i32));
}
extern "C-unwind" fn probe_mark(x: u64) {
    PAD_MARK.store(x, Ordering::SeqCst);
}
unsafe extern "C" {
    fn _Unwind_Resume(ex: *mut u8) -> !;
    fn rust_eh_personality();
}

/// Hand-built GccExceptTable in cg_clif's layout, cleanup-only and without type_info, but
/// covering every call site:
/// - a call site with no handler becomes (ret_addr - 1, len = 1, lpad = 0, action = 0),
///   which rust's find_eh_action maps to EHAction::None via its cs_lpad == 0 branch;
/// - a call site with a cleanup handler becomes (ret_addr - 1, len = 1, pad, action = 0).
///
/// Full coverage is required: rust's find_eh_action returns EHAction::Terminate
/// (= _URC_FATAL) for an ip that has no table entry, unlike libgcc's
/// __gcc_personality_v0, which treats a missing entry as None. That is also why cg_clif
/// emits an lpad = 0 item for handler-less sites. Items follow `buffer.call_sites()` order
/// (= instruction order), which satisfies the rust parser's ordered-table assumption.
fn build_lsda(call_sites: &[(u64, Option<u64>)]) -> Vec<u8> {
    fn uleb(out: &mut Vec<u8>, mut v: u64) {
        loop {
            let mut b = (v & 0x7f) as u8;
            v >>= 7;
            if v != 0 {
                b |= 0x80;
            }
            out.push(b);
            if v == 0 {
                break;
            }
        }
    }
    let mut out = vec![0xff, 0xff, 0x01]; // lpStart=omit, ttype=omit, csEncoding=uleb128
    let mut body = Vec::new();
    for &(ret_addr, pad) in call_sites {
        uleb(&mut body, ret_addr - 1);
        uleb(&mut body, 1);
        uleb(&mut body, pad.unwrap_or(0));
        uleb(&mut body, 0); // action=0
    }
    uleb(&mut out, body.len() as u64);
    out.extend_from_slice(&body);
    while !out.len().is_multiple_of(4) {
        out.push(0);
    }
    out
}

/// After defining a function, returns (UnwindInfo, [(ret_addr, Option<landing_pad>)]) over
/// all call sites; a site without a handler yields None (the lpad = 0 item), matching
/// cg_clif's add_function.
fn unwind_and_sites(
    isa: &dyn TargetIsa,
    cctx: &cranelift_codegen::Context,
) -> (UnwindInfo, Vec<(u64, Option<u64>)>) {
    let cc = cctx.compiled_code().unwrap();
    let ui = cc.create_unwind_info(isa).unwrap().expect("unwind_info");
    let mut cs = Vec::new();
    for site in cc.buffer.call_sites() {
        if site.exception_handlers.is_empty() {
            cs.push((u64::from(site.ret_addr), None));
        }
        for h in site.exception_handlers {
            if let cranelift_codegen::FinalizedMachExceptionHandler::Tag(tag, lp) = h {
                assert_eq!(tag.as_u32(), 0, "the probe emits cleanup tags only");
                cs.push((u64::from(site.ret_addr), Some(u64::from(*lp))));
            }
        }
    }
    (ui, cs)
}

/// Host-only baseline: catch_unwind(probe_raise) with no JIT involved. If this fails, the
/// test binary's unwinding baseline itself is broken and the JIT is not at fault.
#[test]
fn host_baseline_catch() {
    let r = std::panic::catch_unwind(|| probe_raise());
    let p = r.expect_err("the host baseline should receive the payload");
    assert_eq!(*p.downcast::<i32>().unwrap(), 0x2a);
}

/// Direct call of an imported symbol: if probe_mark is reached, the import call chain works.
#[test]
fn import_call_works() {
    PAD_MARK.store(0, Ordering::SeqCst);
    let isa = super::compiler::domain_isa(super::CodeDomain::Plain);
    let mut jb = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
    jb.symbol("probe_mark", probe_mark as *const u8);
    let mut module = JITModule::new(jb);
    let mut fbc = FunctionBuilderContext::new();
    let mark_sig = {
        let mut s = module.make_signature();
        s.params.push(AbiParam::new(types::I64));
        s
    };
    let mark = module
        .declare_function("probe_mark", Linkage::Import, &mark_sig)
        .unwrap();
    let caller_id = module
        .declare_function("caller", Linkage::Local, &mark_sig)
        .unwrap();
    {
        let mut cctx = module.make_context();
        cctx.func.signature = mark_sig.clone();
        let mut b = FunctionBuilder::new(&mut cctx.func, &mut fbc);
        let entry = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let mref = module.declare_func_in_func(mark, b.func);
        let x = b.ins().iconst(types::I64, 7);
        b.ins().call(mref, &[x]);
        b.ins().return_(&[]);
        b.seal_all_blocks();
        b.finalize();
        module.define_function(caller_id, &mut cctx).unwrap();
        module.clear_context(&mut cctx);
    }
    module.finalize_definitions().unwrap();
    let addr = module.get_finalized_function(caller_id) as u64;
    let f: unsafe extern "C-unwind" fn(u64) = unsafe { std::mem::transmute(addr) };
    unsafe { f(0) };
    assert_eq!(
        PAD_MARK.load(Ordering::SeqCst),
        7,
        "direct import call did not run"
    );
}

/// Single JIT frame passthrough: caller calls probe_raise directly, with no intermediate
/// frame.
#[test]
fn cfi_single_frame() {
    let isa = super::compiler::domain_isa(super::CodeDomain::Plain);
    let mut jb = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
    jb.symbol("probe_raise", probe_raise as *const u8);
    let mut module = JITModule::new(jb);
    let mut fbc = FunctionBuilderContext::new();
    let empty_sig = module.make_signature();
    let raise = module
        .declare_function("probe_raise", Linkage::Import, &empty_sig)
        .unwrap();
    let caller_id = module
        .declare_function("caller", Linkage::Local, &empty_sig)
        .unwrap();
    let ui_caller;
    {
        let mut cctx = module.make_context();
        cctx.func.signature = empty_sig.clone();
        let mut b = FunctionBuilder::new(&mut cctx.func, &mut fbc);
        let entry = b.create_block();
        b.switch_to_block(entry);
        let rref = module.declare_func_in_func(raise, b.func);
        b.ins().call(rref, &[]);
        b.ins().return_(&[]);
        b.seal_all_blocks();
        b.finalize();
        module.define_function(caller_id, &mut cctx).unwrap();
        let (ui, _) = unwind_and_sites(module.isa(), &cctx);
        ui_caller = ui;
        module.clear_context(&mut cctx);
    }
    module.finalize_definitions().unwrap();
    let mut table = FrameTable::default();
    let cie = table.add_cie(module.isa().create_systemv_cie().expect("cie"));
    let caller_addr = module.get_finalized_function(caller_id) as u64;
    if let UnwindInfo::SystemV(info) = ui_caller {
        table.add_fde(cie, info.to_fde(Address::Constant(caller_addr)));
    } else {
        panic!("no SystemV UnwindInfo");
    }
    let mut eh = EhFrame(EndianVec::new(RunTimeEndian::Little));
    table.write_eh_frame(&mut eh).unwrap();
    crate::os::unwind::register_frame_section(eh.0.into_vec());
    let caller_fn: unsafe extern "C-unwind" fn() = unsafe { std::mem::transmute(caller_addr) };
    let result = std::panic::catch_unwind(|| unsafe { caller_fn() });
    let payload = result
        .expect_err("the single-frame case should receive the payload")
        .downcast::<i32>()
        .expect("wrong payload type");
    assert_eq!(*payload, 0x2a);
}

/// CFI-only passthrough: no personality and no LSDA; a host panic crosses two JIT frames
/// (plain calls) back to the host catch_unwind. If this fails, basic registration is
/// broken; if it passes, the problem lies in the LSDA/personality/pad half.
#[test]
fn cfi_only_passthrough() {
    let isa = super::compiler::domain_isa(super::CodeDomain::Plain);
    let mut jb = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
    jb.symbol("probe_raise", probe_raise as *const u8);
    let mut module = JITModule::new(jb);
    let mut fbc = FunctionBuilderContext::new();
    let empty_sig = module.make_signature();
    let raise = module
        .declare_function("probe_raise", Linkage::Import, &empty_sig)
        .unwrap();

    let raiser_id = module
        .declare_function("raiser", Linkage::Local, &empty_sig)
        .unwrap();
    let ui_raiser;
    {
        let mut cctx = module.make_context();
        cctx.func.signature = empty_sig.clone();
        let mut b = FunctionBuilder::new(&mut cctx.func, &mut fbc);
        let entry = b.create_block();
        b.switch_to_block(entry);
        let rref = module.declare_func_in_func(raise, b.func);
        b.ins().call(rref, &[]);
        b.ins().return_(&[]);
        b.seal_all_blocks();
        b.finalize();
        module.define_function(raiser_id, &mut cctx).unwrap();
        let (ui, _) = unwind_and_sites(module.isa(), &cctx);
        ui_raiser = ui;
        module.clear_context(&mut cctx);
    }
    let caller_id = module
        .declare_function("caller", Linkage::Local, &empty_sig)
        .unwrap();
    let ui_caller;
    {
        let mut cctx = module.make_context();
        cctx.func.signature = empty_sig.clone();
        let mut b = FunctionBuilder::new(&mut cctx.func, &mut fbc);
        let entry = b.create_block();
        b.switch_to_block(entry);
        let rref = module.declare_func_in_func(raiser_id, b.func);
        b.ins().call(rref, &[]);
        b.ins().return_(&[]);
        b.seal_all_blocks();
        b.finalize();
        module.define_function(caller_id, &mut cctx).unwrap();
        let (ui, _) = unwind_and_sites(module.isa(), &cctx);
        ui_caller = ui;
        module.clear_context(&mut cctx);
    }
    module.finalize_definitions().unwrap();

    let mut table = FrameTable::default();
    let cie = table.add_cie(module.isa().create_systemv_cie().expect("cie"));
    let raiser_addr = module.get_finalized_function(raiser_id) as u64;
    let caller_addr = module.get_finalized_function(caller_id) as u64;
    for (ui, addr) in [(ui_raiser, raiser_addr), (ui_caller, caller_addr)] {
        if let UnwindInfo::SystemV(info) = ui {
            table.add_fde(cie, info.to_fde(Address::Constant(addr)));
        } else {
            panic!("no SystemV UnwindInfo");
        }
    }
    let mut eh = EhFrame(EndianVec::new(RunTimeEndian::Little));
    table.write_eh_frame(&mut eh).unwrap();
    crate::os::unwind::register_frame_section(eh.0.into_vec());

    let caller_fn: unsafe extern "C-unwind" fn() = unsafe { std::mem::transmute(caller_addr) };
    let result = std::panic::catch_unwind(|| unsafe { caller_fn() });
    let payload = result
        .expect_err("the CFI-only case should receive the host payload (registration looks broken)")
        .downcast::<i32>()
        .expect("wrong payload type");
    assert_eq!(*payload, 0x2a);
}

#[test]
fn lsda_cleanup_pad_executes_and_resume_continues() {
    PAD_MARK.store(0, Ordering::SeqCst);

    let isa = super::compiler::domain_isa(super::CodeDomain::Plain);
    let mut jb = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
    jb.symbol("probe_raise", probe_raise as *const u8);
    jb.symbol("probe_mark", probe_mark as *const u8);
    jb.symbol("_Unwind_Resume", _Unwind_Resume as *const u8);
    let mut module = JITModule::new(jb);
    let mut fbc = FunctionBuilderContext::new();

    let empty_sig = module.make_signature(); // () -> ()
    let mark_sig = {
        let mut s = module.make_signature();
        s.params.push(AbiParam::new(types::I64));
        s
    };
    let resume_sig = {
        let mut s = module.make_signature();
        s.params.push(AbiParam::new(types::I64));
        s
    };
    let raise = module
        .declare_function("probe_raise", Linkage::Import, &empty_sig)
        .unwrap();
    let mark = module
        .declare_function("probe_mark", Linkage::Import, &mark_sig)
        .unwrap();
    let resume = module
        .declare_function("_Unwind_Resume", Linkage::Import, &resume_sig)
        .unwrap();

    // raiser: calls probe_raise; the host resume_unwind payload unwinds through its frame.
    let raiser_id = module
        .declare_function("raiser", Linkage::Local, &empty_sig)
        .unwrap();
    let ui_raiser;
    {
        let mut cctx = module.make_context();
        cctx.func.signature = empty_sig.clone();
        let mut b = FunctionBuilder::new(&mut cctx.func, &mut fbc);
        let entry = b.create_block();
        b.switch_to_block(entry);
        let rref = module.declare_func_in_func(raise, b.func);
        b.ins().call(rref, &[]);
        b.ins().return_(&[]);
        b.seal_all_blocks();
        b.finalize();
        module.define_function(raiser_id, &mut cctx).unwrap();
        let (ui, _) = unwind_and_sites(module.isa(), &cctx);
        ui_raiser = ui;
        module.clear_context(&mut cctx);
    }

    // caller: try_call(raiser); normal -> ok (mark 2); tag-0 pad (mark 1 -> _Unwind_Resume(exn)).
    let caller_id = module
        .declare_function("caller", Linkage::Local, &empty_sig)
        .unwrap();
    let (ui_caller, call_sites);
    {
        let mut cctx = module.make_context();
        cctx.func.signature = empty_sig.clone();
        let mut b = FunctionBuilder::new(&mut cctx.func, &mut fbc);
        let entry = b.create_block();
        let ok = b.create_block();
        let pad = b.create_block();
        b.append_block_param(pad, types::I64); // block parameter receiving TryCallExn(0)
        b.switch_to_block(entry);

        let rref = module.declare_func_in_func(raiser_id, b.func);
        // The exception table's signature is the *callee's*, so it is the signature the callee was
        // declared with rather than one built to look like it: this platform's default convention
        // is the CPU's own, and a table naming another one is a verifier error.
        let sig0 = b.func.import_signature(empty_sig.clone());
        let normal = BlockCall::new(ok, [], &mut b.func.dfg.value_lists);
        let pad_call = b.func.dfg.block_call(pad, &[BlockArg::TryCallExn(0)]);
        let et = b.func.dfg.exception_tables.push(ExceptionTableData::new(
            sig0,
            normal,
            [ExceptionTableItem::Tag(
                ExceptionTag::with_number(0).unwrap(),
                pad_call,
            )],
        ));
        b.ins().try_call(rref, &[], et);

        b.switch_to_block(ok);
        let mref = module.declare_func_in_func(mark, b.func);
        let two = b.ins().iconst(types::I64, 2);
        b.ins().call(mref, &[two]);
        b.ins().return_(&[]);

        b.switch_to_block(pad);
        let exn = b.block_params(pad)[0];
        let mref2 = module.declare_func_in_func(mark, b.func);
        let one = b.ins().iconst(types::I64, 1);
        b.ins().call(mref2, &[one]);
        let resref = module.declare_func_in_func(resume, b.func);
        b.ins().call(resref, &[exn]);
        b.ins()
            .trap(cranelift_codegen::ir::TrapCode::user(1).unwrap());
        b.seal_all_blocks();
        b.finalize();

        module.define_function(caller_id, &mut cctx).unwrap();
        let (ui, cs) = unwind_and_sites(module.isa(), &cctx);
        ui_caller = ui;
        call_sites = cs;
        module.clear_context(&mut cctx);
    }
    assert!(
        call_sites.len() >= 2,
        "caller must have several call sites (try_call plus full coverage of the rest)"
    );
    module.finalize_definitions().unwrap();

    // eh_frame: CIE0 has no personality (raiser); CIE1 is rust_eh_personality plus the LSDA
    // (caller). The personality goes through DW.ref indirection, as in cg_clif: the CIE
    // personality pointer targets a static u64 holding the real address. Both this probe and
    // the compiler's own eh_frame writer use that form.
    static PERS_REF: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    PERS_REF.store(rust_eh_personality as *const u8 as u64, Ordering::SeqCst);
    let mut table = FrameTable::default();
    let cie_plain = table.add_cie(module.isa().create_systemv_cie().expect("cie"));
    let mut cie_pers = module.isa().create_systemv_cie().expect("cie");
    cie_pers.lsda_encoding = Some(gimli::DW_EH_PE_absptr);
    cie_pers.personality = Some((
        gimli::DwEhPe(gimli::DW_EH_PE_indirect.0 | gimli::DW_EH_PE_absptr.0),
        Address::Constant(&PERS_REF as *const std::sync::atomic::AtomicU64 as u64),
    ));
    let cie_pers_id = table.add_cie(cie_pers);

    let raiser_addr = module.get_finalized_function(raiser_id) as u64;
    let caller_addr = module.get_finalized_function(caller_id) as u64;
    if let UnwindInfo::SystemV(info) = ui_raiser {
        table.add_fde(cie_plain, info.to_fde(Address::Constant(raiser_addr)));
    } else {
        panic!("raiser has no SystemV UnwindInfo");
    }
    let lsda_bytes = build_lsda(&call_sites);
    let lsda_addr = lsda_bytes.as_ptr() as u64;
    std::mem::forget(lsda_bytes); // The FDE/LSDA must stay valid for the probe process' life.
    if let UnwindInfo::SystemV(info) = ui_caller {
        let mut fde = info.to_fde(Address::Constant(caller_addr));
        fde.lsda = Some(Address::Constant(lsda_addr));
        table.add_fde(cie_pers_id, fde);
    } else {
        panic!("caller has no SystemV UnwindInfo");
    }

    // One complete, zero-terminated eh_frame section registered from the FrameTable.
    let mut eh = EhFrame(EndianVec::new(RunTimeEndian::Little));
    table.write_eh_frame(&mut eh).unwrap();
    crate::os::unwind::register_frame_section(eh.0.into_vec());

    // Fire the whole chain: the host catch_unwind must receive 42, and the pad must have
    // run (mark = 1, not 2).
    let caller_fn: unsafe extern "C-unwind" fn() = unsafe { std::mem::transmute(caller_addr) };
    let result = std::panic::catch_unwind(|| unsafe { caller_fn() });
    assert!(
        result.is_err(),
        "caller did not unwind (pad/unwind chain broken; PAD_MARK={})",
        PAD_MARK.load(Ordering::SeqCst)
    );
    let payload = result.unwrap_err();
    assert_eq!(
        payload.downcast_ref::<i32>(),
        Some(&0x2a),
        "payload lost or replaced"
    );
    assert_eq!(
        PAD_MARK.load(Ordering::SeqCst),
        1,
        "cleanup pad did not run (LSDA/personality missed)"
    );
}
