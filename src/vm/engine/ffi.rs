//! foreign 直通（os:: P7 处置①的通用道）：dlsym + libffi 按冻结签名直调。
//!
//! 真实地址模型的直接收益（账本 C2）：guest 指针即宿主指针，**零编组**——
//! 参数就是 u64 位，按 FfiKind 截取；native 写 guest 内存 = 写真内存，天然可见。
//! tier-0 native.rs 的无 provenance 简化版。
//!
//! 库加载纪律：物化 archive 是必需库（RTLD_NOW，失败携 dlerror 终止）；普通 `-l`
//! 名称是可选候选（best-effort）。全部加载后按 RTLD_DEFAULT → 各句柄解析符号。
//! 变参函数用 Cif::new_variadic(尾参类别由调用点实参冻结，x86_64 AL 语义 libffi 负责)。

use std::collections::HashMap;
use std::ffi::{CStr, CString};

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
    fn resolve(
        &mut self,
        name: &str,
        optional_libs: &[Box<str>],
        required_libs: &[Box<str>],
    ) -> Result<Option<usize>, String> {
        if let Some(&p) = self.syms.get(name) {
            return Ok((p != 0).then_some(p));
        }
        self.ensure_libs(optional_libs, required_libs)?;
        let Ok(cname) = CString::new(name) else {
            return Ok(None);
        };
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
        Ok((p != 0).then_some(p))
    }

    fn ensure_libs(
        &mut self,
        optional_libs: &[Box<str>],
        required_libs: &[Box<str>],
    ) -> Result<(), String> {
        if self.libs_loaded {
            return Ok(());
        }

        for cand in required_libs {
            let cpath =
                CString::new(&**cand).map_err(|_| format!("必需原生库路径含 NUL: `{cand}`"))?;
            // dlerror 是线程局部的粘滞状态；先清空，再在失败后立即复制诊断。
            unsafe { libc::dlerror() };
            let h = unsafe { libc::dlopen(cpath.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
            if h.is_null() {
                let detail = dlerror_string();
                return Err(format!("dlopen 必需原生库 `{cand}` 失败: {detail}"));
            }
            self.handles.push(h as usize);
        }
        for cand in optional_libs {
            let Ok(cpath) = CString::new(&**cand) else {
                continue;
            };
            let h = unsafe { libc::dlopen(cpath.as_ptr(), libc::RTLD_LAZY | libc::RTLD_GLOBAL) };
            if !h.is_null() {
                self.handles.push(h as usize);
            }
        }
        self.libs_loaded = true;
        Ok(())
    }
}

fn dlerror_string() -> String {
    let error = unsafe { libc::dlerror() };
    if error.is_null() {
        "dlerror 未提供详情".into()
    } else {
        unsafe { CStr::from_ptr(error) }
            .to_string_lossy()
            .into_owned()
    }
}

pub(super) fn ffi_type(k: FfiKind) -> FfiType {
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

/// guest 线程栈放大（M5.2 D8a）：pthread_create 且显式 stacksize（std::thread 恒
/// 显式）时临时放大 attr——解释帧宿主成本数十倍于 native 帧，原尺寸会在远浅于
/// native 的 guest 深度打穿宿主栈。返回 Some((attr, 原尺寸)) 时调用方在 create 后
/// 还原（guest 可能复用 attr）。guest 自供栈（pthread_attr_setstack，addr 非空）
/// 不动；attr=NULL（glibc 默认）不动——该形态只出现在 native 代码自建线程，其
/// thunk 再入由 stack_floor 真栈守卫兜底。放大后尺寸是虚拟保留，按需提交。
pub fn amplify_pthread_stack(sym: &str, av: &[u64]) -> Option<(*mut libc::pthread_attr_t, usize)> {
    /// 解释帧 / native 帧的宿主成本比的保守上界（~2KB vs ~64B）
    const AMPLIFY: usize = 32;
    const FLOOR: usize = 64 << 20;
    if sym != "pthread_create" || av.len() < 4 {
        return None;
    }
    let attr = av[1] as *mut libc::pthread_attr_t;
    if attr.is_null() {
        return None;
    }
    unsafe {
        let mut lo: *mut libc::c_void = std::ptr::null_mut();
        let mut size: libc::size_t = 0;
        if libc::pthread_attr_getstack(attr, &mut lo, &mut size) != 0 || size == 0 {
            return None;
        }
        // glibc 细节：未 setstack 的 attr 内部 stackaddr=NULL，getstack 返回
        // `NULL - stacksize`（近 u64 顶的假地址）而非 NULL。x86_64 用户地址
        // ≤ 47 位——超界即"未设"；真用户栈地址（guest 自供栈）落在界内则不动。
        let stack_unset = lo.is_null() || lo as usize >= 1 << 48;
        if !stack_unset {
            return None;
        }
        let want = size.saturating_mul(AMPLIFY).max(FLOOR);
        if want <= size || libc::pthread_attr_setstacksize(attr, want) != 0 {
            return None;
        }
        Some((attr, size))
    }
}

/// 直调。args = 求值好的 u64 位（指针即真地址；F32 位在低 32）。返回 u64 位。
/// Ok(None) = 符号不存在（调用方给诊断）；Err = 必需库加载失败，禁止退化为 dlsym miss。
pub fn call(
    state: &mut FfiState,
    optional_libs: &[Box<str>],
    required_libs: &[Box<str>],
    sym: &str,
    sig: &ForeignSig,
    args: &[u64],
) -> Result<Option<u64>, String> {
    let Some(fnptr) = state.resolve(sym, optional_libs, required_libs)? else {
        return Ok(None);
    };
    Ok(Some(call_addr(fnptr, sig, args)))
}

/// 按真码地址直调（CallForeign 的共用尾；也是 CallIndirect 反查未命中时的
/// native fn-ptr 通道——guest 运行期 dlsym 所得真码，M4.4 FFI 反方向之二）。
pub fn call_addr(fnptr: usize, sig: &ForeignSig, args: &[u64]) -> u64 {
    let types: Vec<FfiType> = sig.args.iter().map(|&k| ffi_type(k)).collect();
    let cif = match sig.fixed {
        Some(nfixed) => Cif::new_variadic(types, nfixed, ffi_type(sig.ret)),
        None => Cif::new(types, ffi_type(sig.ret)),
    };

    // 每参一个 8 字节小端缓冲（libffi 按类型宽度读前缀）
    let bufs: Vec<[u8; 8]> = args.iter().map(|a| a.to_le_bytes()).collect();
    let ffi_args: Vec<Arg<'_>> = bufs.iter().map(Arg::new).collect();
    let mut ret = [0u8; 8];
    // SAFETY: 地址来自 dlsym / guest 持有的真码指针；签名按 rustc fn sig layout 冻结；
    // guest 缓冲即宿主缓冲。fast 立场（C4）：native 调用的正确性由 guest 程序负责。
    unsafe {
        cif.call_return_into(CodePtr(fnptr as *mut _), &ffi_args, Ret::new(&mut ret[..]));
    }
    u64::from_le_bytes(ret)
}

#[cfg(test)]
mod tests {
    use super::FfiState;

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
            .resolve("malloc", &[missing_library()], &[])
            .expect("optional dlopen failure must stay optional");
        assert!(address.is_some(), "malloc should resolve from RTLD_DEFAULT");
    }

    #[test]
    fn missing_required_library_fails_before_same_named_rtld_default_symbol() {
        let missing = missing_library();
        let mut state = FfiState::default();
        let error = state
            .resolve("malloc", &[], std::slice::from_ref(&missing))
            .unwrap_err();

        assert!(
            error.contains(&*missing),
            "required path missing from diagnostic: {error}"
        );
        assert!(
            error.contains("dlopen 必需原生库"),
            "unexpected diagnostic: {error}"
        );
        assert!(
            !error.contains("dlerror 未提供详情"),
            "dlerror detail was lost: {error}"
        );
    }
}
