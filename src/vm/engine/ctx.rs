//! 执行环境：Shared（发布后只读）+ Ctx（每线程执行态，vmctx）。
//!
//! 状态三分（concurrency-arch §2）在真引擎的落地；raw-ptr ctx + 字段级瞬态借用
//! 纪律沿用 spike2/3/4（M4.2 的 CleanupGuard、M4.4 的多线程都依赖此形状）。

use super::frame::ByteRegion;
use super::ir::Module;

/// 发布后只读：加载相建好、执行相 lock-free 共享读。
pub struct Shared {
    pub module: Module,
}

/// 每线程执行态（vmctx）。M4.0 单线程；M4.4 起每 guest 线程一份。
pub struct Ctx {
    pub shared: *const Shared,
    pub region: ByteRegion,
}

impl Ctx {
    pub fn new(shared: &Shared) -> Self {
        Ctx { shared, region: ByteRegion::new() }
    }
}
