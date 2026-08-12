//! 运行期服务三族（自 interp.rs I10-I12 整搬）：signal_thunk（D8d async
//! 信号 AS-trampoline）/ backtrace 混合帧与符号 IP（D8e/E3/E8）/
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
/// ELF 符号镜像不可用时的保守后备 IP：高位在用户地址空间之上、非页对齐，
/// 不与真实代码/数据地址撞；正常 Engine 装载会使用可符号化的 ELF 地址。
const FUNC_IP_BASE: u64 = 0x5f5f_0000_0000_0000;
fn fallback_func_ip(func: u32) -> u64 {
    FUNC_IP_BASE + (func as u64) * 64
}

pub(super) fn func_synth_ip(ctx: *mut Ctx, func: u32) -> u64 {
    unsafe { &*(*ctx).shared }
        .module
        .backtrace_ips
        .get(func as usize)
        .copied()
        .unwrap_or_else(|| fallback_func_ip(func))
}

#[repr(C)]
struct GuestUnwindContext {
    ip: u64,
    cfa: u64,
}

#[repr(C)]
struct HostFrame {
    ip: u64,
    cfa: u64,
}

unsafe extern "C" {
    #[link_name = "_Unwind_Backtrace"]
    fn host_unwind_backtrace(
        trace: extern "C" fn(*mut libc::c_void, *mut libc::c_void) -> i32,
        arg: *mut libc::c_void,
    ) -> i32;
    #[link_name = "_Unwind_GetIP"]
    fn host_unwind_get_ip(ctx: *mut libc::c_void) -> usize;
    #[link_name = "_Unwind_GetCFA"]
    fn host_unwind_get_cfa(ctx: *mut libc::c_void) -> usize;
}

extern "C" fn collect_host_frame(ctx: *mut libc::c_void, arg: *mut libc::c_void) -> i32 {
    let frames = unsafe { &mut *(arg as *mut Vec<HostFrame>) };
    frames.push(HostFrame {
        ip: unsafe { host_unwind_get_ip(ctx) as u64 },
        cfa: unsafe { host_unwind_get_cfa(ctx) as u64 },
    });
    0
}

/// `_Unwind_Backtrace(trace_fn, arg)`：系统展开器读取活动 JIT 真机器帧，再按宿主栈
/// 位置与解释影子帧合并。回调仍只拿到受控的 guest context，不会看到引擎宿主帧。
pub(super) fn unwind_backtrace(ctx: *mut Ctx, trace_fn: u64, arg: u64) -> u64 {
    let shared = unsafe { &*(*ctx).shared };
    let mut host: Vec<HostFrame> = Vec::new();
    unsafe {
        host_unwind_backtrace(
            collect_host_frame,
            &mut host as *mut Vec<HostFrame> as *mut libc::c_void,
        );
    }

    // 回调可能再入 guest 并改变活栈，所以先完整快照。x86_64 栈向低地址增长：
    // CFA 小者在内层；同一 JIT 客体调用只登记 fast 本体，包装帧不会重复出现。
    let mut frames: Vec<GuestUnwindContext> = unsafe {
        (*ctx)
            .shadow
            .iter()
            .map(|frame| GuestUnwindContext {
                ip: frame.ip,
                cfa: frame.cfa,
            })
            .collect()
    };
    frames.extend(host.into_iter().filter_map(|frame| {
        shared
            .jit
            .guest_func_at(frame.ip.saturating_sub(1))
            .map(|func| GuestUnwindContext {
                ip: func_synth_ip(ctx, func),
                cfa: frame.cfa,
            })
    }));
    frames.sort_unstable_by_key(|frame| frame.cfa);

    // 当前正在调用 _Unwind_Backtrace 的 guest 帧由标准实现自身裁掉；我们的合成
    // symbol address 无法与其入口指针直接比较，因此在这里等价跳过最内层客体帧。
    for frame in frames.into_iter().skip(1) {
        let frame_ptr = &frame as *const GuestUnwindContext as u64;
        let r = call_fn_addr(ctx, trace_fn, &[frame_ptr, arg], "_Unwind_Backtrace").0;
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
