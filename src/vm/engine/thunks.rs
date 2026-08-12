//! thunk 工厂（M4.4 D1，本期唯一新机制）：FFI 反方向的兑现。
//!
//! DESIGN §5：std 已把 pthread wrap 好（thread_start 是 std 的 extern "C" Rust fn），
//! 引擎唯一缺口 = 解释态函数指针逃逸给 native 时 materialize 成真机器码。libffi
//! Closure 按冻结签名造 trampoline；入口做**边界 TLS attach**（vmctx-passing §1，
//! JNI 同款）——新 guest 线程执行态（Ctx）的诞生点。
//!
//! 生命周期：thunk 进程级永生（Closure/ThunkData 均 leak）——guest 可长期持有码地址
//! （fn ptr 相等语义由 (条目地址, 签名) 缓存保证），exit 直通下无需回收。

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::Mutex;

use libffi::low::ffi_cif;
use libffi::middle::{Cif, Closure};

use super::ctx::Shared;
use super::ir::{FfiKind, ForeignSig, FuncId};

/// (fn 条目地址, 逃逸位签名) → thunk 真码地址。Mutex = 状态三分的"显式同步"格；
/// 创建是冷路径（每 (fn, 签名) 一次），锁全程持有，简单正确优先。
#[derive(Default)]
pub struct ThunkCache {
    map: Mutex<HashMap<(u64, ForeignSig), u64>>,
}

/// 每 thunk/条目 stub 的冻结数据（leak 进程级；跨线程共享——
/// Shared: Sync，其余为纯数据）。
struct ThunkData {
    engine_id: u64,
    func: FuncId,
    args: Box<[FfiKind]>,
    ret: FfiKind,
}

/// 按声明宽度搬实参（trampoline/entry_trampoline 共用；closure 实参槽只保证
/// 声明宽度有效；引擎值 = 宽度掩码位，LE）。
/// C1：聚合参数 = closure avalue 恒指向聚合字节（各档同形）→ 传**字节真地址**，
/// callee 侧 ParamAbi 展开由 interp::call_guest_ffi 按 FfiAgg 映射。
unsafe fn marshal_args(kinds: &[FfiKind], args: *const *const c_void) -> Vec<u64> {
    let mut av: Vec<u64> = Vec::with_capacity(kinds.len());
    for (i, k) in kinds.iter().enumerate() {
        let p = unsafe { *args.add(i) } as *const u8;
        let v = unsafe {
            match k {
                FfiKind::Agg(_) => p as u64,
                FfiKind::I8 | FfiKind::U8 => p.read() as u64,
                FfiKind::I16 | FfiKind::U16 => (p as *const u16).read_unaligned() as u64,
                FfiKind::I32 | FfiKind::U32 | FfiKind::F32 => {
                    (p as *const u32).read_unaligned() as u64
                }
                FfiKind::I64 | FfiKind::U64 | FfiKind::F64 | FfiKind::Ptr => {
                    (p as *const u64).read_unaligned()
                }
                FfiKind::Void => 0, // lower 已拒 ZST 回调参（不可达）
            }
        };
        av.push(v);
    }
    av
}

/// C1：按值聚合返回（ret = Agg，callee RetAbi **非** Indirect 的小档）重打包——
/// (lo,hi) 按 FfiAgg 声明序字段写回结构体字节（先整面清零保 padding，字段位再覆
/// 写；与 libffi rvalue 的 SysV 字节像逐位一致）。顶层嵌套叶与 Pair/Scalar 返回
/// 通道结构性互斥（同 rustc layout 推导——出现即引擎不变量破坏）。
unsafe fn repack_ret(result: *mut u8, agg: &super::ir::FfiAgg, lo: u64, hi: u64) {
    unsafe { std::ptr::write_bytes(result, 0, agg.size as usize) };
    for (i, f) in agg.fields.iter().enumerate() {
        let (v, leaf) = match i {
            0 => (lo, &f.leaf),
            1 => (hi, &f.leaf),
            _ => super::interp::engine_abort("C1 重打包：>2 顶层字段遇 Pair/Scalar 返回通道"),
        };
        let super::ir::FfiLeaf::Scalar(k) = leaf else {
            super::interp::engine_abort("C1 重打包：顶层嵌套叶遇 Pair/Scalar 返回通道");
        };
        let dst = unsafe { result.add(f.off as usize) };
        unsafe {
            match k {
                FfiKind::I8 | FfiKind::U8 => dst.write(v as u8),
                FfiKind::I16 | FfiKind::U16 => (dst as *mut u16).write_unaligned(v as u16),
                FfiKind::I32 | FfiKind::U32 | FfiKind::F32 => {
                    (dst as *mut u32).write_unaligned(v as u32)
                }
                FfiKind::I64 | FfiKind::U64 | FfiKind::F64 | FfiKind::Ptr => {
                    (dst as *mut u64).write_unaligned(v)
                }
                FfiKind::Void | FfiKind::Agg(_) => {
                    super::interp::engine_abort("C1 重打包：非法叶类")
                }
            }
        }
    }
}

/// trampoline 与 P1 条目 stub 共用的执行体：attach → 按签名搬实参 →
/// 解释 → 返回值写回。返回缓冲恒对齐（整数升位到 ffi_arg / F32 位在低
/// 32，LE）。
/// C1：ret = Agg 时分流——callee RetAbi::Indirect → result 经 call_guest_ffi 作
/// 隐藏首实参（sret 直传，callee memcpy 至该址）；其余 → (lo,hi) 后 repack_ret
/// 重打包为结构体字节。ABI 边界是否允许展开由外层 wrapper 决定，本体不复制
/// 两份语义。
unsafe fn trampoline_body(result: &mut u64, args: *const *const c_void, data: &ThunkData) {
    let Some(shared) = super::ctx::engine(data.engine_id) else {
        super::interp::engine_abort("thunk 所属 Engine 已结束");
    };
    let activation = super::ctx::activate(&shared);
    let ctx = activation.ctx();
    let av = unsafe { marshal_args(&data.args, args) };
    match &data.ret {
        FfiKind::Agg(agg) => {
            let (lo, hi) = super::interp::call_guest_ffi(
                ctx,
                data.func,
                &data.args,
                &av,
                Some(result as *mut u64 as u64),
            );
            if !matches!(
                super::interp::ret_abi_of(ctx, data.func),
                super::ir::RetAbi::Indirect { .. }
            ) {
                unsafe { repack_ret(result as *mut u64 as *mut u8, agg, lo, hi) };
            }
        }
        _ => {
            let (lo, _hi) = super::interp::call_guest_ffi(ctx, data.func, &data.args, &av, None);
            if data.ret != FfiKind::Void {
                *result = lo;
            }
        }
    }
}

/// 普通 `extern "C"` 边界：guest panic 或 foreign exception 不得穿出，
/// Rust 在该边界上保持 native 的 abort 语义。
unsafe extern "C" fn trampoline_c(
    _cif: &ffi_cif,
    result: &mut u64,
    args: *const *const c_void,
    data: &ThunkData,
) {
    unsafe { trampoline_body(result, args, data) }
}

/// `extern "C-unwind"` 边界：允许 guest panic 或 foreign exception 继续穿过
/// libffi closure，交给外层 Rust/C++ handler 处理。
unsafe extern "C-unwind" fn trampoline_c_unwind(
    _cif: &ffi_cif,
    result: &mut u64,
    args: *const *const c_void,
    data: &ThunkData,
) {
    unsafe { trampoline_body(result, args, data) }
}

type ThunkCallback = libffi::low::Callback<ThunkData, u64>;

/// libffi 5.x 在 Rust API 中把 closure callback 类型固定写成了
/// `extern "C"`，但 C 与 C-unwind 的机器调用约定相同；差别只在 Rust
/// 是否允许 unwinder 穿过该函数边界。libffi 只保存并从原生 closure
/// 蹦床间接调用这个地址，不会通过转换后的 Rust `extern "C"` 类型调用
/// 它。因此这里只擦除类型层差异，实际入口仍是 C-unwind wrapper。
fn callback_for(unwind: bool) -> ThunkCallback {
    if unwind {
        let callback: unsafe extern "C-unwind" fn(
            &ffi_cif,
            &mut u64,
            *const *const c_void,
            &ThunkData,
        ) = trampoline_c_unwind;
        // SAFETY: 两种 ABI 的机器签名一致；转换后的值只作为不透明
        // callback 地址交给 libffi，不经 Rust `extern "C"` 调用点执行。
        unsafe {
            std::mem::transmute::<
                unsafe extern "C-unwind" fn(&ffi_cif, &mut u64, *const *const c_void, &ThunkData),
                ThunkCallback,
            >(callback)
        }
    } else {
        trampoline_c
    }
}

/// 取或造：同一 (条目地址, 签名) 恒得同一真码地址（fn ptr 相等语义）。
pub fn get_or_create(shared: &Shared, entry: u64, func: FuncId, sig: &ForeignSig) -> u64 {
    let key = (entry, sig.clone());
    let mut map = shared.thunks.map.lock().unwrap();
    if let Some(&code) = map.get(&key) {
        return code;
    }
    let cif = Cif::new(
        sig.args.iter().map(super::ffi::ffi_type),
        super::ffi::ffi_type(&sig.ret),
    );
    let data: &'static ThunkData = Box::leak(Box::new(ThunkData {
        engine_id: shared.id,
        func,
        args: sig.args.clone().into(),
        ret: sig.ret.clone(),
    }));
    let closure = Closure::new(cif, callback_for(sig.unwind), data);
    let code = *closure.code_ptr() as usize as u64;
    std::mem::forget(closure); // 进程级永生（可执行页不回收——guest 持有码地址）
    map.insert(key, code);
    code
}

// ===== P1 条目可执行化（decision-history §7.6）=====

/// 按配方物化单域全部 stub（先填后封 RX）：sites 位序 = stub 偏移序（lower 按
/// 同序 addr_of 烤进字节码的值，alloc_stub 必须逐位复现）。closure 进程级 leak
/// （可执行页不回收——guest 持有码地址，exit 直通下无需回收）。
fn materialize_domain(
    engine_id: u64,
    sites: &[super::ir::EntryStubSite],
    arena: &mut super::codearena::StubArena,
) -> Result<(), String> {
    for site in sites {
        let cif = Cif::new(
            site.sig.args.iter().map(super::ffi::ffi_type),
            super::ffi::ffi_type(&site.sig.ret),
        );
        let data: &'static ThunkData = Box::leak(Box::new(ThunkData {
            engine_id,
            func: site.func,
            args: site.sig.args.clone().into(),
            ret: site.sig.ret.clone(),
        }));
        let closure = Closure::new(cif, callback_for(site.sig.unwind), data);
        let code = *closure.code_ptr() as usize as u64;
        std::mem::forget(closure);
        let addr = arena.alloc_stub();
        arena.write_stub(addr, code);
    }
    arena.seal();
    Ok(())
}

/// P1 启动相物化器（run_vm_engine 与 argv 终结化/GOT 重填并列的第三道全相
/// 工序）：本域 + absorb 挂载各 image/底座域，配方 → closure → stub 字节 →
/// 整域 RX。冷路径 arena 由 lower 带来（已映射）；warm/装载则在此按域 StrictMap
/// ——域被占 = Err（装载方按 cache miss 处理，绝不在其他基址上重放）。
pub fn materialize_all_entry_stubs(
    module: &mut super::ir::Module,
    engine_id: u64,
) -> Result<(), String> {
    if !module.entry_stub_sites.is_empty() && !module.entry_stubs.is_mapped() {
        let home = module
            .frozen
            .as_ref()
            .and_then(|f| super::addrlayout::code_home_for_frozen(f.home()))
            .ok_or("P1：本域代码基址不可推（冻结域非法）")?;
        module.entry_stubs = super::codearena::StubArena::map_fixed(home)?;
    }
    let own_sites = std::mem::take(&mut module.entry_stub_sites);
    materialize_domain(engine_id, &own_sites, &mut module.entry_stubs)?;
    module.entry_stub_sites = own_sites;
    let image_stubs = std::mem::take(&mut module.image_entry_stubs);
    for (home, sites, mut arena) in image_stubs {
        if !arena.is_mapped() {
            arena = super::codearena::StubArena::map_fixed(home)?;
        }
        materialize_domain(engine_id, &sites, &mut arena)?;
        module.image_entry_stubs.push((home, sites, arena));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct PanicProbe;

    unsafe extern "C-unwind" fn panic_probe(
        _cif: &ffi_cif,
        _result: &mut u64,
        _args: *const *const c_void,
        _data: &PanicProbe,
    ) {
        std::panic::panic_any(0x18u8);
    }

    #[test]
    fn callback_selector_keeps_distinct_abi_wrappers() {
        assert_eq!(callback_for(false) as *const (), trampoline_c as *const ());
        assert_eq!(
            callback_for(true) as *const (),
            trampoline_c_unwind as *const ()
        );
    }

    #[test]
    fn libffi_closure_preserves_c_unwind_callback() {
        let callback: unsafe extern "C-unwind" fn(
            &ffi_cif,
            &mut u64,
            *const *const c_void,
            &PanicProbe,
        ) = panic_probe;
        // SAFETY: 与 callback_for 相同：只向 libffi 传递地址，实际入口
        // 仍是 C-unwind；测试下方也以 C-unwind 类型调用 closure 代码。
        let callback: libffi::low::Callback<PanicProbe, u64> = unsafe {
            std::mem::transmute::<
                unsafe extern "C-unwind" fn(&ffi_cif, &mut u64, *const *const c_void, &PanicProbe),
                libffi::low::Callback<PanicProbe, u64>,
            >(callback)
        };
        let data = PanicProbe;
        let closure = Closure::new(
            Cif::new(
                std::iter::empty::<libffi::middle::Type>(),
                libffi::middle::Type::u64(),
            ),
            callback,
            &data,
        );
        let code: &unsafe extern "C-unwind" fn() -> u64 = unsafe { closure.instantiate_code_ptr() };

        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe { code() }));
        let payload = caught.expect_err("C-unwind callback 不应吞掉 panic");
        assert_eq!(payload.downcast_ref::<u8>(), Some(&0x18));
    }
}
