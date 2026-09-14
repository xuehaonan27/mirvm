//! J1 共享基座（M5.3a，m5.3-design §2）：per-fn 分层状态 = PLT 槽表 + 调用计数。
//!
//! 状态格 = Bytecode →（计数过阈值，M5.3b 编译）→ Machine：槽 0 = 解释执行；
//! 非零 = packed 入口机器地址（i2c 直调）。发布协议只有一次原子指针交换——
//! 编译线程 Release 写、call_guest Acquire 读（D4）。
//!
//! 本模块不依赖 Cranelift：guest 调用面仍只读原子槽；机器码范围和 perf-map 的锁与
//! 文件写入只在编译/工具冷路发生。TSan harness 经 #[path] 同源编译 src/vm；表按 S4
//! 合并后 FuncId 空间建（base 函数同等 tier-up，m5.3-design §4）。

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, RwLock};

/// strict 失败哨兵（MIRVM_JIT_SYNC 验证模式）：可准入函数编译失败时
/// worker 写入 slots——SYNC 等待方据此响亮 abort（区别于 0 = 未编译/
/// 维持解释的正常值域；非 strict 模式绝不写入）。
pub const FAIL_SENTINEL: u64 = u64::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JitCodeRange {
    pub start: u64,
    pub end: u64,
    pub func: u32,
}

/// profiler 可见的机器码范围。它比 `guest_code` 宽：包装层要能在 perf 里显示，
/// 但不能冒充额外的 MIRVM guest 回溯帧。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JitSymbolRole {
    FastBody,
    Guarded,
    Packed,
    C2i,
}

impl JitSymbolRole {
    fn as_str(self) -> &'static str {
        match self {
            Self::FastBody => "fast-body",
            Self::Guarded => "guarded",
            Self::Packed => "packed",
            Self::C2i => "c2i",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JitSymbolRange {
    pub engine_id: u64,
    pub func_id: u32,
    pub role: JitSymbolRole,
    pub start: u64,
    pub size: u64,
    pub display_name: String,
}

impl JitSymbolRange {
    pub fn new(
        engine_id: u64,
        func_id: u32,
        role: JitSymbolRole,
        start: u64,
        size: u64,
        guest_name: &str,
    ) -> Self {
        // perf-map 每个符号占一行；转义保证控制字符和非 ASCII 名称也只有一种写法。
        let guest_name: String = guest_name.chars().flat_map(char::escape_default).collect();
        let display_name = format!(
            "mirvm::engine-{engine_id}::func-{func_id}::{}::{guest_name}",
            role.as_str()
        );
        Self {
            engine_id,
            func_id,
            role,
            start,
            size,
            display_name,
        }
    }

    fn perf_map_line(&self) -> String {
        format!("{:x} {:x} {}\n", self.start, self.size, self.display_name)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PerfMapHealth {
    Inactive,
    Active,
    Incomplete,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PerfMapStatus {
    pub health: PerfMapHealth,
    pub path: Option<PathBuf>,
    pub registered_ranges: usize,
    pub written_ranges: usize,
    pub error: Option<String>,
}

struct PerfMapRegistry {
    ranges: Vec<JitSymbolRange>,
    sink: Option<Box<dyn Write + Send>>,
    health: PerfMapHealth,
    path: Option<PathBuf>,
    written_ranges: usize,
    error: Option<String>,
    control: Option<std::sync::Arc<ControlOperation>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ControlKind {
    Install,
    Stop,
}

struct ControlOperation {
    kind: ControlKind,
    result: Mutex<Option<PerfMapStatus>>,
    completed: std::sync::Condvar,
    #[cfg(test)]
    waiters: std::sync::atomic::AtomicUsize,
}

impl ControlOperation {
    fn new(kind: ControlKind) -> Self {
        Self {
            kind,
            result: Mutex::new(None),
            completed: std::sync::Condvar::new(),
            #[cfg(test)]
            waiters: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn wait(&self) -> PerfMapStatus {
        #[cfg(test)]
        self.waiters.fetch_add(1, Ordering::SeqCst);
        let mut result = self.result.lock().unwrap_or_else(|e| e.into_inner());
        while result.is_none() {
            result = self
                .completed
                .wait(result)
                .unwrap_or_else(|e| e.into_inner());
        }
        let status = result.as_ref().expect("control result is complete").clone();
        #[cfg(test)]
        self.waiters.fetch_sub(1, Ordering::SeqCst);
        status
    }

    fn finish(&self, status: PerfMapStatus) {
        *self.result.lock().unwrap_or_else(|e| e.into_inner()) = Some(status);
        self.completed.notify_all();
    }
}

impl Default for PerfMapRegistry {
    fn default() -> Self {
        Self {
            ranges: Vec::new(),
            sink: None,
            health: PerfMapHealth::Inactive,
            path: None,
            written_ranges: 0,
            error: None,
            control: None,
        }
    }
}

impl PerfMapRegistry {
    fn status(&self) -> PerfMapStatus {
        PerfMapStatus {
            health: self.health,
            path: self.path.clone(),
            registered_ranges: self.ranges.len(),
            written_ranges: self.written_ranges,
            error: self.error.clone(),
        }
    }

    fn mark_incomplete(&mut self, error: &io::Error) {
        self.sink = None;
        self.health = PerfMapHealth::Incomplete;
        self.error = Some(error.to_string());
    }

    fn register(&mut self, range: JitSymbolRange) {
        self.ranges.push(range);
    }
}

fn perf_registry() -> &'static Mutex<PerfMapRegistry> {
    static REGISTRY: OnceLock<Mutex<PerfMapRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(PerfMapRegistry::default()))
}

fn with_perf_registry<T>(f: impl FnOnce(&mut PerfMapRegistry) -> T) -> T {
    with_registry(perf_registry(), f)
}

fn with_registry<T>(
    registry: &Mutex<PerfMapRegistry>,
    f: impl FnOnce(&mut PerfMapRegistry) -> T,
) -> T {
    let mut registry = registry.lock().unwrap_or_else(|e| e.into_inner());
    f(&mut registry)
}

fn install_registry(registry: &Mutex<PerfMapRegistry>, path: &Path) -> io::Result<PerfMapStatus> {
    enum Decision {
        Wait(std::sync::Arc<ControlOperation>),
        Start(std::sync::Arc<ControlOperation>),
        AlreadyActive,
    }

    let operation = loop {
        let decision = with_registry(registry, |registry| {
            if let Some(operation) = registry.control.clone() {
                Decision::Wait(operation)
            } else if registry.health == PerfMapHealth::Active {
                Decision::AlreadyActive
            } else {
                let operation = std::sync::Arc::new(ControlOperation::new(ControlKind::Install));
                registry.control = Some(std::sync::Arc::clone(&operation));
                Decision::Start(operation)
            }
        });
        match decision {
            Decision::Wait(operation) => {
                operation.wait();
            }
            Decision::Start(operation) => break operation,
            Decision::AlreadyActive => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "a perf-map session is already active",
                ));
            }
        }
    };

    // 文件系统操作不能占着 range 锁，否则 JIT worker 的纯内存登记仍会被输出卡住。
    // 一个文件只属于一次 profile；旧文件重名必须报错，不能截断后混入另一代进程。
    let opened = OpenOptions::new().write(true).create_new(true).open(path);
    let finishing = std::sync::Arc::clone(&operation);
    let (status, error) = with_registry(registry, move |registry| {
        registry.sink = None;
        registry.path = Some(path.to_path_buf());
        registry.written_ranges = 0;
        registry.error = None;
        let error = match opened {
            Ok(file) => {
                registry.sink = Some(Box::new(file));
                registry.health = PerfMapHealth::Active;
                None
            }
            Err(error) => {
                registry.mark_incomplete(&error);
                Some(error)
            }
        };
        debug_assert!(
            registry
                .control
                .as_ref()
                .is_some_and(|active| std::sync::Arc::ptr_eq(active, &finishing))
        );
        registry.control = None;
        (registry.status(), error)
    });
    operation.finish(status.clone());
    match error {
        Some(error) => Err(error),
        None => Ok(status),
    }
}

fn stop_registry(registry: &Mutex<PerfMapRegistry>) -> PerfMapStatus {
    enum Decision {
        Join(std::sync::Arc<ControlOperation>),
        Wait(std::sync::Arc<ControlOperation>),
        Start {
            operation: std::sync::Arc<ControlOperation>,
            sink: Box<dyn Write + Send>,
            ranges: Vec<JitSymbolRange>,
        },
        Return(PerfMapStatus),
    }

    let (operation, mut sink, ranges) = loop {
        let decision = with_registry(registry, |registry| {
            if let Some(operation) = registry.control.clone() {
                if operation.kind == ControlKind::Stop {
                    Decision::Join(operation)
                } else {
                    Decision::Wait(operation)
                }
            } else if registry.health != PerfMapHealth::Active {
                Decision::Return(registry.status())
            } else {
                let operation = std::sync::Arc::new(ControlOperation::new(ControlKind::Stop));
                registry.control = Some(std::sync::Arc::clone(&operation));
                // 这个锁点就是 session 的结束：其后才登记/发布的范围属于下一次 session。
                registry.health = PerfMapHealth::Inactive;
                let sink = registry
                    .sink
                    .take()
                    .expect("active perf-map must own its sink");
                Decision::Start {
                    operation,
                    sink,
                    ranges: registry.ranges.clone(),
                }
            }
        });
        match decision {
            Decision::Join(operation) => return operation.wait(),
            Decision::Wait(operation) => {
                operation.wait();
            }
            Decision::Start {
                operation,
                sink,
                ranges,
            } => break (operation, sink, ranges),
            Decision::Return(status) => return status,
        }
    };

    // `stop` 的调用线程承担全部输出；JIT worker 从不持有 sink，也从不调用 Write。
    let mut written = 0;
    let result = (|| -> io::Result<()> {
        for range in &ranges {
            sink.write_all(range.perf_map_line().as_bytes())?;
            written += 1;
        }
        sink.flush()
    })();
    drop(sink);

    let status = with_registry(registry, |registry| {
        registry.written_ranges = written;
        match result {
            Ok(()) => {
                registry.health = PerfMapHealth::Inactive;
                registry.error = None;
            }
            Err(error) => registry.mark_incomplete(&error),
        }
        debug_assert!(
            registry
                .control
                .as_ref()
                .is_some_and(|active| std::sync::Arc::ptr_eq(active, &operation))
        );
        registry.control = None;
        registry.status()
    });
    operation.finish(status.clone());
    status
}

/// 启动进程级 perf-map，只创建本次会话的空文件；范围由 `stop_perf_map` 在控制线程写出。
// P1 先交冷路 API，下一片 P2 再接 CLI。
#[allow(dead_code)]
pub fn install_perf_map(path: impl AsRef<Path>) -> io::Result<PerfMapStatus> {
    install_registry(perf_registry(), path.as_ref())
}

#[allow(dead_code)]
pub fn stop_perf_map() -> PerfMapStatus {
    stop_registry(perf_registry())
}

#[allow(dead_code)]
pub fn perf_map_status() -> PerfMapStatus {
    with_perf_registry(|registry| registry.status())
}

#[allow(dead_code)]
pub fn jit_symbol_ranges() -> Vec<JitSymbolRange> {
    with_perf_registry(|registry| registry.ranges.clone())
}

/// TODO: multiple guest threads may access to this structure, optimize
/// access to this structure, e.g. take care of cache locality, or should
/// it be made volatile.
pub struct JitState {
    /// PLT 槽（interp i2c 面）：FuncId → packed 入口机器地址（0 = 未编译，走解释）。
    pub slots: Vec<AtomicU64>,
    /// PLT 槽（编译码 cc→cc 面）：FuncId → fast 入口 / c2i 蹦床地址（0 = 尚无）。
    /// 只有编译码的调用点读它（load + call_indirect）；interp 不消费。
    pub slots_fast: Vec<AtomicU64>,
    /// 调用计数（Relaxed；竞态丢计无害——只影响触发时刻，不影响语义）
    pub counters: Vec<AtomicU32>,
    /// `--jit off` / `MIRVM_JIT=off` ⇒ false：纯解释，计数也不做（对拍口径）
    pub enabled: bool,
    /// 过阈值投递编译队列（Q3 裁定 1000）
    pub threshold: u32,
    /// `MIRVM_JIT_SYNC=1` 验证模式（audit F-05）：投递后等待发布/失败哨兵
    /// ——threshold=1 的语义从「首调请求编译」升为「首调同步编译发布」，
    /// 可准入函数的编译失败从静默留解释升为响亮 abort（gate 显形）
    pub sync: bool,
    /// 编译请求通道（M5.3b：jit_compile::start 装填；cranelift feature 关 = 恒 None）
    pub queue: std::sync::Mutex<Option<std::sync::mpsc::Sender<u32>>>,
    /// worker 必须在进程退出的分配器清理前 join；丢弃句柄会让 Cranelift
    /// 与 libc/Rust 退出清理并发，造成跨 workload 漂移的堆破坏。
    pub worker: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
    /// 退出收尾置位后，worker 完成当前函数即丢弃尚未消费的编译请求。
    pub stopping: AtomicBool,
    /// 已发布的客体 fast 函数体机器码范围。guarded/packed/c2i 包装不登记；
    /// backtrace 因此每个客体调用只看到一个函数帧。
    pub guest_code: RwLock<Vec<JitCodeRange>>,
    /// 本 Engine 的所有 Cranelift 执行范围，供 profile 和完整性检查使用。
    symbol_ranges: RwLock<Vec<JitSymbolRange>>,
}

impl JitState {
    pub fn new(fn_count: usize) -> Self {
        let enabled = match std::env::var("MIRVM_JIT") {
            Ok(v) => !(v == "off" || v == "0"),
            Err(_) => true, // D4：默认 on
        };
        let threshold = std::env::var("MIRVM_JIT_THRESHOLD")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&t| t > 0)
            .unwrap_or(1000);
        JitState {
            slots: (0..fn_count).map(|_| AtomicU64::new(0)).collect(),
            slots_fast: (0..fn_count).map(|_| AtomicU64::new(0)).collect(),
            counters: (0..fn_count).map(|_| AtomicU32::new(0)).collect(),
            enabled,
            threshold,
            sync: std::env::var_os("MIRVM_JIT_SYNC").is_some(),
            queue: std::sync::Mutex::new(None),
            worker: std::sync::Mutex::new(None),
            stopping: AtomicBool::new(false),
            guest_code: RwLock::new(Vec::new()),
            symbol_ranges: RwLock::new(Vec::new()),
        }
    }

    fn register_symbol_range_with(&self, range: JitSymbolRange, registry: &Mutex<PerfMapRegistry>) {
        debug_assert!(range.size != 0, "Cranelift produced an empty code range");
        if range.role == JitSymbolRole::FastBody {
            self.guest_code
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .push(JitCodeRange {
                    start: range.start,
                    end: range.start.saturating_add(range.size),
                    func: range.func_id,
                });
        }
        self.symbol_ranges
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .push(range.clone());
        with_registry(registry, |registry| registry.register(range));
    }

    /// 两个可调用入口变为可见之前，先登记本批全部范围。
    pub(crate) fn publish_compiled_entries(
        &self,
        func: u32,
        guarded_entry: u64,
        packed_entry: u64,
        ranges: Vec<JitSymbolRange>,
    ) {
        self.publish_compiled_entries_with(
            func,
            guarded_entry,
            packed_entry,
            ranges,
            perf_registry(),
        );
    }

    fn publish_compiled_entries_with(
        &self,
        func: u32,
        guarded_entry: u64,
        packed_entry: u64,
        ranges: Vec<JitSymbolRange>,
        registry: &Mutex<PerfMapRegistry>,
    ) {
        debug_assert!(
            ranges
                .iter()
                .any(|range| range.role == JitSymbolRole::FastBody)
        );
        debug_assert!(
            ranges
                .iter()
                .any(|range| range.role == JitSymbolRole::Guarded)
        );
        debug_assert!(
            ranges
                .iter()
                .any(|range| range.role == JitSymbolRole::Packed)
        );
        for range in ranges {
            self.register_symbol_range_with(range, registry);
        }
        self.slots_fast[func as usize].store(guarded_entry, Ordering::Release);
        self.slots[func as usize].store(packed_entry, Ordering::Release);
    }

    /// c2i 也先登记，编译码随后才能从槽中读到其地址；文件只在 stop 控制边界写出。
    pub(crate) fn publish_c2i_entry(&self, func: u32, entry: u64, ranges: Vec<JitSymbolRange>) {
        self.publish_c2i_entry_with(func, entry, ranges, perf_registry());
    }

    fn publish_c2i_entry_with(
        &self,
        func: u32,
        entry: u64,
        ranges: Vec<JitSymbolRange>,
        registry: &Mutex<PerfMapRegistry>,
    ) {
        debug_assert!(ranges.iter().any(|range| {
            range.func_id == func && range.role == JitSymbolRole::C2i && range.start == entry
        }));
        for range in ranges {
            self.register_symbol_range_with(range, registry);
        }
        self.slots_fast[func as usize].store(entry, Ordering::Release);
    }

    // P2 会把这个 per-Engine 视图与进程级快照一并接出。
    #[allow(dead_code)]
    pub fn symbol_ranges(&self) -> Vec<JitSymbolRange> {
        self.symbol_ranges
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn guest_func_at(&self, ip: u64) -> Option<u32> {
        self.guest_code
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .rev()
            .find(|range| range.start <= ip && ip < range.end)
            .map(|range| range.func)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        JitState, JitSymbolRange, JitSymbolRole, PerfMapHealth, PerfMapRegistry, install_registry,
        stop_registry, with_registry,
    };
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::atomic::Ordering;

    fn test_map_path(name: &str) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "mirvm-{name}-{}-{}.map",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn range(func: u32, role: JitSymbolRole, start: u64, size: u64) -> JitSymbolRange {
        JitSymbolRange::new(17, func, role, start, size, "guest\nname")
    }

    #[test]
    fn tables_sized_to_fn_count_and_zero_initialized() {
        let j = JitState::new(7);
        assert_eq!(j.slots.len(), 7);
        assert_eq!(j.counters.len(), 7);
        assert!(j.slots.iter().all(|s| s.load(Ordering::Acquire) == 0));
        assert!(j.worker.lock().unwrap().is_none());
        assert!(!j.stopping.load(Ordering::Acquire));
        assert!(j.threshold > 0);
        assert!(j.guest_code.read().unwrap().is_empty());
        assert!(j.symbol_ranges().is_empty());
    }

    #[test]
    fn compiled_entry_publication_registers_all_roles_before_slots_become_visible() {
        let registry = Mutex::new(PerfMapRegistry::default());
        let path = test_map_path("jit-publish");
        install_registry(&registry, &path).unwrap();

        let j = JitState::new(4);
        j.publish_compiled_entries_with(
            3,
            0x2000,
            0x3000,
            vec![
                range(3, JitSymbolRole::FastBody, 0x1000, 0x31),
                range(3, JitSymbolRole::Guarded, 0x2000, 0x17),
                range(3, JitSymbolRole::Packed, 0x3000, 0x29),
            ],
            &registry,
        );
        j.publish_c2i_entry_with(
            2,
            0x4000,
            vec![range(2, JitSymbolRole::C2i, 0x4000, 0x1d)],
            &registry,
        );

        assert_eq!(j.slots_fast[3].load(Ordering::Acquire), 0x2000);
        assert_eq!(j.slots[3].load(Ordering::Acquire), 0x3000);
        assert_eq!(j.slots_fast[2].load(Ordering::Acquire), 0x4000);
        let ranges = j.symbol_ranges();
        assert_eq!(ranges.len(), 4);
        assert_eq!(
            with_registry(&registry, |registry| registry.ranges.len()),
            4
        );
        for role in [
            JitSymbolRole::FastBody,
            JitSymbolRole::Guarded,
            JitSymbolRole::Packed,
            JitSymbolRole::C2i,
        ] {
            assert!(ranges.iter().any(|range| range.role == role));
        }
        assert_eq!(j.guest_func_at(0x1010), Some(3));
        assert_eq!(j.guest_func_at(0x2010), None);
        assert_eq!(j.guest_func_at(0x3010), None);
        assert_eq!(j.guest_func_at(0x4010), None);

        assert_eq!(std::fs::read(&path).unwrap(), b"");
        let active = with_registry(&registry, |registry| registry.status());
        assert_eq!(active.health, PerfMapHealth::Active);
        assert_eq!(active.registered_ranges, 4);
        assert_eq!(active.written_ranges, 0);

        let stopped = stop_registry(&registry);
        assert_eq!(stopped.health, PerfMapHealth::Inactive);
        assert_eq!(stopped.written_ranges, 4);
        let map = std::fs::read_to_string(&path).unwrap();
        assert!(map.contains("1000 31 mirvm::engine-17::func-3::fast-body::guest\\nname\n"));
        assert!(map.contains("2000 17 mirvm::engine-17::func-3::guarded::guest\\nname\n"));
        assert!(map.contains("3000 29 mirvm::engine-17::func-3::packed::guest\\nname\n"));
        assert!(map.contains("4000 1d mirvm::engine-17::func-2::c2i::guest\\nname\n"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn map_open_failure_is_incomplete_but_does_not_block_entry_publication() {
        let registry = Mutex::new(PerfMapRegistry::default());
        let missing_parent = test_map_path("missing-parent");
        let path = missing_parent.join("perf.map");
        assert!(install_registry(&registry, &path).is_err());
        assert_eq!(
            with_registry(&registry, |registry| registry.status().health),
            PerfMapHealth::Incomplete
        );

        let j = JitState::new(1);
        j.publish_c2i_entry_with(
            0,
            0x5000,
            vec![range(0, JitSymbolRole::C2i, 0x5000, 0x20)],
            &registry,
        );
        assert_eq!(j.slots_fast[0].load(Ordering::Acquire), 0x5000);
        assert_eq!(j.symbol_ranges().len(), 1);
        assert_eq!(
            with_registry(&registry, |registry| registry.status().health),
            PerfMapHealth::Incomplete
        );
    }

    #[test]
    fn install_never_replaces_an_existing_or_active_map() {
        let registry = Mutex::new(PerfMapRegistry::default());
        let existing = test_map_path("jit-existing");
        std::fs::write(&existing, b"existing\n").unwrap();
        let error = install_registry(&registry, &existing).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&existing).unwrap(), b"existing\n");
        assert_eq!(
            with_registry(&registry, |registry| registry.status().health),
            PerfMapHealth::Incomplete
        );
        std::fs::remove_file(existing).unwrap();

        let active = test_map_path("jit-active");
        install_registry(&registry, &active).unwrap();
        let other = test_map_path("jit-second-active");
        let error = install_registry(&registry, &other).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert!(!other.exists());
        assert_eq!(
            with_registry(&registry, |registry| registry.status().health),
            PerfMapHealth::Active
        );
        stop_registry(&registry);
        std::fs::remove_file(active).unwrap();
    }

    #[test]
    fn inactive_registration_is_written_only_by_explicit_stop() {
        let registry = Mutex::new(PerfMapRegistry::default());
        let j = JitState::new(1);
        j.publish_c2i_entry_with(
            0,
            0x6000,
            vec![range(0, JitSymbolRole::C2i, 0x6000, 0x21)],
            &registry,
        );
        let before = with_registry(&registry, |registry| registry.status());
        assert_eq!(before.health, PerfMapHealth::Inactive);
        assert_eq!(before.path, None);
        assert_eq!(before.registered_ranges, 1);
        assert_eq!(before.written_ranges, 0);

        let path = test_map_path("jit-backfill");
        let after = install_registry(&registry, &path).unwrap();
        assert_eq!(after.health, PerfMapHealth::Active);
        assert_eq!(after.written_ranges, 0);
        assert_eq!(std::fs::read(&path).unwrap(), b"");
        let stopped = stop_registry(&registry);
        assert_eq!(stopped.health, PerfMapHealth::Inactive);
        assert_eq!(stopped.written_ranges, 1);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "6000 21 mirvm::engine-17::func-0::c2i::guest\\nname\n"
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn concurrent_engines_append_complete_lines() {
        let registry = std::sync::Arc::new(Mutex::new(PerfMapRegistry::default()));
        let path = test_map_path("jit-concurrent");
        install_registry(&registry, &path).unwrap();
        let mut workers = Vec::new();
        for engine_id in 1..=8 {
            let registry = std::sync::Arc::clone(&registry);
            workers.push(std::thread::spawn(move || {
                let j = JitState::new(1);
                let start = 0x7000 + engine_id * 0x100;
                let range =
                    JitSymbolRange::new(engine_id, 0, JitSymbolRole::C2i, start, 0x22, "target");
                j.publish_c2i_entry_with(0, start, vec![range], &registry);
                assert_eq!(j.slots_fast[0].load(Ordering::Acquire), start);
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(std::fs::read(&path).unwrap(), b"");
        assert_eq!(
            with_registry(&registry, |registry| registry.status().written_ranges),
            0
        );
        assert_eq!(stop_registry(&registry).written_ranges, 8);

        let map = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<_> = map.lines().collect();
        assert_eq!(lines.len(), 8);
        for engine_id in 1..=8 {
            let start = 0x7000 + engine_id * 0x100;
            assert!(lines.iter().any(|line| {
                *line == format!("{start:x} 22 mirvm::engine-{engine_id}::func-0::c2i::target")
            }));
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn stop_cutoff_excludes_ranges_published_after_its_linearization_point() {
        struct BlockingSink {
            gate: std::sync::Arc<(Mutex<(bool, bool)>, std::sync::Condvar)>,
            bytes: std::sync::Arc<Mutex<Vec<u8>>>,
        }

        impl std::io::Write for BlockingSink {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                let (state, changed) = &*self.gate;
                let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                state.0 = true;
                changed.notify_all();
                while !state.1 {
                    state = changed.wait(state).unwrap_or_else(|e| e.into_inner());
                }
                drop(state);
                self.bytes
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .extend_from_slice(buf);
                Ok(buf.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let registry = std::sync::Arc::new(Mutex::new(PerfMapRegistry::default()));
        let j = JitState::new(2);
        j.publish_c2i_entry_with(
            0,
            0x9000,
            vec![range(0, JitSymbolRole::C2i, 0x9000, 0x24)],
            &registry,
        );
        let gate = std::sync::Arc::new((Mutex::new((false, false)), std::sync::Condvar::new()));
        let bytes = std::sync::Arc::new(Mutex::new(Vec::new()));
        with_registry(&registry, |registry| {
            registry.sink = Some(Box::new(BlockingSink {
                gate: std::sync::Arc::clone(&gate),
                bytes: std::sync::Arc::clone(&bytes),
            }));
            registry.health = PerfMapHealth::Active;
            registry.path = Some(PathBuf::from("blocking.map"));
        });

        let stop_registry_ref = std::sync::Arc::clone(&registry);
        let stop = std::thread::spawn(move || stop_registry(&stop_registry_ref));
        let (gate_state, changed) = &*gate;
        let mut flags = gate_state.lock().unwrap_or_else(|e| e.into_inner());
        while !flags.0 {
            flags = changed.wait(flags).unwrap_or_else(|e| e.into_inner());
        }
        drop(flags);

        let operation = with_registry(&registry, |registry| {
            registry
                .control
                .clone()
                .expect("the first stop must remain in progress")
        });
        let second_registry = std::sync::Arc::clone(&registry);
        let second_stop = std::thread::spawn(move || stop_registry(&second_registry));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while operation.waiters.load(Ordering::SeqCst) == 0 {
            assert!(
                !second_stop.is_finished(),
                "a concurrent stop returned an intermediate status"
            );
            assert!(
                std::time::Instant::now() < deadline,
                "the concurrent stop did not join the active control operation"
            );
            std::thread::yield_now();
        }
        let next = test_map_path("jit-next-session");
        let next_for_install = next.clone();
        let install_registry_ref = std::sync::Arc::clone(&registry);
        let install =
            std::thread::spawn(move || install_registry(&install_registry_ref, &next_for_install));
        while operation.waiters.load(Ordering::SeqCst) < 2 {
            assert!(
                !install.is_finished(),
                "an install raced past the active stop operation"
            );
            assert!(
                std::time::Instant::now() < deadline,
                "the concurrent install did not wait for stop"
            );
            std::thread::yield_now();
        }

        j.publish_c2i_entry_with(
            1,
            0xa000,
            vec![range(1, JitSymbolRole::C2i, 0xa000, 0x25)],
            &registry,
        );
        let mut flags = gate_state.lock().unwrap_or_else(|e| e.into_inner());
        flags.1 = true;
        changed.notify_all();
        drop(flags);

        let status = stop.join().unwrap();
        let second_status = second_stop.join().unwrap();
        assert_eq!(second_status, status);
        assert_eq!(
            install.join().unwrap().unwrap().health,
            PerfMapHealth::Active
        );
        assert_eq!(status.health, PerfMapHealth::Inactive);
        assert_eq!(status.registered_ranges, 2);
        assert_eq!(status.written_ranges, 1);
        let first =
            String::from_utf8(bytes.lock().unwrap_or_else(|e| e.into_inner()).clone()).unwrap();
        assert!(first.contains("9000 24 mirvm::engine-17::func-0::c2i"));
        assert!(!first.contains("func-1::c2i"));

        assert_eq!(stop_registry(&registry).written_ranges, 2);
        let next_map = std::fs::read_to_string(&next).unwrap();
        assert!(next_map.contains("func-0::c2i"));
        assert!(next_map.contains("func-1::c2i"));
        std::fs::remove_file(next).unwrap();
    }

    #[test]
    fn concurrent_stop_waiter_observes_the_same_write_failure() {
        struct BlockingFailingSink {
            gate: std::sync::Arc<(Mutex<(bool, bool)>, std::sync::Condvar)>,
            writes: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        }

        impl std::io::Write for BlockingFailingSink {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                self.writes.fetch_add(1, Ordering::SeqCst);
                let (state, changed) = &*self.gate;
                let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                state.0 = true;
                changed.notify_all();
                while !state.1 {
                    state = changed.wait(state).unwrap_or_else(|e| e.into_inner());
                }
                Err(std::io::Error::other("injected blocked write failure"))
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let registry = std::sync::Arc::new(Mutex::new(PerfMapRegistry::default()));
        let gate = std::sync::Arc::new((Mutex::new((false, false)), std::sync::Condvar::new()));
        let writes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        with_registry(&registry, |registry| {
            registry.sink = Some(Box::new(BlockingFailingSink {
                gate: std::sync::Arc::clone(&gate),
                writes: std::sync::Arc::clone(&writes),
            }));
            registry.health = PerfMapHealth::Active;
            registry.path = Some(PathBuf::from("blocking-failure.map"));
        });
        let j = JitState::new(1);
        j.publish_c2i_entry_with(
            0,
            0xb000,
            vec![range(0, JitSymbolRole::C2i, 0xb000, 0x26)],
            &registry,
        );

        let first_registry = std::sync::Arc::clone(&registry);
        let first = std::thread::spawn(move || stop_registry(&first_registry));
        let (gate_state, changed) = &*gate;
        let mut flags = gate_state.lock().unwrap_or_else(|e| e.into_inner());
        while !flags.0 {
            flags = changed.wait(flags).unwrap_or_else(|e| e.into_inner());
        }
        drop(flags);

        let operation = with_registry(&registry, |registry| {
            registry
                .control
                .clone()
                .expect("the failing stop must remain in progress")
        });
        let second_registry = std::sync::Arc::clone(&registry);
        let second = std::thread::spawn(move || stop_registry(&second_registry));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while operation.waiters.load(Ordering::SeqCst) == 0 {
            assert!(!second.is_finished(), "the second stop returned too early");
            assert!(
                std::time::Instant::now() < deadline,
                "the second stop did not join the failing operation"
            );
            std::thread::yield_now();
        }

        let mut flags = gate_state.lock().unwrap_or_else(|e| e.into_inner());
        flags.1 = true;
        changed.notify_all();
        drop(flags);

        let first_status = first.join().unwrap();
        let second_status = second.join().unwrap();
        assert_eq!(second_status, first_status);
        assert_eq!(first_status.health, PerfMapHealth::Incomplete);
        assert!(
            first_status
                .error
                .as_deref()
                .is_some_and(|error| error.contains("injected blocked write failure"))
        );
        assert_eq!(writes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn stop_write_failure_is_incomplete_without_blocking_published_code() {
        struct FailingSink;

        impl std::io::Write for FailingSink {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("injected perf-map write failure"))
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let registry = Mutex::new(PerfMapRegistry::default());
        with_registry(&registry, |registry| {
            registry.sink = Some(Box::new(FailingSink));
            registry.health = PerfMapHealth::Active;
            registry.path = Some(PathBuf::from("injected.map"));
        });
        let j = JitState::new(1);
        j.publish_c2i_entry_with(
            0,
            0x8000,
            vec![range(0, JitSymbolRole::C2i, 0x8000, 0x23)],
            &registry,
        );

        assert_eq!(j.slots_fast[0].load(Ordering::Acquire), 0x8000);
        let status = stop_registry(&registry);
        assert_eq!(status.health, PerfMapHealth::Incomplete);
        assert_eq!(status.registered_ranges, 1);
        assert_eq!(status.written_ranges, 0);
        assert!(
            status
                .error
                .as_deref()
                .is_some_and(|error| error.contains("injected perf-map write failure"))
        );
    }
}
