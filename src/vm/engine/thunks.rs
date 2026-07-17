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

/// 每 thunk 冻结数据（leak 进程级；跨线程共享——Shared: Sync，其余为纯数据）。
struct ThunkData {
    shared: &'static Shared,
    func: FuncId,
    args: Box<[FfiKind]>,
    ret: FfiKind,
}

/// 按声明宽度搬实参（trampoline/entry_trampoline 共用；closure 实参槽只保证
/// 声明宽度有效；引擎值 = 宽度掩码位，LE）。
unsafe fn marshal_args(kinds: &[FfiKind], args: *const *const c_void) -> Vec<u64> {
    let mut av: Vec<u64> = Vec::with_capacity(kinds.len());
    for (i, &k) in kinds.iter().enumerate() {
        let p = unsafe { *args.add(i) } as *const u8;
        let v = unsafe {
            match k {
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

/// trampoline（libffi Closure 回调，任意线程可入）：attach → 按签名搬实参 → 解释
/// → 返回值写回。返回缓冲恒 8 字节（整数升位到 ffi_arg / F32 位在低 32，LE）。
/// guest panic 穿出此边界 = extern "C" nounwind abort（与 native 一致，设计 §4 风险表）。
unsafe extern "C" fn trampoline(
    _cif: &ffi_cif,
    result: &mut u64,
    args: *const *const c_void,
    data: &ThunkData,
) {
    let ctx = super::ctx::attach(data.shared);
    let av = marshal_args(&data.args, args);
    let (lo, _hi) = super::interp::call_guest(ctx, data.func, &av);
    if data.ret != FfiKind::Void {
        *result = lo;
    }
}

/// 取或造：同一 (条目地址, 签名) 恒得同一真码地址（fn ptr 相等语义）。
pub fn get_or_create(shared: &'static Shared, entry: u64, func: FuncId, sig: &ForeignSig) -> u64 {
    let key = (entry, sig.clone());
    let mut map = shared.thunks.map.lock().unwrap();
    if let Some(&code) = map.get(&key) {
        return code;
    }
    let cif = Cif::new(
        sig.args.iter().map(|&k| super::ffi::ffi_type(k)),
        super::ffi::ffi_type(sig.ret),
    );
    let data: &'static ThunkData = Box::leak(Box::new(ThunkData {
        shared,
        func,
        args: sig.args.clone().into(),
        ret: sig.ret,
    }));
    let closure = Closure::new(cif, trampoline, data);
    let code = *closure.code_ptr() as usize as u64;
    std::mem::forget(closure); // 进程级永生（可执行页不回收——guest 持有码地址）
    map.insert(key, code);
    code
}

// ===== P1 条目可执行化（decision-history §7.6）=====

use std::sync::atomic::{AtomicUsize, Ordering};

/// 条目 stub 蹦床的引擎指针存放：trampoline 在任意 native 线程经它找回引擎
/// （interp::ATEXIT_SHARED 同款单发布点——cli run_vm_engine 在 Shared 提升后
/// 调 publish_shared；发布前被调 = 引擎不变量破坏，assert）。
static ENTRY_SHARED: AtomicUsize = AtomicUsize::new(0);

/// Shared 提升 &;'static 后调用（cold/warm 单一路径的同一发布点）。
pub fn publish_shared(shared: &'static Shared) {
    ENTRY_SHARED.store(shared as *const Shared as usize, Ordering::SeqCst);
}

/// 每条目 stub 的冻结数据（leak 进程级——guest 可长期持有码地址）。
struct EntryThunkData {
    func: FuncId,
    args: Box<[FfiKind]>,
    ret: FfiKind,
}

/// 条目 stub 的统一蹦床：attach → 搬参 → 解释 → 写回（trampoline 同款边界
/// 纪律；panic 穿出 = abort）。
unsafe extern "C" fn entry_trampoline(
    _cif: &ffi_cif,
    result: &mut u64,
    args: *const *const c_void,
    data: &EntryThunkData,
) {
    let shared = ENTRY_SHARED.load(Ordering::SeqCst) as *const Shared;
    assert!(!shared.is_null(), "条目 stub 在 Shared 发布前被调（引擎不变量）");
    let ctx = super::ctx::attach(unsafe { &*shared });
    let av = marshal_args(&data.args, args);
    let (lo, _hi) = super::interp::call_guest(ctx, data.func, &av);
    if data.ret != FfiKind::Void {
        *result = lo;
    }
}

/// 按配方物化单域全部 stub（先填后封 RX）：sites 位序 = stub 偏移序（lower 按
/// 同序 addr_of 烤进字节码的值，alloc_stub 必须逐位复现）。closure 进程级 leak
/// （可执行页不回收——guest 持有码地址，exit 直通下无需回收）。
fn materialize_domain(
    sites: &[super::ir::EntryStubSite],
    arena: &mut super::codearena::StubArena,
) -> Result<(), String> {
    for site in sites {
        let cif = Cif::new(
            site.sig.args.iter().map(|&k| super::ffi::ffi_type(k)),
            super::ffi::ffi_type(site.sig.ret),
        );
        let data: &'static EntryThunkData = Box::leak(Box::new(EntryThunkData {
            func: site.func,
            args: site.sig.args.clone().into(),
            ret: site.sig.ret,
        }));
        let closure = Closure::new(cif, entry_trampoline, data);
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
pub fn materialize_all_entry_stubs(module: &mut super::ir::Module) -> Result<(), String> {
    if !module.entry_stub_sites.is_empty() && !module.entry_stubs.is_mapped() {
        let home = module
            .frozen
            .as_ref()
            .and_then(|f| super::codearena::code_home_for_frozen(f.home()))
            .ok_or("P1：本域代码基址不可推（冻结域非法）")?;
        module.entry_stubs = super::codearena::StubArena::map_fixed(home)?;
    }
    let own_sites = std::mem::take(&mut module.entry_stub_sites);
    materialize_domain(&own_sites, &mut module.entry_stubs)?;
    module.entry_stub_sites = own_sites;
    let image_stubs = std::mem::take(&mut module.image_entry_stubs);
    for (home, sites, mut arena) in image_stubs {
        if !arena.is_mapped() {
            arena = super::codearena::StubArena::map_fixed(home)?;
        }
        materialize_domain(&sites, &mut arena)?;
        module.image_entry_stubs.push((home, sites, arena));
    }
    Ok(())
}
