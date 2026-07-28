# mirvm 高性能日志系统设计方案

> 面向 mirvm 的性能敏感日志系统：自造 backend，分层路由 + 零分配格式化 + ring buffer 异步落盘。
> 目标：热路径同步成本压到 ~100 ns 量级；error/warn 崩溃前不丢。

---

## 1. 背景与目标

mirvm 的日志系统面临普通应用没有的三重约束：

1. **频率极端**：trace 级日志每秒可达百万级；同时 error 级日志罕见但崩溃前必须落盘；
2. **分配受限**：VM 内部堆分配可能引发分配器重入（拦截层自身可能 hook 了内存路径），热路径必须零分配；
3. **不能阻塞 guest**：执行线程绝不允许等待磁盘/终端 I/O。

现成框架（env_logger / fern / log4rs / tracing）均为通用场景设计，同步路径成本在 1–5 µs 量级，且普遍依赖堆分配与全局锁，不满足上述约束。结论：**分层自造**，让不同频率的日志各走最合适的机制。

## 2. 成本模型：一次日志调用的开销分解

```
mirvm_log!([trace], "syscall {} args {:?}", nr, args)
   │
   ├─ ① 级别检查          原子读 max_level              ~1–2 ns
   ├─ ② 参数捕获          format_args!（惰性，零成本）   ~0
   ├─ ③ 元数据            tid（TLS 读）/ file、line（编译期常量）~1–5 ns
   ├─ ④ 时间戳            clock_gettime（vdso，无 syscall）~15–25 ns
   │                      格式化成年月日                 额外 ~50–100 ns（热路径禁用）
   ├─ ⑤ 格式化            fmt::write 完整走一遍         ~50–300 ns（视参数复杂度）
   ├─ ⑥ 堆分配            String/Vec                   ~50–200 ns/次（VM 内禁止）
   ├─ ⑦ 锁                stderr 全局锁                 竞争时成本不可控
   └─ ⑧ write 系统调用    → tty / 管道                  ~1–5 µs    ← 同步方案的大头
                          → 文件（页缓存）               ~300–800 ns
```

两条指导性结论：

- **⑧ 支配一切**：只要每行日志同步 `write`，格式化省下的几百 ns 会被一次系统调用抹平。热路径必须把 I/O 移出执行线程；
- **⑤⑥ 是方案间真正拉开差距的环节**：零分配 + 栈缓冲格式化是必须做的，时间戳格式化成年月日则应在热路径禁用（输出单调时钟相对值，离线转换）。

## 3. 总体架构：按级别分层路由

```
                    ┌────────────────────────────────────────────┐
                    │  mirvm_log!([level], ...) 宏入口            │
                    │  ① log_enabled! 短路（~1 ns，关闭即返回）    │
                    └──────────────┬─────────────────────────────┘
                                   │ 通过级别检查
              ┌────────────────────┼─────────────────────┐
              ▼                    ▼                     ▼
      error / warn           info / debug           trace（热路径）
      罕见、崩溃前不可丢      中频                     与 syscall 同频
              │                    │                     │
              ▼                    ▼                     ▼
   ┌──────────────────┐  ┌──────────────────┐  ┌─────────────────────┐
   │ 同步路径 A        │  │ 同步路径 B        │  │ 异步路径 C           │
   │ 栈缓冲格式化       │  │ 栈缓冲格式化       │  │ 格式化进 ring 槽位   │
   │ 单次 write_all    │  │ 写入带缓冲的文件   │  │ （~100–200 ns）     │
   │ 直写 stderr       │  │                  │  │ 专职线程批量落盘     │
   └──────────────────┘  └──────────────────┘  └─────────────────────┘
                                   另有两道闸门作用于 trace：
           闸门 1（运行时）：log_enabled! 级别短路
           闸门 2（编译期）：Cargo feature 物理移除 trace 代码
```

设计哲学：**不让任何一条日志为不属于自己频率级别的机制付费**。error 走最可靠的路径（成本无所谓），trace 走最快的路径（允许极端情况下丢弃并计数）。

## 4. 组件设计

### 4.1 宏入口与元数据捕获

`file!()`、`line!()`、`module_path!()` 展开的是**宏调用处**的位置，因此元数据必须在宏内捕获，不能下沉到普通函数：

```rust
#[macro_export]
macro_rules! mirvm_log {
    // 各级别入口：小写 ident → log::Level 变体的显式映射（避免任意 ident 误匹配）
    ([error], $($arg:tt)*) => { $crate::log::emit!(::log::Level::Error, $($arg)*) };
    ([warn],  $($arg:tt)*) => { $crate::log::emit!(::log::Level::Warn,  $($arg)*) };
    ([info],  $($arg:tt)*) => { $crate::log::emit!(::log::Level::Info,  $($arg)*) };
    ([debug], $($arg:tt)*) => { $crate::log::emit!(::log::Level::Debug, $($arg)*) };
    ([trace], $($arg:tt)*) => {
        #[cfg(feature = "trace-log")]
        $crate::log::emit!(::log::Level::Trace, $($arg)*)
    };
}

// 内部统一出口
#[doc(hidden)]
#[macro_export]
macro_rules! __mirvm_emit {
    ($level:expr, $($arg:tt)*) => {{
        let level: ::log::Level = $level;
        if ::log::log_enabled!(level) {          // ① 运行时闸门：关闭时一切不求值
            $crate::log::emit($crate::log::Record {
                level,
                file:   file!(),
                line:   line!(),
                module: module_path!(),
                tid:    $crate::log::tid(),
                args:   format_args!($($arg)*),  // ② 惰性参数，零分配
            });
        }
    }};
}
```

要点：

- `log_enabled!` 在最外层，级别关闭时**连参数都不求值**——热路径关闭状态成本 = 一次 relaxed 原子读（~1 ns）；
- `trace` 级别额外受 `trace-log` feature 控制：编译期不存在（真正的零开销），release 构建默认关闭；
- `format_args!` 是惰性的，不产生 `String`，配合栈缓冲实现全链路零分配。

### 4.2 Record 与可配置 decorator

```rust
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decorator { Prefix, Time, Level, Tid, File, Module }

pub struct Record<'a> {
    pub level:  log::Level,
    pub file:   &'static str,
    pub line:   u32,
    pub module: &'static str,
    pub tid:    u64,
    pub args:   fmt::Arguments<'a>,
}
```

decorator 列表**启动时从环境变量解析一次**，格式化时按序派发。这对应 JVM Unified Logging 的 `-Xlog:::time,level,tid` 体验，但用运行时代替 C++ 模板——派发成本是一次小数组迭代（几个分支），相对后续 I/O 可忽略：

```rust
static DECORATORS: OnceLock<Vec<Decorator>> = OnceLock::new();

// MIRVM_LOG_FMT="prefix,time,level,tid,file"
fn parse_decorators(env: &str) -> Vec<Decorator> { /* split(',') + match 逐项解析 */ }
```

输出形如：

```
[mirvm][12345ms][INFO ][tid10123][src/syscall/mod.rs:87] foobar
```

### 4.3 零分配栈缓冲

```rust
pub struct StackBuf {
    buf: [u8; 1024],
    len: usize,
}

impl fmt::Write for StackBuf {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let n = s.len().min(self.buf.len() - self.len);   // 截断而非 panic
        self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        Ok(())
    }
}
```

所有 decorator 与用户消息先 fmt 进栈缓冲，**攒满整行后单次 `write_all`**：

- 避免每个 decorator 一次 `write` 的系统调用放大；
- 持锁一次，多线程输出不交错（整行原子性）；
- 超长消息静默截断，不 panic、不分配。

### 4.4 tid：TLS 缓存的 OS 线程号

`std::thread::ThreadId` 是不透明计数器，与 `top -H` / strace / ftrace 输出对不上。VM 场景必须用 `gettid`，且只在线程启动时 syscall 一次，之后走 TLS：

```rust
use std::cell::Cell;

thread_local! {
    static TID: Cell<u64> = Cell::new(unsafe {
        libc::syscall(libc::SYS_gettid) as u64
    });
}

#[inline]
pub fn tid() -> u64 {
    TID.with(|t| t.get())     // 热路径：一次 TLS 读，~1 ns
}
```

### 4.5 时间戳：单调时钟，热路径不做日历格式化

```rust
#[inline]
fn monotonic_ms() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1000 + ts.tv_nsec as u64 / 1_000_000
}
```

- `clock_gettime(CLOCK_MONOTONIC)` 走 vdso，无系统调用，~15–25 ns；
- 热路径输出**相对毫秒**（或启动时刻为原点的差值），syscall 追踪场景里相对时间比墙钟更有用；
- 若确需日历时间，由落盘线程/离线工具转换，绝不在执行线程格式化年月日（额外 ~50–100 ns + 时区逻辑）。

### 4.6 路径 A/B：同步写出

```rust
pub fn emit_sync(rec: &Record) {
    let mut buf = StackBuf::new();
    format_decorators(&mut buf, rec);        // 按 DECORATORS 顺序派发
    let _ = buf.write_char(' ');
    let _ = fmt::write(&mut buf, rec.args);
    let _ = buf.write_char('\n');

    use std::io::Write as _;
    let mut stderr = std::io::stderr().lock();
    let _ = stderr.write_all(buf.as_slice());  // 单次原子写，吞掉 EPIPE
}
```

- error/warn 走此路径直写 stderr：**崩溃前不丢**是第一优先级，成本无所谓；
- info/debug 走结构相同、目标为带缓冲文件的路径 B；
- 吞掉写错误（`let _ =`）：EPIPE 不应拖垮 guest，见 §6 权衡。

### 4.7 路径 C：ring buffer 异步落盘（热路径核心）

```
执行线程（生产者）                     专职落盘线程（消费者）
┌─────────────────────┐               ┌──────────────────────┐
│ log_enabled! 短路    │               │ 阻塞/轮询等待数据      │
│ 栈缓冲格式化进槽位    │── SPSC/MPSC ──▶│ 批量 read ring        │
│ ~100–200 ns          │    ring       │ writev 批量写文件      │
└─────────────────────┘               └──────────────────────┘
```

- **槽位里存格式化后的字节**（生产者 fmt 进槽位）：实现简单，生产者成本 ~100–200 ns。备选是 ftrace 式"存原始参数 + 格式串指针"，生产者压到 ~20 ns，但需要参数类型擦除、消费者侧类型表，复杂度大增——**先做前者，profile 不够再升级**；
- **ring 选型**：单执行线程用 SPSC（如 `rtrb`）；多执行线程用 MPSC（如 crossbeam 的有界队列或自造 per-thread SPSC 汇聚）；
- **ring 满策略**：丢弃新日志 + 原子计数器（dmesg/ftrace 策略），绝不让 guest 线程阻塞等 I/O；计数器可周期性以 warn 级别输出 "dropped N trace records"；
- **消费者批量 I/O**：一次读多个槽位，`writev` 聚合写出，摊薄系统调用；
- **优雅退出**：进程退出 / VM shutdown 时 join 消费者线程前 drain ring。

### 4.8 崩溃兜底

异步方案的软肋是 crash 时 ring 内未消费数据丢失。缓解措施（按实施成本排序）：

1. **error/warn 永远走同步路径 A**（已内建于架构）——最关键的一手；
2. 致命信号（SIGSEGV/SIGABRT）handler 中 best-effort flush ring 到 stderr（注意 handler 内只能 async-signal-safe 操作，仅做 `write`，不格式化、不分配）；
3. ring 使用共享内存/独立文件映射，崩溃后由外部进程打捞（成本高，按需）。

## 5. 配置面

| 配置项 | 方式 | 说明 |
|---|---|---|
| 全局级别 | `MIRVM_LOG=trace` / `log::set_max_level` | 运行时闸门 1 |
| trace 代码存在性 | Cargo feature `trace-log` | 编译期闸门 2，release 默认关 |
| decorator 与顺序 | `MIRVM_LOG_FMT=prefix,time,level,tid,file` | 启动解析一次 |
| 输出目标 | error→stderr 固定；info/debug/trace→文件路径配置 | |
| ring 容量 | 构建时常量或启动参数（建议 64K–1M 槽位） | |
| 时间基准 | 固定 `CLOCK_MONOTONIC` | |

## 6. 已知权衡与限制

- **写错误被吞掉**：日志系统不应让 guest 因 EPIPE 崩溃；代价是静默丢失。可通过"连续写失败计数 + stderr 一次性告警"缓解；
- **栈缓冲截断**：单行超 1KB 截断。对 syscall 日志足够；若有长 dump 需求，走独立的"大块日志"通道（一次性预分配线程局部大缓冲），不要放大公共栈帧；
- **运行时 decorator 派发**：比 JVM 的编译期模板展开多几个分支，量级 <10 ns，相对 ring 入队成本可忽略。若极端在意，可用 feature flag 把布局定死为编译期常量，代价是失去运行时编排能力；
- **数字均为量级估算**（来自同类系统的常见 profile 结果），非实测。`write` 到 /dev/null、管道、tty 的差异可达 5 倍以上，`{:#?}` 一个复杂结构体可能超过其他环节总和。

## 7. 验证计划（criterion 对照基准）

上线前在目标硬件实测三组指标：

| 基准 | 测量对象 | 达标参考 |
|---|---|---|
| 关闭成本 | 级别关闭时单次 `mirvm_log!([trace], ...)` | ≤ 2 ns |
| 同步写成本 | 路径 A 单次调用（stderr 重定向到 /dev/null 与管道各测一次） | ≤ 1.5 µs |
| 入队成本 | 路径 C 生产者单次调用 | ≤ 200 ns |

另需压测：ring 满时生产者可观测的抖动（应为零阻塞）、多线程整行不交错（输出无撕裂行）。

## 8. 演进路线

1. **v1**：路径 A/B 同步版 + decorator 配置（约 200 行），满足 error–debug 全部需求；
2. **v2**：trace 路径 C（ring + 消费者线程 + feature 闸门）；
3. **v3（按需）**：ftrace 式参数暂存、共享内存打捞、每 subsystem 独立级别（按 `module_path!` target 过滤）；
4. **重新评估 tracing 的时机**：当需求从"平铺日志"升级为"上下文关联"（如把日志归属到具体某次 syscall 分发、某个 vCPU 的执行片段），tracing 的 span 模型比自造结构化字段更划算——届时热路径闸门思想仍然适用。
