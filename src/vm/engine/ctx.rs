//! 执行环境：Shared（发布后只读）+ Ctx（每线程执行态，vmctx）。
//!
//! 状态三分（concurrency-arch §2）在真引擎的落地；raw-ptr ctx + 字段级瞬态借用
//! 纪律沿用 spike2/3/4。M4.4：Shared 提升 `&'static`（Box::leak，进程级），Ctx 落
//! 宿主 thread_local——**边界 TLS attach**（vmctx-passing §1，JNI 同款）是所有入口
//! （run_main / run_export / thunk）进入引擎的唯一门。

use std::cell::RefCell;

use super::ffi::FfiState;
use super::frame::ByteRegion;
use super::ir::Module;

/// 发布后只读：加载相建好、执行相 lock-free 共享读（引擎 Sync 的根基，spike4）。
pub struct Shared {
    pub module: Module,
}

impl Shared {
    pub fn new(module: Module) -> Self {
        Shared { module }
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
}

impl Ctx {
    pub fn new(shared: &Shared) -> Self {
        Ctx { shared, region: ByteRegion::new(), depth: 0, ffi: FfiState::default() }
    }
}

thread_local! {
    /// 本线程的执行态。Box 钉地址（raw-ptr vmctx 在 native 栈间传递）；
    /// 线程退出时 drop 自动清理（ByteRegion munmap 等）。
    static TLS_CTX: RefCell<Option<Box<Ctx>>> = const { RefCell::new(None) };
}

/// 边界 TLS attach：本线程首次进入引擎时创建 Ctx（新 guest 线程执行态的诞生点），
/// 之后幂等返回同一实例——再入（guest→native→thunk→guest）天然拿到同一 vmctx，
/// 操作数区按纪律化栈继续嵌套（spike2 形状）。
///
/// 返回裸指针：borrow 在本函数内结束，指针可跨 call_guest 传递（借用纪律 §9）。
pub fn attach(shared: &'static Shared) -> *mut Ctx {
    TLS_CTX.with(|c| {
        let mut slot = c.borrow_mut();
        let ctx = slot.get_or_insert_with(|| Box::new(Ctx::new(shared)));
        &mut **ctx as *mut Ctx
    })
}
