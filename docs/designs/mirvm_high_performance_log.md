# mirvm 日志、事件与性能剖析设计

> **状态：现行设计与施工蓝图。** §12.0 区分已实现的 1A 参考纵切与待施部分；
> §11 中 L1、P1 与 D0 已完成；L2–L4、P2 和数据裁决仍在施工队列，
> 未完成项不得写成已有能力。
>
> 2026-08-13 审计推翻了旧稿的几个前提：仓库当前没有 mirvm 自有日志
> backend，没有读取 `MIRVM_LOG`，也没有 `trace-log` Cargo feature；旧稿中的
> v1/v2、纳秒成本和 ring 容量均不是已实现或已实测事实。
>
> 本文记录已经确认的合同、实现事实和仍须由实测裁定的参数。§12.2 中的候选数字只有
> 真实数据能转成默认值；这不改变其余已批准部分的施工状态。

---

## 1. 这次设计真正要解决什么

旧稿把 `error`、`warn`、`info`、`debug`、`trace` 当成同一种数据的五个等级，
再按等级选择同步或异步传输。这一模型不够用。严重程度只说明一条诊断有多重要，
不能说明它是否处于 signal frame、是否会每秒发生百万次，也不能说明它应该被人读
还是被 profiler 统计。

本设计需要同时解决六类不同问题：

| 类型 | 白话含义 | 主要使用者 | 合适的基本形态 |
|---|---|---|---|
| 诊断日志 | 给人看的错误、告警和状态文字 | 用户、维护者 | 冷路径文本 |
| 结构化事件 | 固定事件号和固定字段，供程序提取 | 分析脚本 | 二进制定型记录 |
| 聚合指标 | 次数、累计时间、直方图 | 性能评测 | 每线程/每 Engine 先累计，再合并 |
| 采样 profile | 隔一段时间观察 CPU 正在哪里 | `perf`、profiler | 外部样本 + JIT/解释器映射 |
| 时间线 trace | 记录开始、结束和因果顺序 | 时间线工具 | 高频、有明确丢失语义的事件流 |
| 崩溃记录 | 环境已经受损时留下最后几个事实 | 崩溃恢复工具 | 独立的固定 emergency record |

本文把“结构化事件”和“时间线 trace”分开：前者可以很稀疏，后者通常高频且
需要开始/结束配对。二者以后可以共享编码和离线工具，但不能因此假定它们有相同的
容量、丢弃策略或性能预算。

**已确认的基础合同（用户 2026-08-13 裁定）：**诊断日志、结构化事件、聚合指标、
采样 profile、时间线 trace 和崩溃记录是六类不同的数据合同。它们可以共享 session、
进程、线程和 Engine 标识，共享 decoder，也可以在实测证明合适后共享部分落盘设施；
但不再用日志等级决定数据路径，不靠解析人类文本做 profile，也不因共享设施而默认
共享容量、背压或丢失语义。

## 2. 当前仓库事实

设计必须从现有代码出发，而不是从旧稿描述出发：

1. [`src/utils/logs.rs`](../../src/utils/logs.rs) 只有一个宏。`stdout`/`stderr`
   分支直接锁标准流并同步写；等级分支转发给进程全局的 `log` facade。仓库没有安装
   mirvm 自有 logger。
2. mirvm 是嵌入库时，进程全局 logger 可能属于宿主。其 `enabled` 和 `log` 实现
   可以取锁、分配、重入甚至回调 mirvm。内部热路径不能把它当作“一次原子读”；
   宿主 logger 只能是普通冷路径的可选输出端。
3. 当前真实热日志只有
   [`mirvm_syscall_dispatch`](../../src/os/linux/process.rs) 中的
   `MIRVM_SYSCALL_TRACE`：每次被改写的汇编 syscall 都重新查环境变量、格式化文本、锁
   `stderr` 并同步写出。`Builtin::HostSyscall`、自产 native archive 和 libc 内部 syscall
   并不经过它；它是真实的性能债务，却不是“全进程 syscall trace”。
4. `MIRVM_TIMING` 是 CLI 启动阶段的粗粒度相位账本；`MIRVM_JIT_STATS` 是一组
   进程级原子计数器。这两者能回答特定问题，但不能据此宣称已有统一遥测系统。
5. 多 Engine 嵌入已经是产品能力。任何需要归因的记录都至少要自动携带
   `engine_id`；只有 pid、tid 和源码行不足以区分多个 Engine。
6. 当前 fixed signal adapter 的安全基线是：只访问进程期稳定内存、原子和
   local-exec TLS，失败时 `_exit(70)`。新日志或 profiler 的内核 signal 入口不能
   放宽这一纪律。
7. `MIRVM_SEGV_DUMP` 当前会在真实 signal frame 中 `eprintln!`、读 `/proc`、分配
   并写文件。它是现存的开发排障债务，不是生产崩溃记录的范本。

因此，旧稿中的“自造 backend 已存在”“v1 已完成”“`MIRVM_LOG` 和
`trace-log` 已是配置面”均作废。

## 3. 不可能同时成立的保证

在有界内存里，如果写文件线程可以无限停顿，则下面三件事不能同时保证：

1. guest 执行线程永不等待；
2. 内存占用有固定上限；
3. 每条记录都不丢。

设计必须明确选择。把 ring 加大只是延后矛盾，不是解决矛盾。旧稿的“热路径永不
阻塞”和“error/warn 崩溃前不丢”没有给出可实现的共同语义。

本文使用以下精确说法，替代含糊的“crash-safe”：

| 状态 | 能保证什么 | 不能保证什么 |
|---|---|---|
| 内存已提交 | 完整记录已对消费者可见 | 进程死亡后仍存在 |
| 内核已接收 | `write` 已成功交给内核 | 已落到磁盘，或断电后存在 |
| 文件块已提交 | 离线工具能识别完整块 | 机器掉电后仍存在 |
| 存储已持久化 | 明确完成所需同步操作 | 免费、无阻塞 |

**已确认的前进性合同（用户 2026-08-13 裁定）：**只要 Engine 仍在执行或拆除，
诊断、事件、指标和 trace 都不得等待输出端。这里包括 guest、constructor、fini、延后
handler、TSD 回调和 Engine close。缓冲区满时可以丢弃遥测数据，但必须精确累计丢失数，
并在后续可用边界或 session 汇总中报告；结构化事件还应通过 sequence 暴露缺口。

真正的执行错误通过 `Result`、`RunOutcome`、退出码等产品接口交付，不能把“某行日志
最终写成功”当作错误语义的一部分。CLI 已回到控制边界、guest 不再运行时，可以同步
显示最终诊断；嵌入执行路径不能直接调用行为未知的宿主全局 logger。终止路径只做
best-effort emergency record，不承诺 flush 整条普通 ring。

## 4. 执行环境决定能做什么

这里的“环境”是代码当前所在的运行状态。调用者不应手工报名环境；runtime 必须在
机制内知道自己是否处于内核 signal frame、普通执行、关闭或 fork 子进程阶段，并
自动选择允许的最小路径。

| 环境 | 例子 | 允许 | 禁止 |
|---|---|---|---|
| 普通控制路径 | 初始化、配置、显式关闭、离线 flush | 分配、取锁、格式化；阻塞必须写进调用合同 | 假装它是热路径成本 |
| 普通热路径 | 解释/JIT 执行、syscall 拦截 | 固定字段、有限原子操作、有界返回 | I/O、任意 `Display`、全局竞争锁 |
| 可重入普通路径 | 延后执行的 guest/native signal handler、constructor | 无锁有界记录、递归抑制 | 调宿主 logger、持 Engine 锁后回调外部代码 |
| 受损同步路径 | panic/unwind、allocator 故障、fini、pthread 最终 TSD 收尾 | 固定事件号和整数等标量 | 分配、依赖 `Drop`、可 panic 的格式化 |
| 内核 signal frame | fixed adapter、未来采样 handler | 稳定预分配内存、原子、必要的 raw safe syscall | 普通 TLS 初始化、锁、分配、格式化、guest/JIT 调用、flush ring |
| fork 子进程未重建阶段 | 多线程进程 `fork()` 后、服务重建前 | 固定 emergency marker | 父进程的锁、ring producer、已消失的消费者 |
| 终止阶段 | SIGSEGV、SIGABRT、double panic、内部 abort | 最多一条固定 crash marker | join、fsync、遍历或 flush 普通 ring |

### 4.1 两种“signal handler”不是一回事

- **内核 signal frame**：内核直接跳入 fixed adapter。它可能打断同一线程正在进行
  的 ring 写入。这里绝不能格式化或 flush ring；即使 `write(2)` 本身可用，遍历
  ring、等待某个槽提交、读取消费者状态也不因此变得安全。
- **延后 handler**：fixed adapter 只登记事件，mirvm 稍后在安全点运行 guest 或
  image-native handler。它已回到普通栈，可以运行 VM，但仍可能嵌套、触发 Engine
  关闭，或出现在 pthread 最终 TSD 阶段，所以只能使用可重入的有界记录路径。

### 4.2 内核 signal frame 的唯一日志职责

内核入口最多写独立的 emergency record，内容只包含预先定义的事件号和当场可安全
取得的标量，例如 signal number、IP、tid、Engine/registration 标识。它不得：

- 调用 `mirvm_log!`、`log` facade、`eprintln!` 或任意 formatter；
- 扫描、排序或 flush 普通 ring；
- 读取会释放或移动的 Engine 数据；
- 初始化 Rust TLS；
- 覆盖嵌入宿主已经安装的 SIGSEGV/SIGABRT disposition。

普通文件中的已提交块由进程外工具恢复；handler 不负责“抢救所有日志”。

### 4.3 递归、panic 和 `errno`

- logger 被递归调用时，只增加 `recursive_suppressed` 或写固定 marker，不再次格式化。
- `format_args!` 不创建 `String`，但参数表达式和任意 `Display`/`Debug` 仍可能分配、
  取锁、递归或 panic。因此热事件只能接受固定类型字段；人类文本只能走普通冷路径。
- 任何 OS 邻接路径都必须保持调用者最终观察到的 `errno`。刚失败的 syscall 必须先快照
  返回值和 `errno`，再做记录；高频 producer 本身则禁止调用会修改 `errno` 的 libc helper，
  因而不为每条事件增加一对“保存再恢复”的读写。
- logger 自身不得把 panic 或错误传播进 guest。写入失败要成为明确计数/状态，而不是
  再通过同一日志通道递归报告。

## 5. 采集架构

本节同时包含已经逐项确认的不变量和仍待裁定的实现细节；小节标题会明确标注。未标
“已确认”的候选不能据此冒充现行实现。

### 5.1 调用面按数据含义分开

候选调用面如下：

- `diag`：普通冷路径的人类文本；
- `event`：固定 `event_id` 和定型字段；
- `counter`：每线程/每 Engine 的计数或累计值；
- `emergency`：特殊环境中的固定小记录。

这不是让调用者手工声明“我在 signal handler 中”。环境降级由 runtime 自动完成；
调用者只表达数据是什么。内核 adapter 等特殊入口根本不调用普通 `diag`。

mirvm 自己的开启闸门必须是原子或等价的固定成本状态，并位于参数求值之前。普通
诊断在安全环境中可以选择转发给宿主 logger；内部事件和 profile 不经过进程全局
`log` facade。

这里的“安全环境”不只表示“当前不在内核 signal frame”，还要求已经离开 Engine
执行/拆除边界。宿主 logger 可能等待、取锁或重入，因此 Engine 内产生的人类诊断先写
mirvm 自己的有界通道；CLI 或嵌入方明确进入控制边界后，才可以同步转发。日志等级不能
放宽这条规则：`error` 也不能让 guest 因满管道而停住。

### 5.2 普通热事件

热事件在**逻辑解码后**至少需要以下上下文；这不表示每个字段都物理重复写进每条记录，
不需要时间的数据合同也没有 timestamp：

```text
event_id, event_version,
monotonic_timestamp, pid, process_generation,
tid, thread_generation, per_thread_sequence,
engine_id, fixed_typed_payload
```

- `process_generation` 区分 fork 前后；`thread_generation` 防止 tid 复用；
- `per_thread_sequence` 在尝试写入前递增，离线工具可据此发现缺口；
- `engine_id` 由当前 activation 自动取得，无 Engine 的进程级事件使用保留值；物理编码按
  §5.9 从 page header/`EngineContext` 继承，不逐事件重复 8B；
- payload 只保存按值标量、稳定 ID 或当场有界复制的字节，不保存借用和任意 Rust
  对象地址。

跨线程没有天然全序。时间戳只能用于近似合并；真正的因果关系需要明确的
`correlation_id`，例如一次编译请求与发布事件共用同一 ID。

#### 5.2.1 当前优先讨论：时间戳热路径

2026-08-13 用户纠偏：在首个实现和文件都不存在时，不先讨论长期 schema 兼容。当前
设计主线改为逐条确定生产者热路径的真实指令和 cache 行为：

1. 事件时间需要严格指令顺序，还是线程 sequence 为权威、时间只作近似定位；
2. 使用裸 `RDTSC`、有序 TSC 读取，还是 vDSO `clock_gettime`；
3. recorder/write cursor 指针如何到达解释器、JIT 和 runtime helper；
4. 一条事件写多少字节、碰哪些 cache line、何时 commit；
5. 页满、ring 满、递归和 signal 打断时执行哪些指令；
6. producer 如何批量通知 writer，writer 如何避免抢 guest 的 CPU/cache/IO；
7. profile 与 timeline 插桩如何避免污染关闭态和默认 JIT 代码。

这里的“传一个指针”只能是传向预分配 recorder、当前页或保留槽的稳定指针。不能把
guest 内存、调用者栈、`fmt::Arguments` 或任意 payload 指针交给异步 writer：调用返回后
对象可能移动、释放或改变。热路径必须把固定标量复制进自己拥有的槽，writer 只接收
已提交页/块的指针。

#### 5.2.2 已确认：时间戳按语义分三档

不能为了统一而让所有事件支付同一份时钟成本，也不能把裸 `RDTSC` 产生的数值冒充
严格边界。当前裁决如下：

1. **聚合计数不读时间。** 每线程计数器只做本地累加，汇总时再记录时间；
2. **高频普通事件使用近似时间。** 在 TSC 快路资格成立时，生产者以编译器屏障固定源码
   中的取样位置，然后执行裸 `RDTSC`，保存原始 `u64`。硬件仍可让读取与邻近工作重叠，
   所以同线程的 sequence 才是顺序依据，时间只用于近似定位和跨线程合并；
3. **严格 timeline/span 边界使用有序读取。** 结束点优先使用 `RDTSCP`，使测量主体先于
   取样，并同时取得 `TSC_AUX`；开始点还需尾部 `LFENCE`，避免主体越过取样。若定义要求
   前序 store 已对其他核全局可见，还必须另付 store fence，不能把普通 `RDTSCP` 扩大解释
   成该保证。

生产者只写原始 TSC，不在每条事件上乘除换算纳秒。控制线程定期采集
`ordered_tsc -> CLOCK_MONOTONIC_RAW -> ordered_tsc` 的夹点样本，选择夹距最小的样本，记录
TSC 中点、单调时钟值和不确定度；离线工具用连续锚点分段换算。不得从标称 CPU MHz 猜
频率，不得为了看起来单调而偷偷钳平逆跳。发生漂移、逆跳或资格变化时，新建 clock epoch
或退回 vDSO 时钟；历史记录不重写。

TSC 快路由 runtime 自动准入，至少要求 CPU 提供不变 TSC、Linux 当前时钟源认可 TSC
（或提供同等级的内核证据），并通过启动期跨核同步检查。调用者没有“强制相信 TSC”的
开关。不满足时，需要时间的事件自动改用 vDSO
`clock_gettime(CLOCK_MONOTONIC_RAW)`；计数器仍然不读时间。当前 KVM/AMD 开发机使用
`kvm-clock`，且没有 `constant_tsc`/`nonstop_tsc` 证据，正属于必须退化的环境。

这条 fallback 只适用于**合同本身需要时间**、且所在调用边界能够安全执行普通 ABI 的事件；
它不要求每种结构化事件都附带时钟。§5.9 的首版 MIRVM-owned syscall pair 被明确归为
无时间的因果/结果流，不能为了统一字段而在任意 raw syscall 站点调用 vDSO。

`TSC_AUX` 不固定塞进每条记录：严格读取已得到它时，recorder 只在页头或 CPU/clock epoch
变化时记录；通过资格检查的裸 `RDTSC` 路径不为每条事件额外查询 CPU。当前机器上的一次
窄 probe 仅用于说明量级，不是性能承诺：裸 `RDTSC` 约 11.5ns，`LFENCE;RDTSC` 约
24.1ns，`RDTSCP` 约 24.9ns，`RDTSCP;LFENCE` 约 29.3ns，vDSO
`CLOCK_MONOTONIC_RAW` 约 32.8ns。后续必须在真实事件写槽路径中重新测量。

#### 5.2.3 已确认：默认代码零插桩，timeline 使用独立代码域

普通执行和 timeline 采集不能共享一份“每个事件点先检查 enabled”的机器码。默认 JIT
继续使用现有纯 guest fast ABI，代码内没有采集指令、TLS/enabled load、采集分支或隐藏
recorder 参数，也不为采集保留 `r15`。默认 CPU profile 也运行这份代码，由外部采样器
观察，不因 profile 自动换成插桩版。

显式开启 timeline 后，runtime 使用独立的 trace JIT 代码域：独立 Cranelift ISA/module、
发布槽、直接调用目标、展开信息和 JIT 地址映射。该域在 x86-64 将 `r15` 固定为当前宿主
线程稳定的 `ProducerHot*`；它指向 recorder 热状态，不直接指向会轮换的 page。trace
代码之间只经 trace 调用槽互调，不能无包装地跳入普通代码域。第一条 syscall 纵切直接从
`r15` 指向的 `ProducerFast` 取得 `cursor/pair_budget` 并内联写入，不做 TLS 查找或全局
session 检查；其他事件模板也不得借扩展名义把 session 轮询塞回逐事件路径。

所有进入 trace 域的边界必须 save/set/restore `r15`，异常展开也要由 landing pad 恢复后
继续展开，不能只照顾正常返回。native 可以按 ABI 临时使用 `r15`，所以 native 回调进入
guest 时绝不相信入口寄存器；它从当前线程 activation/TLS 取得正确 `ProducerHot*` 后重设。
普通 JIT 和 trace JIT 的对外 guest ABI 仍相同，不把 recorder 塞进 guest 可见参数。

解释器也分 plain/trace 两条循环，不在每个 opcode 检查 enabled。timeline 开启后，一个
已经进入 trace 域、但尚无 trace 机器码的函数先进入 trace interpreter，后台可以再编译
trace 版本；producer 不等待 JIT。

这里必须区分两类边界。现有解释器/JIT 块入口是 signal 等工作的**派送安全点**，但不是
可以替换当前机器帧的**代码域迁移点**。普通 JIT 在块边界仍有活的 SSA 值、寄存器和栈槽，
仓库没有 OSR/deopt map 把它们重建成 trace 帧；因此不能在该安全点把正在运行的 plain 帧
直接换成 trace 帧。方案 B 已确认的是“两套代码和关闭态零插桩”，不包含“动态请求后立即
迁移既有帧”。

**已确认的第一版生效边界**：代码域只在最外层 guest activation 入口选择一次，并在这条
完整调用链内保持不变。这里的最外层 activation 是宿主从 `run_main`、`run_export`、线程
入口或 native callback 等边界首次进入 guest；guest 调 native 后同步回调 guest仍属于同一
调用链并继承原代码域。`start()` 只把会话标为 armed；尚未进入 guest 的线程在下一次最外层
入口选择 trace，已经运行的 plain activation 不迁移。`stop()` 阻止新的 trace activation，
已有 trace activation 继续记录到完整返回。记录中分别保留请求时刻和每线程实际生效/结束
边界，工具不得把尚未覆盖的区间冒充已采集。

这使 plain JIT 继续保持零 session 轮询，也意味着长期不返回宿主的 activation 不能在运行中
动态进入 timeline。该能力已登记为明确重开项：一旦真实 workload 同时证明“长期不返回”
和“必须运行中动态开启/停止 timeline”，重开 OSR/可重建代码域迁移设计。不得以每块轮询
作为临时绕行，因为它既向默认代码收税，也不能自行重建已经存在的机器帧。外部 sampling
profile 仍可在不切代码域的情况下随时附着。

旧 trace 代码、recorder 和 page 在所有旧 trace activation 返回前不回收。最简单的第一版可
保持到进程结束，随后再以真实常驻内存证据决定是否增加 epoch 回收。独立 trace 域会增加
代码体积、编译 CPU，并因占用 `r15` 造成 spill；这些成本只由 timeline 会话承担，且必须用
真实负载与普通域对比。既有窄 fib 中 pinned register 比显式隐藏 ctx 参数快约 8% 只证明
形状可行，不证明相对当前零上下文普通域免费。

### 5.3 已确认：每 pthread 独占写页，按页交给 writer

普通结构化事件使用 per-pthread SPSC page ring。SPSC 是“一个生产者、一个消费者”：
当前宿主 pthread 是自己页环的唯一写者，后台 writer 是所有已发布页的唯一读者。不同
pthread 不在每条事件上争抢进程级 tail。

`ProducerHot` 是稳定的每线程 recorder 对象；`r15` 指向它的首地址。其第一条 cache line
命名为 `ProducerFast`，只放健康 syscall 路径确实会访问的三个机器字。producer 冷状态、
发布方向和归还方向分别占其他 cache line：

```text
ProducerFast, repr(C, align(64)), owner pthread only:
    +00 cursor: u64
    +08 pair_budget: u64
    +16 errno_ptr: u64
    +24..63 reserved and zero

producer-only cold/loss cache line:
    current_page, payload_end, next_sequence, dropped totals,
    active marker count, slow-path scratch and mode

producer -> writer cache line:
    published_tail, retired        // producer release，writer acquire

writer -> producer cache line:
    returned_head, free-page offer // writer release，producer acquire

page:
    header cache line              // first_sequence, context, capacity/class ...
    producer-owned payload ...
```

健康 Enter 只读写 `cursor/pair_budget`；raw Exit 只更新 `cursor`；libc Exit 也只在失败时经
同一行取得 `errno_ptr`。record control 是代码点的固定 immediate，Engine 归因从页头和
`EngineContext` 继承，不从 `ProducerFast` 加载。`end` 已由 `pair_budget` 取代；sequence、
drop、page、session、fork 和 stop 状态都不得搬回逐事件路径。保留的 40B 不能因为“还有空位”
塞入 writer 会读取或其他线程会写的状态。

未发布页完全归 producer 所有。writer 在 producer 活跃期间不读 fast/cold 两行、cursor、
计数器或 payload；只有明确完成 quiescent 所有权转交后，收尾代码才可读取最终冷账本。页写满
或被明确封存时，producer 先完成 header/payload，再以一次 release store 发布；writer acquire 后独占
读取，处理完再以 release store 归还。x86-64 的 acquire/release load/store通常是普通
`mov`，没有 `lock` 原子读改写；而且只在页边界发生。4/64 KiB page class 和 active-page
封存边界已经冻结；仍需由真实纵切决定的是进程硬预算、每 producer 自动页数、4/16/64 KiB
阈值以及页归还长尾。

页内不再使用逐记录 `FREE -> WRITING -> COMMITTED`。普通 recorder 是只复制固定标量、
不分配、不格式化、不调用回调也不 panic 的 leaf；producer 的 cursor 前移就是线程私有的
完成标志。进程崩溃时未发布的当前页归入已接受的“未知尾部”，不会为了抢救它让 writer
并发窥视。真正的内核 signal frame 可能打断任意一条记录，因此只能写独立 emergency 槽
或 sampler ring，绝不写普通页。

以 `r15` 已持有 `ProducerHot*`（其首行即 `ProducerFast`）、§5.9 已确认的 64B Enter +
24B Exit 为第一条真实形状，
一次正常返回 syscall 的页内增量为：

- Enter 用 flags-neutral 的 `jrcxz/lea` 从 producer-local `pair_budget` 扣一个 88B 容量额度；
- 从现有寄存器复制 syscall number 和六个参数，写 8 个 qword，再把 cursor 前移 64B；
- syscall 返回后写 3 个 qword Exit，并把 cursor 前移 24B；
- 不读时间，不做 atomic RMW、锁、额外 syscall，也不触碰多生产者共享 cache line。

`pair_budget` 只是本线程容量账，不是未完成记录。syscall 不返回时只浪费尚未使用的 24B
额度；writer 只看实际 cursor/used。记录起点保持 8B 对齐，但不承诺每条记录只触碰一条
cache line；真实写入行数按 offset/size 计量。

16B `EngineContext` 只能在完整 syscall pair 之间的 activation 冷边界写入。成功写 marker
后，producer 令 `cursor += 16`、增加本页 marker 数 `M`，再按真实剩余空间重算：

```text
pair_budget = floor((payload_end - cursor) / 88)
```

不能沿用开页时的 budget，也不能简单减一，因为 16B 会改变可容纳的完整 88B pair 数。若
16B 放不下，按 §5.9 先滚页，并由新页 header 直接表达新 Engine；若页和 marker 都无法建立，
该逻辑 context event 在 cold/loss 账本中消耗一个 dropped sequence，随后进入已经确认的
`context_unsynced`。

正常 syscall 不逐条更新 sequence。封页时，令 `used` 为 payload 实际字节数、`M` 为本页
成功写入的 16B marker 数，则冷路径必须验证并计算：

```text
used >= 16*M
(used - 16*M) % 88 == 0
P = (used - 16*M) / 88
record_count = 2*P + M
next_sequence = first_sequence + record_count
```

`P` 是完整 syscall pair 数，marker 也按已经确认的规则占一个 event sequence。校验失败表示
recorder 自己破坏了页，不得交给 writer 猜测。无页时丢弃的正常返回 pair 则只在冷账本中令
`next_sequence += 2, dropped += 2`；下一页 header 的 `first_sequence` 暴露缺口。这样成功
Enter/Exit 不为 sequence 或 drop 增加任何 load/store。

无空闲页时 producer 不等待、不 spin、不分配，也不覆盖 writer 尚未消费的页。它只推进
逻辑 sequence、增加线程本地 dropped。§12.1 的推荐 v0 是：只有下一次 Enter 做一次
bounded acquire 检查 free offer，成功便恢复，失败继续 drop；suppressed Exit 不检查。该
默认仍待批量裁决，只有真实 drop storm 证明 returned line 成为瓶颈后才考虑 backoff。
writer 不实时读取 producer 热 cache line 上的 drop counter。

进程 MPSC event ring 不作为普通热路径候选：每条事件至少需要共享 `fetch_add`/CAS，使
tail cache line 在生产者 CPU 之间转移；生产者预留后被抢占还会留下 writer 不能越过的洞。
若将来需要全局“已发布页”通知队列，它最多在**页边界**接收 page pointer，其共享原子和
唤醒成本必须按整页摊销，不能退化成逐事件 MPSC。

producer descriptor/recorder 的生存期至少覆盖线程最后退出、writer drain 和 session
quiescence；stop 时不能因 TLS 仍持地址就立即释放。descriptor 第一版可保持进程期稳定，
但 page 不能因此永久泄漏：只有 producer 已收口、page 已不再 active/published/in-flight，且
writer 已 release 归还后，才可回到同一数据合同的进程页池供后来 producer 使用。fork 子进程
先把继承的 producer 指针置空/隔离，回到普通边界后建立自己的 generation、writer 和页，
不能继续发布父进程页。

profile、timeline trace 和诊断仍需独立容量配额，不能让 profile 洪水挤掉诊断。容量按
字节、线程数、事件率以及“允许积压多少时间”计算，不按拍脑袋的槽数决定。

#### 5.3.1 已确认：热 timeline 不周期发布 active page

active page 没有逐事件时间 deadline、每 K 条 watermark 或 writer flush-request 检查。它只在
以下自然静止边界封存并 release 发布：

1. 页已满；
2. 当前 pthread 的最外层 trace activation 完整返回；
3. 当前 pthread 正常退出；
4. capture 正常 stop，且该 producer 已经离开 trace activation；
5. 显式冷控制路径需要提交它自己产生的记录。

nested activation、guest→native→guest 回调和 deferred handler 都不单独封页；只要仍在同一
最外层 trace 调用链中，它们继续顺序写同一个 pthread producer。这样健康的逐事件路径不加
countdown decrement/test、deadline load/compare、共享 flush generation load，也不让 writer
借 watermark 读取 active page。

低事件率的半页可能直到 activation 返回才对 writer 可见。这是已选择的可见延迟，不是正常
路径丢失；若进程此前崩溃，该页仍属于已接受的未知尾部。低频诊断、emergency crash record
和 sampling profile 有各自通道，不以“诊断应及时”为由向 hot timeline 征收周期发布税。

若真实 workload 证明长期 activation 中查看 timeline 有明确的最大可见延迟要求，重开本项，
用真实事件率比较每 K 条 watermark 与 per-event deadline；在此之前不得以低延迟开关暗中
增加每事件指令。这个重开触发器独立于“长期 activation 中动态启停 timeline”的 OSR 需求：
前者只讨论已经在 trace 域里的 active page 何时可见，后者讨论 plain 帧如何迁入 trace 域。

### 5.4 已确认：writer 睡眠，整页发布时才按需唤醒

未开启采集时不创建 writer。开启采集后，writer 排空当前可见的已发布页；没有工作时不
持续轮询，也不要求 producer 在每条事件上检查唤醒状态。唤醒只发生在整页发布这个冷边界，
使用一个与 producer 热 cache line 分开的进程级 `WriterState`：

```text
writer:
    drain_all_published_pages()
    WriterState.swap(SLEEPING, AcqRel)
    rescan_all_published_tail(Acquire)
    if page_is_visible:
        WriterState.store(AWAKE, Release)
        continue
    futex_wait(WriterState, expected = SLEEPING)

producer, after publishing one sealed page:
    published_tail.store(next, Release)
    old = WriterState.swap(AWAKE, AcqRel)
    if old == SLEEPING:
        futex_wake(WriterState, 1)
```

writer 先声明睡眠再复查所有 `published_tail`，用于封住“最后一次扫描”和真正睡下之间的
竞态。双方都必须对同一个状态字执行读改写，而不能把它弱化成“普通 load 看见
`SLEEPING` 才 CAS”：在 producer 的页发布尚留在 store buffer、writer 的睡眠 store 也尚未
被对方看见时，两边可能都读到旧值，最后留下“有页但 writer 已睡”的状态。用 `swap` 后，
若 producer 的交换先发生，writer 的 acquire 交换会接住该发布并在复查中看到页；若 writer
的交换先发生，producer 会读到 `SLEEPING`、改回 `AWAKE` 并 wake。若发布正好发生在复查后、
`futex_wait` 前，futex 对 expected value 的检查还会让 wait 立即返回。

多个 producer 同时发布时，只有把状态从 `SLEEPING` 改成 `AWAKE` 的第一个 producer 执行
一次 wake；其余交换得到 `AWAKE`。writer 可用一个很长的超时作为故障兜底，但超时不是正常
调度机制。

因此健康页内逐事件路径仍然不读 `WriterState`，没有 futex、syscall 或共享状态；每个封页
边界明确支付一次 locked swap，真正从睡眠转为工作时最多一次 `futex_wake`。按 §5.8 已确认
的 64 KiB hot logical page（含 64B header）和 §5.9 的 88B 正常 syscall pair，就是每 744 次
正常返回 syscall 一次共享读改写；
实际摊销仍须按最终记录宽度重算。writer 首版扫描进程期稳定的 producer 列表。只有真实的“大量休眠
线程导致扫描成本成为瓶颈”证据出现，才考虑在页边界增加全局 ready-page MPSC 队列；不能
先把共享队列成本放回逐事件路径。

本项只确认睡眠与唤醒协议；具体 I/O 形状见后续裁决。

### 5.5 已确认：每个进程代际使用独立 capture writer

普通 capture 关闭时没有 writer，也不创建日志文件。capture 开启后，每个 process generation
恰好创建一个 `mirvm-capture` writer；同一进程中的所有 Engine 共用它，但不同数据合同仍有
各自容量和丢失账本。writer 不与 cache、JIT、惰性解码或 Engine close worker 共用线程。

这不是从现有系统拆出一个线程。当前 L2 cache、deps image 和 package heat-order 仍在各自
控制/收尾路径同步序列化及写盘；所谓 cache write-behind 只是旧设计。JIT worker 每 Engine
一条，Engine close 会停止它；惰性解码 worker 每个 lazy package 一条，guest demand 可能等
它。若让其中任何一条同时执行 capture I/O，一次 cache 序列化、文件系统分配、dirty
throttling 或阻塞 write 就会阻止 trace 页归还；复用解码 worker 还会让 guest 直接等日志。
优先级队列不能抢占已经进入的序列化或内核写调用，因此不能解决这个队头阻塞。

独立 writer 的职责保持窄：排空 sealed pages、组成可恢复 chunk、计算 checksum、写入自己的
文件并归还页。它不在线格式化、符号化、跨线程时间排序或压缩。使用普通 `SCHED_OTHER`，
默认不绑核、不降低 nice；压低它的调度优先级可能反而延迟归还页并增加 guest 丢失。writer
空闲时按 §5.4 睡眠，所以“独立线程”不等于忙等占一个 CPU。

单个 Engine close 不停止进程级 writer，因为 fini、signal 和 TSD 收尾仍可能产生记录。
capture stop 先阻止新的 trace root，等已有 trace activation 到最外层返回并封页，再让 writer
排空、写正常 `End`、关闭文件并 join。若 sink 出现永久 I/O 错误，writer 进入“消费并丢弃”
状态：继续归还 sealed pages 并累计 sink loss，在普通控制边界报告；它不能停在坏 syscall 后
任由所有 producer 页环耗尽。

fork 是实现前置而非用户限制。当前 fork 守卫以 `/proc/self/task` 的 OS 线程总数作基线；若
运行中开启 capture 并创建 writer，它会被误认为 guest 新建 pthread。实现必须由 MIRVM 的
进程级服务生命周期自动登记自身线程，让 fork 守卫区分运行时服务线程和 guest/宿主并发线程；
不得要求调用者避开 fork 或手调基线。child 中继承的 writer 已消失，父代 producer 和 fd 必须
先失效；回到普通可重建边界后自动建立新的 process generation、文件和 writer。

具体 I/O 基线见下一节。

### 5.6 已确认：sealed pages 直接用 buffered `pwritev` 写入

第一版不把 payload 复制进 writer-owned staging，不使用 `io_uring` 或文件 `mmap`，也不
在线压缩。每个 process generation 以 `O_CREAT | O_EXCL | O_WRONLY | O_CLOEXEC` 打开独占
文件；单 writer 自己维护明确文件 offset，不依赖共享 open-file offset 或 `O_APPEND`。

writer 只机会式收集此刻已经可见的有限数量 sealed pages，不等待凑满一批。它复用预分配的
`iovec[]`、chunk header 和 footer，不在每批分配：

```text
chunk header | page 0 有效区 | ... | page N 有效区 | commit footer/checksum
```

checksum 在 writer 线程中对文件实际保存的 page 字节增量计算，footer 最后。随后以 buffered
`pwritev` 写入。必须处理 `EINTR` 和正数短写：推进显式 offset、裁掉已经完成的 iovec 前缀，
继续写剩余部分；批量大小同时受内部 chunk 上限和运行时 `IOV_MAX` 约束。只有完整 footer 已
被内核接受，chunk 才计入 `written`。进程在此之前退出，恢复工具按 §7.3 丢弃不完整尾块。

producer page 的所有权不能在 syscall 前归还。只有该页所引用的全部字节已经被内核接受后，
writer 才 release 推进相应 producer 的连续 `returned_head`；短写已经完整越过的前缀页可以
先归还，部分写入的当前页继续保留。buffered `pwritev` 成功只表示数据已复制进内核/文件系统，
不表示已到介质；这正符合“不逐 chunk `fsync`、不承诺断电数据”的既定合同。

直接路径的主要内存流量是：producer 写 page；writer 读 page 算 checksum；内核
`copy_from_user` 再读 page 并写 page cache。checksum 刚读过的 cache line通常仍热。若一批
有效字节为 `B`，其形状近似：

```text
B / checksum_throughput + pwritev_fixed_cost + B / kernel_copy_throughput
```

staging 会额外读 producer page、写 staging，再让内核读 staging。即使 copy 与 checksum
融合，仍多一遍完整用户态写流量和 cache 污染。它的真实收益只是更早归还 producer page、
用全局额外缓冲遮住有限 I/O 长尾；单个 writer 一旦阻塞在 write 中也不能同时填 staging。
若后续实测 drop 与 write 长尾强相关，必须在相同总内存下比较“更多 per-thread pages”和
“较少 pages + staging”，不能只展示 staging 后页归还变快。

`io_uring` 不消除 buffered write 的 page-cache copy 或 dirty throttling；直接提交 producer
page 还要求它保持到 CQE，并引入 in-flight、取消、stop、fork、registered buffer/
`RLIMIT_MEMLOCK` 和部署回退状态。`SQPOLL` 还会常驻占 CPU。只有真实存储仍有并行带宽、
单 outstanding `pwritev` 明显吃不满设备并导致 drop 时，才重开对比。

文件 `mmap` 可用一次用户态 copy 直接写 page cache，但需预扩文件/映射窗口，错误可能表现为
`SIGBUS`，writeback 失败更晚，fork 还会继承可写映射。只有 writer 的 syscall/复制成本而非
存储吞吐成为 guest 扰动主因时才对拍。在线压缩同理：它自然引入输出 staging并消耗 LLC；
只有原始长期字节率逼近 sink 吞吐或文件预算时才比较，否则留给离线工具。

任何方案都必须满足长期输入率 `R` 小于实际 sink 长期吞吐 `D`。若 `R >= D`，额外 ring、
staging 或异步提交只能推迟丢失，不能消除它。

### 5.7 已确认：进程硬页池，per-pthread ring 懒建并自动伸缩

不能给每个 pthread 固定相同的 `N` 张页。冷线程会浪费内存，热线程却仍可能在很短的 writer
停顿中耗尽容量。例如固定每线程两张 64 KiB 页，10,000 条已进入 trace 的 pthread 仅底座就
约占 1.22 GiB；而对单线程每秒百万条 64B 事件，这些容量只能覆盖约数毫秒。

每一种数据合同使用自己的进程级硬页池，不能跨合同借光容量。pthread 只在首次进入 trace
root 的冷边界自动创建 `ProducerHot` 和私有 SPSC ring，不在 capture start 遍历报名，也不在
第一条事件中分配。初始至少有 active page 与一张可发布/归还页；额外页由 writer 根据该
producer 的发布率和实际 page-return gap 自动放入该线程的 free-page 队列。全局池的分配和
回收只在控制路径/writer 上发生；producer 在页满慢路径只从自己的 SPSC free queue 取稳定
page pointer，不取全局锁、不分配。

池不足时，producer 立即进入 drop 路径并继续推进 sequence/本地 loss 账本。页被归还或预算
重新可用后，writer 自动补入 free queue，producer 在后续页边界恢复；不需要调用者干预。
分配不能采用“先抢到者占满”的策略，而应让活跃 producer 在同一预算下覆盖尽量一致的 writer
停顿窗口。退出 producer 在现有 pthread 最终 TSD/signal 收口后封存 active page并标记 retired；
writer 排空所有在途页后把页归池。descriptor 可继续稳定，但短命线程不能永久消耗页容量。

容量按字节率和归还长尾计算。设：

```text
P      = page 总分配字节
U      = 扣除页头、对齐和最大尾碎片后的保证可写字节
q_i    = 一批 pwritev 同时扣住 producer i 的最大页数
A_i(L) = producer i 在 L 时间内可能编码的最大字节
N_i    = producer i 需要的页数
```

从 active page 可能已接近写满的最坏相位出发：

```text
N_i >= q_i + 1 + ceil(A_i(L) / U)
```

若事件近似为固定速率：

```text
A_i(L) = burst_i + events_per_sec_i * bytes_per_event_i * L
```

总页内存为 `M_pages = sum(N_i * P)`。给定硬预算后，运行时按真实 producer 速率求所有活跃
producer 共同可覆盖的最大 page-return gap `L*`；高率线程自然取得更多页，低率线程只持底座。
长期仍须满足所有 producer 的总输入率小于 sink 持续吞吐；恢复一次长度为 `L` 的停顿还需要
额外追赶时间，不能只用平均 write 延迟估算容量。

绝对进程预算、最小/最大页数和 `q_i` 尚不冻结。它们必须由第一条真实 MIRVM-owned syscall
event 的事件率、page-return-gap 分布、线程数和同总内存 drop 曲线决定。外部 memory cap
只改变可吸收窗口和丢失量，不是运行正确性或线程报名开关。

### 5.8 已确认：4 KiB starter 自动晋升 64 KiB hot

这里的 logical page 大小是**包含页头的总长**。新 producer 从两张 4 KiB starter 开始；每页
第一个 64B cache line 是 header，可写 payload 为 4032B。writer 根据真实编码字节率、
page-return gap 和页池预算，自动向该 producer 的 free queue 补充 64 KiB hot pages；hot 页
payload 为 65472B。producer 只在页满慢路径接过另一等级的 page pointer，逐事件路径仍只看
`cursor/pair_budget`，不增加页级别分支或用户旋钮。

以已确认的正常返回 syscall pair（Enter 64B + Exit 24B = 88B）计算；不返回 syscall 消耗
一个容量额度但只写 Enter，因此实际 used bytes 更少：

| logical page 总长 | 满页 pair 数 | 页尾未分配字节 | 承载百万 pair 所需页数 |
| ---: | ---: | ---: | ---: |
| 4 KiB | 45 | 72 | 22,223 |
| 16 KiB | 185 | 40 | 5,406 |
| 64 KiB | 744 | 0 | 1,345 |
| 2 MiB | 23,830 | 48 | 42 |

starter 因 `FULL` 封页不等于一定晋升。一个极低频 producer 运行数小时也会填满 4 KiB；若
只看“填满过”就会把冷线程错误变成大页。writer 先用自己观察到的 sealed bytes、到达间隔、
队深、drop delta 以及真实 return gap 估算速率，再在硬预算内决定是否补 hot page；不能为了
估速让 raw 页满路径读取时钟。`ROOT_RETURN` 封存的半页
明确不触发晋升。producer 静止、在途页全部归还且池有压力时，冷路径自动收回 hot pages；
调用者不参与升级或降级。

64 KiB 可能超过常见 L1D，但 payload 是一次顺序写，不要求整页同时驻留；常驻热状态仍在
独立的 `ProducerHot` cache line。相较 4 KiB，hot page 把容量分支 taken、全局 locked swap、
发布/归还、页头处理和 writer iovec 数降到约 1/16.5。短 root activation 返回时无论页多大都
会发布一次半页，因此大页没有摊销优势，这也是必须从小 starter 开始的原因。

16 KiB 不作为首版固定第三等级，但必须在相同进程总页字节下挑战 4K/64K 两级方案；若真实
producer 速率集中在中档并证明它改善 drop/guest cycles，才加入第三等级。2 MiB 永不作为
logical publish page：它的最低内存、填满可见延迟和异常未知尾部都过大。若 DTLB 实测成为
瓶颈，只比较“2 MiB backing slab 内切分 64 KiB logical pages”，不能扩大发布粒度。

logical page 只要求 64B 对齐；普通匿名 `mmap` 的 4 KiB 对齐已经满足，不要求 64 KiB 对齐
或 hugepage。页池由少量匿名 mapping slab 切分，禁止每页/每 pthread 一次 `mmap`。header
占第一条 cache line，payload 从下一条开始；record 至少 8B 对齐且不得跨页，sealed page整体
作为一个 iovec。Linux `IOV_MAX=1024` 的常见环境中，扣除 chunk header/footer 后一批最多
1022 张页；约 1 MiB page bytes 分别需要 256 张 4K 页或 16 张 64K 页。

starter 由 owner pthread 在首次 trace root 冷边界 first-touch，优先取得本地 NUMA 内存。
hot page 是由 owner 慢路径预触还是 writer 预触，必须实测一次性 guest minor-fault 停顿与长期
远端 NUMA 访问后再定；不得把 mmap/page fault 放进普通逐事件路径。

### 5.9 已确认：内部 syscall 事件与全进程 syscall 流分开

第一条 SPSC 纵切只记录 **MIRVM-owned syscall event**，即由 MIRVM 自己生成、解释或能安全
双物化的 syscall 点。首批范围包括 `Builtin::HostSyscall`，以及没有独立持久状态、可以分别生成
plain/trace 版本的 inline-asm wrapper。plain 版本保留原 syscall 指令和原调用语义，不读取
recorder 或 TLS；trace 版本在同一站点内联写固定整数记录，再执行完全相同的 syscall。

每次正常返回的内部 syscall 产生两条各自完整的记录：调用前提交 `SyscallEnter`，返回后另写
`SyscallExit`。不得先预留一条记录、跨 syscall 保持未完成，再回头补返回值；syscall 可能长期
阻塞，`exec`/`exit` 可能永不返回，`fork` 又会切开 process generation。未返回的 syscall 只有
Enter 是合法结果。任一侧出现 sequence 缺口时，离线工具必须把该 span 标为不完整，不得猜测
返回值或耗时。Enter-only 只保留为测量 Exit 增量成本的 benchmark 对照，不是正式采集模式。

Enter/Exit 不携带单独的 `call_id`。每个 `(process generation, thread generation)` 的 decoder
按 event sequence 维护“已经进入、尚未返回”的 syscall 栈；同线程发生嵌套时，Exit 只关闭
最近的 Enter。显式 `call_id` 不仅占用两条记录的字段，raw 路径还必须跨 syscall 保存它，而
任意 raw 站点没有可白拿的寄存器。遇到 sequence 缺口、fork generation 切换、session 结尾
或异常未知尾部时，decoder 立即截断当前配对栈；未闭合 Enter 和孤立 Exit 只标
`incomplete`，不得按 syscall 编号、参数或邻近时间猜配。

这个离线栈是防御性解码结构，不要求 producer 为嵌套付热税。首版合法普通页不存在
`Enter -> Enter -> Exit -> Exit`：`HostSyscall` 从 Enter 到同步 syscall 返回之间没有 safe
point 或 callback；fixed signal adapter 在真实内核 frame 中只登记事件，受管 guest/image
handler 必须等当前 syscall Exit 写完后的普通 safe point 才执行。因此 `ProducerHot` 不增加
`open_depth` 或 `tainted_page`。若 decoder 在首版流中看见第二个 Enter，它先把前一个标为
incomplete，并报告采集合同违例；保留栈只为将来确有新可重入事件源时无需改文件语义。

页内采用 syscall 专用的两种定长记录，而不是把所有事件填成同一宽度：

```text
SyscallEnter  64B = control:u64 + nr:i64 + args[6]:u64
SyscallExit   24B = control:u64 + result:i64 + status:u64
```

`control` 自描述 kind、length 和 raw/libc 语义；`status` 打包 errno、有效位和语义位。raw
Exit 保存原始 `RAX` 并把 errno 标为无效；libc Exit 先快照返回值和 errno，成功返回后的旧
errno 不得被误解成这次 syscall 的错误。event sequence 不逐条占 8B，而由页头
`first_sequence` 和 decoder 扫描到的记录次序推导。

raw syscall 必须保持 guest RFLAGS，不能用普通 `cmp` 做容量检查。冷路径按当前可用空间设置
`pair_budget = floor(remaining_bytes / 88)`。Enter 只用不改 flags 的 `mov/jrcxz/lea` 检查并
扣减一个额度，然后提交完整 64B；Exit 已由 Enter 保证有 24B 空间，返回后直接提交，不再
比较容量。这个额度不创建 Exit、不推进 Exit 的 cursor，也不让 writer 看见半条记录；若
syscall 不返回，额度自然留空。writer 的 `pwritev` 只写 64B page header 加实际
`used_bytes`，页尾、额度空洞和回收页旧内容永不落盘。

Enter 在滚页后仍无法提交时，本次调用立即锁定为 **unrecorded-pair**，也就是“这一对记录
整体不再抢救”。producer 先为实际发生但未写出的 Enter 推进一次 event sequence 和 drop
计数，再执行完全相同的 syscall。若它在同一 process generation 正常返回，再为被抑制的
Exit 推进一次 sequence/drop，然后直接返回；返回点不重新检查 free page、不封页、不唤醒
writer，也不写无法解释的孤立 Exit。采集只允许从下一次 Enter 的普通页边界恢复。即使阻塞
期间 writer 已归还页，外层 Exit 仍按原决策抑制；下一条成功记录暴露的 sequence 缺口会让
decoder 清空配对状态。内核 signal frame 不得借这个机会写普通页，受管延后 handler 则要等
Exit 路径结束后才能运行。

成功的 `exec`、`exit`、`exit_group` 或其他不返回路径没有 Exit，因此 Enter 失败时只计一次
drop，不能在 syscall 前预记两次。失败的 exec 和其他正常错误返回仍计第二次 suppressed
Exit。fork 父进程按正常返回处理；child 在返回任何 trace 代码前先废弃父 generation 的
producer/page/sequence，不得向复制来的父账本写 Exit 或增加 Exit drop，随后由 child
generation marker 重建。该规则同时适用于泛型 `HostSyscall(SYS_fork)`，不能只覆盖专门的
`HostFork`。raw 代码生成成功/失败两条尾路径，不跨 syscall 保存布尔状态；drop 计数和跳转
也必须保持 guest RFLAGS。

专门的 `HostFork` 围绕 `libc::fork()`，宿主 `pthread_atfork` callback 可能同步重入；它只走
fork generation 生命周期协议，不冒充首版“直接 syscall pair”。泛型
`HostSyscall(SYS_fork)` 才按本节的直接调用记录，并在 child 返回 trace 代码前完成切代。
未知 external native signal handler 自己发出的 syscall 只属于内核 raw-syscall stream；首版
trace thunk 地址不逃逸给这类 handler，不能用 `open_depth` 掩盖真实 signal frame 对普通页的
重入。raw `SYS_rt_sigreturn` 本身位于 signal frame 且非局部返回，必须 plain bypass，绝不写
普通 Enter。

相对统一 64B Exit，88B pair 把每次正常 syscall 的 qword stores 从 16 降到 11，并把 producer
写入、checksum、内核复制和文件字节从 128B 降低 31.25%。32B Exit 的 96B pair 只作为同预算
benchmark 挑战项：只有实测证明更整齐的对齐显著降低 guest cycles，才用持续多写的 8B 换它。

`engine_id` 不塞进每条 syscall 记录。每页 header 快照第一条记录处的实际 Engine；`0` 表示
当前不属于任何 Engine。同一 pthread 的实际 Engine 改变时，activation 冷边界写一条
16B `EngineContext { control, engine_id }`，后续记录继承它；A→B→A 依次写 B、A，同一
Engine 的递归 activation 不写。若 active page 剩余空间不足，先滚页，新页 header 直接写
当前实际 Engine，不再重复 marker。marker/control record 也占一个 event sequence。

若 marker 写不下且拿不到新页，producer 进入 `context_unsynced`：实际 event sequence 和
context-loss 计数继续前进，但所有 payload 都 drop，`pair_budget` 保持不可用；直到新页 header
或 marker 按 runtime 的**实际当前 Engine**成功重建后才恢复。不能因 cached encoded ID
碰巧又等于外层 Engine 就跳过恢复，也绝不能让 B 的事件沿用 A 的 ID。页 rollover 读取实际
当前 Engine，fork child 则丢弃父页和 encoded state，在新 generation 重建。decoder 遇任一
sequence 缺口时同时清空 syscall 配对栈和页内 Engine 归因，只有下一页 header 才能重新建立
可信上下文。

首版内部 pair 也不携带逐事件时间戳；其权威信息是 sequence、syscall 参数、返回结果以及
raw/libc errno 语义。严格 syscall span 若使用 TSC，开始和结束都必须有序读取；当前窄 probe
仅两次取时就约需 108–117 cycles，还没有计入状态保存和写记录。TSC 不合格时，vDSO 又是
普通 SysV 调用：把它插进任意 raw syscall 站点会触碰 guest 栈，并要求保存 caller-saved GPR
和完整向量状态，重新引入旧 trampoline 的核心成本与正确性问题。因此 syscall 时间只来自
独立 Linux raw-syscall stream；仅当两边按 tid、顺序和 syscall 身份无歧义，且两份流都没有
相关 loss 时才可离线关联。内核流不可用、丢失或关联不唯一时，工具明确报告 duration
unavailable，不推测时间。若以后真实问题要求带 Engine 归因的 guest-observed latency，另开
严格 timeline 合同，不回头污染这条结构化事件热路。

raw x86-64 `syscall` 失败时返回 `-errno`；libc `syscall()` 则返回 `-1` 并设置线程 `errno`。
记录功能不得把二者互换，也不得用普通 ABI trampoline 改变栈、red zone、向量寄存器或 flags。
raw `SyscallExit` 保存原始 `RAX`；libc 语义的 Exit 必须在任何记录操作之前取得返回值和
`errno`，并保证记录后 guest 仍观察到相同 `errno`。

每个 pthread 只在创建本线程 producer 的冷边界调用一次 `__errno_location()`，把返回的
`errno_ptr` 缓存在仅由该 pthread 读取的 producer cache line 中。未开启 capture 时不创建
producer，也不为此调用；热路不得每次重新调用 `__errno_location()`，更不得硬编码 glibc 的
`FS:` 私有偏移。`HostSyscall` 返回后先保留 result：只有 `result == -1` 时才从缓存指针读取
一次 errno，并把 `errno_valid=1` 和该值写入 Exit；成功时不读取 errno，统一写
`errno_valid=0, errno=0`。成功后线程里残留的旧 errno 仍保持原值，但不冒充本次 syscall 的
结果。raw Exit 从不读取或写入 libc errno。

producer 的所有路径必须 **errno-transparent**，也就是完全不修改当前 pthread 的 errno：
逐事件写入、页满、drop、封页和唤醒只允许普通内存操作、原子操作以及返回原始负错误码的
raw futex leaf；禁止经 `libc::syscall`、logger、allocator 或其他可能设置 errno 的 helper。
因此热路没有 errno restore store；若以后某个 recorder 慢路必须调用会改 errno 的函数，说明
它违反这条热路合同，不能靠给所有 syscall 补保存/恢复来掩盖。writer 只使用自己的 pthread
errno，且永不解引用 producer 的 `errno_ptr`。fork child 会随父 generation 一起废弃旧
producer 和缓存指针；在新的普通冷边界重建 producer 时重新取得本线程指针。pthread 退出后
descriptor 和 writer 也不得再访问该指针。
当前旧路径只作为债务基线：本机 release 窄基准中，裸 syscall 约 332 cycles，理想
`slot 间接 call -> syscall; ret leaf` 约 384 cycles，旧 TRACE-off trampoline 约 588 cycles，
TRACE-on 即使 `stderr=/dev/null` 也约 16,828 cycles；旧 trampoline 还只保存 XMM 低 128 位、
会写调用者栈，不能作为新 recorder 的正确性或性能底座。

“全进程 syscall 流”是另一份数据。自产 stateful native/global-asm image、opaque archive、
libc 内部和其他库真正进入内核的 syscall，不能靠 MIRVM 掌握的少数代码点完整覆盖。完整流
由 Linux `perf_event_open` 的 raw-syscall tracepoint（内核在 syscall 入口/出口提供的事件点）
采集，写入独立的 raw stream；平台或权限不支持时必须明确报告“完整流不可用”，不能把局部
内部事件改名冒充完整结果。内部 SPSC 与内核流各自有容量、sequence 和 loss 账本，离线工具
按线程、时钟锚点和关联字段合并，不共用一个满载即互相挤掉数据的 ring。

## 6. 生命周期与嵌入边界

### 6.1 采集会话及其激活

**采集会话**是一次有明确起止边界的进程级遥测采集。它统一保存构建身份、采集配置、
进程与 fork 代际、各 Engine 身份，以及本次产生的事件、指标和 profile 产物。它不等于
某个 Engine 的生命周期，也不要求调用者逐 Engine 或逐线程登记。

**已确认的激活合同（用户 2026-08-13 裁定）：**

- 普通产品错误始终通过 `Result`、`RunOutcome`、退出码和 CLI 最终诊断交付，不依赖
  采集会话；
- 进程启动时自动准备一个极小、固定容量的内存 emergency 槽；未开启采集时不因此创建
  文件或后台线程；
- 诊断文件、结构化事件、指标、trace 和 profile 默认关闭，只有明确请求时才创建会话；
- CLI 由 `profile`/`capture` 类命令自动创建目录和传递配置；嵌入方使用进程级 API，
  环境变量只保留为排障兼容入口，不是唯一正式接口；
- 会话开启后，已有和后来创建的 Engine、当前和后来接入的线程自动纳入，并由
  `engine_id`、线程身份区分；调用者不手工报名 handler 或线程；
- fork 子进程自动切换到新的 process generation 和独立文件，回到安全环境后重建采集；
- 默认不常驻记录普通事件的“黑匣子”。只有真实崩溃需求证明固定 emergency 槽不足时，
  才重新评估默认常驻采集及其永久性能、内存和数据暴露成本。

简言之：设施自动准备，昂贵采集明确开启；开启后的生命周期由机制接管。

### 6.2 初始化和 Engine 关闭

- 采集设施必须在 native constructor 运行前自动准备好；不能要求用户先调用一个
  “注册当前线程/handler”的补丁 API。
- Engine 创建、进入 Closing、Closed 都发固定生命周期事件。
- Engine close 只结束该 Engine 的记录归属，不关闭进程级 writer。fini、signal 和
  TSD 收尾在关闭过程中仍可能产生记录。
- 异步记录不得借用 `Shared` 或延长 Engine 生命周期。需要的名称/ID 在普通态复制为
  稳定字典项。
- 不能依赖 TLS destructor flush：主线程正常进程退出未必走同一路径，最终 TSD 阶段
  也可能产生新事件。若采用 per-thread ring，其存储至少要稳定到进程退出或明确被
  进程级服务接管。

### 6.3 fork

父子进程不能继续写同一个活动段，也不能在子进程触碰父进程可能锁住的 mutex 或已
消失的消费者线程。

已经确认的方向是：fork child hook 只用稳定内存写入标记新的 `process_generation`、作废
父代 producer 身份和缓存 tid；第一次回到普通可重建边界后自动建立子进程自己的段和
writer。重建前普通 recorder 必须指向进程期 drop-only producer，不能继续使用父页、fd、锁
或已消失的 writer。该机制必须同时覆盖 `HostFork`、通用 `HostSyscall(SYS_fork)` 和任何允许
的 native→guest 再入，不能靠用户声明“这里会 fork”。精确状态机仍列在 §12.1 待批量裁定。

## 7. 落盘格式候选

**已确认的格式分层（用户 2026-08-13 裁定）：**机器读取的权威原始记录使用二进制
格式；JSONL、CSV、可读文本和 Perfetto 都是离线派生格式。人类诊断文本不是分析脚本
的稳定输入。每个进程/每个 fork generation 独占自己的原始文件，不允许多个进程并发
追加同一个文件。

这项裁决只确定格式分层，没有冻结 magic、header、chunk、checksum、扩展名或具体字段
宽度。第一版候选仍是 append-only 二进制分段；以下细节要继续逐项讨论。

### 7.1 session 目录

候选 session 目录包含：

```text
session metadata               构建、命令、平台、配置和时钟说明
events-<pid>-<generation>-*.mlog[.partial]
diagnostics.log                可选的人类文本，不是机器合同
perf.data                      profile 开启时由 perf 产生
jit-*.dump / perf map          JIT 真代码地址映射，按选定 perf 方案产生
```

名字和容器尚未冻结。关键规则是：不同进程不并发写同一个段；profile 的原始产物不
伪装成普通日志文件。

### 7.2 文件头和记录

候选文件头使用固定 little-endian 编码，至少包含：

- magic、major/minor 格式版本、session ID；
- 构建身份、pid、process generation、segment number；
- 指针宽度和事件 schema 版本；
- 单调时钟种类，以及与墙钟的起始锚点。

逻辑解码结果可以包含 producer、sequence、时间和 Engine，但它们不构成统一的逐记录物理
头。已确认的首个 syscall 流使用64B page header继承 producer、`first_sequence` 和初始
Engine，再用记录自身 `control` 给出 kind/length/version；`SyscallEnter`/`SyscallExit` 分别
固定64B/24B且没有时间戳，Engine 变化由16B `EngineContext` 表达。v0 仍须在施工前冻结
page header、control/status bit、file/chunk/footer 的精确 little-endian 布局。未知 kind 可以按
length 跳过；但若未知的是 context-control，decoder 必须把 Engine 归因标为 unknown，直到
下一页 header，不能跳过后继续冒充归因可信。

高频记录不重复保存源码路径和长字符串。字符串和符号通过字典记录引用，字典项必须
在引用前提交，或提供离线可判定的缺失语义。

### 7.3 chunk 提交与崩溃恢复

**已确认的恢复合同（用户 2026-08-13 裁定）：**消费者按 chunk 写入 header、payload
和 commit footer/checksum。只有三者完整且校验通过的 chunk 才可信；普通进程崩溃后，
离线工具恢复此前所有可信 chunk，并明确舍弃不完整尾块。signal handler 不扫描或 flush
普通 ring，也不尝试补写当前 chunk。

- 正常关闭写入 `End`，精确汇总 produced、written、dropped、recursive-suppressed 和
  write error；没有 `End` 只能称为 unclean session；
- 异常关闭时，工具只报告已提交块、可证实的 sequence 缺口和未知尾部，不猜测内存
  ring 或在途 chunk 中究竟丢了多少；
- 活动原文件暂称 `.partial`；恢复工具不修改它，而是另行输出可信前缀。扩展名仍未冻结；
- clean close 后可在同一文件系统内原子 rename 为最终文件；
- 轮转只发生在 chunk 边界；
- 首个纵切是否提供文件 byte cap/rotation 仍待裁定；推荐 v0 不做，ENOSPC 进入 sink-failed，
  不静默删除早期证据，也不在既有 trace root 中途切域；
- checksum 只由消费者按块计算，不能增加生产者热路径成本。

不为每个 chunk 执行 `fsync`。这个合同只定义“哪些字节可以信”：它保证普通进程崩溃
不会使更早的完整 chunk 失去边界，但不保证内存 ring、当前 chunk 或断电时尚未持久化的
数据仍存在。checksum 算法、chunk 大小和刷盘周期留待真实负载评测。

## 8. 配套分析工具

建议提供一个共享 decoder 和两个用户入口，避免四个 shell script 各自实现一遍格式：

### 8.1 `scripts/mirvm-log`

- `inspect FILE|SESSION`：验证 magic、版本、chunk、checksum、sequence 和 `End`；输出
  Engine/线程/事件计数、drop、洞、截断和写失败。没有时间的流明确输出
  `time_range: unavailable`；发现损坏应非零退出。
- `export ...`：按 Engine、tid、事件和 sequence 过滤，导出 JSONL；只有记录本身有可信时间
  时才允许时间过滤。CSV/可读文本属于后续派生面；
  JSONL 是交换格式，不是运行时落盘格式。
- `summarize ...`：聚合计数、持续时间、直方图和未闭合 span；所有结论同时报告样本
  数和 drop，不能在数据不完整时给出假精度。
- `recover PARTIAL --output ...`：复制最后一个完整 commit footer 之前的有效前缀，
  保留原文件，并明确报告丢弃的尾部字节。

### 8.2 `scripts/mirvm-profile`

- `capture -- COMMAND...`：检查 perf 权限，记录命令、构建身份、JIT 模式、机器、内核、
  采样参数和 mirvm session；权限不足时响亮失败，不能静默退化成另一种 profile。
- `report SESSION`：协调 `perf.data`、JIT 地址映射和 mirvm 的解释器样本/指标；输出
  可复现的报告输入，而不是只打印一张不可追溯的表。

第一版不做 `to-perfetto`。当 timeline schema 已经由真实负载验证后，可以增加 Perfetto
导出，使现有 UI/SQL 工具消费数据；不应一开始就让 Perfetto schema 反过来决定 mirvm
热路径内存布局。

工具建设遵守基础设施预算纪律：第一条产品 RED 只需要 `inspect` 和 `export`，就不先
造通用查询语言、完整 provenance 数据库或格式插件系统。

## 9. log 与 profile 如何配合

### 9.1 先用最低扰动的方法回答问题

推荐顺序：

1. **相位计时**回答“慢在启动、lower、cache 还是执行”；
2. **聚合指标**回答“某类操作发生多少次、累计多久”；
3. **外部采样**回答“CPU 实际花在哪里”；
4. 只有前三者无法解释因果时，才开启高流量 timeline trace。

全量文本日志不是 profiler。它会把被测程序变成“格式化和写日志的程序”，热点排名
可能只是观测工具自身。

### 9.2 native 与 JIT profile

第一版 CPU profile 应优先使用 Linux `perf record`/`perf report`，让关闭内部事件时的
mirvm 尽量接近生产二进制。

现有 backtrace 最小 ELF 只给 `FuncId` 造不可执行的 token 地址，不覆盖 Cranelift
真实代码范围，不能直接给 perf 做 JIT 符号化。需要在 JIT 发布点导出真实
`[start, end)` 地址、函数身份和生命周期，再评测：

- perf map：实现简单，适合先验证地址和名称；
- jitdump：能把 JIT load/move/close 事件注入 perf 流，更适合完整生命周期。

`perf inject --jit` 已能处理 jitdump 并生成对应 mmap/ELF 信息，因此不应自造一套只
有 mirvm 脚本能看的 native sampling 格式。当前 JIT 代码不 move/unload/reuse，首版推荐
先用 perf-map；jitdump 只有真实生命周期或回放缺口出现后再重开，仍待批量裁定。

### 9.3 解释器 profile

外部 perf 能看到解释器的 native 热点，却不会自然显示“当前是哪一个 guest FuncId”。
有两个不同精度的候选：

1. 在普通安全点低频采样当前 guest 逻辑栈：实现相对简单，但只观察到达安全点的时刻，
   对长基本块、阻塞 foreign call 有偏差；
2. 由采样 signal 记录预先发布的稳定栈快照：时间偏差更小，但 signal handler 只能复制
   稳定指针/标量，不能遍历当前 `Vec<ShadowFrame>`、分配或现场符号化。

这两者不能用同一个“sample”名称混过去。报告必须标明采样方法和已知偏差。没有真实
问题证明安全点采样不够前，不施工第二种高复杂度机制。

### 9.4 可以共享什么，不能共享什么

可以共享：

- session/build/进程/线程/Engine 标识；
- 时钟说明和相关性 ID；
- 二进制 decoder、字典和离线导出代码；
- 在证据支持下共享普通落盘服务。

不能默认共享：

- ring 容量和 drop 预算；
- signal-safe producer 与普通 producer；
- profile 样本、timeline trace 与人类诊断的背压；
- cache write-back 与日志的调度保证。

## 10. 性能预算与验证

旧稿的 `2 ns`、`100 ns`、`200 ns` 是估算，不是门槛。预算应从真实事件率和允许的总
扰动反推：100 ns × 100 万事件/秒已经占一个 CPU 核的约 10%；每条写 1 KiB 则是约
1 GiB/s 的内存写流量。孤立单次延迟不能代表整个系统成本。

### 10.1 建议先讨论的总预算

- 关闭态：代表性真实 workload 的总开销相对 baseline 不超过 1%；
- 默认采样 profile：总 wall/cycles 扰动先以不超过 5% 为讨论起点；
- timeline trace：不承诺一个脱离事件率的纳秒数，只报告给定线程数、记录大小和
  消费者速度下的最大无丢吞吐与丢失曲线。

这些数字仍是候选，必须经用户确认和实测后才能成为 gate。

### 10.2 微基准矩阵

首个 syscall 纵切至少测量：

- plain、Enter-only、完整pair、页滚动、无页drop、`context_unsynced`；
- 1/2/8/32 producer；
- 消费者空闲、正常落盘、故意变慢；
- 24B/32B Exit 和 4/16/64 KiB page 挑战项；首版正式流固定无时间；
- p50/p95/p99/max producer latency；
- cycles、instructions、cache misses、消费者 CPU、RSS、文件带宽和 drop rate。

“ring 满时抖动为零”不可验证，也不真实。可验证合同是：不等待、不取锁、不 syscall，
并有界返回。

### 10.3 真实 workload

微基准之后必须在现有真实负载上交替运行 baseline/instrumented，并保持 correctness
oracle：warm/cold 启动、fib/JIT、rayon、多线程、syscall 密集和分配密集用例。不要
为了日志重新复活整套停放 benchmark harness；当前 workload 已能给可信结论时，直接
使用它。

profile 还要比较：热点前 N 名是否稳定、样本丢失率、采样频率变化后结论是否收敛。
只看 wall time 低于 5% 仍不足以证明 profiler 没有改变热点排序。

### 10.4 特殊环境验收

- 分别在记录字段写入、cursor 前移、封页和 release 发布处注入 signal；writer 始终不读
  active page，延后 handler 返回普通态后仍能继续记录；
- fatal signal 打断半条记录时只损失尚未发布的 active page，不把半记录当作已提交数据；
- `Display` 递归和 panic 后，下一条普通记录仍可读；
- allocator 故障、panic、fini、pthread 最终 TSD 中记录，不分配、不展开；
- 生产者/消费者繁忙时 fork，父继续，子重建前后不死锁，pid/tid 不串；
- SIGABRT/SIGSEGV 后，工具恢复全部已提交 chunk，并报告尾部和 drop；
- 填满 stderr pipe，证明普通热路径仍有界；
- 反汇编审计内核/采样入口：无锁、分配、Rust TLS 初始化、格式化、unwind、guest/JIT
  调用。

## 11. 现行施工队列

下面的 L 是日志采集主线，P 是用 Linux perf 做外部采样的 profile 线。顺序表示真实依赖，
不是要求两条线串行等待。

1. **L1——页和线程生命周期闭合（2026-08-19 已完成）**：固定 4 KiB 页下已有进程硬页池、
   page-less producer 自动救援、returned-page 通道、TSD retire 后移出活跃扫描表，以及
   writer 每轮每 producer 至多取一页的公平基线。零预算 attach、退线程归页后恢复、2,048
   个短命线程不积累扫描项、双 producer 公平顺序和 offer/retire 同轮所有权已有回归；真实
   arm session 的 publish/return/retire/rescue 链也已在 TSan 下零竞争告警。
2. **L2——fork 子代自动重建（下一步；第一片完成 2026-09-18）**：child hook 先把子进程切入 drop-only，普通边界
   自动创建新 process generation、文件、页池、writer、producer 和 errno pointer。覆盖
   `HostFork`、泛型 `SYS_fork` 和 native 再入；父子不得共享账本、fd 或页状态。
   **已完成** = 实现前置（MIRVM 服务线程自动登记，fork 守卫按
   `os_thread_count − service_thread_count` 判定 guest 线程数，fork 子代按 pid 自愈基线）、
   子代代际身份（文件头与文件名同源）、以及 fork 安全的**重建配方**
   （`RebuildRecipe`，泄漏到进程结束 + 原子地址，子代读派生内存即可）。
   **剩余** = 重建的消费者：子代在普通边界用配方建自己的文件、页池、writer、producer 与
   errno pointer，并为其孤儿会话实现进程退出收尾（停会话 → 唤醒 writer → 有界 join →
   正常 `End`）。在完成前子代仍是 drop-only，不得写成已有能力。
3. **L3——真正的 HostSyscall 热路径（随后）**：稳定 `ProducerFast` ABI 和共用冷慢路，
   移除通用 JIT builtin helper 与健康 pair 的逐记录冷 sequence 更新；trace JIT 用 `r15`
   固定当前 producer，plain 代码继续保持零 telemetry/TLS 读取和零保留 `r15`。
4. **L4——1B stateless inline-asm raw site（L3 之后）**：复用同一 producer ABI 双物化 raw
   syscall 站点，以 RFLAGS、除 `rcx/r11` 外 GPR、red zone、栈、XMM/YMM/ZMM 和 raw 返回值
   对拍为交付门。L4 完成后，首个 MIRVM-owned syscall 纵切才算完整。
5. **P1——JIT 机器码地址登记（2026-08-19 已完成）**：独立 `JitSymbolRange`
   覆盖 fast body、guarded、packed、c2i 全部执行范围。编译请求只在本地收集范围；
   finalize 成功后先把整批登记到内存 registry，再以 Release 发布入口 slot，失败批次
   直接丢弃。`install` 在 registry mutex 外以 no-replace 创建空 map；显式 `stop` 在锁内先切
   `Inactive` 并快照，再在锁外批量 write/flush，截断点后的范围归下一 session。JIT worker
   不做 map I/O；fork child registry/map 重置仍留给 L2/P2。
6. **P2——真实 profile 工具（待施，可与 L2–L4 并行）**：交付 `mirvm profile capture` 和
   薄脚本，首版只承诺 Linux user-space、IP-only、inherit。权限不足、lost samples 或缺地址
   映射必须响亮失败或标成 incomplete；随后立即重跑 `fib(32)` 与 D16 真实 workload。
7. **D0——诊断通道分层（2026-08-19 已完成）**：默认 `mirvm run` 仍按 Cargo
   语义，让 compiler（含 frontend/lower）诊断、MIRVM control 和 guest stderr 物理共用 fd2，
   字节与顺序不变。capture 在 command boundary 建立 `DiagnosticRouter`，把前两类逐字节
   tee 到 `diagnostics.log`；child attached marker 避免 runner 重复路由，guest fd2 绝不进入 router
   或普通事件 ring。direct、cargoless、Cargo runner 及主流程前失败的逐字节合同
   31/31 通过；正常路径 atexit 收口并 no-replace 发布，异常结束保留 partial。
8. **数据裁决——自适应页池和 writer 调参（L1–L4 与 P2 之后）**：在相同进程内存预算下
   比较 4/16/64 KiB、24/32B Exit、return gap、drop 曲线、guest cycles、RSS 和 writer CPU，
   再实现 4 KiB→64 KiB 自动晋升/回收并裁定批量与 checksum。16 KiB、ready MPSC、staging
   和其他 I/O 路径只有实测胜出才进入实现，不能先写死数字。

不要为了“统一”先迁移 `MIRVM_TIMING`、`MIRVM_JIT_STATS` 等所有探针。L/P 当前交付物足够
裁判具体产品问题后，冻结工具基建并回到被测产品路径。

## 12. 已确认项与剩余批量裁决

2026-08-17 用户要求停止逐项问答，剩余决策一次列全。2026-08-18 用户批准按这份分组开始
施工：§12.1 成为首版实现合同；§12.2 必须由真实数据裁判，不再人工拍数字；§12.3 不进入
首版，但每项都有明确进入条件，不作无限期搁置。施工按 §11 的 L1–L4、P1–P2、D0 与数据
裁决推进；当前 1A HostSyscall 参考实现不能冒充整个 syscall 纵切或性能终态。2026-08-19
L1、P1 与 D0 已完成；主线下一项仍是 L2，P2 可与 L2–L4 并行进入。

1. **已确认**：六类数据各有独立合同，只共享必要且经证明合适的底层设施；
2. **已确认**：Engine 执行/拆除期间所有遥测均不得等待输出端；允许丢失但必须计数，
   CLI 回到控制边界后才可同步显示最终诊断；
3. **已确认**：固定 emergency 内存自动准备；普通采集明确开启并由 CLI/进程级 API
   自动建立 session。当前进程中的线程和 Engine 自动纳入；fork 子代先进入 drop-only，
   自动建立独立代际属于 L2，完成前不得写成已有能力；
4. **已确认**：每进程/fork 代际的权威原始记录使用二进制；JSONL、CSV、可读文本和
   Perfetto 都是离线派生格式。1A 已有内部 v0 字节布局，但尚无跨版本兼容承诺；
5. **已确认**：原始文件按 chunk 提交和校验；崩溃恢复所有完整块、舍弃尾部半块，
   signal handler 不 flush，正常 `End` 才提供精确总账；
6. **后置到格式首次演进**：schema 兼容在首个实现和真实文件出现后讨论；当前先细化热路径；
7. **已确认**：时间戳按语义分三档；计数不读时间，普通事件用合格的 relaxed TSC，
   严格边界用有序 TSC，不合格环境自动退到 vDSO；生产者只存原始值，离线按锚点换算；
8. **已确认**：普通 JIT 零采集指令；timeline 使用独立代码域，并用 `r15` 保存稳定的
   每线程 `ProducerHot*`；解释器同样按入口选择 plain/trace 循环。代码域只在最外层 guest
   activation 入口选择，调用链内不迁移；长期运行中动态开启 timeline 在相应真实 workload
   进入验收时重开 OSR/可重建 frame；
9. **已确认**：普通事件使用 per-pthread SPSC 页环，页内无逐事件原子 commit，整页以
   release/acquire 转交；进程 MPSC 不进入逐事件热路。active page 只在页满或线程自然离开
   trace 的静止边界发布，不做周期 watermark/deadline；
10. **已确认**：每种合同使用独立的进程硬页池；producer 懒建、至少双页且按真实速率自动
    伸缩。4 KiB starter 可在证据和预算允许时晋升 64 KiB hot，16 KiB 留作同预算挑战项；
11. **已确认**：每进程代际使用独立 capture writer；页发布以双方 AcqRel swap 封住漏唤醒，
    writer 直接 buffered `pwritev` sealed pages，不与 cache/JIT/close worker 共线程；
12. **已确认**：第一条内部 SPSC 纵切是 MIRVM-owned syscall event；正常返回分别提交
    `SyscallEnter` 和独立的 `SyscallExit`，绝不跨 syscall 保留半条记录。全进程 syscall 另由
    Linux raw-syscall tracepoint 形成独立 stream，二者不得互相冒充。Enter/Exit 不写
    `call_id`，按线程 sequence 嵌套配对，任何缺口或代际边界都截断而不猜。首版内部 pair
    不读时间；duration 只在独立内核流可用且可无歧义关联时产生。Enter 固定 64B、Exit
    固定 24B；Enter 以 flags-neutral 的额度计数预扣 88B，Exit 不再做容量比较。Engine 归因
    从页头和16B `EngineContext` 继承；无法同步 context 时整段计数并 drop，不误记。
    pthread producer 冷边界只缓存一次 `errno_ptr`；libc Exit 仅在 `result == -1` 时读取它，
    producer 全路径不得修改 errno，成功和 raw Exit 均不碰 errno。Enter 最终失败即锁定为
    `unrecorded-pair`：正常返回只计 suppressed Exit drop，不在返回点抢救孤立 Exit。
    `ProducerFast` 首行只含 `cursor/pair_budget/errno_ptr`；writer 不读 producer fast/cold 两行，
    marker 后冷路径重算 budget，封页由 used bytes 和 marker 数推导 sequence。

### 12.0 当前施工状态（2026-08-19）

第一条 **1A 可运行纵切**已经落地：进程级 capture session、每 pthread 双 4 KiB
独占页、页级 SPSC 发布、独立 writer、v0 chunk/BLAKE3/End 总账、
`Builtin::HostSyscall` 的 64B Enter + 24B Exit、Engine 上下文、严格 decoder、
`mirvm log inspect|export` 和薄脚本已经连通。CLI 用内部 argv 把 capture 目录传给
Cargo runner/cargoless root，不写入 guest 可见环境；正常完成以 no-replace rename
发布，不能覆盖旧日志。返回型 syscall、失败 errno、两页耗尽/恢复、`SYS_exit`
提前封口、fork 父账本、解释/JIT 自动改写和 TSan 同源构建均已有回归。

L1 也已闭合：所有 producer 共用有硬字节上限的进程页池；拿不到双 4 KiB starter 仍会
attach，并在后续 Enter 以一次 acquire 检查 writer 的 offer；retired producer 排空后从
writer 活跃扫描表单遍摘除并归页。writer 每轮每 producer 最多写一页。会话结束还会回收
全部页内存；这条 ownership 链已由定向竞态测试和真实 TSan arm session 覆盖。

P1 也已闭合：所有 Cranelift fast body/guarded/packed/c2i 地址范围由成功编译
请求成批登记，再以 Release 发布入口 slot；失败请求的本地批次直接丢弃。
registry 登记只改内存；perf-map 仅在显式 stop 控制边界快照后在锁外批量写入/
flush，不再拖住 JIT worker 或 teardown。真正启动 Linux perf 及 fork child map 代际仍属
P2/L2。D0 同样已闭合：capture 从 command boundary 起路由 compiler/frontend/lower 与
MIRVM control，子进程通过 attached marker 继承；默认 fd2 合流字节/顺序不变，
guest fd2 不进 router。direct、cargoless、runner、早期参数/输入错误等 31/31
逐字节合同已通过。

这仍然**不是性能终态，也不是 1A/1B 全部完成**。当前 HostSyscall trace 仍走通用 JIT
builtin helper并逐 record 更新冷 sequence；每个获页 producer 仍固定使用双 4 KiB starter，
公开入口的 64 MiB 硬池值只是首轮实现默认值，不是数据裁决。4 KiB→64 KiB 自适应、fork
child 新 generation/独立文件、trace JIT pinned producer 快路、1B stateless inline-asm raw
site 和 P2 profile 命令仍未完成。

现行主线下一项是 L2，L3–L4 随后闭合 HostSyscall 热路和 raw site；P2 可并行进入，
并须在数据裁决阶段前完成。数据裁决才负责冻结硬页池预算、4 KiB→64 KiB
晋升阈值和后续 writer 参数，L1 的固定双页实现不能充当这些数字的最终证据。

### 12.1 已确认：首版施工合同（用户 2026-08-18 整体批准）

1. **首个纵切和控制面**：正式完成态覆盖 `Builtin::HostSyscall` 与可双物化的 stateless
   inline-asm raw site；施工可先做 1A HostSyscall + writer/file/decoder，再做 1B raw site，
   但1A不能冒充全完。推荐 CLI `mirvm capture -- COMMAND...` 与同一进程级 embedding API，
   不靠 env；`request_stop` 只禁止新 trace root，`finish_stop` 等旧 root 返回、封页、写
   `End`、close/rename/join。等待超时只报告仍在进行，不能偷读 active page。
2. **producer ABI 与 raw wrapper**：`ProducerFast` 偏移用 `repr(C, align(64))` 和编译期断言
   锁住。JIT trace 域可用 `r15`；现有 inline-asm wrapper 会把 `r15` 当合法 operand，且已有
   `rbx` slot-buffer ABI，推荐 trace wrapper 从预留 buffer slot 取得 producer pointer，不能
   无条件相信入口 `r15`。plain wrapper 保持原字节；trace raw site 保 RFLAGS、red zone、
   stack、除 `rcx/r11` 外 GPR 及完整向量态，page/drop 两条尾路径都执行原始 syscall 语义。
3. **page 所有权和自动恢复**：冻结唯一状态链
   `FREE/OFFERED -> ACTIVE -> SEALED/PUBLISHED -> IN_FLIGHT -> RETURNED`，producer→writer
   与 writer→producer 两条 SPSC 索引分 cache line。无 active page 时，每个新的 Enter 只做
   一次 acquire 检查 free offer；成功开页，失败立即 drop，不 spin、不定时、不让 suppressed
   Exit 探测。writer 永不读取 ACTIVE。
4. **attach、retire 与硬池**：首次 trace root 自动 attach；硬池拿不到 starter 时仍建立
   page-less producer，后续自动救援，不能突破 cap 或拒绝 guest。最终 MIRVM TSD 在所有
   deferred/fini 后 seal/retire；writer drain 后从活跃扫描表摘除 descriptor、页面归池。
   tombstone 可留到 session 结束，但扫描表不能随历史短命线程无限增长。额外页必须在交给
   producer 前完整初始化/prefault；quiescent producer 需要可证明无竞态的 hot-page retract。
5. **v0 最小二进制布局**：施工前一次冻结64B page header、64/24/16B record control/status、
   file header、chunk header 和 commit footer 的 little-endian 位布局及 reserved-bit 规则。
   page 至少表达 page/used bytes、producer/thread generation、OS tid、first sequence、初始
   Engine、page ordinal、record/loss 核对信息；file继承 process generation。未知普通 kind
   按 length 跳过，未知 context-control 使 Engine 归因失效到下一页。推荐 footer 用仓库已有
   BLAKE3，format冻结前再由真实 writer CPU 数据挑战。
6. **sequence、loss 和终结总账**：每个实际或策略抑制的逻辑 record 占一个 sequence；至少
   分开 capacity/context/recursive producer drop 与 sink loss，不能重复计数。推荐核对式
   `attempted = encoded + producer_drop`、`encoded = committed + sink_loss`。正常 thread
   retire 必须在最后页之后交 `ProducerEnd` 冷账本；session `End` 汇总各 producer。无 End
   只能报告已证缺口和未知尾部。
7. **writer 状态机**：推荐每轮每 producer 最多取一页（初始 `q_i=1`），机会式组成不超过
   约1MiB且受 `IOV_MAX-2` 限制的 chunk，不等凑满。严格处理 `EINTR`、正短写、footer commit
   和前缀页归还；提前归还页时，chunk summary 必须保存其账本。open/header失败使 start
   同步失败；运行中的非EINTR错误或零进展转 `SINK_FAILED`，停止追加、保留 `.partial`，继续
   消费归页并累计 sink loss。推荐 v0 无 rotation/byte cap，ENOSPC 走同一失败面。
8. **fork 与 emergency**：child hook 只能用稳定写操作切 generation，并把当前执行切到进程期
   drop-only producer；父 fd/page/锁/writer 全部不可达，普通边界再建新 fd、pool、writer、
   producer 和 errno pointer。必须覆盖 HostFork、泛型 SYS_fork 和 native再入。另须明确
   emergency 的首版范围：推荐内存槽只承诺 core/控制边界可见，不冒充进程死亡后持久记录；
   持久 fatal marker 另用预开独立 fd/固定记录协议，不能借普通 ring/chunk。
9. **decoder 与 scripts**：二进制解析只实现一次，放 Rust 产品模块；`mirvm log inspect|export`
   调它，`scripts/mirvm-log` 只是薄入口，不用 shell 重写 parser。首版只交 inspect 与 JSONL
   export；支持 Engine/producer/tid/kind/sequence，u64 ID用十进制字符串、原始位/地址用定宽
   hex，no-time 流明确 unavailable。summarize、独立 recover、CSV/text 延期。
10. **首版 profile**：推荐 `scripts/mirvm-profile capture -- COMMAND...` 从进程启动前用 Linux
    perf 采样，不承诺任意 PID 动态 attach；profile 不切 trace 域。先用 perf-map，不做
    jitdump；独立 `JitSymbolRange` 覆盖 fast body、guarded、packed、c2i 等所有可执行范围，
    并在入口 slot Release 发布前登记。首版只承诺 user-space/IP-only/inherit；权限不足、lost
    samples、缺 range/map 响亮失败或标 incomplete。解释器 logical sampling 不进首版。
11. **正确性与结构门**：必须有 golden/roundtrip/截断/checksum/unknown-kind 测试；libc成功/
    失败errno、raw返回值、RFLAGS/GPR/red-zone/XMM-YMM-ZMM、plain/trace行为对拍；页满/drop/
    context、短写/sink error、线程退出、busy fork、signal在写字段/cursor/seal/publish四点注入。
    反汇编硬门是 plain 无 telemetry/TLS/保留r15，健康pair无原子/锁/额外syscall，drop有界。
12. **诊断通道**：默认 `mirvm run` 的 fd2 字节和顺序保持 Cargo 语义；内部路由区分
    compiler/lower、MIRVM control、guest stderr。capture/profile 只把前两类 tee 到独立
    diagnostics stream，绝不截获 guest fd2 或把文本塞入普通事件 ring。direct、cargoless、
    Cargo runner 三条路径必须同时接通，不接受只修其中一条的临时分叉。

### 12.2 已确认：只能由专项基准纵切裁判，不预设数字

1A 已经产生真实文件，但它仍含通用 helper、固定双页和逐记录冷账本，不能用来冻结性能
参数。L1–L4 与 P2 完成后，按 §11 的数据裁决阶段在相同预算、交替 workload 下裁定下列项目：

1. 进程硬页池绝对字节、每 producer min/max、water-fill 公平参数、return-gap 分位数；
2. 4K→64K 晋升/回收阈值、16K 第三类、owner与writer prefault/NUMA、2MiB backing slab；
3. 24B Exit 对32B挑战，以及4/16/64KiB在相同总内存下的guest cycles/drop曲线；
4. 每次 dropped Enter 探测 returned line 是否需本地 backoff，大量休眠 producer 是否需页级
   ready MPSC；没有证据维持“一次 acquire”和稳定列表扫描；
5. `q_i=1`、约1MiB chunk、BLAKE3与CRC32C、writer公平轮询的CPU/归还长尾；
6. 更多 per-thread pages 对 staging；只有对应瓶颈成立后才测 io_uring、file mmap、在线压缩；
7. 正式性能数字：plain/closed候选上限1%、默认sampling候选5%都先不冻结。用交替真实负载
   决定 cycles/event、p99/max、无丢吞吐、drop曲线、RSS/writer CPU，并检查profile top-N
   排名和loss是否收敛；
8. perf 默认event/frequency/period/callgraph、perf-map是否足够、是否需要jitdump；
9. external perf若只显示解释器热点而无法归因，再裁安全点 logical sampling；只有其偏差被
   实证不可接受，才讨论signal采样。trace JIT也只在trace interpreter真实成本后决定。

### 12.3 已确认：后置队列及进入条件

这些项目不阻塞 L1–L4/P1–P2，但也不作无限期搁置；满足各自条件时进入下一轮设计或施工：

1. **schema 兼容**：v0 第一次需要被第二版 producer/consumer 读取，或准备对外承诺稳定格式
   时进入；同时裁定最终扩展名、session 品牌、字典和格式插件边界。
2. **kernel raw-syscall 完整流与 duration 关联**：L4 完成后，首个需要观察 libc 内部、opaque
   archive 或全进程 syscall duration 的真实诊断进入时施工；内部流不得代替它。
3. **一般 timeline、Perfetto 和通用查询**：P2、内部 syscall 流与需要时的 kernel 流仍不能
   回答一个具名性能问题时进入；不得为了格式统一提前建设。
4. **长期 activation 动态启停**：真实 workload 同时证明 guest 长期不返回宿主且必须中途
   start/stop 时，重开 OSR/deopt 或其他可重建 frame 方案；已经处于 trace 域的长运行若提出
   最大可见延迟，再独立裁定 active-page watermark/deadline。
5. **解释器 logical/signal sampling**：P2 只显示解释器宿主热点、不能归因 guest 逻辑位置时
   先做安全点 logical sampling；只有其偏差实测不可接受才进入 signal sampling。任意 PID
   attach 和非 Linux/ELF/x86-64 profiler 在出现对应部署需求时进入。
6. **rotation 与 durability**：首个长时运行或有磁盘上限、掉电恢复要求的 capture 场景进入
   前施工 byte cap、rotation、final fsync 或持久黑匣子；短命开发采集不预付该成本。
7. **未触发 I/O 优化**：数据裁决证明稳定 producer 扫描、`pwritev` 或调度造成主瓶颈后，
   分别挑战 ready-page MPSC、staging/io_uring/mmap/compression、CPU affinity/nice；未证明者
   不进入产品路径。
8. **完整 epoch 回收**：动态 capture start/stop 或 trace code/producer descriptor 数量可随
   进程寿命无界增长前施工；此前只允许扫描表摘除后的有界 tombstone。
9. **全面迁移旧探针**：有一个具体问题必须联合查询 `MIRVM_TIMING/JIT_STATS` 与新采集流时
   才迁移对应探针，不启动六类合同大统一。工具足够裁当前问题后仍按基础设施预算纪律冻结。

## 13. 外部依据

- Linux [`signal-safety(7)`](https://man7.org/linux/man-pages/man7/signal-safety.7.html)：
  signal handler 中只有有限函数被保证安全；`write` 可用不代表普通数据结构可重入。
- Linux [`pthread_atfork(3)`](https://man7.org/linux/man-pages/man3/pthread_atfork.3.html)：
  多线程 fork 子进程在 exec 前只能依赖 async-signal-safe 操作，继承锁无法普遍修复。
- Linux kernel
  [Lockless Ring Buffer Design](https://docs.kernel.org/trace/ring-buffer-design.html)：
  reserve/commit、嵌套 writer 和 committed record 可见性的参考；不是直接选型结论。
- Linux [`perf record`](https://man7.org/linux/man-pages/man1/perf-record.1.html) 与
  [`perf inject --jit`](https://man7.org/linux/man-pages/man1/perf-inject.1.html)：
  native sampling 和 JIT 地址注入的现成工具链。
- Perfetto [service model](https://perfetto.dev/docs/concepts/service-model) 与
  [Trace Processor](https://perfetto.dev/docs/contributing/embedding)：作为后续时间线
  导出和查询参考，不预先绑定 mirvm 的热路径布局。
