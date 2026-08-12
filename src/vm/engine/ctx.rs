//! 执行环境：Shared（发布后只读）+ Ctx（每线程执行态，vmctx）。
//!
//! 状态三分（concurrency-arch §2）在真引擎的落地；raw-ptr ctx + 字段级瞬态借用
//! 纪律沿用 spike2/3/4。Shared 由 Engine 以 Arc 持有；每线程 Ctx 也持一份 Arc，
//! 因而执行态结束前模块不会被释放。Ctx 落 **自管 pthread key**——
//! **边界 TLS attach**（vmctx-passing §1，JNI 同款）是所有入口
//! （run_main / run_export / thunk）进入引擎的唯一门。
//!
//! 为什么不用宿主 `thread_local!`：guest 的 TLS dtor（std run_dtors 经 pthread_key
//! 注册，thunk 化）跑在 pthread TSD 相位，而宿主 C++ TLS 析构**先于** TSD 相位
//! （glibc start_thread：__call_tls_dtors → __nptl_deallocate_tsd）——彼时 Ctx 已亡，
//! dtor thunk 内 attach 撞已销毁宿主 TLS。自管 pthread key + **迟退 N 轮**（dtor 里
//! 重新 setspecific 挂回，glibc 上限 4 轮）让 Ctx 存活到 guest key dtor 之后。

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, Weak};

use super::ffi::FfiState;
use super::frame::ByteRegion;
use super::ir::Module;

/// 发布后只读：加载相建好、执行相 lock-free 共享读（引擎 Sync 的根基，spike4）。
/// 例外 = thunks（M4.4 D1）：执行期按需物化的 thunk 缓存——Mutex 显式同步，
/// 状态三分（concurrency-arch §2）的第三格，合法。
pub struct Shared {
    pub id: u64,
    pub module: Module,
    pub thunks: super::thunks::ThunkCache,
    /// J1 分层基座（M5.3a）：PLT 槽 + 计数，按合并后 FuncId 空间建。
    /// 槽的写入者是 M5.3b 编译线程（单原子交换发布），此外发布后只读纪律不变。
    pub jit: super::jit::JitState,
    fork_baseline_threads: std::sync::atomic::AtomicUsize,
}

impl Shared {
    pub fn new(mut module: Module) -> Self {
        static NEXT_ENGINE_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        module.ensure_function_names();
        super::backtrace::materialize_symbols(&mut module)
            .unwrap_or_else(|error| panic!("客体 backtrace 符号映像建立失败: {error}"));
        let jit = super::jit::JitState::new(module.funcs.len());
        Shared {
            id: NEXT_ENGINE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            module,
            thunks: super::thunks::ThunkCache::default(),
            jit,
            fork_baseline_threads: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

static ENGINES: std::sync::LazyLock<std::sync::RwLock<HashMap<u64, Weak<Shared>>>> =
    std::sync::LazyLock::new(|| std::sync::RwLock::new(HashMap::new()));

pub fn engine(id: u64) -> Option<Arc<Shared>> {
    ENGINES.read().unwrap().get(&id).and_then(Weak::upgrade)
}

pub struct Engine {
    shared: Arc<Shared>,
}

impl Engine {
    pub fn new(shared: Shared) -> Self {
        let shared = Arc::new(shared);
        ENGINES
            .write()
            .unwrap()
            .insert(shared.id, Arc::downgrade(&shared));
        #[cfg(feature = "cranelift")]
        super::jit::start(&shared);
        Self { shared }
    }

    pub fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        #[cfg(feature = "cranelift")]
        super::jit::stop(&self.shared);
        super::interp::discard_engine_state(self.shared.id);
        ENGINES.write().unwrap().remove(&self.shared.id);
    }
}

/// 每线程执行态（vmctx）。M4.4 起每 guest 线程一份，生命周期 = 宿主 thread_local。
pub struct Ctx {
    pub shared: *const Shared,
    shared_owner: Arc<Shared>,
    pub region: ByteRegion,
    /// 解释帧递归深度（诊断计数；溢出判定改用 stack_floor 真栈守卫，M5.2 D8a）
    pub depth: u32,
    /// 引擎自身失败正在退到执行边界。这个路径不执行 guest cleanup；此前的
    /// 进程退出语义同样不会执行它们。
    pub engine_faulting: bool,
    /// 宿主执行栈安全下界（M5.2 D8a）：本线程栈低端 + 安全边距。interp_frame 的
    /// 栈指针近似值低于此 = guest 栈溢出（诊断退出而非宿主 SIGSEGV）。真栈字节
    /// 守卫替代旧的固定帧数上限（8000）：随线程真实栈自适应（主执行线程 1 GiB、
    /// guest 线程放大后的栈、外来 native 线程 thunk 再入均正确）。0 = 探测失败，
    /// 不守卫（与旧世界的裸奔等价，getattr_np 在 glibc 上对含主线程的所有线程可用）。
    pub stack_floor: usize,
    /// foreign 直通状态（dlsym 缓存 + dlopen 句柄；dlsym 幂等，每线程独立缓存无碍）
    pub ffi: FfiState,
    /// guest TLS 实例表（M4.4 D3）：TlsId → 本线程实例真地址（0 = 未物化，首访
    /// heap 分配 + 模板拷贝）。guest dtor 先由 pthread-key thunk 执行，Ctx 最后
    /// 一轮析构时再释放实例内存。
    pub tls: Vec<u64>,
    /// 活动解释帧。IP 供 guest unwinder 消费，CFA 是该解释调用在宿主栈上的位置；
    /// backtrace 用 CFA 把解释帧与系统展开器读出的 JIT 真机器帧恢复成一个调用序列。
    /// enter 时 push、FrameGuard::drop 时 pop（与 depth 同生命周期，unwind 安全）。
    pub shadow: Vec<ShadowFrame>,
}

#[derive(Clone, Copy)]
pub struct ShadowFrame {
    pub ip: u64,
    pub cfa: u64,
}

impl Ctx {
    pub fn new(shared: &Arc<Shared>) -> Self {
        Ctx {
            shared: Arc::as_ptr(shared),
            shared_owner: Arc::clone(shared),
            region: ByteRegion::new(),
            depth: 0,
            engine_faulting: false,
            stack_floor: thread_stack_floor(),
            ffi: FfiState::default(),
            tls: Vec::new(),
            shadow: Vec::new(),
        }
    }
}

impl Drop for Ctx {
    fn drop(&mut self) {
        for (id, addr) in self.tls.iter().copied().enumerate() {
            if addr == 0 {
                continue;
            }
            let slot = self.shared().module.tls[id];
            super::heap::dealloc(addr, slot.size.max(1), slot.align as u64);
        }
    }
}

impl Ctx {
    pub fn shared(&self) -> &Shared {
        &self.shared_owner
    }

    pub fn shared_arc(&self) -> Arc<Shared> {
        Arc::clone(&self.shared_owner)
    }
}

struct ThreadContexts {
    by_engine: HashMap<usize, Box<Ctx>>,
    current: *mut Ctx,
    teardown_rounds: u8,
}

impl ThreadContexts {
    fn new() -> Self {
        Self {
            by_engine: HashMap::new(),
            current: std::ptr::null_mut(),
            teardown_rounds: 0,
        }
    }

    fn attach(&mut self, shared: &Arc<Shared>) -> *mut Ctx {
        let key = shared.id as usize;
        let ctx = self
            .by_engine
            .entry(key)
            .or_insert_with(|| Box::new(Ctx::new(shared)));
        let ptr = &mut **ctx as *mut Ctx;
        self.current = ptr;
        ptr
    }
}

/// 本线程栈安全下界：os::thread 取 [lo, lo+size)，下界加安全边距。
/// 边距覆盖单次 interp_frame 的宿主最坏用量 + 最深处的 FFI/unwind/诊断路径；
/// 小栈取 1/8 防止边距吃光可用区。仅 Ctx 创建时调一次（getattr 对主线程读
/// /proc，非热路径）。
fn thread_stack_floor() -> usize {
    let Some((lo, size)) = crate::os::thread::current_stack_bounds() else {
        return 0;
    };
    if lo == 0 {
        return 0;
    }
    let margin = (size / 8).clamp(256 << 10, 4 << 20);
    lo + margin
}

/// Ctx 的 pthread key（进程唯一；dtor = ctx_key_dtor）。
static CTX_KEY: OnceLock<crate::os::thread::TlsKey> = OnceLock::new();

/// fork 守卫基线（M5.2 D8f）：guest main 启动时的 OS 线程数（`/proc/self/task`）。
/// 此刻 = mirvm 内部线程（main-in-join、guest-exec、分配器）+ 0 个 guest 派生线程。
/// **用真 OS 线程数而非 Ctx 计数**：pthread_create 返回后新线程即存在，但其 Ctx
/// 要到 trampoline attach 才建——Ctx 计数有 TOCTOU 窗口会漏计。fork 只在当前线程数
/// == 基线（guest 未派生任何线程）时放行。
/// guest main 启动点调用（run_main/run_export）：钉住单 guest 线程的基线。
pub fn set_fork_baseline(shared: &Shared) {
    shared.fork_baseline_threads.store(
        crate::os::thread::os_thread_count(),
        std::sync::atomic::Ordering::SeqCst,
    );
}

/// guest 是否已派生额外线程（HostFork 守卫）：当前 OS 线程数 > 基线 = 是。
/// 基线未设（0）或读取失败时保守判"多线程"（拒绝 fork）。
/// # Safety
///
/// `ctx` 必须是当前线程仍处于激活范围内的 `Ctx`。
pub unsafe fn guest_spawned_threads(ctx: *mut Ctx) -> bool {
    let base = unsafe { &*(*ctx).shared }
        .fork_baseline_threads
        .load(std::sync::atomic::Ordering::SeqCst);
    base == 0 || crate::os::thread::os_thread_count() > base
}

/// TSD 相位的 Ctx 收尾：迟退 3 轮（重新挂回 → glibc 追加轮次，上限 4）——guest 的
/// pthread-key dtor（std run_dtors thunk，键序不可控）总能在存活的 Ctx 上执行；
/// 末轮真正销毁（ByteRegion munmap 等）。
#[cfg(not(sanitize = "thread"))]
unsafe extern "C" fn ctx_key_dtor(p: *mut std::ffi::c_void) {
    let contexts = p as *mut ThreadContexts;
    unsafe {
        if (*contexts).teardown_rounds < 3 {
            (*contexts).teardown_rounds += 1;
            crate::os::thread::tls_set(*CTX_KEY.get().unwrap(), p);
            return;
        }
        drop(Box::from_raw(contexts));
    }
}

/// 边界 TLS attach：本线程首次进入引擎时创建 Ctx（新 guest 线程执行态的诞生点），
/// 之后幂等返回同一实例——再入（guest→native→thunk→guest）天然拿到同一 vmctx，
/// 操作数区按纪律化栈继续嵌套（spike2 形状）。
///
/// 返回裸指针（Box 钉地址，raw-ptr vmctx 在 native 栈间传递——借用纪律 §9）。
/// 主线程的 Ctx 随进程 exit 一并回收（glibc exit 不走 TSD 相位，与 native 同）。
pub fn attach(shared: &Arc<Shared>) -> *mut Ctx {
    let key = *CTX_KEY.get_or_init(|| {
        // TSan 配置：不注册 dtor——TSan 的线程态在 TSD 相位前已析构，插桩代码
        // 不可在彼时运行（Ctx 每线程泄漏，仅测试配置；dtor 链由 threads_panic
        // 差分在真配置验证）。
        #[cfg(sanitize = "thread")]
        let dtor: Option<unsafe extern "C" fn(*mut std::ffi::c_void)> = None;
        #[cfg(not(sanitize = "thread"))]
        let dtor = Some(ctx_key_dtor as unsafe extern "C" fn(*mut std::ffi::c_void));
        crate::os::thread::tls_key_create(dtor)
    });
    unsafe {
        let p = crate::os::thread::tls_get(key);
        let contexts = if p.is_null() {
            let contexts = Box::into_raw(Box::new(ThreadContexts::new()));
            crate::os::thread::tls_set(key, contexts as *mut std::ffi::c_void);
            contexts
        } else {
            p as *mut ThreadContexts
        };
        (*contexts).attach(shared)
    }
}

pub fn current() -> *mut Ctx {
    let Some(key) = CTX_KEY.get().copied() else {
        panic!("JIT 助手在 Engine 激活前被调用");
    };
    let contexts = unsafe { crate::os::thread::tls_get(key) } as *mut ThreadContexts;
    assert!(!contexts.is_null(), "JIT 助手在线程 attach 前被调用");
    let ctx = unsafe { (*contexts).current };
    assert!(!ctx.is_null(), "JIT 助手在 Engine 激活范围外被调用");
    ctx
}

pub struct ActivationGuard {
    contexts: *mut ThreadContexts,
    previous: *mut Ctx,
    ctx: *mut Ctx,
}

impl ActivationGuard {
    pub fn ctx(&self) -> *mut Ctx {
        self.ctx
    }
}

impl Drop for ActivationGuard {
    fn drop(&mut self) {
        unsafe { (*self.contexts).current = self.previous };
    }
}

pub fn activate(shared: &Arc<Shared>) -> ActivationGuard {
    let key = *CTX_KEY.get_or_init(|| {
        #[cfg(sanitize = "thread")]
        let dtor: Option<unsafe extern "C" fn(*mut std::ffi::c_void)> = None;
        #[cfg(not(sanitize = "thread"))]
        let dtor = Some(ctx_key_dtor as unsafe extern "C" fn(*mut std::ffi::c_void));
        crate::os::thread::tls_key_create(dtor)
    });
    unsafe {
        let p = crate::os::thread::tls_get(key);
        let contexts = if p.is_null() {
            let contexts = Box::into_raw(Box::new(ThreadContexts::new()));
            crate::os::thread::tls_set(key, contexts as *mut std::ffi::c_void);
            contexts
        } else {
            p as *mut ThreadContexts
        };
        let previous = (*contexts).current;
        let ctx = (*contexts).attach(shared);
        ActivationGuard {
            contexts,
            previous,
            ctx,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Weak};

    use super::{Engine, Shared, attach};
    use crate::vm::engine::ir::Module;

    #[test]
    fn one_host_thread_keeps_distinct_contexts_for_distinct_engines() {
        let first = Arc::new(Shared::new(Module::default()));
        let second = Arc::new(Shared::new(Module::default()));

        let first_ctx = attach(&first);
        let second_ctx = attach(&second);

        assert_ne!(first_ctx, second_ctx);
        assert_eq!(unsafe { (*first_ctx).shared }, Arc::as_ptr(&first));
        assert_eq!(unsafe { (*second_ctx).shared }, Arc::as_ptr(&second));
    }

    #[test]
    fn nested_engine_activation_restores_the_outer_context() {
        let first = Arc::new(Shared::new(Module::default()));
        let second = Arc::new(Shared::new(Module::default()));
        let outer = super::activate(&first);
        assert_eq!(super::current(), outer.ctx());
        {
            let inner = super::activate(&second);
            assert_eq!(super::current(), inner.ctx());
        }
        assert_eq!(super::current(), outer.ctx());
    }

    #[test]
    fn engine_unregisters_and_releases_shared_state_on_drop() {
        let (id, weak): (u64, Weak<Shared>) = {
            let engine = Engine::new(Shared::new(Module::default()));
            let id = engine.shared().id;
            assert!(super::engine(id).is_some());
            super::super::interp::seed_engine_state_for_test(id);
            assert!(super::super::interp::has_engine_state_for_test(id));
            (id, Arc::downgrade(engine.shared()))
        };
        assert!(super::engine(id).is_none());
        assert!(!super::super::interp::has_engine_state_for_test(id));
        assert!(weak.upgrade().is_none());
    }
}
