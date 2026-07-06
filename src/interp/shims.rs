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
    abi: &FnAbi<'tcx, Ty<'tcx>>,
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
            // 兜底 1：按符号名在所有已链接 crate 里找导出的 Rust 函数
            // （rust_begin_unwind / __rdl_* 等都走这条路）
            if let Some(instance) = find_exported_symbol(ecx, link_name)? {
                return interp_ok(Some((ecx.load_mir(instance.def, None)?, instance)));
            }
            // 兜底 2：libffi 原生直调（C 依赖库函数）
            if super::native::call_native(ecx, link_name, abi.c_variadic, args, dest)? {
                ecx.return_to_block(ret)?;
                return interp_ok(None);
            }
            throw_machine_stop!(Termination::Unsupported(format!(
                "mirvm: 尚未实现的 foreign 函数 `{link_name}`（shim/导出符号/native 库都没找到；欢迎补充 src/interp/shims.rs）"
            )));
        }
    }
    interp_ok(None)
}

/// libm 数学函数（宿主 f64 直算）。返回 None = 非数学函数。
/// 覆盖 f64 单/双参数版与 f32（`...f`）版；结果与 libm/SSE 逐位一致（host==target）。
fn emulate_libm<'tcx>(
    ecx: &mut MirvmInterpCx<'tcx>,
    name: &str,
    args: &[OpTy<'tcx, Prov>],
    dest: &PlaceTy<'tcx, Prov>,
) -> InterpResult<'tcx, Option<EmulateItemResult>> {
    // f32 版：去掉尾缀 'f' 后按 f64 逻辑算，再窄化
    let (base, is_f32) = match name.strip_suffix('f') {
        Some(b) if is_libm_name(b) => (b, true),
        _ => (name, false),
    };
    if !is_libm_name(base) {
        return interp_ok(None);
    }

    use rustc_apfloat::Float as _;
    let rd = |ecx: &mut MirvmInterpCx<'tcx>, i: usize| -> InterpResult<'tcx, f64> {
        let s = ecx.read_scalar(&args[i])?;
        interp_ok(if is_f32 {
            f32::from_bits(s.to_f32()?.to_bits() as u32) as f64
        } else {
            f64::from_bits(s.to_f64()?.to_bits() as u64)
        })
    };
    let a = rd(ecx, 0)?;
    let r: f64 = match base {
        "sqrt" => a.sqrt(),
        "cbrt" => a.cbrt(),
        "sin" => a.sin(),
        "cos" => a.cos(),
        "tan" => a.tan(),
        "asin" => a.asin(),
        "acos" => a.acos(),
        "atan" => a.atan(),
        "sinh" => a.sinh(),
        "cosh" => a.cosh(),
        "tanh" => a.tanh(),
        "exp" => a.exp(),
        "exp2" => a.exp2(),
        "expm1" => a.exp_m1(),
        "log" => a.ln(),
        "log2" => a.log2(),
        "log10" => a.log10(),
        "log1p" => a.ln_1p(),
        "floor" => a.floor(),
        "ceil" => a.ceil(),
        "round" => a.round(),
        "trunc" => a.trunc(),
        "fabs" => a.abs(),
        "rint" | "nearbyint" => a.round_ties_even(),
        // 双参数
        "pow" => a.powf(rd(ecx, 1)?),
        "fmod" => a % rd(ecx, 1)?,
        "hypot" => a.hypot(rd(ecx, 1)?),
        "atan2" => a.atan2(rd(ecx, 1)?),
        "copysign" => a.copysign(rd(ecx, 1)?),
        "fmin" => a.min(rd(ecx, 1)?),
        "fmax" => a.max(rd(ecx, 1)?),
        "fdim" => (a - rd(ecx, 1)?).max(0.0),
        "ldexp" | "scalbn" => a * 2f64.powi(ecx.read_scalar(&args[1])?.to_i32()?),
        _ => unreachable!(),
    };
    let scalar = if is_f32 {
        rustc_middle::mir::interpret::Scalar::from_f32(rustc_apfloat::ieee::Single::from_bits(
            (r as f32).to_bits() as u128,
        ))
    } else {
        rustc_middle::mir::interpret::Scalar::from_f64(rustc_apfloat::ieee::Double::from_bits(
            r.to_bits() as u128,
        ))
    };
    ecx.write_scalar(scalar, dest)?;
    interp_ok(Some(EmulateItemResult::NeedsReturn))
}

fn is_libm_name(n: &str) -> bool {
    matches!(
        n,
        "sqrt" | "cbrt" | "sin" | "cos" | "tan" | "asin" | "acos" | "atan"
            | "sinh" | "cosh" | "tanh" | "exp" | "exp2" | "expm1" | "log" | "log2"
            | "log10" | "log1p" | "floor" | "ceil" | "round" | "trunc" | "fabs"
            | "rint" | "nearbyint" | "pow" | "fmod" | "hypot" | "atan2" | "copysign"
            | "fmin" | "fmax" | "fdim" | "ldexp" | "scalbn"
    )
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
    // libm 数学函数：用宿主 f64 直算（target==host 逐位一致）。放在最前，
    // 否则会被 compiler_builtins 的 Rust 实现（内联汇编）遮蔽。
    if let Some(res) = emulate_libm(ecx, name, args, dest)? {
        return interp_ok(res);
    }
    match name {
        // ===== I/O =====
        "write" => {
            let fd = ecx.read_scalar(&args[0])?.to_i32()?;
            let buf = ecx.read_pointer(&args[1])?;
            let count = ecx.read_target_usize(&args[2])?;
            let bytes = ecx.read_bytes_ptr_strip_provenance(buf, Size::from_bytes(count))?.to_vec();
            #[cfg(debug_assertions)]
            if count > 0 {
                // 真实地址不变式（账本 C2）：guest 指针的绝对地址就是宿主地址，
                // 直读必须与解释器视角一致。每次 write 都在验证。
                let host =
                    unsafe { std::slice::from_raw_parts(buf.addr().bytes() as *const u8, bytes.len()) };
                debug_assert_eq!(host, &bytes[..], "真实地址内存不变式被破坏");
            }
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
                // SYS_futex：std 的 Mutex/Condvar/Once/park 全押在这
                202 => return emulate_futex(ecx, args, dest),
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
            let tid = 1000 + ecx.machine.threads.active_id() as i32;
            ecx.write_scalar(
                rustc_middle::mir::interpret::Scalar::from_int(tid, dest.layout.size),
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

        // ===== pthread TLS key（值 per-thread，析构在线程退出时运行）=====
        "pthread_key_create" => {
            let key_out = ecx.read_pointer(&args[0])?;
            let dtor_ptr = ecx.read_pointer(&args[1])?;
            let dtor = if dtor_ptr.addr().bytes() != 0 { Some(dtor_ptr) } else { None };
            let key = ecx.machine.threads.next_pthread_key;
            ecx.machine.threads.next_pthread_key += 1;
            ecx.machine.threads.key_dtors.insert(key, dtor);
            let u32_layout = ecx.layout_of(ecx.tcx.types.u32)?;
            let place = ecx.ptr_to_mplace(key_out, u32_layout);
            ecx.write_scalar(rustc_middle::mir::interpret::Scalar::from_u32(key), &place)?;
            write_i32_ret(ecx, 0, dest)?;
        }
        "pthread_setspecific" => {
            let key = ecx.read_scalar(&args[0])?.to_u32()?;
            let valp = ecx.read_pointer(&args[1])?;
            if valp.addr().bytes() == 0 {
                // 置 null = 删除（TLS 析构轮次依赖"map 里只有非空值"这一不变式）
                ecx.machine.threads.active_mut().pthread_tls.remove(&key);
            } else {
                let val = ecx.read_scalar(&args[1])?;
                ecx.machine.threads.active_mut().pthread_tls.insert(key, val);
            }
            write_i32_ret(ecx, 0, dest)?;
        }
        "pthread_getspecific" => {
            let key = ecx.read_scalar(&args[0])?.to_u32()?;
            match ecx.machine.threads.active().pthread_tls.get(&key).copied() {
                Some(v) => ecx.write_scalar(v, dest)?,
                None => ecx.write_scalar(
                    rustc_middle::mir::interpret::Scalar::from_target_usize(0, ecx),
                    dest,
                )?,
            }
        }
        "pthread_key_delete" => {
            let key = ecx.read_scalar(&args[0])?.to_u32()?;
            ecx.machine.threads.key_dtors.remove(&key);
            ecx.machine.threads.active_mut().pthread_tls.remove(&key);
            write_i32_ret(ecx, 0, dest)?;
        }
        "pthread_self" => {
            let id = ecx.machine.threads.active_id() as u64;
            ecx.write_scalar(
                rustc_middle::mir::interpret::Scalar::from_target_usize(id + 1, ecx),
                dest,
            )?;
        }

        // ===== 线程生命周期 =====
        "pthread_create" => {
            let thread_out = ecx.read_pointer(&args[0])?;
            // args[1] = attr（忽略；栈由我们管理）
            let start = ecx.read_pointer(&args[2])?;
            let arg = ecx.read_immediate(&args[3])?;
            let tid = spawn_guest_thread(ecx, start, arg)?;
            let usize_layout = ecx.layout_of(ecx.tcx.types.usize)?;
            let place = ecx.ptr_to_mplace(thread_out, usize_layout);
            ecx.write_scalar(
                rustc_middle::mir::interpret::Scalar::from_target_usize(tid as u64, ecx),
                &place,
            )?;
            write_i32_ret(ecx, 0, dest)?;
        }
        "pthread_join" => {
            let tid = ecx.read_target_usize(&args[0])? as super::threads::ThreadId;
            // std 传 null 作 retval_out，不支持非 null（罕见）
            match ecx.machine.threads.get(tid).map(|t| t.state.clone()) {
                Some(super::threads::ThreadState::Terminated) => {
                    write_i32_ret(ecx, 0, dest)?;
                }
                Some(_) => {
                    // 阻塞当前线程；先写好返回值 0（唤醒后直接可见），再让出
                    write_i32_ret(ecx, 0, dest)?;
                    ecx.machine
                        .threads
                        .block_active(super::threads::BlockReason::Join(tid));
                }
                None => {
                    write_i32_ret(ecx, libc::ESRCH, dest)?;
                }
            }
        }
        "pthread_detach" => {
            let tid = ecx.read_target_usize(&args[0])? as super::threads::ThreadId;
            if let Some(t) = ecx.machine.threads.get_mut(tid) {
                t.detached = true;
            }
            write_i32_ret(ecx, 0, dest)?;
        }
        "pthread_attr_init" | "pthread_attr_setstacksize" | "pthread_attr_destroy" => {
            write_i32_ret(ecx, 0, dest)?;
        }
        "sched_yield" => {
            ecx.machine.threads.yield_requested = true;
            write_i32_ret(ecx, 0, dest)?;
        }
        // spin_loop 提示（core::hint::spin_loop → _mm_pause）：让出时间片
        "llvm.x86.sse2.pause" => {
            ecx.machine.threads.yield_requested = true;
        }
        // 线程命名（PR_SET_NAME）等：记录名字，其余忽略
        "prctl" => {
            let op = ecx.read_scalar(&args[0])?.to_i32()?;
            if op == libc::PR_SET_NAME {
                let name = read_c_bytes(ecx, ecx.read_pointer(&args[1])?)?;
                ecx.machine.threads.active_mut().name =
                    String::from_utf8_lossy(&name).into_owned();
            }
            write_i32_ret(ecx, 0, dest)?;
        }
        "nanosleep" | "clock_nanosleep" => {
            // nanosleep(req, rem) / clock_nanosleep(clk, flags, req, rem)
            let req_idx = if name == "nanosleep" { 0 } else { 2 };
            let req_ptr = ecx.read_pointer(&args[req_idx])?;
            let ts = read_timespec(ecx, req_ptr)?;
            let deadline = std::time::Instant::now()
                + std::time::Duration::new(ts.0 as u64, ts.1 as u32);
            write_i32_ret(ecx, 0, dest)?; // 醒来即成功
            ecx.machine
                .threads
                .block_active(super::threads::BlockReason::Sleep { deadline });
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
            ecx.machine.threads.active_mut().unwind_payloads.push(payload);
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

/// 机器内的 errno 单元（per-thread，惰性分配）。
fn errno_cell<'tcx>(
    ecx: &mut MirvmInterpCx<'tcx>,
) -> InterpResult<'tcx, rustc_const_eval::interpret::Pointer<super::machine::Prov>> {
    if let Some(p) = ecx.machine.threads.active().errno_cell {
        return interp_ok(p);
    }
    let p = ecx.allocate_ptr(
        Size::from_bytes(4),
        rustc_abi::Align::from_bytes(4).unwrap(),
        MemoryKind::Machine(MirvmMemoryKind::Machine),
        AllocInit::Zero,
    )?;
    ecx.machine.threads.active_mut().errno_cell = Some(p);
    interp_ok(p)
}

/// 宿主调用之后把宿主 errno 同步进解释器（std 的 last_os_error 走 __errno_location）。
fn sync_errno<'tcx>(ecx: &mut MirvmInterpCx<'tcx>) -> InterpResult<'tcx> {
    let host_errno = unsafe { *libc::__errno_location() };
    write_errno(ecx, host_errno)
}

/// native FFI 用的公开包装。
pub(crate) fn sync_errno_pub<'tcx>(ecx: &mut MirvmInterpCx<'tcx>) -> InterpResult<'tcx> {
    sync_errno(ecx)
}

/// 直接写解释器侧 errno。
fn write_errno<'tcx>(ecx: &mut MirvmInterpCx<'tcx>, v: i32) -> InterpResult<'tcx> {
    let cell = errno_cell(ecx)?;
    let i32_layout = ecx.layout_of(ecx.tcx.types.i32)?;
    let place = ecx.ptr_to_mplace(cell.into(), i32_layout);
    ecx.write_scalar(rustc_middle::mir::interpret::Scalar::from_i32(v), &place)
}

/// 读 guest 内存里的 timespec（tv_sec: i64, tv_nsec: i64）。
fn read_timespec<'tcx>(
    ecx: &MirvmInterpCx<'tcx>,
    ptr: rustc_const_eval::interpret::Pointer<Option<Prov>>,
) -> InterpResult<'tcx, (i64, i64)> {
    let i64_layout = ecx.layout_of(ecx.tcx.types.i64)?;
    let sec = ecx.read_scalar(&ecx.ptr_to_mplace(ptr, i64_layout))?.to_i64()?;
    let nsec = ecx
        .read_scalar(&ecx.ptr_to_mplace(ptr.wrapping_offset(Size::from_bytes(8), ecx), i64_layout))?
        .to_i64()?;
    interp_ok((sec, nsec))
}

/// 创建 guest 线程：新线程 + 临时切 active 压 start_routine(arg) 根帧（Miri 同款戏法）。
fn spawn_guest_thread<'tcx>(
    ecx: &mut MirvmInterpCx<'tcx>,
    start: rustc_const_eval::interpret::Pointer<Option<Prov>>,
    arg: rustc_const_eval::interpret::ImmTy<'tcx, Prov>,
) -> InterpResult<'tcx, super::threads::ThreadId> {
    use super::helpers::EcxExt as _;
    use rustc_const_eval::interpret::ReturnContinuation;

    let instance = ecx.get_ptr_fn(start)?.as_instance()?;
    let tid = ecx.machine.threads.create_thread();

    // start_routine 返回 *mut c_void：给它一个落点（join 传递用）
    let ret_layout = ecx.layout_of(rustc_middle::ty::Ty::new_mut_ptr(
        ecx.tcx.tcx,
        ecx.tcx.types.u8,
    ))?;
    let ret_place = ecx.allocate(ret_layout, MemoryKind::Machine(MirvmMemoryKind::Machine))?;

    let prev = ecx.machine.threads.set_active(tid);
    let res = ecx.call_function(
        instance,
        &[arg],
        Some(&ret_place),
        ReturnContinuation::Stop { cleanup: true },
    );
    ecx.machine.threads.set_active(prev);
    res?;
    ecx.machine.threads.get_mut(tid).unwrap().ret_place = Some(ret_place);
    interp_ok(tid)
}

/// futex（syscall 202）：WAIT/WAKE/WAIT_BITSET/WAKE_BITSET。
/// 阻塞语义：先推测性写 0（被唤醒的结果），阻塞让出；超时路径由调度器改写
/// 为 -1 + ETIMEDOUT（见 eval.rs 的 wake_due 处理）。
fn emulate_futex<'tcx>(
    ecx: &mut MirvmInterpCx<'tcx>,
    args: &[OpTy<'tcx, Prov>],
    dest: &PlaceTy<'tcx, Prov>,
) -> InterpResult<'tcx, EmulateItemResult> {
    use super::threads::BlockReason;

    let addr = ecx.read_pointer(&args[1])?;
    let op = ecx.read_scalar(&args[2])?.to_i32()?;
    let base_op = op & !(libc::FUTEX_PRIVATE_FLAG | libc::FUTEX_CLOCK_REALTIME);

    match base_op {
        libc::FUTEX_WAIT | libc::FUTEX_WAIT_BITSET => {
            let expected = ecx.read_scalar(&args[3])?.to_u32()?;
            // 原子性由协作调度保证（step 内不可分割）：读当前值比较
            let u32_layout = ecx.layout_of(ecx.tcx.types.u32)?;
            let current = ecx.read_scalar(&ecx.ptr_to_mplace(addr, u32_layout))?.to_u32()?;
            if current != expected {
                write_errno(ecx, libc::EAGAIN)?;
                ecx.write_scalar(
                    rustc_middle::mir::interpret::Scalar::from_int(-1i128, dest.layout.size),
                    dest,
                )?;
                return interp_ok(EmulateItemResult::NeedsReturn);
            }
            // 超时：WAIT 相对，WAIT_BITSET 绝对（一律按单调钟近似）
            let timeout_ptr = ecx.read_pointer(&args[4])?;
            let deadline = if timeout_ptr.addr().bytes() != 0 {
                let (sec, nsec) = read_timespec(ecx, timeout_ptr)?;
                let dur = std::time::Duration::new(sec.max(0) as u64, nsec.max(0) as u32);
                Some(if base_op == libc::FUTEX_WAIT {
                    std::time::Instant::now() + dur
                } else {
                    // 绝对单调时刻：换算为 now + (abs - now_monotonic)。
                    // guest 的 Instant 基于我们的 clock_gettime(MONOTONIC) 直通，
                    // 与宿主同源，可直接比对。
                    monotonic_to_instant(sec, nsec)
                })
            } else {
                None
            };
            // 推测性写 0（唤醒即成功）；固化 dest 供超时改写
            let dest_m = ecx.force_allocation(dest)?;
            ecx.write_scalar(
                rustc_middle::mir::interpret::Scalar::from_uint(0u128, dest_m.layout.size),
                &dest_m,
            )?;
            let addr_bytes = addr.addr().bytes();
            ecx.machine.threads.active_mut().futex_wake =
                Some(super::threads::FutexWake { dest: dest_m });
            ecx.machine.threads.block_active(BlockReason::Futex {
                addr: addr_bytes,
                deadline,
            });
            interp_ok(EmulateItemResult::NeedsReturn)
        }
        libc::FUTEX_WAKE | libc::FUTEX_WAKE_BITSET => {
            let n = ecx.read_scalar(&args[3])?.to_u32()? as usize;
            let woken = ecx.machine.threads.futex_wake(addr.addr().bytes(), n);
            for tid in &woken {
                // 清掉唤醒者的超时改写钩子（结果保持推测写入的 0）
                ecx.machine.threads.get_mut(*tid).unwrap().futex_wake = None;
            }
            ecx.write_scalar(
                rustc_middle::mir::interpret::Scalar::from_int(woken.len() as i128, dest.layout.size),
                dest,
            )?;
            interp_ok(EmulateItemResult::NeedsReturn)
        }
        _ => throw_unsup_format!("futex op {op} 未实现"),
    }
}

/// 给指定线程写 errno（futex 超时等跨线程结果写回用）。
pub(crate) fn set_thread_errno<'tcx>(
    ecx: &mut MirvmInterpCx<'tcx>,
    tid: super::threads::ThreadId,
    v: i32,
) -> InterpResult<'tcx> {
    let cell = match ecx.machine.threads.get(tid).and_then(|t| t.errno_cell) {
        Some(p) => p,
        None => {
            let p = ecx.allocate_ptr(
                Size::from_bytes(4),
                rustc_abi::Align::from_bytes(4).unwrap(),
                MemoryKind::Machine(MirvmMemoryKind::Machine),
                AllocInit::Zero,
            )?;
            ecx.machine.threads.get_mut(tid).unwrap().errno_cell = Some(p);
            p
        }
    };
    let i32_layout = ecx.layout_of(ecx.tcx.types.i32)?;
    let place = ecx.ptr_to_mplace(cell.into(), i32_layout);
    ecx.write_scalar(rustc_middle::mir::interpret::Scalar::from_i32(v), &place)
}

/// 把 CLOCK_MONOTONIC 的绝对 timespec 换算成宿主 Instant。
fn monotonic_to_instant(sec: i64, nsec: i64) -> std::time::Instant {
    let mut now_ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now_ts) };
    let now = std::time::Duration::new(now_ts.tv_sec as u64, now_ts.tv_nsec as u32);
    let target = std::time::Duration::new(sec.max(0) as u64, nsec.max(0) as u32);
    let host_now = std::time::Instant::now();
    if target > now { host_now + (target - now) } else { host_now }
}
