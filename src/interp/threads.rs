//! guest 线程运行时（DESIGN.md §5.3 C8 的分层落地）。
//!
//! ## 语义层（永久，两种执行策略共用）
//! - GuestThread：guest 线程的存在性——每线程调用栈、TLS、pthread key、errno、
//!   unwind payloads、join 簿记、名字
//! - ThreadManager 的意图 API：spawn / block / unblock / terminate / futex 等待队列
//! - pthread key 析构在线程退出时的运行轮次
//!
//! ## 策略层（本文件实现协作式 = 确定性模式；1:1 并行属 VM tier，见账本 C1/C8）
//! - 确定性 round-robin + 语句粒度时间片
//! - 全员阻塞时：按最早期限宿主真睡；无期限 = 死锁报错
//!
//! 结构参考 rust-lang/miri concurrency/thread.rs（MIT/Apache-2.0），大幅简化。

use std::time::Instant;

use rustc_const_eval::interpret::{Frame, ImmTy, MPlaceTy};
use rustc_data_structures::fx::FxHashMap;
use rustc_middle::mir::interpret::Scalar;
use rustc_middle::throw_machine_stop;
use rustc_span::def_id::DefId;

use super::helpers::InterpResult;
use super::machine::{FrameExtra, MPtr, Prov, Termination};

pub type ThreadId = usize;
pub const MAIN_THREAD: ThreadId = 0;

/// 语句粒度时间片：到点强制轮转（防自旋活锁；固定值保证确定性）。
pub const STEPS_PER_SLICE: u32 = 4096;

#[derive(Debug, Clone, PartialEq)]
pub enum ThreadState {
    Runnable,
    Blocked(BlockReason),
    Terminated,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BlockReason {
    /// 等待目标线程退出
    Join(ThreadId),
    /// futex 等待（地址 + 可选绝对期限）
    Futex { addr: u64, deadline: Option<Instant> },
    /// nanosleep
    Sleep { deadline: Instant },
}

/// futex 被唤醒/超时后要写回的 syscall 结果位置。
pub struct FutexWake<'tcx> {
    pub dest: MPlaceTy<'tcx, Prov>,
}

pub struct GuestThread<'tcx> {
    pub state: ThreadState,
    pub stack: Vec<Frame<'tcx, Prov, FrameExtra<'tcx>>>,
    /// panic payload 栈（catch_unwind 消费）
    pub unwind_payloads: Vec<ImmTy<'tcx, Prov>>,
    /// thread-local static → 本线程实例
    pub tls_statics: FxHashMap<DefId, MPtr>,
    /// pthread key → 本线程值
    pub pthread_tls: FxHashMap<u32, Scalar<Prov>>,
    /// errno 单元（惰性分配）
    pub errno_cell: Option<MPtr>,
    /// start_routine 返回值落点（pthread_join 传递用）
    pub ret_place: Option<MPlaceTy<'tcx, Prov>>,
    /// futex 阻塞的结果写回信息
    pub futex_wake: Option<FutexWake<'tcx>>,
    pub detached: bool,
    pub name: String,
}

impl<'tcx> GuestThread<'tcx> {
    fn new(name: String) -> Self {
        GuestThread {
            state: ThreadState::Runnable,
            stack: Vec::new(),
            unwind_payloads: Vec::new(),
            tls_statics: FxHashMap::default(),
            pthread_tls: FxHashMap::default(),
            errno_cell: None,
            ret_place: None,
            futex_wake: None,
            detached: false,
            name,
        }
    }
}

pub struct ThreadManager<'tcx> {
    threads: Vec<GuestThread<'tcx>>,
    active: ThreadId,
    /// futex 地址 → 等待线程队列（FIFO，确定性）
    futex_waiters: FxHashMap<u64, Vec<ThreadId>>,
    /// pthread key → 析构函数指针（线程退出时运行）
    pub key_dtors: FxHashMap<u32, Option<rustc_const_eval::interpret::Pointer<Option<Prov>>>>,
    pub next_pthread_key: u32,
    /// 当前时间片已执行语句数
    pub steps_in_slice: u32,
    /// 主动让出（sched_yield / 阻塞后置位）
    pub yield_requested: bool,
}

impl<'tcx> Default for ThreadManager<'tcx> {
    fn default() -> Self {
        ThreadManager {
            threads: vec![GuestThread::new("main".into())],
            active: MAIN_THREAD,
            futex_waiters: FxHashMap::default(),
            key_dtors: FxHashMap::default(),
            next_pthread_key: 1,
            steps_in_slice: 0,
            yield_requested: false,
        }
    }
}

// ===== 语义层：线程簿记与意图 API =====

impl<'tcx> ThreadManager<'tcx> {
    pub fn active_id(&self) -> ThreadId {
        self.active
    }

    pub fn active(&self) -> &GuestThread<'tcx> {
        &self.threads[self.active]
    }

    pub fn active_mut(&mut self) -> &mut GuestThread<'tcx> {
        &mut self.threads[self.active]
    }

    pub fn get(&self, id: ThreadId) -> Option<&GuestThread<'tcx>> {
        self.threads.get(id)
    }

    pub fn get_mut(&mut self, id: ThreadId) -> Option<&mut GuestThread<'tcx>> {
        self.threads.get_mut(id)
    }

    pub fn active_stack(&self) -> &[Frame<'tcx, Prov, FrameExtra<'tcx>>] {
        &self.threads[self.active].stack
    }

    pub fn active_stack_mut(&mut self) -> &mut Vec<Frame<'tcx, Prov, FrameExtra<'tcx>>> {
        &mut self.threads[self.active].stack
    }

    /// 创建新 guest 线程（根帧由调用方压入——需要 ecx，见 shims::pthread_create）。
    pub fn create_thread(&mut self) -> ThreadId {
        let id = self.threads.len();
        self.threads.push(GuestThread::new(format!("thread-{id}")));
        id
    }

    /// 临时切换 active（给新线程压根帧用的戏法，Miri 同款）。
    pub fn set_active(&mut self, id: ThreadId) -> ThreadId {
        std::mem::replace(&mut self.active, id)
    }

    /// 阻塞当前线程并请求让出。
    pub fn block_active(&mut self, reason: BlockReason) {
        debug_assert_eq!(self.threads[self.active].state, ThreadState::Runnable);
        if let BlockReason::Futex { addr, .. } = &reason {
            self.futex_waiters.entry(*addr).or_default().push(self.active);
        }
        self.threads[self.active].state = ThreadState::Blocked(reason);
        self.yield_requested = true;
    }

    pub fn unblock(&mut self, id: ThreadId) {
        let t = &mut self.threads[id];
        if matches!(t.state, ThreadState::Blocked(_)) {
            t.state = ThreadState::Runnable;
        }
    }

    /// futex 唤醒至多 n 个等待者，返回唤醒数。被唤醒者由调用方写结果。
    pub fn futex_wake(&mut self, addr: u64, n: usize) -> Vec<ThreadId> {
        let Some(q) = self.futex_waiters.get_mut(&addr) else { return vec![] };
        let take = n.min(q.len());
        let woken: Vec<ThreadId> = q.drain(..take).collect();
        if q.is_empty() {
            self.futex_waiters.remove(&addr);
        }
        for &id in &woken {
            self.threads[id].state = ThreadState::Runnable;
        }
        woken
    }

    /// 从 futex 等待队列移除（超时唤醒时防止残留）。
    fn futex_remove_waiter(&mut self, addr: u64, id: ThreadId) {
        if let Some(q) = self.futex_waiters.get_mut(&addr) {
            q.retain(|&t| t != id);
            if q.is_empty() {
                self.futex_waiters.remove(&addr);
            }
        }
    }

    /// 等待 `target` 退出的所有线程。
    fn join_waiters(&self, target: ThreadId) -> Vec<ThreadId> {
        self.threads
            .iter()
            .enumerate()
            .filter(|(_, t)| t.state == ThreadState::Blocked(BlockReason::Join(target)))
            .map(|(i, _)| i)
            .collect()
    }

    /// 有线程退出：唤醒 join 等待者。
    pub fn on_thread_terminated(&mut self, id: ThreadId) {
        self.threads[id].state = ThreadState::Terminated;
        self.threads[id].stack = Vec::new(); // 释放栈
        for w in self.join_waiters(id) {
            self.unblock(w);
        }
    }

    pub fn main_terminated(&self) -> bool {
        self.threads[MAIN_THREAD].state == ThreadState::Terminated
    }
}

// ===== 策略层：协作式调度（确定性模式）=====

/// 调度结果：切到哪个线程，或全员阻塞时如何处置。
pub enum Schedule {
    /// 继续跑（active 已设置好）
    Run,
    /// 所有线程终止（或 main 已终止）
    Done,
    /// 全员阻塞且存在最早期限：宿主睡到该时刻后唤醒（返回待超时唤醒的线程）
    SleepUntil(Instant, Vec<ThreadId>),
    /// 全员阻塞且无期限：死锁
    Deadlock,
}

impl<'tcx> ThreadManager<'tcx> {
    /// 确定性 round-robin：从 active+1 起找第一个 Runnable。
    pub fn schedule(&mut self) -> Schedule {
        self.steps_in_slice = 0;
        self.yield_requested = false;

        if self.main_terminated() {
            return Schedule::Done;
        }

        let n = self.threads.len();
        for off in 1..=n {
            let id = (self.active + off) % n;
            if self.threads[id].state == ThreadState::Runnable {
                self.active = id;
                return Schedule::Run;
            }
        }

        // 无 Runnable：看有没有带期限的阻塞者
        let now = Instant::now();
        let mut earliest: Option<Instant> = None;
        for t in &self.threads {
            let dl = match &t.state {
                ThreadState::Blocked(BlockReason::Sleep { deadline }) => Some(*deadline),
                ThreadState::Blocked(BlockReason::Futex { deadline: Some(d), .. }) => Some(*d),
                _ => None,
            };
            if let Some(d) = dl {
                earliest = Some(earliest.map_or(d, |e: Instant| e.min(d)));
            }
        }
        match earliest {
            Some(deadline) => {
                let wake_at = deadline.max(now);
                let due: Vec<ThreadId> = self
                    .threads
                    .iter()
                    .enumerate()
                    .filter(|(_, t)| match &t.state {
                        ThreadState::Blocked(BlockReason::Sleep { deadline }) => *deadline <= wake_at,
                        ThreadState::Blocked(BlockReason::Futex { deadline: Some(d), .. }) => {
                            *d <= wake_at
                        }
                        _ => false,
                    })
                    .map(|(i, _)| i)
                    .collect();
                Schedule::SleepUntil(wake_at, due)
            }
            None => Schedule::Deadlock,
        }
    }

    /// 超时唤醒：sleep 正常醒；futex 等待者按 ETIMEDOUT 醒（结果由调用方写）。
    /// 返回需要写 ETIMEDOUT 结果的线程。
    pub fn wake_due(&mut self, due: &[ThreadId]) -> Vec<ThreadId> {
        let mut futex_timeouts = Vec::new();
        for &id in due {
            let state = self.threads[id].state.clone();
            match state {
                ThreadState::Blocked(BlockReason::Sleep { .. }) => {
                    self.threads[id].state = ThreadState::Runnable;
                }
                ThreadState::Blocked(BlockReason::Futex { addr, .. }) => {
                    self.futex_remove_waiter(addr, id);
                    self.threads[id].state = ThreadState::Runnable;
                    futex_timeouts.push(id);
                }
                _ => {}
            }
        }
        futex_timeouts
    }
}

/// 死锁时的报错（放这里方便带上线程状态快照）。
pub fn deadlock_error<'tcx>(mgr: &ThreadManager<'tcx>) -> InterpResult<'tcx, ()> {
    let mut desc = String::from("死锁：所有 guest 线程都被阻塞且无超时。线程状态：");
    for (i, t) in mgr.threads.iter().enumerate() {
        desc.push_str(&format!("\n  [{i}] {} — {:?}", t.name, t.state));
    }
    throw_machine_stop!(Termination::Abort(desc));
}

