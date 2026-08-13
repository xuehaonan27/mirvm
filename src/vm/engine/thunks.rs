//! thunk 工厂（M4.4 D1，本期唯一新机制）：FFI 反方向的兑现。
//!
//! DESIGN §5：std 已把 pthread wrap 好（thread_start 是 std 的 extern "C" Rust fn），
//! 引擎唯一缺口 = 解释态函数指针逃逸给 native 时 materialize 成真机器码。libffi
//! Closure 按冻结签名造 trampoline；入口做**边界 TLS attach**（vmctx-passing §1，
//! JNI 同款）——新 guest 线程执行态（Ctx）的诞生点。
//!
//! 生命周期：thunk 代码与小型 ThunkData 进程级永生，保证已逃逸地址始终可调用；
//! ThunkData 只持 EngineControl tombstone，不持 Module。Engine 关闭后 C-unwind thunk
//! 抛 EngineClosed，普通 C thunk按不可展开 ABI 终止，Module 仍可正常回收。

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::LazyLock;
use std::sync::{Arc, Mutex};

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
    control: Arc<super::ctx::EngineControl>,
    engine_id: u64,
    func: FuncId,
    args: Box<[FfiKind]>,
    ret: FfiKind,
    kind: ThunkKind,
}

static THUNK_DATA: LazyLock<Mutex<HashMap<u64, &'static ThunkData>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

enum ThunkKind {
    Ordinary,
    PthreadStart(Arc<super::deferred::PthreadStart>),
    TsdDestructor(Arc<super::deferred::TsdRegistration>),
    NativePthreadStart {
        registration: Arc<super::deferred::PthreadStart>,
        callback: usize,
    },
    NativeTsdDestructor {
        registration: Arc<super::deferred::TsdRegistration>,
        callback: usize,
    },
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
unsafe fn trampoline_body(
    result: &mut u64,
    args: *const *const c_void,
    data: &ThunkData,
    lease: super::ctx::ExecutionLease,
) {
    let shared = lease.shared();
    // A same-Engine native callback reuses its thread's Ctx, but it is still
    // a distinct Engine entry. Always assign a fresh activation nonce so the
    // callback cannot claim an outer run_main panic catcher.
    let activation = super::ctx::activate(shared);
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
    let mut tsd_callback = None;
    let lease = match &data.kind {
        ThunkKind::PthreadStart(start)
        | ThunkKind::NativePthreadStart {
            registration: start,
            ..
        } => start.enter(&data.control),
        ThunkKind::TsdDestructor(registration)
        | ThunkKind::NativeTsdDestructor { registration, .. } => {
            match registration.enter_callback() {
                Ok((lease, callback)) => {
                    tsd_callback = Some(callback);
                    Ok(lease)
                }
                // pthread_key_delete can race a destructor already fetched by
                // libc. The leaked closure is a stable tombstone after revoke;
                // an obsolete void destructor becomes a no-op instead of calling
                // through a closed Engine.
                Err(_) => return,
            }
        }
        ThunkKind::Ordinary => super::ctx::ExecutionLease::for_thunk(&data.control),
    };
    let lease = match lease {
        Ok(lease) => lease,
        Err(_) => {
            eprintln!("mirvm[m4-engine]: plain C thunk called after its Engine was closed");
            std::process::abort()
        }
    };
    match data.kind {
        ThunkKind::NativePthreadStart { callback, .. } => {
            let activation = super::ctx::activate(lease.shared());
            let argument = unsafe { *(*args).cast::<*mut c_void>() };
            let callback: extern "C" fn(*mut c_void) -> *mut c_void =
                unsafe { std::mem::transmute(callback) };
            *result = callback(argument) as u64;
            drop(activation);
            drop(lease);
            return;
        }
        ThunkKind::NativeTsdDestructor { callback, .. } => {
            let activation = super::ctx::activate(lease.shared());
            let argument = unsafe { *(*args).cast::<*mut c_void>() };
            let callback: unsafe extern "C" fn(*mut c_void) =
                unsafe { std::mem::transmute(callback) };
            unsafe { callback(argument) };
            drop(activation);
            drop(lease);
            drop(tsd_callback);
            return;
        }
        _ => {}
    }
    unsafe { trampoline_body(result, args, data, lease) };
    drop(tsd_callback);
}

/// `extern "C-unwind"` 边界：允许 guest panic 或 foreign exception 继续穿过
/// libffi closure，交给外层 Rust/C++ handler 处理。
unsafe extern "C-unwind" fn trampoline_c_unwind(
    _cif: &ffi_cif,
    result: &mut u64,
    args: *const *const c_void,
    data: &ThunkData,
) {
    let lease = match super::ctx::ExecutionLease::for_thunk(&data.control) {
        Ok(lease) => lease,
        Err(_) => super::unwind::raise_engine_closed(Arc::clone(&data.control)),
    };
    unsafe { trampoline_body(result, args, data, lease) }
}

type ThunkCallback = libffi::low::Callback<ThunkData, u64>;

struct PendingEntryClosure {
    closure: Option<Closure<'static>>,
    data: *mut ThunkData,
    code: u64,
}

impl Drop for PendingEntryClosure {
    fn drop(&mut self) {
        if let Some(closure) = self.closure.take() {
            drop(closure);
            unsafe { drop(Box::from_raw(self.data)) };
        }
    }
}

/// P1 closures are reclaimable until Engine publication. Once constructors
/// may run, their addresses can escape through arbitrary native code and are
/// converted into process-lifetime tombstones by `commit`.
pub(crate) struct EntryClosures {
    pending: Vec<PendingEntryClosure>,
}

impl EntryClosures {
    pub(crate) fn commit(mut self) {
        for entry in &mut self.pending {
            let closure = entry
                .closure
                .take()
                .expect("uncommitted P1 closure missing");
            let data = unsafe { &*entry.data };
            THUNK_DATA.lock().unwrap().insert(entry.code, data);
            std::mem::forget(closure);
        }
    }
}

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
pub(crate) fn get_or_create(shared: &Shared, entry: u64, func: FuncId, sig: &ForeignSig) -> u64 {
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
        control: Arc::clone(shared.control()),
        engine_id: shared.id,
        func,
        args: sig.args.clone().into(),
        ret: sig.ret.clone(),
        kind: ThunkKind::Ordinary,
    }));
    let closure = Closure::new(cif, callback_for(sig.unwind), data);
    let code = *closure.code_ptr() as usize as u64;
    std::mem::forget(closure); // 进程级永生（可执行页不回收——guest 持有码地址）
    THUNK_DATA.lock().unwrap().insert(code, data);
    map.insert(key, code);
    code
}

/// A signal handler address is either unrelated native code, a callable guest
/// function, or a MIRVM-owned address that must not fall through to the kernel.
/// The last case includes stale Engine tombstones and callbacks with another
/// ABI: treating either as native code would execute an invalid MIRVM entry in
/// a kernel signal frame.
pub(crate) enum SignalHandlerResolution {
    Unknown,
    Valid {
        control: Arc<super::ctx::EngineControl>,
        func: FuncId,
    },
    KnownInvalid,
}

fn has_signal_body_shape(shared: &Shared, func: FuncId) -> bool {
    let Some(body) = shared.module.funcs.get(func as usize) else {
        return false;
    };
    matches!(body.ret, super::ir::RetAbi::Zst)
        && matches!(body.params.as_slice(), [super::ir::ParamAbi::Scalar(_)])
        && body.caller_loc_off.is_none()
}

fn has_signal_thunk_shape(data: &ThunkData) -> bool {
    matches!(&data.kind, ThunkKind::Ordinary)
        && data.args.as_ref() == [FfiKind::I32]
        && data.ret == FfiKind::Void
}

/// Resolve both the current Engine's guest function addresses and escaped
/// process-lifetime thunk/P1 addresses. `Unknown` alone may be considered a
/// genuine native address by the signal layer.
pub(crate) fn resolve_signal_handler(shared: &Shared, code: u64) -> SignalHandlerResolution {
    if let Some(&func) = shared.module.fn_addrs.get(&code) {
        if !has_signal_body_shape(shared, func) {
            return SignalHandlerResolution::KnownInvalid;
        }

        // A materialized P1 entry is real executable code and therefore must
        // also match its preserved C ABI metadata. Hand-built Modules used by
        // focused tests have only a logical fn_addrs entry, so their strongest
        // available check is the FuncBody shape above.
        if shared.module.is_executable_entry(code) {
            let thunks = THUNK_DATA.lock().unwrap();
            let Some(data) = thunks.get(&code).copied() else {
                return SignalHandlerResolution::KnownInvalid;
            };
            if !Arc::ptr_eq(&data.control, shared.control())
                || data.func != func
                || !has_signal_thunk_shape(data)
                || !data.control.accepts_signal_install()
            {
                return SignalHandlerResolution::KnownInvalid;
            }
        }

        return SignalHandlerResolution::Valid {
            control: Arc::clone(shared.control()),
            func,
        };
    }

    let thunks = THUNK_DATA.lock().unwrap();
    let Some(data) = thunks.get(&code).copied() else {
        return SignalHandlerResolution::Unknown;
    };
    if !has_signal_thunk_shape(data) || !data.control.accepts_signal_install() {
        return SignalHandlerResolution::KnownInvalid;
    }
    SignalHandlerResolution::Valid {
        control: Arc::clone(&data.control),
        func: data.func,
    }
}

fn create_registered(shared: &Shared, func: FuncId, sig: &ForeignSig, kind: ThunkKind) -> u64 {
    create_registered_data(
        Arc::clone(shared.control()),
        shared.id,
        func,
        sig.args.clone().into(),
        sig.ret.clone(),
        sig.unwind,
        kind,
    )
}

fn create_registered_data(
    control: Arc<super::ctx::EngineControl>,
    engine_id: u64,
    func: FuncId,
    args: Box<[FfiKind]>,
    ret: FfiKind,
    unwind: bool,
    kind: ThunkKind,
) -> u64 {
    let cif = Cif::new(
        args.iter().map(super::ffi::ffi_type),
        super::ffi::ffi_type(&ret),
    );
    let data: &'static ThunkData = Box::leak(Box::new(ThunkData {
        control,
        engine_id,
        func,
        args,
        ret,
        kind,
    }));
    let closure = Closure::new(cif, callback_for(unwind), data);
    let code = *closure.code_ptr() as usize as u64;
    std::mem::forget(closure);
    THUNK_DATA.lock().unwrap().insert(code, data);
    code
}

pub(crate) fn wrap_pthread_start(
    owner: u64,
    code: u64,
) -> Option<(Arc<super::deferred::PthreadStart>, u64)> {
    let data = THUNK_DATA.lock().unwrap().get(&code).copied();
    let control = data
        .filter(|data| data.engine_id == owner)
        .map(|data| Arc::clone(&data.control))
        .or_else(|| super::ctx::control_for_engine(owner))?;
    let start = super::deferred::PthreadStart::new(&control).ok()?;
    let (func, kind) = match data {
        Some(data) if data.engine_id == owner && matches!(data.kind, ThunkKind::Ordinary) => {
            (data.func, ThunkKind::PthreadStart(Arc::clone(&start)))
        }
        _ => (
            0,
            ThunkKind::NativePthreadStart {
                registration: Arc::clone(&start),
                callback: code as usize,
            },
        ),
    };
    let proxy = create_registered_data(
        control,
        owner,
        func,
        vec![FfiKind::Ptr].into(),
        FfiKind::Ptr,
        false,
        kind,
    );
    Some((start, proxy))
}

pub(crate) fn wrap_tsd_destructor(
    owner: u64,
    code: u64,
) -> Option<(Arc<super::deferred::TsdRegistration>, u64)> {
    let data = THUNK_DATA.lock().unwrap().get(&code).copied();
    let control = data
        .filter(|data| data.engine_id == owner)
        .map(|data| Arc::clone(&data.control))
        .or_else(|| super::ctx::control_for_engine(owner))?;
    let registration = super::deferred::TsdRegistration::pending(&control).ok()?;
    let (func, kind) = match data {
        Some(data) if data.engine_id == owner && matches!(data.kind, ThunkKind::Ordinary) => (
            data.func,
            ThunkKind::TsdDestructor(Arc::clone(&registration)),
        ),
        _ => (
            0,
            ThunkKind::NativeTsdDestructor {
                registration: Arc::clone(&registration),
                callback: code as usize,
            },
        ),
    };
    let proxy = create_registered_data(
        control,
        owner,
        func,
        vec![FfiKind::Ptr].into(),
        FfiKind::Void,
        false,
        kind,
    );
    registration.set_code(proxy);
    Some((registration, proxy))
}

enum PendingCallback {
    PthreadStart(Arc<super::deferred::PthreadStart>),
    PthreadKey {
        registration: Arc<super::deferred::TsdRegistration>,
        key_out: u64,
    },
}

/// Per-foreign-call callback registrations. Recognized pthread APIs get a
/// unique closure and a concrete lifecycle contract. All other callback
/// arguments keep the ordinary cached tombstone behavior.
pub(crate) struct ForeignCallbacks {
    pending: Vec<PendingCallback>,
    operation: Option<super::deferred::TsdOperation>,
    completed: bool,
}

impl ForeignCallbacks {
    pub(crate) fn complete(mut self, result: u64) {
        let succeeded = result == 0;
        for pending in &self.pending {
            match pending {
                PendingCallback::PthreadStart(start) => {
                    if !succeeded {
                        start.cancel();
                    }
                }
                PendingCallback::PthreadKey {
                    registration,
                    key_out,
                } => {
                    if succeeded {
                        let key =
                            unsafe { (*key_out as *const libc::pthread_key_t).read_unaligned() };
                        registration.commit(key);
                    } else {
                        registration.cancel();
                    }
                }
            }
        }
        if let Some(operation) = self.operation.take() {
            operation.complete(result);
        }
        self.completed = true;
    }
}

impl Drop for ForeignCallbacks {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        for pending in &self.pending {
            match pending {
                PendingCallback::PthreadStart(start) => start.cancel(),
                PendingCallback::PthreadKey { registration, .. } => registration.cancel(),
            }
        }
    }
}

pub(crate) fn prepare_foreign_callbacks(
    shared: &Shared,
    sym: &str,
    sig: &ForeignSig,
    args: &mut [u64],
) -> ForeignCallbacks {
    let mut pending = Vec::new();
    for (pos, inner) in &sig.thunk_args {
        let value = args[*pos];
        if value == 0 {
            continue;
        }
        let Some(&func) = shared.module.fn_addrs.get(&value) else {
            continue;
        };
        match (sym, *pos) {
            ("pthread_create", 2) => {
                let start =
                    super::deferred::PthreadStart::new(shared.control()).unwrap_or_else(|_| {
                        super::interp::engine_abort(
                            "pthread_create callback registered while Engine was finalizing",
                        )
                    });
                args[*pos] = create_registered(
                    shared,
                    func,
                    inner,
                    ThunkKind::PthreadStart(Arc::clone(&start)),
                );
                pending.push(PendingCallback::PthreadStart(start));
            }
            ("pthread_key_create", 1) => {
                let registration = super::deferred::TsdRegistration::pending(shared.control())
                    .unwrap_or_else(|_| {
                        super::interp::engine_abort(
                            "pthread TSD callback registered while Engine was finalizing",
                        )
                    });
                let code = create_registered(
                    shared,
                    func,
                    inner,
                    ThunkKind::TsdDestructor(Arc::clone(&registration)),
                );
                registration.set_code(code);
                args[*pos] = code;
                pending.push(PendingCallback::PthreadKey {
                    registration,
                    key_out: args[0],
                });
            }
            _ if !shared.module.is_executable_entry(value) => {
                args[*pos] = get_or_create(shared, value, func, inner);
            }
            _ => {}
        }
    }
    ForeignCallbacks {
        pending,
        operation: super::deferred::prepare_pthread_operation(shared, sym, args),
        completed: false,
    }
}

// ===== P1 条目可执行化（decision-history §7.6）=====

/// 按配方为一个 Engine 物化独有 closure。site.link_addr 是 artifact 身份，
/// closure code 是本 Engine 的运行身份；closure 与 tombstone 进程级永生，旧地址
/// 永不复用，因此关闭后仍能稳定归属原 Engine。
fn materialize_domain(
    module: &mut super::ir::Module,
    control: &Arc<super::ctx::EngineControl>,
    sites: &[super::ir::EntryStubSite],
    closures: &mut EntryClosures,
) -> Result<(), String> {
    for site in sites {
        let cif = Cif::new(
            site.sig.args.iter().map(super::ffi::ffi_type),
            super::ffi::ffi_type(&site.sig.ret),
        );
        let data: &'static ThunkData = Box::leak(Box::new(ThunkData {
            control: Arc::clone(control),
            engine_id: control.id(),
            func: site.func,
            args: site.sig.args.clone().into(),
            ret: site.sig.ret.clone(),
            kind: ThunkKind::Ordinary,
        }));
        let closure = Closure::new(cif, callback_for(site.sig.unwind), data);
        let code = *closure.code_ptr() as usize as u64;
        closures.pending.push(PendingEntryClosure {
            closure: Some(closure),
            data: std::ptr::from_ref(data).cast_mut(),
            code,
        });
        module.load_map.add_exact(site.link_addr, code)?;
        module.executable_entry_addrs.insert(code);
    }
    Ok(())
}

/// P1 启动相物化器（run_vm_engine 与 argv 终结化/GOT 重填并列的第三道全相
/// 工序）：本域 + absorb 挂载各 image/底座域，配方 → 每 Engine closure →
/// LoadMap exact 映射。所有映射就绪后统一重建 fn_addrs 并应用冻结指针重定位。
pub(crate) fn materialize_all_entry_stubs(
    module: &mut super::ir::Module,
    control: &Arc<super::ctx::EngineControl>,
) -> Result<EntryClosures, String> {
    let mut closures = EntryClosures {
        pending: Vec::new(),
    };
    let own_sites = std::mem::take(&mut module.entry_stub_sites);
    materialize_domain(module, control, &own_sites, &mut closures)?;
    module.entry_stub_sites = own_sites;
    let image_stubs = std::mem::take(&mut module.image_entry_stubs);
    for (home, sites, arena) in image_stubs {
        materialize_domain(module, control, &sites, &mut closures)?;
        module.image_entry_stubs.push((home, sites, arena));
    }
    // The fixed arenas are link-time address allocators only. Runtime executes the direct
    // per-Engine closures above, so retaining those mappings would reintroduce conflicts.
    module.entry_stubs = Default::default();
    for (_, _, arena) in &mut module.image_entry_stubs {
        *arena = Default::default();
    }
    module.rebuild_fn_addrs();
    module.apply_frozen_relocs()?;
    Ok(closures)
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
