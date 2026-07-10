//! 执行环境：Shared（发布后只读）+ Ctx（每线程执行态，vmctx）。
//!
//! 状态三分（concurrency-arch §2）在真引擎的落地；raw-ptr ctx + 字段级瞬态借用
//! 纪律沿用 spike2/3/4。M4.4：Shared 提升 `&'static`（Box::leak，进程级），Ctx 落
//! **自管 pthread key**——**边界 TLS attach**（vmctx-passing §1，JNI 同款）是所有入口
//! （run_main / run_export / thunk）进入引擎的唯一门。
//!
//! 为什么不用宿主 `thread_local!`：guest 的 TLS dtor（std run_dtors 经 pthread_key
//! 注册，thunk 化）跑在 pthread TSD 相位，而宿主 C++ TLS 析构**先于** TSD 相位
//! （glibc start_thread：__call_tls_dtors → __nptl_deallocate_tsd）——彼时 Ctx 已亡，
//! dtor thunk 内 attach 撞已销毁宿主 TLS。自管 pthread key + **迟退 N 轮**（dtor 里
//! 重新 setspecific 挂回，glibc 上限 4 轮）让 Ctx 存活到 guest key dtor 之后。

use std::sync::OnceLock;

use super::ffi::FfiState;
use super::frame::ByteRegion;
use super::ir::Module;

/// 发布后只读：加载相建好、执行相 lock-free 共享读（引擎 Sync 的根基，spike4）。
/// 例外 = thunks（M4.4 D1）：执行期按需物化的 thunk 缓存——Mutex 显式同步，
/// 状态三分（concurrency-arch §2）的第三格，合法。
pub struct Shared {
    pub module: Module,
    pub thunks: super::thunks::ThunkCache,
}

impl Shared {
    pub fn new(module: Module) -> Self {
        Shared { module, thunks: super::thunks::ThunkCache::default() }
    }
}

/// 每线程执行态（vmctx）。M4.4 起每 guest 线程一份，生命周期 = 宿主 thread_local。
pub struct Ctx {
    pub shared: *const Shared,
    pub region: ByteRegion,
    /// 解释帧递归深度（guest 栈溢出防护，frame-abi §9——到界诊断退出）
    pub depth: u32,
    /// foreign 直通状态（dlsym 缓存 + dlopen 句柄；dlsym 幂等，每线程独立缓存无碍）
    pub ffi: FfiState,
    /// guest TLS 实例表（M4.4 D3）：TlsId → 本线程实例真地址（0 = 未物化，首访
    /// heap 分配 + 模板拷贝）。实例本体泄漏（Drop 副作用由 guest 的 run_dtors 走
    /// pthread-key dtor thunk 兑现——见模块注释）。
    pub tls: Vec<u64>,
    /// TSD 迟退轮计数（ctx_key_dtor 用，见模块注释）
    teardown_rounds: u8,
}

impl Ctx {
    pub fn new(shared: &Shared) -> Self {
        Ctx {
            shared,
            region: ByteRegion::new(),
            depth: 0,
            ffi: FfiState::default(),
            tls: Vec::new(),
            teardown_rounds: 0,
        }
    }
}

/// Ctx 的 pthread key（进程唯一；dtor = ctx_key_dtor）。
static CTX_KEY: OnceLock<libc::pthread_key_t> = OnceLock::new();

/// TSD 相位的 Ctx 收尾：迟退 3 轮（重新挂回 → glibc 追加轮次，上限 4）——guest 的
/// pthread-key dtor（std run_dtors thunk，键序不可控）总能在存活的 Ctx 上执行；
/// 末轮真正销毁（ByteRegion munmap 等）。
#[cfg(not(sanitize = "thread"))]
unsafe extern "C" fn ctx_key_dtor(p: *mut libc::c_void) {
    let ctx = p as *mut Ctx;
    unsafe {
        if (*ctx).teardown_rounds < 3 {
            (*ctx).teardown_rounds += 1;
            libc::pthread_setspecific(*CTX_KEY.get().unwrap(), p);
            return;
        }
        drop(Box::from_raw(ctx));
    }
}

/// 边界 TLS attach：本线程首次进入引擎时创建 Ctx（新 guest 线程执行态的诞生点），
/// 之后幂等返回同一实例——再入（guest→native→thunk→guest）天然拿到同一 vmctx，
/// 操作数区按纪律化栈继续嵌套（spike2 形状）。
///
/// 返回裸指针（Box 钉地址，raw-ptr vmctx 在 native 栈间传递——借用纪律 §9）。
/// 主线程的 Ctx 随进程 exit 一并回收（glibc exit 不走 TSD 相位，与 native 同）。
pub fn attach(shared: &'static Shared) -> *mut Ctx {
    let key = *CTX_KEY.get_or_init(|| unsafe {
        // TSan 配置：不注册 dtor——TSan 的线程态在 TSD 相位前已析构，插桩代码
        // 不可在彼时运行（Ctx 每线程泄漏，仅测试配置；dtor 链由 threads_panic
        // 差分在真配置验证）。
        #[cfg(sanitize = "thread")]
        let dtor: Option<unsafe extern "C" fn(*mut libc::c_void)> = None;
        #[cfg(not(sanitize = "thread"))]
        let dtor = Some(ctx_key_dtor as unsafe extern "C" fn(*mut libc::c_void));
        let mut k: libc::pthread_key_t = 0;
        let rc = libc::pthread_key_create(&mut k, dtor);
        assert_eq!(rc, 0, "pthread_key_create 失败: {rc}");
        k
    });
    unsafe {
        let p = libc::pthread_getspecific(key);
        if !p.is_null() {
            return p as *mut Ctx;
        }
        let ctx = Box::into_raw(Box::new(Ctx::new(shared)));
        libc::pthread_setspecific(key, ctx as *mut libc::c_void);
        ctx
    }
}
