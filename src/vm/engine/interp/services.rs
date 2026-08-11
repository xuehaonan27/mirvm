//! 运行期服务三族（自 interp.rs I10-I12 整搬）：signal_thunk（D8d async
//! 信号 AS-trampoline）/ backtrace 影子帧（D8e 合成 IP，诚实 <unknown>）/
//! atexit 家族（D8g，每 Engine 注册表 + LIFO 回调执行）。

use super::call::call_fn_addr;
use super::*;

pub(super) fn signal_thunk(ctx: *mut Ctx, signum: i32, handler: u64) -> usize {
    // 同步故障信号：guest handler 不可支持（诊断退出而非静默）
    if matches!(
        signum,
        crate::os::signal::SIGSEGV
            | crate::os::signal::SIGBUS
            | crate::os::signal::SIGFPE
            | crate::os::signal::SIGILL
            | crate::os::signal::SIGTRAP
    ) {
        engine_abort(&format!(
            "guest handler for synchronous fault signal {signum}（SEGV/BUS/FPE/ILL/TRAP：\
             宿主与 guest 故障不可分辨，D8l）"
        ));
    }
    let shared: &'static Shared = unsafe { &*(*ctx).shared };
    let Some(&func) = shared.module.fn_addrs.get(&handler) else {
        engine_abort(&format!(
            "signal handler {handler:#x} 不是已知 guest fn 条目"
        ));
    };
    // 信号 handler ABI = `extern "C" fn(c_int)`；thunk 工厂造真码入口 + 边界 attach。
    let sig = crate::vm::engine::ir::ForeignSig {
        args: vec![FfiKind::I32],
        ret: FfiKind::Void,
        fixed: None,
        thunk_args: vec![],
        unwind: false,
    };
    crate::vm::engine::thunks::get_or_create(shared, handler, func, &sig) as usize
}

// ===== backtrace 影子帧（D8e）=====
/// 合成 IP 基址：高位在用户地址空间之上、非页对齐 → 绝不与真实代码/数据地址撞，
/// dladdr 找不到（诚实 `<unknown>` 符号化，禁止伪造宿主符号）。
const FUNC_IP_BASE: u64 = 0x5f5f_0000_0000_0000;
pub(super) fn func_synth_ip(func: u32) -> u64 {
    FUNC_IP_BASE + (func as u64) * 64
}

/// `_Unwind_Backtrace(trace_fn, arg)`（D8e）：逐影子帧（栈顶→底）调 guest trace_fn
/// (synth_ctx, arg)；trace_fn 返 0（_URC_NO_REASON）续，非 0 停。synth_ctx 指向一个
/// 存 IP 的小缓冲，`_Unwind_GetIP(ctx)` 从中读。返回 _URC_END_OF_STACK(5)。
pub(super) fn unwind_backtrace(ctx: *mut Ctx, trace_fn: u64, arg: u64) -> u64 {
    // 快照影子帧（回调再入会 push/pop，不能借活栈迭代）。跳过栈顶自身
    //（_Unwind_Backtrace 的帧不该出现在回溯里，= native 语义）。
    let frames: Vec<u64> = {
        let s = unsafe { &(*ctx).shadow };
        s.iter().rev().skip(1).copied().collect()
    };
    for ip in frames {
        // synth _Unwind_Context = 单字缓冲存 IP（GetIP 读它）
        let cell: u64 = ip;
        let cell_ptr = &cell as *const u64 as u64;
        let r = call_fn_addr(ctx, trace_fn, &[cell_ptr, arg], "_Unwind_Backtrace").0;
        if r != 0 {
            break; // _URC_FOREIGN_EXCEPTION_CAUGHT / _URC_FAILURE 等 → 停
        }
    }
    5 // _URC_END_OF_STACK
}

// ===== atexit 家族（D8g）=====
// glibc 不导出 `atexit` 供 guest dlsym；引擎自持 LIFO 注册表 + 一个 native
// trampoline（经引擎自身链接的 libc `atexit` 挂载，非 dlsym）。进程收尾时 libc
// 在主线程调 trampoline，逐条 LIFO 解释执行 guest 回调（fresh Ctx attach）。
#[derive(Clone, Copy)]
pub(super) enum AtexitKind {
    Plain,  // atexit：fn()
    CxaArg, // __cxa_atexit：fn(arg)
    OnExit, // on_exit：fn(status=0, arg)
}
pub(super) struct AtexitEntry {
    func: u64,
    kind: AtexitKind,
    arg: u64,
}
static ATEXIT: std::sync::LazyLock<Mutex<std::collections::HashMap<usize, Vec<AtexitEntry>>>> =
    std::sync::LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

pub(super) fn discard_atexit_callbacks(engine_id: u64) {
    ATEXIT.lock().unwrap().remove(&(engine_id as usize));
}

#[cfg(test)]
pub(super) fn seed_atexit_callback(engine_id: u64) {
    ATEXIT
        .lock()
        .unwrap()
        .entry(engine_id as usize)
        .or_default()
        .push(AtexitEntry {
            func: 0,
            kind: AtexitKind::Plain,
            arg: 0,
        });
}

#[cfg(test)]
pub(super) fn has_atexit_callbacks(engine_id: u64) -> bool {
    ATEXIT.lock().unwrap().contains_key(&(engine_id as usize))
}

pub(super) fn atexit_register(ctx: *mut Ctx, func: u64, kind: AtexitKind, arg: u64) -> u64 {
    // fn 必须是已知 guest 条目（非 guest 回调不接——防静默）
    let shared = unsafe { &*(*ctx).shared };
    let module: &Module = &shared.module;
    if !module.fn_addrs.contains_key(&func) {
        engine_abort(&format!("atexit 回调 {func:#x} 不是已知 guest fn 条目"));
    }
    let mut reg = ATEXIT.lock().unwrap();
    reg.entry(shared.id as usize)
        .or_default()
        .push(AtexitEntry { func, kind, arg });
    0
}

/// 本 Engine 的虚拟进程收尾：LIFO 解释执行自己的 guest 回调。
pub(super) fn run_atexit_callbacks(ctx: *mut Ctx, status: i32) {
    let shared = unsafe { &*(*ctx).shared };
    let key = shared.id as usize;
    // LIFO：后注册先执行（C 语义）
    loop {
        let entry = {
            let mut reg = ATEXIT.lock().unwrap();
            let entry = reg.get_mut(&key).and_then(Vec::pop);
            if reg.get(&key).is_some_and(Vec::is_empty) {
                reg.remove(&key);
            }
            entry
        };
        let Some(entry) = entry else { break };
        let args: &[u64] = match entry.kind {
            AtexitKind::Plain => &[],
            AtexitKind::CxaArg => &[entry.arg],
            AtexitKind::OnExit => &[status as u64, entry.arg],
        };
        // guest 回调 panic 穿到 C 退出路径 = abort（与 native 一致）
        let _ = call_fn_addr(ctx, entry.func, args, "atexit");
    }
}
