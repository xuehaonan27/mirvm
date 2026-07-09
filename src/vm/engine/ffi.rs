//! foreign 直通（os:: P7 处置①的通用道）：dlsym + libffi 按冻结签名直调。
//!
//! 真实地址模型的直接收益（账本 C2）：guest 指针即宿主指针，**零编组**——
//! 参数就是 u64 位，按 FfiKind 截取；native 写 guest 内存 = 写真内存，天然可见。
//! tier-0 native.rs 的无 provenance 简化版。
//!
//! 符号解析顺序：RTLD_DEFAULT(进程自带 libc/libm) → `-l` 指令 dlopen 的共享库。
//! 变参函数用 Cif::new_variadic(尾参类别由调用点实参冻结，x86_64 AL 语义 libffi 负责)。

use std::collections::HashMap;
use std::ffi::CString;

use libffi::middle::{Arg, Cif, CodePtr, Ret, Type as FfiType};

use super::ir::{FfiKind, ForeignSig};

/// 每线程 FFI 状态（dlsym 结果缓存 + dlopen 句柄；dlsym 幂等，M4.4 各线程独立缓存无碍）。
#[derive(Default)]
pub struct FfiState {
    syms: HashMap<Box<str>, usize>,
    handles: Vec<usize>,
    libs_loaded: bool,
}

impl FfiState {
    /// 解析符号真地址（缓存，含缺席缓存）。None = 全部搜索域都没有。
    fn resolve(&mut self, name: &str, libs: &[Box<str>]) -> Option<usize> {
        if let Some(&p) = self.syms.get(name) {
            return (p != 0).then_some(p);
        }
        self.ensure_libs(libs);
        let cname = CString::new(name).ok()?;
        let mut p = unsafe { libc::dlsym(std::ptr::null_mut(), cname.as_ptr()) } as usize;
        if p == 0 {
            for &h in &self.handles {
                p = unsafe { libc::dlsym(h as *mut libc::c_void, cname.as_ptr()) } as usize;
                if p != 0 {
                    break;
                }
            }
        }
        self.syms.insert(name.into(), p);
        (p != 0).then_some(p)
    }

    fn ensure_libs(&mut self, libs: &[Box<str>]) {
        if self.libs_loaded {
            return;
        }
        self.libs_loaded = true;
        for cand in libs {
            let Ok(cpath) = CString::new(&**cand) else { continue };
            let h = unsafe { libc::dlopen(cpath.as_ptr(), libc::RTLD_LAZY | libc::RTLD_GLOBAL) };
            if !h.is_null() {
                self.handles.push(h as usize);
            }
        }
    }
}

fn ffi_type(k: FfiKind) -> FfiType {
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
    }
}

/// 直调。args = 求值好的 u64 位（指针即真地址；F32 位在低 32）。返回 u64 位。
/// None = 符号不存在（调用方给诊断）。
pub fn call(
    state: &mut FfiState,
    libs: &[Box<str>],
    sym: &str,
    sig: &ForeignSig,
    args: &[u64],
) -> Option<u64> {
    let fnptr = state.resolve(sym, libs)?;

    let types: Vec<FfiType> = sig.args.iter().map(|&k| ffi_type(k)).collect();
    let cif = match sig.fixed {
        Some(nfixed) => Cif::new_variadic(types, nfixed, ffi_type(sig.ret)),
        None => Cif::new(types, ffi_type(sig.ret)),
    };

    // 每参一个 8 字节小端缓冲（libffi 按类型宽度读前缀）
    let bufs: Vec<[u8; 8]> = args.iter().map(|a| a.to_le_bytes()).collect();
    let ffi_args: Vec<Arg<'_>> = bufs.iter().map(|b| Arg::new(b)).collect();
    let mut ret = [0u8; 8];
    // SAFETY: 符号来自 dlsym；签名按 rustc fn sig layout 冻结；guest 缓冲即宿主缓冲。
    // fast 立场（C4）：native 调用的正确性由 guest 程序负责。
    unsafe {
        cif.call_return_into(CodePtr(fnptr as *mut _), &ffi_args, Ret::new(&mut ret[..]));
    }
    Some(u64::from_le_bytes(ret))
}
