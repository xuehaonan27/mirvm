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
    let mut av: Vec<u64> = Vec::with_capacity(data.args.len());
    for (i, &k) in data.args.iter().enumerate() {
        let p = unsafe { *args.add(i) } as *const u8;
        // 按声明宽度读（closure 实参槽只保证声明宽度有效）；引擎值 = 宽度掩码位
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
    let (lo, _hi) = super::interp::interp_frame(ctx, data.func, &av);
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
