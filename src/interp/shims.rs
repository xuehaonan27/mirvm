//! foreign fn shims：第一批 Unix/分配器/unwind 符号。
//! 参考 rust-lang/miri shims/*（MIT/Apache-2.0），只保留单线程快乐路径。

use std::io::Write as _;

use rustc_abi::Size;
use rustc_ast::expand::allocator::SpecialAllocatorMethod;
use rustc_const_eval::interpret::{
    AllocInit, InterpResult, MemoryKind, OpTy, PlaceTy, interp_ok,
};
use rustc_middle::mir;
use rustc_middle::ty::{self, Ty};
use rustc_middle::{throw_machine_stop, throw_unsup_format};
use rustc_span::Symbol;
use rustc_target::callconv::FnAbi;

use super::machine::{MirvmMemoryKind, Prov, Termination};
use super::helpers::write_uint;
use super::{EmulateItemResult, MirvmInterpCx};

/// 处理 foreign item 调用。返回 Some(body) 表示改跑该 MIR；None 表示已就地处理。
pub fn emulate_foreign_item<'tcx>(
    ecx: &mut MirvmInterpCx<'tcx>,
    link_name: Symbol,
    _abi: &FnAbi<'tcx, Ty<'tcx>>,
    args: &[OpTy<'tcx, Prov>],
    dest: &PlaceTy<'tcx, Prov>,
    ret: Option<mir::BasicBlock>,
    unwind: mir::UnwindAction,
) -> InterpResult<'tcx, Option<(&'tcx mir::Body<'tcx>, ty::Instance<'tcx>)>> {
    // 分配前哨兵：空操作
    if link_name == ecx.machine.no_alloc_shim_sym {
        ecx.return_to_block(ret)?;
        return interp_ok(None);
    }
    // 分配器 shim（__rust_alloc 等 mangled 符号）
    if let Some(shim) = ecx.machine.allocator_shims.get(&link_name) {
        match shim {
            super::machine::AllocShim::Special(m) => {
                let m = *m;
                emulate_allocator(ecx, m, args, dest)?;
                ecx.return_to_block(ret)?;
                return interp_ok(None);
            }
            super::machine::AllocShim::Forward(sym) => {
                throw_unsup_format!("暂不支持转发的分配器符号（{sym}，如自定义 #[global_allocator] / alloc_error_handler）");
            }
        }
    }

    match emulate_by_name(ecx, link_name.as_str(), args, dest)? {
        EmulateItemResult::NeedsReturn => {
            ecx.return_to_block(ret)?;
        }
        EmulateItemResult::NeedsUnwind => {
            ecx.unwind_to_block(unwind)?;
        }
        EmulateItemResult::AlreadyJumped => {}
        EmulateItemResult::NotSupported => {
            // 兜底：按符号名在所有已链接 crate 里找导出的 Rust 函数
            // （rust_begin_unwind / __rdl_* 等都走这条路）
            if let Some(instance) = find_exported_symbol(ecx, link_name)? {
                return interp_ok(Some((ecx.load_mir(instance.def, None)?, instance)));
            }
            throw_machine_stop!(Termination::Unsupported(format!(
                "mirvm: 尚未实现的 foreign 函数 `{link_name}`（M1 shim 集之外；欢迎补充 src/interp/shims.rs）"
            )));
        }
    }
    interp_ok(None)
}

/// 按符号名查找已导出的 Rust 函数（Miri lookup_exported_symbol 的简化移植）。
fn find_exported_symbol<'tcx>(
    ecx: &mut MirvmInterpCx<'tcx>,
    link_name: Symbol,
) -> InterpResult<'tcx, Option<ty::Instance<'tcx>>> {
    use rustc_hir::def_id::{DefId, LOCAL_CRATE};
    use rustc_middle::middle::codegen_fn_attrs::CodegenFnAttrFlags;
    use rustc_middle::middle::exported_symbols::ExportedSymbol;
    use rustc_session::config::CrateType;

    if let Some(cached) = ecx.machine.exported_symbols_cache.get(&link_name) {
        return interp_ok(*cached);
    }
    let tcx = ecx.tcx.tcx;

    // (instance, is_weak)；非 weak 覆盖 weak
    let mut found: Option<(ty::Instance<'tcx>, bool)> = None;
    let mut consider = |def_id: DefId| {
        if tcx.is_foreign_item(def_id) {
            return;
        }
        let attrs = tcx.codegen_fn_attrs(def_id);
        let instance = ty::Instance::mono(tcx, def_id);
        if tcx.symbol_name(instance).name != link_name.as_str() {
            return;
        }
        let is_weak = attrs.linkage == Some(rustc_hir::attrs::Linkage::WeakAny);
        match &found {
            Some((_, prev_weak)) if !prev_weak => {} // 已有强定义
            _ if !is_weak => found = Some((instance, false)),
            None => found = Some((instance, true)),
            _ => {}
        }
    };

    // 本地 crate：遍历 HIR（exported_symbols 会漏 #[used]）
    for def_id in tcx.hir_crate_items(()).definitions() {
        if !tcx.def_kind(def_id).has_codegen_attrs() {
            continue;
        }
        let attrs = tcx.codegen_fn_attrs(def_id);
        let exported = attrs.contains_extern_indicator()
            || attrs.flags.contains(CodegenFnAttrFlags::USED_COMPILER)
            || attrs.flags.contains(CodegenFnAttrFlags::USED_LINKER);
        if !exported || tcx.generics_of(def_id).requires_monomorphization(tcx) {
            continue;
        }
        consider(def_id.into());
    }
    // 依赖 crate
    let dependency_formats = tcx.dependency_formats(());
    if let Some(format) = dependency_formats.get(&CrateType::Executable) {
        for (cnum, &linkage) in format.iter_enumerated() {
            if cnum == LOCAL_CRATE
                || linkage == rustc_middle::middle::dependency_format::Linkage::NotLinked
            {
                continue;
            }
            for &(symbol, _) in tcx.exported_non_generic_symbols(cnum) {
                if let ExportedSymbol::NonGeneric(def_id) = symbol {
                    consider(def_id);
                }
            }
        }
    }

    let res = found.map(|(i, _)| i);
    ecx.machine.exported_symbols_cache.insert(link_name, res);
    interp_ok(res)
}

fn emulate_allocator<'tcx>(
    ecx: &mut MirvmInterpCx<'tcx>,
    method: SpecialAllocatorMethod,
    args: &[OpTy<'tcx, Prov>],
    dest: &PlaceTy<'tcx, Prov>,
) -> InterpResult<'tcx> {
    use SpecialAllocatorMethod::*;
    let heap = MemoryKind::Machine(MirvmMemoryKind::Heap);
    match method {
        Alloc | AllocZeroed => {
            let size = ecx.read_target_usize(&args[0])?;
            let align = ecx.read_target_usize(&args[1])?;
            let ptr = ecx.allocate_ptr(
                Size::from_bytes(size),
                rustc_abi::Align::from_bytes(align).unwrap(),
                heap,
                if matches!(method, AllocZeroed) { AllocInit::Zero } else { AllocInit::Uninit },
            )?;
            ecx.write_pointer(ptr, dest)
        }
        Dealloc => {
            let ptr = ecx.read_pointer(&args[0])?;
            let size = ecx.read_target_usize(&args[1])?;
            let align = ecx.read_target_usize(&args[2])?;
            ecx.deallocate_ptr(
                ptr,
                Some((Size::from_bytes(size), rustc_abi::Align::from_bytes(align).unwrap())),
                heap,
            )
        }
        Realloc => {
            let ptr = ecx.read_pointer(&args[0])?;
            let old_size = ecx.read_target_usize(&args[1])?;
            let align = ecx.read_target_usize(&args[2])?;
            let new_size = ecx.read_target_usize(&args[3])?;
            let align = rustc_abi::Align::from_bytes(align).unwrap();
            let new_ptr = ecx.reallocate_ptr(
                ptr,
                Some((Size::from_bytes(old_size), align)),
                Size::from_bytes(new_size),
                align,
                heap,
                AllocInit::Uninit,
            )?;
            ecx.write_pointer(new_ptr, dest)
        }
    }
}

fn emulate_by_name<'tcx>(
    ecx: &mut MirvmInterpCx<'tcx>,
    name: &str,
    args: &[OpTy<'tcx, Prov>],
    dest: &PlaceTy<'tcx, Prov>,
) -> InterpResult<'tcx, EmulateItemResult> {
    match name {
        // ===== I/O =====
        "write" => {
            let fd = ecx.read_scalar(&args[0])?.to_i32()?;
            let buf = ecx.read_pointer(&args[1])?;
            let count = ecx.read_target_usize(&args[2])?;
            let bytes = ecx.read_bytes_ptr_strip_provenance(buf, Size::from_bytes(count))?.to_vec();
            let written = match fd {
                1 => {
                    let mut out = std::io::stdout().lock();
                    out.write_all(&bytes).and_then(|_| out.flush()).map(|_| count)
                }
                2 => {
                    let mut out = std::io::stderr().lock();
                    out.write_all(&bytes).and_then(|_| out.flush()).map(|_| count)
                }
                _ => throw_unsup_format!("write 到 fd {fd}（仅支持 1/2）"),
            };
            let ret = written.map(|n| n as i128).unwrap_or(-1);
            ecx.write_scalar(
                rustc_middle::mir::interpret::Scalar::from_int(ret, dest.layout.size),
                dest,
            )?;
        }

        // ===== 随机数（确定性）=====
        "getrandom" => {
            let buf = ecx.read_pointer(&args[0])?;
            let len = ecx.read_target_usize(&args[1])?;
            fill_random(ecx, buf, len)?;
            ecx.write_scalar(
                rustc_middle::mir::interpret::Scalar::from_int(len as i128, dest.layout.size),
                dest,
            )?;
        }
        "syscall" => {
            // 只认 SYS_getrandom（x86_64: 318）
            let nr = ecx.read_target_usize(&args[0])?;
            if nr == 318 {
                let buf = ecx.read_pointer(&args[1])?;
                let len = ecx.read_target_usize(&args[2])?;
                fill_random(ecx, buf, len)?;
                ecx.write_scalar(
                    rustc_middle::mir::interpret::Scalar::from_int(len as i128, dest.layout.size),
                    dest,
                )?;
            } else {
                throw_unsup_format!("syscall({nr}) 未实现");
            }
        }

        // ===== 进程控制 =====
        "exit" | "_exit" => {
            let code = ecx.read_scalar(&args[0])?.to_i32()?;
            throw_machine_stop!(Termination::Exit(code));
        }
        "abort" => {
            throw_machine_stop!(Termination::Abort("程序调用了 abort()".into()));
        }
        "gettid" => {
            ecx.write_scalar(
                rustc_middle::mir::interpret::Scalar::from_int(1001, dest.layout.size),
                dest,
            )?;
        }
        // 空环境：getenv 恒为 null（与 environ 空表一致）
        "getenv" => {
            ecx.write_scalar(
                rustc_middle::mir::interpret::Scalar::from_target_usize(0, ecx),
                dest,
            )?;
        }
        "strlen" => {
            let ptr = ecx.read_pointer(&args[0])?;
            let u8_layout = ecx.layout_of(ecx.tcx.types.u8)?;
            let mut len: u64 = 0;
            loop {
                let cell = ecx.ptr_to_mplace(ptr.wrapping_offset(Size::from_bytes(len), ecx), u8_layout);
                if ecx.read_scalar(&cell)?.to_u8()? == 0 {
                    break;
                }
                len += 1;
            }
            ecx.write_scalar(
                rustc_middle::mir::interpret::Scalar::from_target_usize(len, ecx),
                dest,
            )?;
        }

        // ===== unwinding：panic_unwind(gcc.rs) 的落点 =====
        "_Unwind_RaiseException" => {
            // 参数 = *mut _Unwind_Exception；记为 payload，开始解释器 unwinding。
            // catch 侧（catch_unwind intrinsic）会把它原样传给 catch_fn →
            // __rust_panic_cleanup 用 container-of 恢复 Box<Exception>。
            let payload = ecx.read_immediate(&args[0])?;
            ecx.machine.unwind_payloads.push(payload);
            return interp_ok(EmulateItemResult::NeedsUnwind);
        }

        // 分配失败处理（OOM 才会触碰）
        "__rust_alloc_error_handler" => {
            throw_machine_stop!(Termination::Abort("内存分配失败（alloc_error_handler）".into()));
        }

        _ => return interp_ok(EmulateItemResult::NotSupported),
    }
    interp_ok(EmulateItemResult::NeedsReturn)
}

/// xorshift64* 确定性填充：HashMap 种子等对正确性无要求，确定性利于复现。
fn fill_random<'tcx>(
    ecx: &mut MirvmInterpCx<'tcx>,
    ptr: rustc_const_eval::interpret::Pointer<Option<Prov>>,
    len: u64,
) -> InterpResult<'tcx> {
    let mut s = ecx.machine.rng_state;
    let bytes: Vec<u8> = (0..len)
        .map(|_| {
            s ^= s >> 12;
            s ^= s << 25;
            s ^= s >> 27;
            (s.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 56) as u8
        })
        .collect();
    ecx.machine.rng_state = s;
    ecx.write_bytes_ptr(ptr, bytes)
}

// 引用以避免 unused warning（write_uint 供后续 shims 使用）
#[allow(unused_imports)]
use write_uint as _keep;
