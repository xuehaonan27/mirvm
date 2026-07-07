# corpus 驱动补全（M2.5）

## 0. corpus 是什么

corpus = 一组**真实生态 crate 的最小驱动程序**，逐个在 tier-0 mirvm 上跑，用来**发现真实代码对抽象机 / VM 边界提出的要求**。

它不是"给 tier-0 刷通过率"——tier-0（rustc `InterpCx` + 协作调度）大概率整体推翻。corpus 的产物是**信号**，每个失败归类成两种之一：

- **设计信号（design-signal）**：暴露 RAM 语义、VM 边界、`os::` 处置、并发模型上的真实需求。→ M4 设计票据，必须记。
- **琐碎缺口（trivial-gap）**：只是某个 libc/intrinsic 直通还没接，M4 架构本来就覆盖。→ 低信号，可为"解锁探针"顺手补，但不投入打磨。

判据：*这个失败是在告诉我们抽象机/边界要处理什么，还是只是 tier-0 恰好没接一个 M4 已规划的通路？*

跑法：`bash tests/corpus.sh`（release 二进制；程序在 `corpus/c_*.rs`，frontmatter 脚本）。

## 1. 当前 corpus 与结果

| crate | 子系统 | 结果 | 备注 |
|---|---|---|---|
| itertools | 纯迭代器组合子 | ✅ | 无 OS 边界，如期通过 |
| anyhow | 错误链 / Context / `?` | ✅ | 默认不抓 backtrace，纯 |
| chrono | 日期算术（Naive，不调 now） | ✅ | 纯计算 |
| indexmap | 有序容器 + hashbrown | ✅ | `RandomState` 播种走 getrandom（已直通） |
| clap | 参数解析 + derive 宏 | ✅ | `parse_from` 显式给参数 |
| csv | CSV + serde 反序列化 | ✅ | 内存字符串，纯 |
| crossbeam | scoped 线程 + MPMC 通道 | ✅ | **协作调度器 + 模拟 futex 跑通** |
| rayon | work-stealing 数据并行 | ✅* | 补 `volatile_store` 后通过；**28s**（性能信号，见 §2.4） |
| tokio (current_thread) | async 运行时 | ✅ | **epoll/eventfd 经 dlsym 直通真内核**（见 §2.1、§3） |
| tokio (multi_thread) | 4 worker 运行时 | ✅ | 补 `pthread_setname_np`+math intrinsic 后通过；**不死锁**（证伪初判，见 §2.1） |
| blake3 | SIMD 密集哈希 | ❌ | **内联汇编 `__cpuid`**——设计信号，见 §2.2 |

计分：主 corpus 9 通过 / 1 失败（+ 2 个定向探针：tokio_mt 通过、blocking_io 故意挂死）。

补丁记录（解锁探针 / 通用琐碎缺口，非打磨）：
- `volatile_store` / `volatile_load` intrinsic（`intrinsics.rs`）：协作式单线程下等同普通读写。为拿 rayon 真实并发信号。
- `pthread_setname_np` shim（`shims.rs`）：良性线程元数据，存名字、VM 内部处置（`pthread_` denylist 太粗，见 §2.3）。
- float 数学 intrinsic 桥接（`powf64`/`sqrtf64`/… → 复用 `emulate_libm`，`intrinsics.rs`）：**普遍缺口**（任何数值代码都碰），一次性接掉一整类。与 native 逐位一致。

## 2. 逼出的设计票据

### 2.1 协作调度 vs 真阻塞 syscall（并发模型）

tokio 通过靠的是：未知外部函数（`epoll_create1`/`epoll_ctl`/`epoll_wait`/`eventfd`）不在 shim 白名单、不在 native denylist → **dlsym 直通真 libc/内核**（`src/interp/native.rs`）。而协作调度器把所有 guest 线程多路复用到**一条真解释器线程**上。

**初判（错）**：多线程运行时 worker 会各自阻塞在真 `epoll_wait` 上互等 → 死锁。**实测证伪**：多线程 tokio（4 worker + 定时器负载）正确跑通、不死锁。corpus 纠正了这个粗糙假设。原因：
- worker 空闲 park 走的是**模拟 futex**（`emulate_futex`，非真 syscall），协作调度器能正确阻塞/唤醒；
- 持 IO driver 的 worker 调 `epoll_wait` 时，定时器轮给它**有限超时**（sleep 1ms）→ 自释放 → 放行调度器。

**真实危险（钉死）**：guest 线程在**无超时**的真阻塞 syscall 上、且其释放**依赖另一 guest 线程推进**时才死。隔离验证（`corpus/c_blocking_io.rs`）：两个 std 线程，主线程在 socketpair 上 `read_exact`（真 `read()` 直通），另一线程负责写——主线程阻塞整条真线程 → 写线程永远跑不起来 → **挂死（timeout）**。

→ **票据**：dlsym 直通阻塞 syscall **仅当调用能自释放**（有限超时，或等待事件来自 guest 之外的真外部 IO/真定时器）才安全；**释放依赖另一 guest 线程推进时即死**。tokio 能活是因为它精心围绕超时 + 模拟 futex park 构建，**不是模型本身健全**。这是 M4 真 1:1 线程的核心必要性（账本 C8）——每个 guest 线程是真线程，阻塞 syscall 只挡自己，别人在别的核上跑。

### 2.2 内联汇编 / 虚拟 CPU 模型（新边界类别）

blake3 → `cpufeatures` → `core_arch::x86::__cpuid` 是内联汇编；解释器无法执行 asm（错误来自 rustc 内核，非 mirvm）。

MIR `TerminatorKind::InlineAsm` 结构（实测）：
```
asm!("add {0}, 5", inout(reg) copy _1 => _2, options(PURE | NOMEM | NOSTACK)) -> [return: bb1, unwind unreachable]
```
= { template 片段, operands（寄存器类 + in/out place）, options, targets, unwind }。处理选项：

- **A 模板拦截（tier-0 务实）**：按字符串匹配 `cpuid`/`rdtsc`/`xgetbv`/fence/`pause`，合成对 operand 的效果。覆盖真实 crate 95% 场景（特性检测/计时/栅栏），任意 asm 则败。
- **B 把 asm 块 JIT（M4）**：template + 寄存器绑定 → 汇编成真函数执行。完全通用，是 cg_clif/Cranelift 既有能力。M4 答案。
- **C 拒绝（现状）**。

→ **票据（虚拟 CPU 模型）**：`cpuid` 不是 OS 调用、是 CPU 指令——这是 `os::` 三分类（直通 / VM 内部 / 合成）之外的**第四类：虚拟 CPU**。附带决策"报什么特性位"：
  - 解释器跑不了 AVX512 SIMD（除非实现那些 intrinsic）→ **tier-0 报保守基线**（x86-64-v1 / SSE2），guest 走可移植路径，最小化 SIMD intrinsic 负担；
  - **M4-JIT 报宿主特性**，Cranelift 发真 SIMD，正确且快。
  - RAM 视角：CPU 特性检测是环境相关 / unspecified 可观察行为（类同 `now()`、线程调度）——差分对拍**不得比对**（见 ram-spec.md §2）。

### 2.3 dlsym 直通边界（`os::` 处置验证）

现状派发：**shim 白名单 → 未命中 → dlsym 直通（RTLD_DEFAULT）**，另有 **denylist**：`pthread_*`/`exec`/`setjmp`/`longjmp`/`__cxa_`/`fork`/`vfork`/`clone`/`exit`/`signal` 绝不直通（会绕过 VM 的线程/进程/控制流模型）。

→ corpus 验证了 P7 `os::` 的形状是可行的：**默认直通 + denylist（必须 VM 内部的）+ 合成（statx→ENOSYS 之类）**。denylist 的内容与理由正是"VM-internal 处置"的清单。M4 的 `os::` 模块把这套从"散落 + 隐式 dlsym 兜底"收敛成显式登记。

### 2.4 协作调度性能（tier-0 弃子实证）

rayon 用 28s：按语句时间片 round-robin 多路复用 work-stealing 池跑 10 万次并行迭代。正确性没问题，但坐实协作模型只能做对拍基底，**性能要真线程 + JIT**。非设计票据，是 tier-0 是弃子的又一实证。

## 3. 通过项验证了什么（架构确认）

- **async-stackless 成立**：tokio current_thread 运行时（定时器 + mpsc + spawn 任务）跑出正确结果（1530/15 msgs），无需引擎特殊支持——任务是编译器降解的无栈状态机，poll 驱动（见 async-stackless.md）。
- **协作 + 模拟 futex 是合法 SC 执行**：rayon / crossbeam 正确跑通，说明良好同步的并发程序在 round-robin 上产出合法的顺序一致执行——这是 tier-0 作对拍基底的价值。但它把一切串行化，**验证不了真并行**（弱内存、数据竞争、真并行推进）——正是 M4 必补。
- **真 OS 原语直通可用**：epoll/eventfd/getrandom/文件 IO 经 dlsym/shim 直达内核，无需模拟——印证"有真 OS 就直接用"。

## 4. 下一批候选（去撞更多边界）

- **真文件/目录 IO**（tempfile / walkdir）：文件 IO 直通深度。
- **num-bigint / smallvec / bytes**：纯计算 + 少量 unsafe，探布局/别名假设。
- **sha2（asm 特性关）/ aes**：更多内联汇编，验证 §2.2 模板拦截的覆盖面；也可试"tier-0 报保守基线 cpuid"后 blake3 是否走可移植路径通过。
- **网络**（若环境允许）：`std::net::TcpStream` 直连——socket/connect/send/recv 直通 + 阻塞语义（注意 §2.1 危险：无超时阻塞读若等 guest 内部推进会挂）。

已落地的定向探针（不入自动批，`c_blocking_io` 会故意挂）：
- `c_tokio_mt.rs`：多线程 tokio，证伪 §2.1 初判（不死锁）。
- `c_blocking_io.rs`：隔离 §2.1 真实危险（socketpair 阻塞读 → 挂死）。
