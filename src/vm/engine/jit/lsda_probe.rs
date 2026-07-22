//! M5.4 前置 probe：LSDA 管线最小验证（cg_clif GccExceptTable 同构；自 jit_compile.rs 整搬）。
//!
//! 验证链（任一环失败即 M5.4 LSDA 方案需要重评）：try_call（tag0=cleanup，
//! `BlockArg::TryCallExn(0)` 传异常指针）→ 从 `buffer.call_sites()` 手工构建
//! GccExceptTable（ret_addr-1 单字节 call-site 项）→ CIE(rust_eh_personality,
//! absptr) + FDE.lsda → `__register_frame` → 宿主 panic 载荷（resume_unwind）→
//! cleanup pad 执行 → `_Unwind_Resume(exn)` 续传至宿主 catch_unwind。

use super::*;
use cranelift_codegen::ir::{
    AbiParam, BlockArg, BlockCall, ExceptionTableData, ExceptionTableItem, ExceptionTag,
    InstBuilder, Signature, types,
};
use cranelift_codegen::isa::unwind::UnwindInfo;
use cranelift_codegen::isa::{CallConv, TargetIsa};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{Linkage, Module as ClifModule};
use gimli::RunTimeEndian;
use gimli::write::{Address, EhFrame, EndianVec, FrameTable};
use std::sync::atomic::{AtomicU64, Ordering};

/// pad 执行标记（0=未走, 1=pad 已走, 2=正常返回）；宿主侧断言用
static PAD_MARK: AtomicU64 = AtomicU64::new(0);

/// 宿主 panic 载荷源（spike3/M4.2 同形态：resume_unwind 携带 Rust payload）
extern "C-unwind" fn probe_raise() {
    std::panic::resume_unwind(Box::new(0x2a_i32));
}
extern "C-unwind" fn probe_mark(x: u64) {
    PAD_MARK.store(x, Ordering::SeqCst);
}
extern "C-unwind" fn probe_unwind_resume(ex: *mut u8) -> ! {
    unsafe { _Unwind_Resume(ex) }
}
unsafe extern "C" {
    fn _Unwind_Resume(ex: *mut u8) -> !;
    fn rust_eh_personality();
}

/// 手工 GccExceptTable（cleanup-only，无 type_info；cg_clif 版式 + **全覆盖**）：
/// - 无 handler 的调用点：(ret_addr-1, len=1, lpad=0, action=0) —— 命中即
///   EHAction::None（rust find_eh_action 的 cs_lpad==0 分支）
/// - cleanup handler 调用点：(ret_addr-1, len=1, pad, action=0)
/// **rust 版 find_eh_action 对"ip 不在表中"返回 EHAction::Terminate（= _URC_FATAL），
/// 与 libgcc 的 __gcc_personality_v0（no-entry = None）不同——call-site 表必须覆盖
/// 函数内全部调用点**（cg_clif 对无 handler 站点同样发 lpad=0 项的原因）。
/// 项按 buffer.call_sites() 序（= 指令序，满足 rust 解析器的有序表假设）。
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

/// 定义后取 (UnwindInfo, [(ret_addr, Option<landing_pad>)])——全调用点
/// （cg_clif add_function 同数据源同口径：无 handler → None（lpad=0 项））
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
                assert_eq!(tag.as_u32(), 0, "probe 只发 cleanup tag");
                cs.push((u64::from(site.ret_addr), Some(u64::from(*lp))));
            }
        }
    }
    (ui, cs)
}

/// 二分定位（分支 -1）：纯宿主基线——catch_unwind(probe_raise) 无 JIT 参与。
/// 此分支若挂 = 测试二进制的 unwind 基线本身坏了，与 JIT 无关。
#[test]
fn host_baseline_catch() {
    let r = std::panic::catch_unwind(|| probe_raise());
    let p = r.expect_err("宿主基线应收到 payload");
    assert_eq!(*p.downcast::<i32>().unwrap(), 0x2a);
}

/// 二分定位（probe 分支 0）：导入符号直调——probe_mark 可见即 import 调用链好。
#[test]
fn import_call_works() {
    PAD_MARK.store(0, Ordering::SeqCst);
    let mut fb = settings::builder();
    fb.set("opt_level", "speed").unwrap();
    let isa = cranelift_native::builder()
        .unwrap()
        .finish(settings::Flags::new(fb))
        .unwrap();
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
    assert_eq!(PAD_MARK.load(Ordering::SeqCst), 7, "import 直调未生效");
}

/// 二分定位（分支 A0）：单 JIT 帧穿越（caller 直调 probe_raise，无中间帧）
#[test]
fn cfi_single_frame() {
    let mut fb = settings::builder();
    fb.set("opt_level", "speed").unwrap();
    fb.set("unwind_info", "true").unwrap();
    fb.set("preserve_frame_pointers", "true").unwrap();
    let isa = cranelift_native::builder()
        .unwrap()
        .finish(settings::Flags::new(fb))
        .unwrap();
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
        panic!("无 SystemV UnwindInfo");
    }
    let mut eh = EhFrame(EndianVec::new(RunTimeEndian::Little));
    table.write_eh_frame(&mut eh).unwrap();
    let mut bytes = eh.0.into_vec();
    bytes.extend_from_slice(&[0, 0, 0, 0]);
    let buf: &'static [u8] = Box::leak(bytes.into_boxed_slice());
    unsafe extern "C" {
        fn __register_frame(fde: *const u8);
    }
    unsafe {
        let start = buf.as_ptr();
        let end = start.add(buf.len());
        let mut cur = start;
        while cur < end {
            let len = u32::from_le_bytes(std::ptr::read(cur as *const [u8; 4])) as usize;
            if len == 0 {
                break;
            }
            let cie_ptr = u32::from_le_bytes(std::ptr::read(cur.add(4) as *const [u8; 4]));
            if cie_ptr != 0 {
                __register_frame(cur);
            }
            cur = cur.add(len + 4);
        }
    }
    let caller_fn: unsafe extern "C-unwind" fn() = unsafe { std::mem::transmute(caller_addr) };
    let result = std::panic::catch_unwind(|| unsafe { caller_fn() });
    let payload = result
        .expect_err("单帧分支应收到 payload")
        .downcast::<i32>()
        .expect("载荷类型错");
    assert_eq!(*payload, 0x2a);
}

/// 二分定位（probe 分支 A）：纯 CFI 穿越——无 personality/LSDA，宿主 panic 经
/// 两个 JIT 帧（普通 call）传回宿主 catch_unwind。此分支不过 = 基础注册坏；
/// 过 = 问题在 LSDA/personality/pad 半区。
#[test]
fn cfi_only_passthrough() {
    let mut fb = settings::builder();
    fb.set("opt_level", "speed").unwrap();
    fb.set("unwind_info", "true").unwrap();
    fb.set("preserve_frame_pointers", "true").unwrap();
    let isa = cranelift_native::builder()
        .unwrap()
        .finish(settings::Flags::new(fb))
        .unwrap();
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
            panic!("无 SystemV UnwindInfo");
        }
    }
    let mut eh = EhFrame(EndianVec::new(RunTimeEndian::Little));
    table.write_eh_frame(&mut eh).unwrap();
    let mut bytes = eh.0.into_vec();
    bytes.extend_from_slice(&[0, 0, 0, 0]);
    let buf: &'static [u8] = Box::leak(bytes.into_boxed_slice());
    unsafe extern "C" {
        fn __register_frame(fde: *const u8);
    }
    unsafe {
        let start = buf.as_ptr();
        let end = start.add(buf.len());
        let mut cur = start;
        while cur < end {
            let len = u32::from_le_bytes(std::ptr::read(cur as *const [u8; 4])) as usize;
            if len == 0 {
                break;
            }
            let cie_ptr = u32::from_le_bytes(std::ptr::read(cur.add(4) as *const [u8; 4]));
            if cie_ptr != 0 {
                __register_frame(cur);
            }
            cur = cur.add(len + 4);
        }
    }

    let caller_fn: unsafe extern "C-unwind" fn() = unsafe { std::mem::transmute(caller_addr) };
    let result = std::panic::catch_unwind(|| unsafe { caller_fn() });
    let payload = result
        .expect_err("CFI-only 分支应收到宿主 payload（基础注册疑似坏）")
        .downcast::<i32>()
        .expect("载荷类型错");
    assert_eq!(*payload, 0x2a);
}

#[test]
fn lsda_cleanup_pad_executes_and_resume_continues() {
    PAD_MARK.store(0, Ordering::SeqCst);

    let mut fb = settings::builder();
    fb.set("opt_level", "speed").unwrap();
    fb.set("unwind_info", "true").unwrap();
    fb.set("preserve_frame_pointers", "true").unwrap();
    let isa = cranelift_native::builder()
        .unwrap()
        .finish(settings::Flags::new(fb))
        .unwrap();
    let mut jb = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
    jb.symbol("probe_raise", probe_raise as *const u8);
    jb.symbol("probe_mark", probe_mark as *const u8);
    jb.symbol("_Unwind_Resume", {
        unsafe extern "C" {
            fn _Unwind_Resume(ex: *mut u8) -> !;
        }
        _Unwind_Resume as *const u8
    });
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

    // raiser：调 probe_raise（宿主 resume_unwind 载荷经其帧穿过）
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

    // caller：try_call(raiser)；normal → ok(mark 2)；tag0 pad(mark 1 → _Unwind_Resume(exn))
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
        b.append_block_param(pad, types::I64); // TryCallExn(0) 的落点块参
        b.switch_to_block(entry);

        let rref = module.declare_func_in_func(raiser_id, b.func);
        let sig0 = b.func.import_signature(Signature::new(CallConv::SystemV));
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
        "caller 应有多个 call-site（try_call + 其余调用点全覆盖）"
    );
    module.finalize_definitions().unwrap();

    // eh_frame：CIE0 无 personality（raiser）；CIE1 = rust_eh_personality + lsda（caller）。
    // personality 走 DW.ref 间接（cg_clif 形态）：CIE 的 personality 指针指向一个
    // 持有真 personality 地址的静态 u64——absptr 直嵌在本环境被证伪（空 LSDA 也 abort）。
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
        panic!("raiser 无 SystemV UnwindInfo");
    }
    let lsda_bytes = build_lsda(&call_sites);
    let lsda_addr = lsda_bytes.as_ptr() as u64;
    std::mem::forget(lsda_bytes); // FDE/LSDA 终身有效（probe 进程期）
    if let UnwindInfo::SystemV(info) = ui_caller {
        let mut fde = info.to_fde(Address::Constant(caller_addr));
        fde.lsda = Some(Address::Constant(lsda_addr));
        table.add_fde(cie_pers_id, fde);
    } else {
        panic!("caller 无 SystemV UnwindInfo");
    }

    // spike5 同款注册：FrameTable → eh_frame 字节 + 终止零长 + 逐 FDE __register_frame
    let mut eh = EhFrame(EndianVec::new(RunTimeEndian::Little));
    table.write_eh_frame(&mut eh).unwrap();
    let mut bytes = eh.0.into_vec();
    bytes.extend_from_slice(&[0, 0, 0, 0]);
    let buf: &'static [u8] = Box::leak(bytes.into_boxed_slice());
    unsafe extern "C" {
        fn __register_frame(fde: *const u8);
    }
    unsafe {
        let start = buf.as_ptr();
        let end = start.add(buf.len());
        let mut cur = start;
        while cur < end {
            let len = u32::from_le_bytes(std::ptr::read(cur as *const [u8; 4])) as usize;
            if len == 0 {
                break;
            }
            let cie_ptr = u32::from_le_bytes(std::ptr::read(cur.add(4) as *const [u8; 4]));
            if cie_ptr != 0 {
                __register_frame(cur);
            }
            cur = cur.add(len + 4);
        }
    }

    // 全链点火：宿主 catch_unwind 应收到 42；pad 应已走（mark=1，而非 2）
    let caller_fn: unsafe extern "C-unwind" fn() = unsafe { std::mem::transmute(caller_addr) };
    let result = std::panic::catch_unwind(|| unsafe { caller_fn() });
    assert!(
        result.is_err(),
        "caller 未抛出（pad/unwind 链断裂；PAD_MARK={}）",
        PAD_MARK.load(Ordering::SeqCst)
    );
    let payload = result.unwrap_err();
    assert_eq!(payload.downcast_ref::<i32>(), Some(&0x2a), "载荷丢失/替换");
    assert_eq!(
        PAD_MARK.load(Ordering::SeqCst),
        1,
        "cleanup pad 未执行（LSDA/personality 未命中）"
    );
}
