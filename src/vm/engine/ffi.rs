//! foreign 直通（os:: P7 处置①的通用道）：dlsym + libffi 按冻结签名直调。
//!
//! 真实地址模型的直接收益（账本 C2）：guest 指针即宿主指针，**零编组**——
//! 参数就是 u64 位，按 FfiKind 截取；native 写 guest 内存 = 写真内存，天然可见。
//! tier-0 native.rs 的无 provenance 简化版。
//!
//! 库加载纪律：物化 archive 是必需库（RTLD_NOW，失败携 dlerror 终止）；普通 `-l`
//! 名称是可选候选（best-effort）。解析序：归档 hidden 符号 .symtab 兜底表（链接
//! 期绑定，恒胜全局）→ RTLD_DEFAULT → 各句柄（见 archive_fallbacks 字段注）。
//! 变参函数用 Cif::new_variadic(尾参类别由调用点实参冻结，x86_64 AL 语义 libffi 负责)。

use std::collections::HashMap;
use std::ffi::CString;

use libffi::middle::{Arg, Cif, CodePtr, Ret, Type as FfiType};

use super::ir::{FfiKind, ForeignSig};

/// 每线程 FFI 状态（dlsym 结果缓存 + dlopen 句柄；dlsym 幂等，M4.4 各线程独立缓存无碍）。
#[derive(Default)]
pub struct FfiState {
    syms: HashMap<Box<str>, usize>,
    /// 必需归档库的 dlopen 句柄（required_native_libs 序 = 链接序同构）：
    /// 其 .dynsym 可见符号的解析 **先于 RTLD_DEFAULT**（native 链接期绑定——guest
    /// 自己链进来的对象恒胜宿主进程同名库；psm 的 rust_psm_on_stack vs 宿主
    /// librustc_driver 内嵌副本即此实锤，corpus c_polars_frame）。可选库句柄
    /// 另列，全域之后再查。
    required_handles: Vec<usize>,
    handles: Vec<usize>,
    /// 必需归档库的 hidden 符号兜底表（装载基址, 符号→st_value）：只收不进
    /// .dynsym 的符号（-fvisibility=hidden 归档，ring/zstd-sys 一族）。解析序
    /// = required_native_libs 序（与链接序同构），且**先于 dlsym 全域**——静态
    /// 归档成员链进 guest 后其定义恒胜全局命名空间（native 链接期绑定；宿主
    /// libLLVM 内嵌 ZSTD_* 一族同名库会静默截胡，corpus c_zstd_stream 实锤）。
    archive_fallbacks: Vec<(u64, HashMap<Box<str>, u64>)>,
    libs_loaded: bool,
}

impl FfiState {
    /// 解析符号真地址（缓存，含缺席缓存）。None = 全部搜索域都没有。
    pub(crate) fn resolve(
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
        // ①归档 hidden 符号兜底表（先于全域）：native 链接期绑定语义——归档内
        // 定义恒胜全局命名空间。dlsym 优先会把 guest 的 ZSTD_* 静默绑到宿主
        // libLLVM 内嵌库（同 ABI、不同策略行，输出合法但错误的字节）。
        let mut p = 0usize;
        for (bias, syms) in &self.archive_fallbacks {
            if let Some(&v) = syms.get(name) {
                p = (bias + v) as usize;
                break;
            }
        }
        // ①′MC 镜像（mode B 片③：包内自装载的自产 global_asm/dep_asm 族；
        // 与②同一语义位——guest 自产对象恒胜宿主同名库）
        if p == 0
            && let Some(addr) = super::mcload::resolve(name)
        {
            p = addr;
        }
        // ②必需归档句柄 dlsym（链接序）：归档 .dynsym 可见符号的 native 链接期
        // 绑定——guest 自己的对象恒胜宿主同名库；句柄解析与装载序无关，可复现
        // （①的 hidden 类同理；残余 = 归档【内部】跨引用碰撞符号仍走全局序，
        // 已知记档，corpus 无此形态）。
        if p == 0 {
            for &h in &self.required_handles {
                p = crate::os::dll::sym(h, &cname);
                if p != 0 {
                    break;
                }
            }
        }
        // ③dlsym 全域（真系统库；归档的 dynsym 可见符号也经 RTLD_GLOBAL 装载
        // 在此命中——但撞宿主同名库时 ②已先命中归档，无歧义）
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
    ) -> Result<(), String> {
        if self.libs_loaded {
            return Ok(());
        }

        for cand in required_libs {
            let cpath =
                CString::new(&**cand).map_err(|_| format!("必需原生库路径含 NUL: `{cand}`"))?;
            let h = crate::os::dll::open(&cpath, crate::os::dll::Mode::Now)
                .map_err(|detail| format!("dlopen 必需原生库 `{cand}` 失败: {detail}"))?;
            self.required_handles.push(h);
            // hidden 符号 .symtab 兜底表（口径见字段注）。基址或解析失败不建表：
            // dlsym 可见面不受影响，hidden 符号由 resolve 的既有诊断兜底——宁缺
            // 勿滥，错基址表会把符号静默解到野地址。
            if let Some(bias) = crate::os::dll::load_bias(h)
                && let Ok(syms) = crate::elfsym::hidden_symtab_values(cand)
            {
                self.archive_fallbacks.push((bias as u64, syms));
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

/// P2 启动相 GOT 重填（decision-history §7.5c）：以与运行期 foreign 调用同一
/// 解析序重解析全部 foreign 符号，逐修补点写 `resolved + addend`。冷/热单一
/// 路径——冷路径结果必与 lower 初填一致（幂等）；热路径（L2/image 回放）用它
/// 把上进程陈旧宿主地址换成本进程真值。非 weak 未命中 = Err（响亮：陈旧地址
/// 是 SIGSEGV 级静默错值源）；weak 未命中写 0（extern weak 缺席语义）。
pub(crate) fn resolve_got_fixups(module: &mut super::ir::Module) -> Result<(), String> {
    if module.got_fixups.is_empty() {
        return Ok(());
    }
    let mut ffi = FfiState::default();
    ffi.ensure_libs(&module.native_libs, &module.required_native_libs)?;
    let mut resolved: Vec<u64> = Vec::with_capacity(module.foreign_syms.len());
    for s in &module.foreign_syms {
        match (
            ffi.resolve(&s.name, &module.native_libs, &module.required_native_libs)?,
            s.weak,
        ) {
            (Some(p), _) => resolved.push(p as u64),
            (None, true) => resolved.push(0),
            (None, false) => {
                return Err(format!(
                    "foreign 符号 `{}` 启动相未命中（GOT 重填；归档兜底表 / dlsym 全域均无）",
                    s.name
                ));
            }
        }
    }
    for f in &module.got_fixups {
        // 修补点 addr 恒指冻结域内 8 字节格（lower 登记纪律）；冻结区映射终身 RW。
        unsafe { *(f.addr as *mut u64) = resolved[f.sym as usize].wrapping_add(f.addend) };
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

/// C1：冻结聚合 → libffi 结构类型（递归嵌套；size/align 由 libffi 依字段自洽计算）。
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

/// guest 线程栈放大（M5.2 D8a）：pthread_create 且显式 stacksize（std::thread 恒
/// 显式）时临时放大 attr——解释帧宿主成本数十倍于 native 帧，原尺寸会在远浅于
/// native 的 guest 深度打穿宿主栈。返回 Some((attr, 原尺寸)) 时调用方在 create 后
/// 还原（guest 可能复用 attr）。guest 自供栈（pthread_attr_setstack，addr 非空）
/// 不动；attr=NULL（glibc 默认）不动——该形态只出现在 native 代码自建线程，其
/// thunk 再入由 stack_floor 真栈守卫兜底。放大后尺寸是虚拟保留，按需提交。
pub fn amplify_pthread_stack(sym: &str, av: &[u64]) -> Option<(*mut std::ffi::c_void, usize)> {
    /// 解释帧 / native 帧的宿主成本比的保守上界（~2KB vs ~64B）
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
    // 未设 stacksize 的 attr（glibc 假地址形态判定在 os::thread）不动；
    // guest 自供栈（setstack，addr 落在用户地址界内）不动。
    if !crate::os::thread::stack_addr_is_unset(lo) {
        return None;
    }
    let want = size.saturating_mul(AMPLIFY).max(FLOOR);
    if want <= size || !crate::os::thread::attr_set_stack_size(attr, want) {
        return None;
    }
    Some((attr, size))
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
    ret_dst: Option<u64>,
) -> Result<Option<u64>, String> {
    let Some(fnptr) = state.resolve(sym, optional_libs, required_libs)? else {
        return Ok(None);
    };
    Ok(Some(call_addr(fnptr, sig, args, ret_dst)))
}

/// 按真码地址直调（CallForeign 的共用尾；也是 CallIndirect 反查未命中时的
/// native fn-ptr 通道——guest 运行期 dlsym 所得真码，M4.4 FFI 反方向之二）。
pub fn call_addr(fnptr: usize, sig: &ForeignSig, args: &[u64], ret_dst: Option<u64>) -> u64 {
    // F-07：实参/签名等长不变量——zip 静默截断曾吞掉变参真实尾参的类型
    if args.len() != sig.args.len() {
        eprintln!(
            "mirvm[m4-engine]: FFI 实参与签名不等长（实参 {} / 签名 {}；签名漂移或变参冻结缺口）",
            args.len(),
            sig.args.len()
        );
        std::process::exit(70);
    }
    let types: Vec<FfiType> = sig.args.iter().map(ffi_type).collect();
    let cif = match sig.fixed {
        Some(nfixed) => Cif::new_variadic(types, nfixed, ffi_type(&sig.ret)),
        None => Cif::new(types, ffi_type(&sig.ret)),
    };

    // 标量每参一个 8 字节小端缓冲（libffi 按类型宽度读前缀）；
    // C1 聚合参数 avalue 直指 eval 出的聚合字节真地址（零拷贝）。
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

    if let FfiKind::Agg(agg) = &sig.ret {
        // C1 按值聚合返回：结果缓冲按 8 对齐桶分配（align>8 已在 freeze 边界拒），
        // 调用后 memcpy size 字节至调用方目的地址（寄存器对档与 sret 档都由 libffi
        // 依结构类型内建解释 rtype——语义不自证）。
        let dst = ret_dst.expect("按值聚合返回的调用方目的地址（引擎不变量）");
        let mut rbuf: Vec<u64> = vec![0; (agg.size as usize).div_ceil(8)];
        unsafe {
            cif.call_return_into(CodePtr(fnptr as *mut _), &ffi_args, Ret::new(&mut rbuf[..]));
            std::ptr::copy_nonoverlapping(
                rbuf.as_ptr() as *const u8,
                dst as *mut u8,
                agg.size as usize,
            );
        }
        return 0;
    }
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

    /// 归档 hidden 符号先于 RTLD_DEFAULT（native 链接期绑定：归档定义恒胜全局
    /// 同名——宿主 libLLVM 内嵌 ZSTD_* 静默截胡 corpus c_zstd_stream 的修法）。
    /// 探针：hidden `malloc`（进程全域恒有 libc 本尊）必须解到归档内定义。
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
        // 前提：hidden malloc 不进 .dynsym，进程全域只有 libc 本尊
        let libc_malloc = crate::os::dll::sym(0, c"malloc");
        assert!(libc_malloc != 0);
        let mut state = FfiState::default();
        let required: Box<str> = so.display().to_string().into();
        let resolved = state
            .resolve("malloc", &[], std::slice::from_ref(&required))
            .expect("required lib loads")
            .expect("malloc resolves");
        assert_ne!(
            resolved, libc_malloc,
            "归档 hidden malloc 必须盖过 RTLD_DEFAULT 的 libc malloc"
        );
        let f: unsafe extern "C" fn(u64) -> *mut std::ffi::c_void =
            unsafe { std::mem::transmute(resolved) };
        assert_eq!(unsafe { f(0) }, 0x2a as *mut std::ffi::c_void);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
