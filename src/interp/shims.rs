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

/// 遍历"最终二进制会链接到"的全部 def（本地 crate + 依赖的导出符号）。
/// 服务于按符号名找函数与 .init_array 静态量扫描（Miri iter_exported_symbols 同构）。
pub fn for_each_linked_def<'tcx>(
    tcx: ty::TyCtxt<'tcx>,
    mut f: impl FnMut(rustc_hir::def_id::DefId) -> InterpResult<'tcx, ()>,
) -> InterpResult<'tcx, ()> {
    use rustc_hir::def_id::LOCAL_CRATE;
    use rustc_middle::middle::codegen_fn_attrs::CodegenFnAttrFlags;
    use rustc_middle::middle::exported_symbols::ExportedSymbol;
    use rustc_session::config::CrateType;

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
        f(def_id.into())?;
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
                    f(def_id)?;
                }
            }
        }
    }
    interp_ok(())
}

/// 按符号名查找已导出的 Rust 函数（Miri lookup_exported_symbol 的简化移植）。
fn find_exported_symbol<'tcx>(
    ecx: &mut MirvmInterpCx<'tcx>,
    link_name: Symbol,
) -> InterpResult<'tcx, Option<ty::Instance<'tcx>>> {
    if let Some(cached) = ecx.machine.exported_symbols_cache.get(&link_name) {
        return interp_ok(*cached);
    }
    let tcx = ecx.tcx.tcx;

    // (instance, is_weak)；非 weak 覆盖 weak
    let mut found: Option<(ty::Instance<'tcx>, bool)> = None;
    for_each_linked_def(tcx, |def_id| {
        if tcx.is_foreign_item(def_id) {
            return interp_ok(());
        }
        let attrs = tcx.codegen_fn_attrs(def_id);
        let instance = ty::Instance::mono(tcx, def_id);
        if tcx.symbol_name(instance).name != link_name.as_str() {
            return interp_ok(());
        }
        let is_weak = attrs.linkage == Some(rustc_hir::attrs::Linkage::WeakAny);
        match &found {
            Some((_, prev_weak)) if !prev_weak => {} // 已有强定义
            _ if !is_weak => found = Some((instance, false)),
            None => found = Some((instance, true)),
            _ => {}
        }
        interp_ok(())
    })?;

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
                n if ecx.machine.host_fds.contains(&n) => {
                    let r = unsafe { libc::write(n, bytes.as_ptr().cast(), bytes.len()) };
                    sync_errno(ecx)?;
                    if r < 0 { Err(std::io::Error::last_os_error()) } else { Ok(r as u64) }
                }
                _ => throw_unsup_format!("write 到未知 fd {fd}"),
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
            let nr = ecx.read_target_usize(&args[0])?;
            match nr {
                // SYS_getrandom
                318 => {
                    let buf = ecx.read_pointer(&args[1])?;
                    let len = ecx.read_target_usize(&args[2])?;
                    fill_random(ecx, buf, len)?;
                    ecx.write_scalar(
                        rustc_middle::mir::interpret::Scalar::from_int(len as i128, dest.layout.size),
                        dest,
                    )?;
                }
                // SYS_statx：报 ENOSYS，std 会缓存并回退到 fstat 系
                332 => {
                    write_errno(ecx, libc::ENOSYS)?;
                    ecx.write_scalar(
                        rustc_middle::mir::interpret::Scalar::from_int(-1i128, dest.layout.size),
                        dest,
                    )?;
                }
                _ => throw_unsup_format!("syscall({nr}) 未实现"),
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
        // 环境变量：查 eval 建好的表（与 environ 指向同一批 C 串）
        "getenv" => {
            let name = read_c_bytes(ecx, ecx.read_pointer(&args[0])?)?;
            match ecx.machine.env_map.get(&name).copied() {
                Some(p) => ecx.write_pointer(p, dest)?,
                None => ecx.write_scalar(
                    rustc_middle::mir::interpret::Scalar::from_target_usize(0, ecx),
                    dest,
                )?,
            }
        }
        "strlen" => {
            let ptr = ecx.read_pointer(&args[0])?;
            let len = read_c_bytes(ecx, ptr)?.len() as u64;
            ecx.write_scalar(
                rustc_middle::mir::interpret::Scalar::from_target_usize(len, ecx),
                dest,
            )?;
        }

        // ===== 时间（直通宿主；target == host 保证 C 结构布局一致）=====
        "clock_gettime" => {
            let clk = ecx.read_scalar(&args[0])?.to_i32()?;
            let ts_ptr = ecx.read_pointer(&args[1])?;
            let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
            let r = unsafe { libc::clock_gettime(clk, &mut ts) };
            sync_errno(ecx)?;
            if r == 0 {
                let bytes: [u8; size_of::<libc::timespec>()] = unsafe { std::mem::transmute(ts) };
                ecx.write_bytes_ptr(ts_ptr, bytes)?;
            }
            write_i32_ret(ecx, r, dest)?;
        }

        // ===== 文件（直通宿主 fd）=====
        "open" | "open64" => {
            let path = read_c_bytes(ecx, ecx.read_pointer(&args[0])?)?;
            let flags = ecx.read_scalar(&args[1])?.to_i32()?;
            let mode = if args.len() > 2 { ecx.read_scalar(&args[2])?.to_i32()? } else { 0 };
            let cpath = std::ffi::CString::new(path).unwrap();
            let fd = unsafe { libc::open(cpath.as_ptr(), flags, mode) };
            sync_errno(ecx)?;
            if fd >= 0 {
                ecx.machine.host_fds.insert(fd);
            }
            write_i32_ret(ecx, fd, dest)?;
        }
        "read" => {
            let fd = ecx.read_scalar(&args[0])?.to_i32()?;
            let buf_ptr = ecx.read_pointer(&args[1])?;
            let count = ecx.read_target_usize(&args[2])?;
            let mut buf = vec![0u8; count as usize];
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            sync_errno(ecx)?;
            if n > 0 {
                ecx.write_bytes_ptr(buf_ptr, buf[..n as usize].iter().copied())?;
            }
            ecx.write_scalar(
                rustc_middle::mir::interpret::Scalar::from_int(n as i128, dest.layout.size),
                dest,
            )?;
        }
        "close" => {
            let fd = ecx.read_scalar(&args[0])?.to_i32()?;
            let r = if ecx.machine.host_fds.remove(&fd) {
                let r = unsafe { libc::close(fd) };
                sync_errno(ecx)?;
                r
            } else {
                0 // 不碰宿主自己的 fd
            };
            write_i32_ret(ecx, r, dest)?;
        }
        "lseek" | "lseek64" => {
            let fd = ecx.read_scalar(&args[0])?.to_i32()?;
            let off = ecx.read_scalar(&args[1])?.to_i64()?;
            let whence = ecx.read_scalar(&args[2])?.to_i32()?;
            let r = unsafe { libc::lseek64(fd, off, whence) };
            sync_errno(ecx)?;
            ecx.write_scalar(
                rustc_middle::mir::interpret::Scalar::from_int(r as i128, dest.layout.size),
                dest,
            )?;
        }
        "fstat" | "fstat64" => {
            let fd = ecx.read_scalar(&args[0])?.to_i32()?;
            let out = ecx.read_pointer(&args[1])?;
            let mut st: libc::stat64 = unsafe { std::mem::zeroed() };
            let r = unsafe { libc::fstat64(fd, &mut st) };
            sync_errno(ecx)?;
            if r == 0 {
                write_c_struct(ecx, out, &st)?;
            }
            write_i32_ret(ecx, r, dest)?;
        }
        "stat" | "stat64" | "lstat" | "lstat64" => {
            let path = read_c_bytes(ecx, ecx.read_pointer(&args[0])?)?;
            let out = ecx.read_pointer(&args[1])?;
            let cpath = std::ffi::CString::new(path).unwrap();
            let mut st: libc::stat64 = unsafe { std::mem::zeroed() };
            let r = if name.starts_with('l') {
                unsafe { libc::lstat64(cpath.as_ptr(), &mut st) }
            } else {
                unsafe { libc::stat64(cpath.as_ptr(), &mut st) }
            };
            sync_errno(ecx)?;
            if r == 0 {
                write_c_struct(ecx, out, &st)?;
            }
            write_i32_ret(ecx, r, dest)?;
        }
        "fstatat" | "fstatat64" | "newfstatat" => {
            let dirfd = ecx.read_scalar(&args[0])?.to_i32()?;
            let path = read_c_bytes(ecx, ecx.read_pointer(&args[1])?)?;
            let out = ecx.read_pointer(&args[2])?;
            let flags = ecx.read_scalar(&args[3])?.to_i32()?;
            let cpath = std::ffi::CString::new(path).unwrap();
            let mut st: libc::stat64 = unsafe { std::mem::zeroed() };
            let r = unsafe { libc::fstatat64(dirfd, cpath.as_ptr(), &mut st, flags) };
            sync_errno(ecx)?;
            if r == 0 {
                write_c_struct(ecx, out, &st)?;
            }
            write_i32_ret(ecx, r, dest)?;
        }

        // 动态符号探测（getrandom crate 等走 dlsym(RTLD_DEFAULT, ..)）：
        // 认识的符号给合成函数指针（调用走 call_extra_fn → 本表），否则 null
        "dlsym" => {
            let name = read_c_bytes(ecx, ecx.read_pointer(&args[1])?)?;
            const DLSYM_KNOWN: &[&str] = &["getrandom", "gettid", "strlen", "clock_gettime"];
            let name_str = String::from_utf8_lossy(&name).into_owned();
            if DLSYM_KNOWN.contains(&name_str.as_str()) {
                let fnptr = ecx.fn_ptr(rustc_const_eval::interpret::FnVal::Other(
                    Symbol::intern(&name_str),
                ));
                ecx.write_pointer(fnptr, dest)?;
            } else {
                ecx.write_scalar(
                    rustc_middle::mir::interpret::Scalar::from_target_usize(0, ecx),
                    dest,
                )?;
            }
        }

        // ===== pthread TLS key（单线程平凡实现）=====
        "pthread_key_create" => {
            let key_out = ecx.read_pointer(&args[0])?;
            // 析构器忽略：线程退出析构在 M2 不运行（记录于 DESIGN 偏差）
            let key = ecx.machine.next_pthread_key;
            ecx.machine.next_pthread_key += 1;
            let u32_layout = ecx.layout_of(ecx.tcx.types.u32)?;
            let place = ecx.ptr_to_mplace(key_out, u32_layout);
            ecx.write_scalar(rustc_middle::mir::interpret::Scalar::from_u32(key), &place)?;
            write_i32_ret(ecx, 0, dest)?;
        }
        "pthread_setspecific" => {
            let key = ecx.read_scalar(&args[0])?.to_u32()?;
            let val = ecx.read_scalar(&args[1])?;
            ecx.machine.pthread_tls.insert(key, val);
            write_i32_ret(ecx, 0, dest)?;
        }
        "pthread_getspecific" => {
            let key = ecx.read_scalar(&args[0])?.to_u32()?;
            match ecx.machine.pthread_tls.get(&key).copied() {
                Some(v) => ecx.write_scalar(v, dest)?,
                None => ecx.write_scalar(
                    rustc_middle::mir::interpret::Scalar::from_target_usize(0, ecx),
                    dest,
                )?,
            }
        }
        "pthread_key_delete" => {
            let key = ecx.read_scalar(&args[0])?.to_u32()?;
            ecx.machine.pthread_tls.remove(&key);
            write_i32_ret(ecx, 0, dest)?;
        }
        "pthread_self" => {
            ecx.write_scalar(
                rustc_middle::mir::interpret::Scalar::from_target_usize(1, ecx),
                dest,
            )?;
        }

        // ===== C 分配器（std::alloc::System 与部分 crate 直调）=====
        "malloc" => {
            let size = ecx.read_target_usize(&args[0])?;
            if size == 0 {
                ecx.write_scalar(
                    rustc_middle::mir::interpret::Scalar::from_target_usize(0, ecx),
                    dest,
                )?;
            } else {
                let ptr = ecx.allocate_ptr(
                    Size::from_bytes(size),
                    rustc_abi::Align::from_bytes(16).unwrap(), // glibc malloc 对齐
                    MemoryKind::Machine(MirvmMemoryKind::Heap),
                    AllocInit::Uninit,
                )?;
                ecx.write_pointer(ptr, dest)?;
            }
        }
        "calloc" => {
            let n = ecx.read_target_usize(&args[0])?;
            let sz = ecx.read_target_usize(&args[1])?;
            let total = n.checked_mul(sz).unwrap_or(0);
            if total == 0 {
                ecx.write_scalar(
                    rustc_middle::mir::interpret::Scalar::from_target_usize(0, ecx),
                    dest,
                )?;
            } else {
                let ptr = ecx.allocate_ptr(
                    Size::from_bytes(total),
                    rustc_abi::Align::from_bytes(16).unwrap(),
                    MemoryKind::Machine(MirvmMemoryKind::Heap),
                    AllocInit::Zero,
                )?;
                ecx.write_pointer(ptr, dest)?;
            }
        }
        "free" => {
            let ptr = ecx.read_pointer(&args[0])?;
            if ptr.addr().bytes() != 0 {
                ecx.deallocate_ptr(ptr, None, MemoryKind::Machine(MirvmMemoryKind::Heap))?;
            }
        }
        "realloc" => {
            let ptr = ecx.read_pointer(&args[0])?;
            let new_size = ecx.read_target_usize(&args[1])?;
            let align = rustc_abi::Align::from_bytes(16).unwrap();
            if ptr.addr().bytes() == 0 {
                let p = ecx.allocate_ptr(
                    Size::from_bytes(new_size),
                    align,
                    MemoryKind::Machine(MirvmMemoryKind::Heap),
                    AllocInit::Uninit,
                )?;
                ecx.write_pointer(p, dest)?;
            } else {
                let p = ecx.reallocate_ptr(
                    ptr,
                    None,
                    Size::from_bytes(new_size),
                    align,
                    MemoryKind::Machine(MirvmMemoryKind::Heap),
                    AllocInit::Uninit,
                )?;
                ecx.write_pointer(p, dest)?;
            }
        }

        "unlink" => {
            let path = read_c_bytes(ecx, ecx.read_pointer(&args[0])?)?;
            let cpath = std::ffi::CString::new(path).unwrap();
            let r = unsafe { libc::unlink(cpath.as_ptr()) };
            sync_errno(ecx)?;
            write_i32_ret(ecx, r, dest)?;
        }

        // errno：机器内一格 i32，宿主调用后由 sync_errno 同步
        "__errno_location" => {
            let cell = errno_cell(ecx)?;
            ecx.write_pointer(cell, dest)?;
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

/// 读以 NUL 结尾的字节串（不含 NUL）。
fn read_c_bytes<'tcx>(
    ecx: &MirvmInterpCx<'tcx>,
    ptr: rustc_const_eval::interpret::Pointer<Option<Prov>>,
) -> InterpResult<'tcx, Vec<u8>> {
    let u8_layout = ecx.layout_of(ecx.tcx.types.u8)?;
    let mut out = Vec::new();
    let mut i = 0u64;
    loop {
        let cell = ecx.ptr_to_mplace(ptr.wrapping_offset(Size::from_bytes(i), ecx), u8_layout);
        let b = ecx.read_scalar(&cell)?.to_u8()?;
        if b == 0 {
            return interp_ok(out);
        }
        out.push(b);
        i += 1;
    }
}

fn write_i32_ret<'tcx>(
    ecx: &mut MirvmInterpCx<'tcx>,
    v: i32,
    dest: &PlaceTy<'tcx, Prov>,
) -> InterpResult<'tcx> {
    ecx.write_scalar(
        rustc_middle::mir::interpret::Scalar::from_int(v as i128, dest.layout.size),
        dest,
    )
}

/// 把宿主 C 结构按字节写进解释器内存（target == host，布局精确一致）。
fn write_c_struct<'tcx, T: Copy>(
    ecx: &mut MirvmInterpCx<'tcx>,
    ptr: rustc_const_eval::interpret::Pointer<Option<Prov>>,
    val: &T,
) -> InterpResult<'tcx> {
    let bytes =
        unsafe { std::slice::from_raw_parts((val as *const T).cast::<u8>(), size_of::<T>()) };
    ecx.write_bytes_ptr(ptr, bytes.iter().copied())
}

/// 机器内的 errno 单元（惰性分配）。
fn errno_cell<'tcx>(
    ecx: &mut MirvmInterpCx<'tcx>,
) -> InterpResult<'tcx, rustc_const_eval::interpret::Pointer<super::machine::Prov>> {
    if let Some(p) = ecx.machine.errno_cell {
        return interp_ok(p);
    }
    let p = ecx.allocate_ptr(
        Size::from_bytes(4),
        rustc_abi::Align::from_bytes(4).unwrap(),
        MemoryKind::Machine(MirvmMemoryKind::Machine),
        AllocInit::Zero,
    )?;
    ecx.machine.errno_cell = Some(p);
    interp_ok(p)
}

/// 宿主调用之后把宿主 errno 同步进解释器（std 的 last_os_error 走 __errno_location）。
fn sync_errno<'tcx>(ecx: &mut MirvmInterpCx<'tcx>) -> InterpResult<'tcx> {
    let host_errno = unsafe { *libc::__errno_location() };
    write_errno(ecx, host_errno)
}

/// 直接写解释器侧 errno。
fn write_errno<'tcx>(ecx: &mut MirvmInterpCx<'tcx>, v: i32) -> InterpResult<'tcx> {
    let cell = errno_cell(ecx)?;
    let i32_layout = ecx.layout_of(ecx.tcx.types.i32)?;
    let place = ecx.ptr_to_mplace(cell.into(), i32_layout);
    ecx.write_scalar(rustc_middle::mir::interpret::Scalar::from_i32(v), &place)
}
