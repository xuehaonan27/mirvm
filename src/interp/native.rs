//! libffi 原生 FFI（账本 C2 的直接收益：guest 指针即宿主指针，原样直传）。
//! 结构参考 rust-lang/miri shims/native_lib（MIT/Apache-2.0），大幅简化——
//! 无 ptrace 追踪、无地址翻译：真实地址内存让共享视图天然成立。
//!
//! 解析顺序（shims → 导出 Rust 符号之后的兜底）：
//! RTLD_DEFAULT（mirvm 进程自带 libc/libm 等）→ 按 `-l` 链接指令 dlopen 的共享库。
//!
//! v1 限制（记录于 DESIGN）：
//! - 仅标量/指针参数与返回（按值传结构体不支持）；不支持变参
//! - native 代码自己 malloc 的内存，guest 无法解引用（Miri 同款限制）
//! - 调用前对指针实参可达的分配做 process_native_write（标记已初始化、
//!   清相关 provenance——真实地址下 provenance 丢失退化为 wildcard，仍可解引用）

use std::ffi::CString;

use libffi::middle::{Arg, Cif, CodePtr, Ret, Type as FfiType};
use rustc_const_eval::interpret::{
    AllocId, AllocKind, InterpResult, OpTy, PlaceTy, Provenance as _, interp_ok,
};
use rustc_data_structures::fx::FxHashSet;
use rustc_middle::ty::layout::TyAndLayout;
use rustc_middle::{throw_unsup_format, ty};
use rustc_span::Symbol;

use super::machine::Prov;
use super::MirvmInterpCx;

/// 危险符号：绝不透传 native（会绕开我们的运行时/进程模型）。
const DENYLIST_PREFIX: &[&str] = &["pthread_", "exec", "setjmp", "longjmp", "__cxa_"];
const DENYLIST_EXACT: &[&str] =
    &["fork", "vfork", "clone", "exit", "_exit", "abort", "signal", "sigaction", "atexit", "raise"];

/// 尝试原生调用。返回 false = 找不到符号（调用方继续报不支持）。
pub fn call_native<'tcx>(
    ecx: &mut MirvmInterpCx<'tcx>,
    link_name: Symbol,
    c_variadic: bool,
    args: &[OpTy<'tcx, Prov>],
    dest: &PlaceTy<'tcx, Prov>,
) -> InterpResult<'tcx, bool> {
    let name = link_name.as_str();
    if DENYLIST_EXACT.contains(&name) || DENYLIST_PREFIX.iter().any(|p| name.starts_with(p)) {
        return interp_ok(false); // 走"不支持"错误路径，提示用户补 shim
    }

    let Some(fnptr) = find_symbol(ecx, name) else {
        return interp_ok(false);
    };
    if c_variadic {
        throw_unsup_format!("原生直调暂不支持变参函数 `{name}`");
    }

    // 参数编组：ffi 类型 + 值的字节表示（指针字节即真实宿主地址）
    let mut types = Vec::with_capacity(args.len());
    let mut bytes: Vec<Box<[u8]>> = Vec::with_capacity(args.len());
    for a in args {
        let (t, b) = op_to_ffi(ecx, a)?;
        types.push(t);
        bytes.push(b);
    }
    let ret_ty = match ty_to_ffitype(dest.layout) {
        Ok(t) => t,
        Err(t) => throw_unsup_format!("原生调用不支持的返回类型 {t}"),
    };

    // native 可能写我们的内存：对实参可达的分配按 native 写语义处理
    expose_reachable(ecx, args)?;

    // 固化返回位置
    let dest_m = ecx.force_allocation(dest)?;
    let ret_size = dest_m.layout.size.bytes() as usize;
    let mut retbuf = vec![0u8; ret_size];

    let cif = Cif::new(types, ret_ty);
    let ffi_args: Vec<Arg<'_>> = bytes.iter().map(|b| Arg::new(&**b)).collect();
    // SAFETY: 符号来自 dlsym；类型描述按 rustc FnAbi 派生；guest 缓冲即宿主缓冲。
    // fast 语义立场（账本 C4）：native 调用的正确性由 guest 程序负责。
    unsafe {
        cif.call_return_into(CodePtr(fnptr), &ffi_args, Ret::new(&mut retbuf[..]));
    }

    // 写回返回值 + errno 同步
    if ret_size > 0 {
        ecx.write_bytes_ptr(dest_m.ptr(), retbuf.iter().copied())?;
        // 返回字节的 provenance 未知：按 native 写语义处理该范围
        let (alloc_id, offset, _) = ecx.ptr_get_alloc_id(dest_m.ptr(), 0)?;
        let tcx = ecx.tcx;
        let (alloc, _) = ecx.get_alloc_raw_mut(alloc_id)?;
        alloc.process_native_write(
            &tcx,
            Some(rustc_middle::mir::interpret::alloc_range(offset, dest_m.layout.size)),
        );
    }
    super::shims::sync_errno_pub(ecx)?;
    interp_ok(true)
}

// ===== 符号解析 =====

fn find_symbol<'tcx>(ecx: &mut MirvmInterpCx<'tcx>, name: &str) -> Option<*mut libc::c_void> {
    ensure_libs_loaded(ecx);
    let cname = CString::new(name).ok()?;
    // 进程自身（libc/libm/...）
    let p = unsafe { libc::dlsym(std::ptr::null_mut(), cname.as_ptr()) };
    if !p.is_null() {
        return Some(p);
    }
    for &h in &ecx.machine.native_handles {
        let p = unsafe { libc::dlsym(h as *mut libc::c_void, cname.as_ptr()) };
        if !p.is_null() {
            return Some(p);
        }
    }
    None
}

/// 按会话的 `-l` 链接指令 dlopen 共享库（惰性一次）。
/// 静态库（.a）不存在共享版本时静默跳过——符号缺失会在调用处清晰报错。
fn ensure_libs_loaded<'tcx>(ecx: &mut MirvmInterpCx<'tcx>) {
    if ecx.machine.native_libs_loaded {
        return;
    }
    ecx.machine.native_libs_loaded = true;

    let sess = ecx.tcx.sess;
    let search_dirs: Vec<std::path::PathBuf> =
        sess.opts.search_paths.iter().map(|sp| sp.dir.clone()).collect();

    for lib in &sess.opts.libs {
        let name = lib.name.as_str();
        let mut candidates: Vec<String> = Vec::new();
        for d in &search_dirs {
            candidates.push(d.join(format!("lib{name}.so")).display().to_string());
        }
        candidates.push(format!("lib{name}.so"));
        candidates.push(format!("lib{name}.so.1"));
        for c in candidates {
            let Ok(cpath) = CString::new(c) else { continue };
            let h = unsafe { libc::dlopen(cpath.as_ptr(), libc::RTLD_LAZY | libc::RTLD_GLOBAL) };
            if !h.is_null() {
                ecx.machine.native_handles.push(h as usize);
                break;
            }
        }
    }
}

// ===== 类型映射与编组 =====

/// rustc layout → libffi 类型（标量与指针；聚合不支持）。
fn ty_to_ffitype<'tcx>(layout: TyAndLayout<'tcx>) -> Result<FfiType, ty::Ty<'tcx>> {
    use rustc_abi::{BackendRepr, Float, Integer, Primitive};
    if let BackendRepr::Scalar(s) = layout.backend_repr {
        return Ok(match s.primitive() {
            Primitive::Int(Integer::I8, true) => FfiType::i8(),
            Primitive::Int(Integer::I16, true) => FfiType::i16(),
            Primitive::Int(Integer::I32, true) => FfiType::i32(),
            Primitive::Int(Integer::I64, true) => FfiType::i64(),
            Primitive::Int(Integer::I8, false) => FfiType::u8(),
            Primitive::Int(Integer::I16, false) => FfiType::u16(),
            Primitive::Int(Integer::I32, false) => FfiType::u32(),
            Primitive::Int(Integer::I64, false) => FfiType::u64(),
            Primitive::Float(Float::F32) => FfiType::f32(),
            Primitive::Float(Float::F64) => FfiType::f64(),
            Primitive::Pointer(_) => FfiType::pointer(),
            _ => return Err(layout.ty),
        });
    }
    match layout.ty.kind() {
        ty::Tuple(l) if l.is_empty() => Ok(FfiType::void()),
        _ => Err(layout.ty),
    }
}

/// 参数 → (ffi 类型, 值的小端字节)。指针取绝对地址（= 宿主地址）。
fn op_to_ffi<'tcx>(
    ecx: &MirvmInterpCx<'tcx>,
    v: &OpTy<'tcx, Prov>,
) -> InterpResult<'tcx, (FfiType, Box<[u8]>)> {
    let t = match ty_to_ffitype(v.layout) {
        Ok(t) => t,
        Err(t) => throw_unsup_format!("原生调用不支持的参数类型 {t}"),
    };
    let imm = ecx.read_immediate(v)?;
    let scalar = imm.to_scalar();
    let size = v.layout.size;
    let bits: u128 = match scalar.try_to_scalar_int() {
        Ok(int) => int.to_bits(size),
        // 带 provenance 的指针：绝对地址就是宿主地址
        Err(_) => scalar.to_pointer(ecx)?.addr().bytes() as u128,
    };
    let bytes = bits.to_le_bytes()[..size.bytes() as usize].to_vec().into_boxed_slice();
    interp_ok((t, bytes))
}

// ===== 内存暴露：native 写语义 =====

/// 从指针实参出发，沿 provenance 做传递闭包：可变分配按 native 写处理
/// （标记全初始化 + 清 provenance——真实地址下退化为 wildcard 仍可用）。
fn expose_reachable<'tcx>(
    ecx: &mut MirvmInterpCx<'tcx>,
    args: &[OpTy<'tcx, Prov>],
) -> InterpResult<'tcx, ()> {
    let mut queue: Vec<AllocId> = Vec::new();
    for a in args {
        if let Ok(imm) = ecx.read_immediate(a).report_err()
            && let rustc_middle::mir::interpret::Scalar::Ptr(p, _) = imm.to_scalar()
            && let Some(id) = p.provenance.get_alloc_id()
        {
            queue.push(id);
        }
    }
    let mut seen = FxHashSet::default();
    while let Some(id) = queue.pop() {
        if !seen.insert(id) {
            continue;
        }
        let info = ecx.get_alloc_info(id);
        if !matches!(info.kind, AllocKind::LiveData) {
            continue;
        }
        // 先收集内部指针（只读），再做可变处理
        let inner: Vec<AllocId> = ecx
            .get_alloc_raw(id)?
            .provenance()
            .provenances()
            .filter_map(|p| p.get_alloc_id())
            .collect();
        queue.extend(inner);
        if info.mutbl.is_mut() {
            let tcx = ecx.tcx;
            let (alloc, _) = ecx.get_alloc_raw_mut(id)?;
            alloc.process_native_write(&tcx, None);
        }
    }
    interp_ok(())
}


