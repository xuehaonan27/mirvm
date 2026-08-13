# mirvm — 设计文档

> 工作代号 `mirvm`（MIR Virtual Machine）。
> 奠基 2026-07-03；2026-07-05 确立当前心智模型（抽象机器 + VM 作者视角）。
> 本文档是项目的**长期心智模型与设计契约**，不是阶段进度表。当前实现、已知缺口和下一步见
> [docs/current-status.md](docs/current-status.md)；文档权威顺序见 [docs/README.md](docs/README.md)；
> frame/vmctx 等可逆决策及旧模型见 [docs/decision-history.md](docs/decision-history.md)。
> 本文后半保留早期 tier-0、候选目录和里程碑记录作为历史；标为目标的结构不得误写成现状。

---

## 0. 命题（一句话）

**mirvm 是 Rust 抽象机器（Rust Abstract Machine, RAM）的一个事实标准实现，按 JVM 级系统软件的方式构建。**

- 它**实现一台抽象机器**，不是"给 Rust 套个解释器"。正确性以"是否忠实实现 RAM"来定义。
- 它是**运行参考实现**：Miri 是 RAM 的*检查*参考（宁慢勿漏 UB），mirvm 是 RAM 的*运行*参考（假设合法、追求快）。两者实现同一台 RAM。
- 它按 **VM 作者的视角**设计（托管堆、执行引擎分层、OS 线程、加载/链接、JIT），**不是** Miri 的扩展。凡是遇到设计抉择，问的是"一台 JVM 类系统软件会怎么做"，而不是"Miri 怎么做"。

下面几节先立**心智模型**（§1–§3 抽象机器与 VM 架构），再落**具体模型**（§4–§7
内存/线程/执行/边界），最后是**工程与历史**（§8 决策、§9 里程碑与 tier-0 日志）。

---

## 1. Rust 抽象机器（我们实现的东西）

Rust 没有官方形式化规范，但存在一台**事实上的**抽象机器：rustc 的 MIR 操作语义 + opsem 团队的内存模型（借自 C++20）+ provenance 模型 + rustc 的 layout 算法。Miri 是它的可执行参考。mirvm 立志成为它的**权威、快速的可执行定义**。**完整语义契约见 [docs/designs/ram-spec.md](docs/designs/ram-spec.md)**（正确性契约、定义度四级 well-defined/unspecified/non-det/UB、RAM 边界、as-if 自由、声明的偏差、与 native/Miri 的关系）——本节是其摘要。

RAM 由五部分组成：

1. **存储（Storage）**：分配（互异、对齐、有大小、有生死）；字节（已初始化/未初始化，指针字节携带 provenance）；指针 = 地址 + provenance；int↔ptr 转换与 provenance 暴露。别名模型（Tree Borrows）*定义* UB，但合法程序不违反——**快速实现无需强制**（那是检查，不是合法程序的语义）。
2. **值与布局（Layout）**：类型如何 realize 成字节——size/align/字段偏移/判别式/niche，由 rustc layout 固定。标量、标量对、聚合体。
3. **计算（Computation）**：MIR 操作语义——place/rvalue/语句/终结符；函数调用、unwinding、Drop。
4. **并发（Concurrency）**：C++20 派生的内存模型——原子操作与序（SeqCst/Acq/Rel/Relaxed）、happens-before、数据竞争 = UB；线程；TLS。
5. **可观测行为（Observable behavior）**：I/O、syscall 效果、volatile——as-if 规则的边界。

以及贯穿其中的 **UB（未定义行为）**：RAM 未定义的程序状态。符合规范的实现对 UB 不受约束；*检查*实现（Miri）在此陷入报错；*标准*实现（mirvm fast）**假设它不发生**。

### 正确性契约

> 对任何在 RAM 下有已定义行为的程序，mirvm 的可观测行为符合 RAM。

native codegen 是 RAM 的**另一个**实现——所以"mirvm 输出 == native 输出"是**同源的必然**，不是巧合。这也是差分测试（对拍 native）为什么是有效的正确性度量：两边都在实现同一台 RAM。

### as-if 规则 = 我们的自由

RAM 只要求**可观测行为**一致，其余全自由。这是贯穿一切的授权书：

- **tier 化**（解释 → 字节码 VM → JIT）合法——只要每 tier 保持可观测等价。
- **托管堆的 arena/TLAB 分配器**合法——RAM 对分配只要求"互异、对齐、非空"，钱从哪来自由。
- **内部执行策略可替换**——只要它仍满足 Rust 对线程身份、OS 互操作和可观察并发行为的要求；
  早期协作式 tier-0 因无法提供真实 `pthread_t` 已被判为不适合作为产品实现（§5）。

凡是"我们能不能这么优化"的问题，答案永远回到：**可观测行为变了吗？没变就自由。**

---

## 2. VM 架构（JVM 类比是脚手架）

用 JVM 这台成熟系统软件的结构来锚定 VM 作者视角。每个部件都是"实现 RAM 的某一面"：

| JVM 部件 | mirvm 对应 | 说明 |
|---|---|---|
| 类加载 + 字节码验证 | **rustc 前端**（解析/宏/typeck/borrowck/MIR）+ **惰性单态化** | RAM 的"加载器/验证器"。这是十年工程，我们**复用**不重建。加载 = 取得某个 instance 的 MIR |
| 字节码 | **MIR → 自研 typed bytecode**（M4 已落地） | 加载相冻结 RAM 计算所需信息；执行相不再访问 tcx |
| 托管堆（GC） | **Rust Heap**（托管，**不搬迁**，Drop 而非 GC；现实现 = mimalloc crate 后端，hand-rolled arena/TLAB 后置 E14） | RAM 存储的 realize，见 §4 |
| 执行引擎（解释→C1→C2） | **typed-bytecode 解释器 + 方法级 Cranelift JIT（均已落地，JIT 默认开启）** | RAM 计算的执行，见 §6 |
| 线程（1:1 OS） | **1:1 OS 线程** | M4 已落地，解释态回调通过 thunk + TLS attach 进入 VM，见 §5 |
| JNI | **FFI**，但软边界（真实地址、零编组） | RAM 的**边界**，见 §7 |
| intrinsics / native 方法 | **Rust intrinsics + VM 内建运行时**（分配/线程/unwind） | RAM 内建操作，VM 自己实现 |

关键区别（为什么不是照抄 JVM）：**JVM 为"跑在 VM 上的语言"服务，与 native 的边界是硬的（GC 堆、对象头、handle），所以需要 JNI 那样的重编组；mirvm 为 native 语言做 VM，边界是软的（同一内存模型、真实地址、无 GC），所以要的不是 JNI，而是"抽象机器边界 + 完整的内建虚拟化"。** 这个差异决定了下面每一个具体设计。

### 为什么复用 rustc 前端（四堵墙）

绕不开 rustc 做 RAM 的加载器：① 前端（trait solver/类型推断/宏卫生）是十年工程；② proc-macro 是编译期运行的原生代码，纯解释不成立；③ 泛型跨 crate 单态化，依赖 MIR 也必须能执行；④ std 地基是 unsafe + intrinsics + syscall。复用后 **layout/ABI 与 native 精确一致**（直接用 rustc layout query），这正是真实地址内存与 FFI 的正确性根基。自研含量集中在**执行引擎**——最有价值、最该自己掌控的部分。

---

## 3. 应用场景（是 RAM 实现的推论，不是身份）

有了一台正确、快速、可嵌入的 RAM 实现，这些能力是自然推论（均为 P0，前三项优先）：

1. **LLM/Agent 脚本执行**：单文件快启动；沙箱与资源限制；rustc 自带的结构化 JSON 诊断（免费）。语法对齐 cargo script frontmatter（RFC 3424）。
2. **项目开发内循环加速**：`mirvm run .` 跑完整 cargo 项目，改一行亚秒重跑（省 codegen+链接；依赖 MIR 一次构建全局缓存）。
3. **REPL/Notebook**（后置，未立项——原「M6」编号已被轨 C 冷启动占用，见 docs/open-issues.md D11）：VM 拥有持久堆，状态持久化天然成立，跨 cell 借用不再是问题。
4. **嵌入式引擎**：真实 `Package`/Engine 生命周期已经存在，但公开信任边界必须写准。
   `Package::load` 是 safe 的 owned snapshot 校验；`Package::instantiate`、手工 Module 和
   无类型 raw export 是 `unsafe`，因为系统无法证明包内 native/FFI 声明。当前不是完整的
   safe typed API。

**执行引擎现状 = 解释器 + 方法级 JIT 双轨**（M5 全收：M5.3 骨架 + M5.4a–d 翻译器
全覆盖 + M5.5 vmctx 终裁计量与 gate6 收口均已落地，JIT 默认开启、可 `--jit off`
回退纯解释）；vmctx T 骨架生产定稿，R 复测挂双触发器（E6 进场 / 多 Engine 立项，
见 docs/designs/vmctx-passing.md §7）。CLI 兼容旧的 `--engine vm` 写法。

---

## 4. 内存模型（RAM 存储的实现）

RAM 存储在 mirvm 里 realize 为**三个隔离的部分**（管理与污染维度，非地址空间维度——地址空间只有一个，宿主的）：

| 部分 | 是什么 | 归属 | 关键性质 |
|---|---|---|---|
| **VM 元数据** | provenance 表、MIR/字节码缓存、线程表、layout 表 | **不属于 RAM**，实现私有 | 必须与下面隔离——guest UB（RAM 之外）绝不能污染实现自身 |
| **Rust Heap** | Rust 分配（`__rust_alloc`：Box/Vec/被解释代码的 Rust 分配） | RAM 存储 | 大块连续内存，**托管但不搬迁**（地址钉死，回收靠 Drop 非 GC；现实现 = mimalloc crate 后端，arena/TLAB 自管形态后置 E14） |
| **Native Heap** | native 分配（`libc::malloc` 直调、C 库内部分配） | RAM 之外 | 直通真 libc malloc；VM 不追踪其元数据 |

### 一个地址空间，两种代码都能碰两个堆

从 OS 视角，**整个 VM 在一个连续地址空间内**：Rust Heap 是 VM 向 libc 要的大块内存，Native Heap 是 libc 另外给的，都是真地址。**真实地址模式下，内存访问就是裸宿主 read/write——有没有 AllocId 都一样，字节都在真地址上。** 于是 native 代码能写 Rust Heap（zlib 把结果写进 guest 的 Vec，已验证），解释代码也能读写 Native Heap（拿 C 给的指针解引用——合法程序常有，运行时就该让它成立）。

### AllocId 是什么：检查器的元数据 overlay，VM tier 甩掉

**AllocId 不决定"字节在哪"**（永远在真地址）。它是 **Miri 每分配元数据侧表的钥匙**——init mask（字节是否已初始化）、provenance map（哪些区块是指针）、bounds/liveness。**这三样全是"检查"基础设施**，早期 InterpCx tier-0 曾继承它们；当前 M4 引擎已经没有 AllocId overlay。**fast machine（假设合法）在真实地址模式下几乎不需要**：不查 UB→不需 init mask；不做别名检查、指针即地址位→不需 provenance；不查越界/UAF→不需 bounds。

两种"元数据"要分清：

| | 是什么 | fast VM 需要吗 | 键 |
|---|---|---|---|
| **类型级** | layout/字段偏移/判别式/vtable | **必需**（解释 MIR 靠它） | 按**类型**（tcx layout / C8 冻结进字节码），VM 私有表 |
| **每分配** | init mask / provenance / bounds | **不需要**（检查器 overlay） | 早期 tier-0 按 **AllocId**；当前 M4 引擎已甩掉 |

所以：**裸宿主访问是当前根本路径；AllocId 元数据只是已删除 tier-0 从 Miri 继承的 overlay。**
“解释代码读 Native Heap 靠裸访问”不是特例回退，而是 M4 引擎的统一模型：无 per-allocation
检查元数据、无 AllocId 间接寻址。

分工的动机：`__rust_alloc`（热路径，每个 Box/Vec）进**托管** Rust Heap，为速度/并发/隔离；`libc::malloc`（较罕见、属 native 世界）**直通**真 libc，落 Native Heap——否则 guest `libc::malloc` 的指针交给 C `free` 会崩（Rust-Heap 指针 vs libc free 不匹配）。

### 为什么 Rust Heap 是"托管但不搬迁"（Rust 特有，非照抄 JVM）

- **托管**（拿 JVM 的红利）：RAM 对分配只要求互异/对齐/非空，as-if 允许 VM 自己管。**per-thread arena / TLAB（bump 分配）** 是 JVM 让分配在多核 scale 的招，直接服务生而并发（快路径无锁）。对比"每次 round-trip libc malloc + 建 Allocation + 注册进共享表"——那是解释器做法，是串行化点，慢且反并发。
- **不搬迁**（Rust 现实，与 JVM 分道）：Java 对象可被 GC 移动；Rust Heap **绝不能移动**——ptr↔int、provenance、FFI 直传都依赖地址钉死。所以借 JVM 的**管理骨架**，按 Rust 的**钉地址/无 GC** 改造。这是真正 Rust-native 的设计。

### 真实地址（FFI 与并行的共同地基）

分配的基址 = 其宿主缓冲的真实地址。于是 guest 指针**就是**宿主指针：热路径 load/store 零翻译；FFI 把 guest 缓冲原样传给 native（native 直接读写，见 §7）；并行 tier 原子操作直落宿主地址。这是 §7 FFI 廉价、§5 并行可行的根本前提。

### 隔离保证（鲁棒性）

Rust Heap 与 VM 元数据分池是结构目标，当前 M4 的 guest heap 已由独立分配器管理；但真实地址、
native FFI 与 inline asm 仍意味着 guest UB 可以越界破坏同进程 VM 元数据。它不是 Wasm 式安全
隔离，checked 模式也尚未实现；诚实边界见 current-status。

---

## 5. 线程与并发（RAM 并发的实现）

**核心原则：不 emulate 线程、也不 wrap pthread。用真 OS 线程。** std 已经把 pthread 包好了（`std::thread` → `pthread_create`）；guest 被解释时会**自己**走到 `pthread_create`。我们不重造线程机制、也不在 pthread 外面再套一层——**唯一要"搞点东西"的是：让解释态的 `thread_start` 变成 native 可调用**（见下）。

### std 线程的真实机制（源码实证，sys/thread/unix.rs）

```rust
let data = Box::into_raw(init);                          // Rust Heap 里的闭包
pthread_create(&mut native, attr, thread_start, data);  // 真 libc；thread_start 是 std 的 extern C fn
// native: 真 libc::pthread_t，存进 Thread.id
extern "C" fn thread_start(data) { Box::from_raw(data).init()(); null }  // 有 MIR
fn join(self)          { pthread_join(self.id, null); }   // 真 pthread_join
fn into_pthread_t(self)-> RawPthread { self.id }          // 交出真 pthread_t 给 C
```

### 为什么还要"搞点东西"——不是 wrap pthread，是补 FFI 的反方向

正常编译的二进制里 `thread_start` 是**机器码**、有真地址，OS 线程直接 jump——std 的 wrapper 完整够用。VM 里 `thread_start` 是**解释态 MIR、没有机器码地址**（指针值是占位假地址），而真 OS 线程启动必须 jump 到真机器码。矛盾只在这一点：**native（OS 线程启动器）要调用一段解释态代码。**

这正是抽象机器边界的**反方向**：

| 方向 | 机制 |
|---|---|
| 解释 → native（FFI，已做） | 编组参数，call 真机器码 |
| **native → 解释**（回调/线程体） | 把解释态函数 **materialize 成真 thunk（libffi closure）**，被调时重入解释器 |

所以：**当一个指向 guest 函数的指针要逃逸给 native 时，把它 materialize 成真 thunk。**
thunk 是一小段可执行桥，native 调它时会先取得 Engine 执行租约，再 attach 当前线程的
`Ctx`，最后进入解释器或已发布 JIT 代码。`Thread.id` 仍是真 `pthread_t`，join、futex、
mutex 和线程调度仍由真 libc/内核完成。当前只对 `pthread_create` 和 pthread 线程私有数据
key 的创建、赋值、删除增加生命周期桥：它不模拟 pthread，只记录“回调已经登记但还没
执行或撤销”的时期，防止 close 提前释放 Engine。普通回调和 P1（交给原生代码调用的 guest
函数入口）即使目标函数已 JIT，
也继续使用每 Engine 独有的稳定 closure 地址，因为原生代码可能比较或长期保存函数指针。

### 拦截边界（对应 §7）：只拦有明确生命周期合同的窄入口

拦截发生在 guest 的 foreign-call 边界，以及 mirvm 自己生成的 native archive 桥内。
越过这些已知入口后不能假装看见第三方库内部状态：
- guest（std 或裸 `extern "C"`）调 pthread_create → 先登记一次性延迟持有，再调用真 libc；
  线程入口开始或创建失败时清账。✓
- guest `dlopen` 取真函数指针直接 call → 走 FFI，回调若指向解释态函数亦走 thunk。✓
- mirvm 自产 archive 内调 pthread → 隐藏槽转到同一生命周期桥，线程仍是真 native 线程。✓
- guest 或自产 archive 调 `signal`/`sigaction`/`raise` → 进入带 Engine owner 的进程级信号
  登记表；自产 archive 只包住这三个已知符号，不外推到任意第三方动态库内部。✓
- 任意第三方库私自保存回调且不给完成/撤销事件 → 无法推知安全释放时刻；closure 保留到
  进程结束，Engine close 后只留下稳定的关闭身份。

### 真线程现状与已淘汰的 tier-0 对照

| | 当前 M4 真线程模型 | 已删除 tier-0（协作式） |
|---|---|---|
| pthread_create | 生命周期桥登记后调用真 libc（thread_start = 真 thunk） | 建 GuestThread，假线程复用一条宿主线程 |
| into_pthread_t / 交 C | 真 pthread_t，**能用** | **假值，一用就废——合法程序跑错** |
| join/futex | 真 libc 直通 | 自建等待队列（emulation） |

> ⚠️ 重大修正（2026-07-05，看过 std 源码后）：**协作式 ThreadManager/调度器/futex 队列是 emulation，且被 `into_pthread_t` 判为错误实现（非仅慢）**——一个合法 Rust 程序取出 pthread_t 交给 C 时，协作式没有真 pthread_t。不围绕它设计，是要推倒的。
>
> 当时曾提出 GIL-over-真线程作为 tier-0 的正确过渡，但 M4 完成后 tier-0 被直接删除，因此这条
> 迁移路线没有实施。保留此论证是为了说明为什么不能把协作式线程重新当作产品方案。

### 为什么自研引擎能真并行

InterpCx 的内存 map、MonoHashMap（RefCell 遍地）不 Sync；M4 通过冻结执行元数据、`Shared` 发布后
只读、每线程 `Ctx` 与真宿主原子摆脱了这个限制。引擎自身的并发安全由 TSan gate 持续验证。

### Engine 关闭与宿主线程回收

Engine 的公开状态是 `Running -> Closing -> Closed`；内部 `Finalizing` 表示普通调用、
延迟回调和 native 析构已经结束，正在解除 Shared 所拥有的重资源。每次执行先取得
`ExecutionLease`（执行租约），也就是“本次调用还在使用 Engine”的计数凭据；pthread 已
接收但尚未启动的回调、线程私有数据析构器和被 native catch 暂停的 MIRVM 异常使用
`DeferredHold`（延迟持有）覆盖没有活动调用栈的空窗。`close` 禁止新的普通入口，但允许
原调用链、已登记回调和析构函数完成；`wait_closed` 等它们清账。本线程仍在该 Engine 的
调用链里时，等待会明确返回错误，避免等待自己退出。
构造函数的受控失败会转成 `Result` 并启动关闭；析构函数已处于不可回滚的拆除
阶段，任何 guest、引擎、foreign 或宿主 Rust 异常逃出都固定诊断后 `abort`，不能
续传并把 Engine 留在 `Closing`。

`Ctx` 是每个宿主线程进入某个 Engine 时使用的执行上下文。宿主线程的 TLS 只保存一个
`CtxSlot`（上下文槽），Engine 以弱引用登记这些槽。所有租约退出后，finalizer 会从任意
收尾线程清空槽内的 `Ctx`；因此长期不退出的线程不会继续持有 guest TLS、帧 ByteRegion、
frozen 或整份 Shared。槽本身是很小的空墓碑，等宿主线程退出再消失。

### Rust 三红利（并行 VM 比 JVM 当年容易）

无 GC；内存模型现成（C++20）；safe 代码类型系统保证无数据竞争 → 只需保护**实现自身**状态。

### 历史偏差：tier-0 的弱内存序不可见

已删除的 GIL/协作 tier-0 会把操作串成单一全序。当前 M4 使用真线程和宿主原子，不应再把该
偏差写成现状；并行程序的差分仍必须使用输出不变式，不能逐字节比较时序敏感输出。

---

## 6. 执行引擎（RAM 计算的执行，分 tier）

| tier | 是什么 | 状态 | 定位 |
|---|---|---|---|
| **历史 bootstrap** | fast Machine on rustc `InterpCx` | **已删除（2026-07-09）** | M0–M2.5 的探索工具；代码只在 Git 历史中，不再是 oracle 或运行模式 |
| **typed-bytecode 解释器** | MIR → 冻结 IR → tree-walking 执行 | **M4 完成，差分 oracle 本体** | tcx-free、真线程、FFI/unwind/thunk；语义范围与缺口见 current-status |
| **asm stub** | GAS wrapper → `.so` → native call | **M5.0 完成** | 解释器可调用的局部机器码机件，不等于方法级 JIT |
| **方法级 JIT** | 从冻结引擎字节码生成 Cranelift 机器码 | **M5.3/M5.4a–d/M5.5 全落地（默认开启）** | ABI 全形态、unwind 产品化、准入三表穷尽、vmctx T 骨架定稿（docs/open-issues.md T1–T3 已闭合） |

InterpCx 曾帮助项目快速探索 RAM 边界，但其不 Sync 与 AllocId overlay 不适合作为产品地基。
M4 完成后它没有“退居 oracle”，而是被删除；当前差分 oracle 是同源 native 编译执行。

**M4 当前采用 Model A 的 tree-walking 形态**：每个 guest 调用活动对应一个宿主
`interp_frame` 递归帧；解释态局部字节位于随递归 LIFO 推进的 mmap ByteRegion，而非直接内联
native 栈。调用活动与局部存储是两条正交轴。完整 A/B 选择理由、旧模型价值与重开条件见
[docs/designs/frame-stack-models.md](docs/designs/frame-stack-models.md)、[docs/designs/frame-abi-bytecode.md](docs/designs/frame-abi-bytecode.md)
和 [docs/decision-history.md](docs/decision-history.md)。

mode B 的 `.mirvm` v4 包是可重复实例化的映像。`Package::load` 把源文件复制为不可变
进程内快照并完整校验；每次实例化再建立独立 frozen/TLS、自产 native/MC 映像和 P1
closure。包内的 `LinkAddr` 是逻辑链接地址，不是固定运行地址；`LoadMap` 在每个 Engine
启动时把它翻成该实例的真实地址，再应用 `FrozenReloc` 修补静态指针。P1 是交给原生代码
调用的 guest 函数入口；旧 P1 地址永不复用，所以关闭 A 后的陈旧指针不会碰巧调用后来
创建的 B。

---

## 7. 抽象机器边界（FFI / intrinsics / native）

**FFI = 抽象机器的边界。** 边界之内实现 RAM 语义；边界之外（native）RAM 不建模，我们只移交控制权。这一条统一了之前所有关于"什么该 shim"的含糊。

一个 extern 调用，按它相对 RAM 的位置分类。**注意：guest 始终被解释，所以这些 foreign 调用我们永远看得见；"直通"指 handler 把活转交给真 OS，而非"看不见"。native 代码内部的同名调用（如 zlib 内部 malloc）是机器码打到真 libc，我们看不见也不该管（RAM 之外）。**

1. **RAM 内建 / 由 handler 服务**：
   - **intrinsics**：RAM 计算的一部分，VM 原生理解（如 JVM 字节码指令）。
   - **分配器**：拦截动机是**归属 + 元数据**。`__rust_alloc`（Rust 分配器，编译器合成、无 MIR，MIR 层的 foreign-call 边界拦截）→ 落**托管 Rust Heap**（速度/并发/隔离），解释器为其建元数据以解引用。`libc::malloc`/`calloc`/`free` 等 native 分配器 → **直通真 libc、落 Native Heap**（否则 guest malloc 的指针交 C free 会崩）。解释器对 Native Heap 指针回退裸宿主访问（§4）。**"一处"= 托管 Rust Heap 的分配器入口；native 分配不进这一处，是另一条真 libc 直通路。**
   - **unwind**：M4 复用宿主 panic/unwinder；解释器 raw catch 与 JIT landing pad 按当前捕获到的
     异常指针决定执行还是跳过 guest cleanup。`FrameGuard` 只恢复解释帧的 region、shadow 与
     深度，不负责判断异常类别。“跨平台无痛”仍需逐平台验证，不能从 Linux 基线外推。
   - **线程**（§5）：**不 emulate、不 wrap pthread，用真 OS 线程**。解释态 `thread_start`
     逃逸时物化成 libffi closure thunk，入口通过 TLS attach 当前线程的 `Ctx`。这项机件支持
     pthread 与普通 native 回调。
   - **signal**：传统异步 signal 使用独立机制，不走 libffi closure。每次 guest 注册物化固定
     22 字节 RX 桩；内核信号帧只做固定 TLS 读取与原子登记，不加锁、不分配，也不执行 guest
     或展开。进程定向事件进入注册 Engine 的 inbox（待处理信号箱）；`SI_TKILL` 是内核表示
     “这个信号发给指定 pthread”的来源码，这类事件进入目标 pthread 的稳定 cell（槽）。槽按
     `(目标 pthread, registration generation)` 建立；registration generation 指一次成功的
     handler 安装，因此同一传统信号可以在同一代内合并，但替换前后的注册不会混在一起。
     目标线程在块入口/返回等安全点取得执行租约和全新 activation（本次执行上下文）后调用
     handler。真实 libc `raise` 也产生 `SI_TKILL`：未阻塞时，受控 `HostRaise`（MIRVM 的
     `raise` 入口）返回前排空本线程
     事件；阻塞时事件继续留在内核，`sigwaitinfo` 可见真实来源码。close 先关闭登记门、按进程级
     owner 链恢复仍存活的上一层或原生 disposition，再等待已进入桩的 frame 和已接收的目标
     线程事件；它不能换线程代跑。线程退出把 pthread 全局最后一轮 TSD 按原始 key 号扫描，
     与本线程 signal cell 交替排空，再在内核阻塞可捕获的传统信号、复查并关闭线程 inbox；
     当前线程若仍有自己的目标事件，`wait_closed` 返回而不睡住等待自己；退出收口最后释放
     已不能再进入第五轮的 TSD 残值。`signal`/`sigaction` 查询和 `oldact` 仍反译为 guest 地址。
     固定桩、registration 和线程 cell 保留到进程结束；owner 关闭后若原生代码回装旧桩，裸
     内核投递以 `_exit(70)` 失败，受控 `HostRaise` 报 `EngineFault(70)`。同步故障、realtime、
     `SA_SIGINFO/SA_ONSTACK/SA_NODEFER/SA_RESETHAND` 仍响亮拒绝；进程定向外部信号的可见
     延迟以 owner Engine 安全点为界。


2. **纯直通（FFI 到系统 libc）**——真资源、不涉及解释态实体：open/read/clock/getrandom、数学函数（libm）。**用系统 libc**（`dlsym(RTLD_DEFAULT)` + 按 `-l` 指令 dlopen）；target==host 保证 ABI 逐位一致。这批现在是手写 shim（历史包袱），neat 的终态是通用直通通道——但那是 polish，不急。

3. **inline asm**——不是 call，是嵌在函数中间的不透明机器码，无符号无调用边界。**永远无法在一处代理**，只能：模拟具体模板，或按名拦截外层函数（我们对 sqrt/std_detect 就是后者）。VM 编码调用时须知：foreign call 有边界，inline asm 没有。

### native 写 guest 内存

zlib 这类 C 库 FFI 进去后是真机器码，其内部的 malloc/memcpy 打到真 libc，**我们不拦也无需拦**（在 RAM 之外）。边界就是那一次 FFI 调用：我们只管**递给它的**缓冲（真实地址让 native 直接写回，解释器看得见）。Miri 同款限制：native 自 malloc、返回指针指望被解释代码解引用 → 失败（无元数据）；算力型 C 库不这样，无痛。

### 沙箱（安全 vs 虚拟化）

安全的 choke point 是**进程级 OS 沙箱**（seccomp-bpf/namespaces 罩住 mirvm 进程）——给 native 程序用了几十年的机制，不管调用走 shim 还是 FFI，syscall 都在内核那道关被拦。**mirvm 不重造安全拦截。** mirvm 层钩子只用于 OS 沙箱表达不了的**虚拟化**：假文件系统、路径重定向、资源计费、区分"guest 调 open"vs"解释器自己读缓存"。即：默认直通 + OS 沙箱管安全 + mirvm 钩子按需虚拟化。

---

## 8. 设计原则与决策账本

### 原则（从抽象机器脊柱推出）

- **P0 实现 RAM，用 as-if 换自由**：正确性 = 忠实 RAM；一切内部实现（分配器、tier、调度）只要保持可观测等价即自由。
- **P1 VM 作者视角**：遇抉择问"JVM 类系统软件怎么做"，不问"Miri 怎么做"。Miri 是检查参考、shim/intrinsic 的代码参考，但**不是心智模型来源**。
- **P2 复用前端，自研引擎**：绝不重建 trait solver/类型系统/前端（RAM 的加载器）；执行引擎全自研（RAM 的执行器）。
- **P3 不做 borrowck / 不做 UB 检测**：那是前端与 Miri 的职责；我们假设程序合法、追求快。
- **P4 边界即 RAM 边界，绝不广泛拦截 native**：界内（解释代码的 foreign-call 边界）实现语义，界外（native 代码、裸函数指针 call）我们看不见也**不试图拦截——那是工程灾难**。真实地址让边界"软"而廉价（两种代码共享一个地址空间，见 §4）。
- **P5 不 emulate，用真 OS**：能用真 OS 原语（线程/futex/文件/时钟）就直接用，只在 foreign-call 边界"搞最小的一点"（如 pthread_create 插蹦床）。emulate 一套机制（如自建线程调度）是反模式——既慢又常常对合法程序跑错（如 into_pthread_t）。VM tier 以真并行为默认；tier-0 用 GIL-over-真线程过渡，非协作 emulation。
- **P6 Unix 优先**：unwinding/FFI 都 Unix 优先，macOS 次之，初期不支持 Windows。
- **P7 OS 交互集中在 `os::` 边界（2026-07-18 已物理收口）**：一切触及真 OS /
  系统库的东西都经 `src/os/` 原语层——**非 os 域的 `libc::` 触点已机械清零**
  （grep 门禁，仅剩注释；spikes 冻结原型不在门禁内）。草图中的多数文件
  （fs/time/net/rand/math/ffi）从未成为独立触点——全 syscall 族经
  `os::process::syscall` 变参单点直通、FFI 在 `vm/engine/ffi.rs`（业务）调
  `os::dll`（原语）——故落地形态比草图小，按实收口如下：

```
src/os/
  mod.rs        — 边界契约（leaf/原语不裁决/直通优先）+ 平台选定
                  （非 linux compile_error!，与 global_asm x86_64 硬门同前提）
  linux/
    mem.rs      — mmap/mprotect/munmap + 偏好固定基址（frozen/codearena/frame 归并）
    thread.rs   — pthread TLS 族 + 栈界探测 + attr 栈尺寸 + /proc 线程计数
    signal.rs   — signal/sigaction 原语、disposition 查询/安装与 SEGV dump
    dll.rs      — dlopen/dlsym/dlerror/dlinfo 装载基址（+ 测试档 open_with_flags/close）
    process.rs  — getenv/write/strlen/fork/atexit/syscall 变参单点 + JIT libcall 地址族
```

  guest 语义裁决（fork 守卫、信号白名单、sigaction 改拷贝文案）仍在引擎业务侧，
  os:: 只供原语——OpenJDK `os::` 同款纪律。固定基址数值另提升为
  `vm/engine/addrlayout.rs` 共享常量层（frozen/codearena/ir 白名单三方共享）。

### 约束账本（按时间追加的历史账本）

> 下列 C0–C13 保存当时的约束与理由，不保证每句仍描述当前代码。例如 C5/C6 只属于已删除
> tier-0，C11/C12 的部分未来承诺尚未实现。当前归并结论与重开条件见 decision-history。

- **C0 抽象机器脊柱**（§1）：正确性契约 + as-if 自由 + FFI=RAM 边界。一切决策的根。
- **C1 生而并发**（§5，第一红线）：产品引擎默认 1:1 真并行。早期曾计划用
  GIL-over-真线程迁移 tier-0，但 tier-0 后来直接删除。依据：现代 Rust 负载天然并发，
  永久 GIL 不满足产品目标。
- **C2 并发内存模型**（§4）：三分（元数据私有隔离 / Rust Heap 托管非搬迁 per-thread arena / native heap 外）；原子直落宿主指令；真实地址；引擎状态三态（每线程私有 / 发布后不可变 / 显式同步）。**内存访问 = 裸宿主 read/write（有无 AllocId 都在真地址）；AllocId 是 Miri 的每分配元数据 overlay（init/provenance/bounds），fast machine 不需要，VM tier 甩掉——省掉间接寻址（fib(27) 慢因之一）。类型级元数据（layout/offset/vtable）另说，按类型冻结进字节码（C8），永远需要。**
- **C3 Rust 三红利**（§5）：无 GC、内存模型现成、safe 无竞争。
- **C4 guest UB 立场**：unsafe 竞争/UAF = 宿主竞争/UAF，与 native 一致（fast 立场）；隔离区（保护实现自身）+ 可选 TSan 后置。
- **C5 tier-0 硬约束（historical）**：InterpCx 不 Sync，限制了早期 bootstrap；tier-0 已删除，
  不再作为 oracle 存在。
- **C6 tier-0 弱内存序偏差（historical）**：协作模式 SC 执行、无重排、调度确定；当前 M4
  真线程引擎不继承这一实现偏差。
- **C7 性能基准**：regex 编译（DFA + Unicode 表）42s 是 1 号基准；VM 评审须给该案例预估收益。
- **C8 真线程架构（§5，看过 std 源码后定，2026-07-05）**：**不 emulate、不 wrap pthread，用真 OS 线程**。std 已 wrap pthread（`thread_start` 是 std 的 extern C fn，`Thread.id` 是真 pthread_t）。VM 唯一要做的是 **FFI 的反方向**：解释态函数指针逃逸给 native 时 materialize 成**真 thunk（libffi closure，Miri `build_libffi_closure` 同款）**。于是 pthread_create **纯直通**（非拦截/wrap），join/into_pthread_t/as_pthread_t/futex/mutex 全走真 libc。同一 thunk 机制解决 qsort 等普通 C→Rust 回调；当时把 signal 也归入此路，现已由 §7.54 推翻为固定登记桩 + 安全点派送。JIT tier 下 thread_start 是真机器码，thunk 消失。tier-0 不 Sync → GIL-over-真线程过渡（into_pthread_t 成立，结构同 VM tier，去锁即并行）；**协作式是扔掉的 emulation**。引擎线程安全三招：**降低时元数据冻结**（layout/偏移/vtable 烘焙进字节码，运行期不触 tcx）、**降低在加载相 + JIT 后台服务线程**（HotSpot compiler-thread 同构，为 M5 后台 JIT 铺路）、Rust Heap per-thread arena（**TLAB，直接上**）。**并发架构完整设计见 docs/designs/concurrency-arch.md**：状态三分（每线程私有/发布后只读/显式同步）；唯一敌人=tcx 不 Sync——**tcx 是加载相的事、执行相永远无 tcx**（Rust 单态化静态→eager 降完全部字节码；模式 A 有 tcx 但关在单线程加载相、模式 B .mirvm 根本无 tcx；JIT 后台服务也不碰 tcx）；atomics 引擎不介入（真原子指令真地址）；spike 验收=引擎自身过 TSan（guest 竞争排除，那是 guest 责任）。开放问题：发布协议、TLAB remote-free 细节、隔离强度。
- **C9 FFI/native 写内存**（§7）：真实地址让 FFI 零编组；一个地址空间两种代码碰两个堆（native 写 Rust Heap；解释器对 Native Heap 指针回退裸宿主访问——运行时可，检查器不可）；值表示不能假设"只有解释器写内存"；libm 逃逸宿主直算通道 VM tier 要保留（intrinsic 化）。
- **C10 边界与拦截（§7，2026-07-05 三次修正）**：拦截只在**解释代码的 foreign-call 边界**，**绝不广泛拦截 native 操作（工程灾难）**。动机——**归属+元数据**（`__rust_alloc`→托管 Rust Heap，MIR 层拦；`libc::malloc`→真 libc→Native Heap 直通）/ **真线程最小介入**（只 pthread_create 插蹦床，余皆真 libc 直通）/ **纯直通**（真资源，handler 转发真 OS）。inline asm 无调用边界，只能模拟或函数级拦。native 内部/裸指针调用看不见，记录为限制。沙箱：OS 级(seccomp)管安全，mirvm 钩子仅虚拟化。
- **C11 帧栈模型 = A（2026-07-05 定，详见 docs/designs/frame-stack-models.md）**：greenfield +
  JIT 硬约束下选 A（HotSpot/V8 式），非 B（CPython/Lua）。M4 落地的是 tree-walking A1：
  guest **调用活动**在 native 栈，局部字节在 slaved ByteRegion；不能简写成“所有 guest 帧字节
  都内联 native 栈”。B 的完整论证与重开条件仍保留在 decision-history。
- **C12 JIT 后端 + 字节码 + 分发（2026-07-05 定，详见 docs/designs/frame-abi-bytecode.md §7.5）**：
  - **JIT = Cranelift，藏在 `JITBackend` trait 后**（P7 同纪律，copy-and-patch 备选）。为 JIT 而生、≈10× 快于 LLVM 编译、质量≈debug build（正合目标）；**cg_clif 已趟通 MIR→Cranelift+Rust ABI+unwinding**，复用。耦合可控：抽不掉的只有调用约定（=Rust ABI，我们本就用）+ unwind 模型（=Rust 原生 landing-pad，逃不掉），**都非 Cranelift 特有**；不为它牺牲内存/线程/元数据模型。
  - **unwind = 已实现的 A**：解释器借宿主系统展开，Cranelift 生成 landing pad/LSDA，
    两者复用 Rust personality。MIRVM 自有异常类只负责区分 guest panic、`EngineFault`
    和所属 Engine，不另写 personality 或自研栈行走。每个捕获点按实际异常对象分类，线程内
    token 栈只核对 owner 与 LIFO 消费顺序（现行裁决见 decision-history §7.52）。
  - **字节码贴近 MIR**（不下沉 CLIF）→ 解释器与 JIT 共享 MIR 级真理源、复用 cg_clif。**两级结构**：mirvmc（rustc 前端全 check → **Stable MIR/rustc_public + serde** → .mirvm 分发件，= .class/.jar 类比）；运行期"class loading"（按 target 冻结 layout C8 → 解释器寄存器字节码 + 喂 Cranelift，每平台一次缓存）。版本绑定诚实（classfile 版本号式，semver 转换）。
  - **分发格式 = 多 target 打包（定，2026-07-05 用户确认）**：**单产物跑任意 target 对完整 Rust 理论上不可能**（cfg 编译期按 target 剪枝=字面不同的程序 + usize/可观测 layout/const-eval；Java 能因无编译期 cfg/JVM 定 layout/定长基本类型，Rust 三条全违反=语言固有）。**采纳 fat artifact**：mirvmc 对 N 个 triple 各跑前端、打包 N 段（`.mirvm` 容器 = target 索引 + 各段 Stable-MIR），运行期挑匹配段 load → 消费端零工具链、覆盖常见平台、运行期可 JIT。os:: 每平台 build 时选定，与分发格式无关。
  - **解释帧局部终裁（2026-08-12）**：slaved ByteRegion 是正式方案；JIT 编译帧已由
    Cranelift 使用 native 栈与 SSA。alloca 不再是必做终态，只在真实解释器负载证明端到端
    收益后重开（decision-history §7.49）。
- **C13 VM 鲁棒性 / guest 打穿 VM（2026-07-05，详见 concurrency-arch.md §6）**：真实地址下 guest 与 VM 共享地址空间，故 guest **unsafe UB / FFI 缺陷 / inline asm** 能写 VM 自有内存 → 崩。**但 safe guest 代码证明上做不到（C3），只有 UB/native 缺陷能触发**。根本张力：真实地址与 Wasm 式廉价封闭不兼容、无免费午餐。**分层（用户定：砍 L2 MPK[太 arch-specific]/L4 沙箱[out of scope]，聚焦 L1+L3）**：L0 类型系统（白送，覆盖 safe 代码）；**L1 结构隔离**（VM 内存放已知地址区+guard page，且让 L3 检查退化成单次范围比较）；**L3 checked 模式**（opt-in，不可信/LLM 用）——**关键：Rust 类型系统让它比 Wasm 便宜**：safe 引用访问证明上有效不查，**只查 raw 指针解引用**（MIR 按指针类型区分，检查点极少）；检查=region check（addr∈guest 区，compare+branch predicted-taken）；**JIT 能插检查**（我们做 MIR→CLIF 降低，Cranelift 编我们给的 CLIF，机器码不脱掌控）；借 JVM（BCE 静态消除、deopt/profile；implicit-trap guard-page 对我们难因 guest 内存散=Wasm Memory64 问题）；诚实界（只查 raw 解引用漏"洗进 &T"路径，但野写几乎都走 raw 指针，性价比高；checked 是 lite→full Miri 的谱）。**Model-A 相互作用**：slaved 区让检查便宜，alloca 需要更贵的逐帧追踪；2026-08-12 已终裁解释器正式保留 slaved，alloca 只作真实性能证据触发的候选（decision-history §7.49）。checked 与局部存储仍通过 `GuestMemory::contains(addr)` 的窄接口解耦，不把安全语义硬编码进存储实现。profile checked 开销、opt-in。与 §7 OS 沙箱同源。
- **FFI unwind 合同（2026-08-12）**：direct libffi 调用、native fn pointer、callback
  thunk 与 P1 条目都保全并消费源 `C/System { unwind }` 属性。普通 C 是终止边界；
  C-unwind 允许原 Rust panic/C++ exception 穿过并执行 cleanup，不做跨语言异常转换。
  guest panic 的内层对象继续归 guest 标准库，但展开时包在带 owner 的 MIRVM 自有异常
  外壳中；guest catch 只消费本 Engine 的 panic，未捕获 payload 也交回 guest std 清理。
  每个解释器 raw catch 和 JIT landing pad 都按当前异常指针分类：只有当前对象确为
  `EngineFault` 才跳过 guest cleanup。线程内带 nonce 的 LIFO token 栈只记录 owner 和消费
  顺序，不参与 cleanup 决策；因此 native catch 暂停外层故障后，同线程重入 guest panic
  仍会 cleanup，嵌套 `EngineFault` 也能独立入栈和消费。lower 精确标记真实 `lang_start`
  的 `MainPanicBoundary`，每次 `run_main` 的状态栈再把 main panic 与正常 `Termination`
  返回 101 分开；解释器、JIT、pack 与 verifier 共守该合同。C++ exception 到达 guest
  `catch_unwind` 时按固定 rustc 终止；若直接到达 Engine 顶层，则不消费、不改写，外层
  C++ 仍可按原类型捕获。详见 `docs/designs/c-unwind-contract.md` 与 decision-history
  §7.50-§7.52。
- **嵌入生命周期合同（2026-08-13）**：最后一个 Engine handle drop 或显式 `close`
  进入 Closing；执行租约与延迟持有共同阻止过早 Finalizing。pthread start 与线程私有数据
  （TSD）析构器
  有明确完成或撤销事件，close 必须等待并清账；没有这类事件的任意 library-retained
  callback 不得让 `wait_closed` 永久等待，而是保留进程期 closure 和小型关闭墓碑。native
  映像先重定位、填 P1/GOT/bridge 槽，再逐实例运行 ctor；只有 ctor 全部完成才在 close
  逆序运行一次 fini。ctor 受控异常分类成 `Result` 失败；fini 不允许异常逃出，
  任何展开都固定诊断后 `abort`。已发布 JIT/MC/native 代码及 unwinder 表保留到进程结束；Shared、
  frozen、guest TLS、Ctx 和未发布资源按 Engine 回收。详见 decision-history §7.53。
- **嵌入 signal 合同（2026-08-13）**：进程级 registry 保存每个 signal 的原生基线和按安装
  次序排列的 Engine 节点；guest 可见 action 与内核固定桩分开保存，查询/`oldact` 不泄漏桩
  地址。进程定向事件进 owner inbox，`SI_TKILL` 线程定向事件进目标 pthread 的逐注册代际
  cell；close 等待二者和在途 frame，不能把线程事件拿到别处代跑，线程退出负责先阻塞再排空。
  自产 native archive 的 `signal`/`sigaction`/`raise` 经隐藏 owner 槽进入同一合同，未阻塞的
  `raise` 返回前完成 handler。固定桩、registration 和 cell 进程期保留；关闭后回装旧桩会以
  状态 70 明确失败。详见 decision-history §7.54-§7.55；拒绝边界见 open-issues R1/R21。

### 工程决策

- **nightly 锁定 + 定期 bump**：rustc_private API 随 nightly 漂移；`rust-toolchain.toml` 锁日期版本（当前 nightly-2026-07-02 / rustc 1.98.0-nightly），每月 bump；rustc 交互隔离在少数模块。关注 Miri 同步提交作迁移指南。
- **依赖以 `-Zalways-encode-mir` 构建**：rlib 携全部函数 MIR；内容寻址全局缓存（`$HOME/.mirvm`，`MIRVM_HOME` 改址）。
- **engine 是 library，CLI 是薄壳**：公开的安全加载入口只有 `Package::load`；从可信包
  建 Engine 的 `Package::instantiate` 仍是 `unsafe`。既有 Engine 上的 `run_main`、状态、
  `close`/`wait_closed` 是 safe，执行出口用 `RunOutcome`/`RunErrorKind` 区分正常返回、guest
  panic、关闭和引擎错误。内部 `Shared` 不公开；手工 Module 与无类型 raw export 收在
  `vm::engine::raw` 的 unsafe 面。完整 safe typed export API 仍未建立。
- **借鉴 Miri 代码**（MIT/Apache-2.0，保留 attribution）：shim 结构、intrinsic 清单、native-lib 机制是最佳代码参考——但**仅代码，不是心智模型**（P1）。

---

## 9. 里程碑与历史 tier-0 实现日志

### 里程碑

- **M0 工具链打通**（✅ 2026-07-03）：rustc_private 驱动编译源文件、定位 entry fn、dump MIR。
- **M1 最小 RAM 实现**（✅ 2026-07-03）：fast Machine on InterpCx，std 程序端到端。差分 5/5（fib、String/Vec、HashMap、panic+exit 101、catch_unwind+Drop）。
- **M2 吃下真实生态**（✅ 2026-07-05）：cargo 依赖图、proc-macro、frontmatter 脚本、.init_array（args/env 转正）、时间/文件/malloc shims。serde_json+rand+regex 与 native 逐字节一致。
- **M2.5 生态补全**（历史完成，2026-07-05）：
  - ✅ 线程（tier-0 协作实现，§5）：pthread/futex/nanosleep/TLS 析构；差分 12/12。
  - ✅ 真实地址内存（§4 前身）：MirvmAllocBytes 真对齐 + prepared 破指针环。
  - ✅ libffi FFI（§7）：dlsym + 编组 + native 写内存暴露；libz-sys 真 C 库往返与 native 逐字节一致（diff 14/14 + cargo 3/3）。
  - corpus 调研随后成为 M4/M5 的输入；其 2026-07-05 快照见 docs/corpus.md。
- **RAM-SPEC 文档**（✅ 2026-07-05 草稿，docs/designs/ram-spec.md）：抽象机器语义契约——正确性契约、定义度四级、RAM 边界、as-if 自由、声明的偏差、与 native/Miri 关系。mirvm"事实标准实现"的书面承诺。
- **M3 产品面**（未实现/后置）：daemon、agent API、资源治理与正式沙箱。
- **M4 字节码 VM**（✅ 2026-07-10）：自研 typed-bytecode、tcx-free tree-walking engine、
  FFI/unwind/真线程/TLS/thunk；tier-0 同期退役。实际结果见 docs/history/m4-log.md。
- **M5.0 asm-stub 工厂**（✅ 2026-07-11，复审 2026-07-12）：有限 x86_64 inline asm
  加载相物化；实际结果见 docs/history/m5-log.md。
- **M5.1 语义轨收口**（✅ 2026-07-12）：addcarry/subborrow 已使 numbigint 转绿，
  xgetbv 已 native 差分，pshufb/SHA helpers 已使 sha2 转绿；静态归档产品接入与 ecosystem
  补面分别使 blake3/ecosystem 转绿；diff_cargo 3/3。signal guest handler 是独立明确 XFAIL。
- **M5.3 方法级 JIT 骨架**（✅ 2026-07-15）：J1 基座 + 翻译器标量子集 + CFI；
  **M5.4a/b 翻译器**（✅ 同日）：帧模型 v2、标量全集 + 128 位族 + atomics。
  **M5.4c/d**（✅ 2026-07-21）：ABI 泛化全形态 + 五调用助手 + unwind 产品化
  （双 CIE 全覆 LSDA）+ 准入三表穷尽（SIMD 族经 interp 共享本体助手）。
  **M5.5**（✅ 2026-07-21）：vmctx 终裁（T 骨架生产定稿 + 复测双触发器）+
  m5_gate6 收口全绿——M5 战役全收（docs/decision-history.md §7.19/§7.20）。
  施工日志见 docs/history/m5-log.md。
- **M6 冷启动/轨 C**（✅ 2026-07-14~15）：S1 小件包、S2 依赖剪 codegen、S4 std
  预降底座、S3′b A2 纯化聚合 deps-image。编号说明：原愿景「M6 REPL/Notebook」被
  轨 C 占用，REPL 未立项（docs/open-issues.md D11）。施工日志见 docs/history/m6-log.md。
- **M7+ safe typed 嵌入绑定 / REPL/Notebook**（后置；真实 Package/Engine 生命周期已落地，
  剩余信任边界见 E22）。

### tier-0 实现日志（历史：如何在 rustc 解释器上 bootstrap 出 RAM 实现）

> 这些是已删除 tier-0（InterpCx）阶段的具体实现与踩坑，只用于理解决策来源。路径和偏差均不
> 描述当前代码；有些边界洞察被 M4 继承，有些已经被后续证据推翻。

**结构**：`src/interp/{machine,eval,shims,intrinsics,helpers,addrs,alloc_bytes,threads,native,mono_map}.rs`；`src/cargo_shim.rs`（cargo 集成）；`src/sysroot.rs`（自建 MIR sysroot）。engine 在 lib，CLI 薄壳。

**M1 踩坑**：`__rust_no_alloc_shim_is_unstable_v2` 分配前哨兵（单独识别为空操作）；`rust_begin_unwind` 是 core 视角 foreign fn（需 lookup_exported_symbol 按名在全链接 crate 找 MIR）；Linux std 的 getrandom/statx 走 weak linkage（extern static 值 = 合成函数指针，`ExtraFnVal=Symbol`）；`#[track_caller]` 的 caller ABI 末尾假装有 Location 参数不真传（call_function 须传 with_caller_location）；`assert_inhabited` 系无 fallback（fast machine 直接跳过）；panic 穿出 main = 引擎 "unwinding past topmost frame" → eval 翻译成退出码 101。

**M2 踩坑**：裸 "rustc" 被 rustup 按 cwd 解析 → wrapper 与 sysroot builder 必须钉死 rustc 版本，否则缓存哈希乒乓重建；std_detect CPUID 内联汇编 → 按 DefPath 拦截 detect_features 返回全零（走标量路径）；statx 双保险回 ENOSYS；runner 剥 `--error-format/--json`、跳 CARGO_MAKEFLAGS。cargo 集成三阶段（phase_cargo 注入 wrapper+runner+独立 target → phase_wrapper host 透传/target 加 MIR sysroot/最终 bin 写 JSON 假二进制 → phase_runner 读 JSON 驱动解释）。

**真实地址内存踩坑**：`Box<[u8]>` 只保证 1 字节对齐 → 换 MirvmAllocBytes（真对齐，size 0 也占 1 字节保唯一）；全局分配的指针环 → prepared 机制（先预分配零缓冲取地址，材料化时取走同缓冲拷内容）；解释器读 vtable 走 tcx 查询从不读字节 → 函数/vtable 泄漏 1 字节占位即可；死分配地址被宿主复用 → 反查表 (addr,id) 双键防错删。

**FFI 踩坑**：compiler_builtins 的内联汇编 sqrt 遮蔽 libc → libm 37 函数宿主 f64 直算（apfloat↔f64 走 Float::to_bits/from_bits），排在导出符号解析前；未引用的 -sys crate 链接指令被 native 丢弃（测试改直接调 libz_sys 符号）；native 写内存 → 对指针实参可达可变分配做 process_native_write。

**线程踩坑**：阻塞 shim 先写结果再阻塞（futex 推测写 0，超时改写 -1+ETIMEDOUT）；pthread key 析构在线程根帧返回后逐个压 dtor 帧；spin_loop→yield；线程结果传递（Packet+catch_unwind）是被解释的 guest 代码，panic→join Err 白捡。

**性能数据（release mirvm，全缓存热启动）**：纯 std 脚本 0.24s；serde_json 项目 0.26s；regex 编译 42s（C7 头号靶子）；fib(27) 解释 1.6s vs native debug 4ms。mirvm 自身必须 release（debug 慢 ~7×）。

**版本漂移（nightly-2026-07-02）**：`MachineStopType` 精简为 Any+Display+Debug+Send；帧压栈走 `init_stack_frame`+`ReturnContinuation`；`write_mir_pretty`→`MirWriter`；`catch_with_exit_code`→`ExitCode`；`Linkage`→`rustc_hir::attrs`；throw_* 需 `feature(yeet_expr)`。

**tier-0 残余偏差 / 已知错误**：主线程名 `<unnamed>`（非 main）；退出不跑 rt cleanup 与主线程 TLS 析构；getrandom 确定性（--real-random 留 M3）；内存尚未按 §4 分池隔离。**已知错误（非仅偏差，待重做）**：① 线程是协作式 emulation（§5）——`into_pthread_t`/把线程句柄交给 C 会跑错（无真 pthread_t）；正确做法是真线程+蹦床（tier-0 用 GIL）。② `libc::malloc` 现路由进 Rust Heap，应改为真 libc 直通落 Native Heap（§4/§7）。③ 解释器对无 AllocId 的真地址指针报错，应回退裸宿主访问以支持 Rust↔native 内存互操作（§4）。

---

## 10. mirvm 不是什么

- **不是 Miri**：Miri 是 RAM 的*检查*实现（宁慢勿漏 UB）；mirvm 是*运行/标准*实现（假设合法、追求快）。同一台 RAM，不同质量取向。UB 检测对 mirvm 是可选 QoI，不是身份。
- **不是 evcxr**：evcxr 是编译器外壳（每 cell 走 rustc+链接）；mirvm 有自己的执行引擎。
- **不重建 rustc 前端 / trait solver**：那是 RAM 的加载器，复用。

## 11. 风险与对策

| 风险 | 对策 |
|---|---|
| nightly API 漂移 | 锁定+月度 bump；rustc 交互隔离；跟 Miri 同步提交 |
| shim 工作量（最大风险） | 按需实现；§7 边界模型减少无谓 shim（真资源走直通）；借鉴 Miri 代码 |
| 测试假阳性 / 语义误报 | 绿色必须比较输出或不变式；双方都失败不得算 PASS；预期红锁定原因 |
| silent stub | 未实现的可观察语义必须 Trap 或真实实现，不允许返回成功伪装支持 |
| M4 解释器性能天花板 | 方法级 JIT 已落地（M5.3/M5.4a–d/M5.5，默认开启）；JIT-on/off/native 三方差分；fib(32) JIT 54–80ms ≤80ms 硬门 |
| FFI C→Rust 回调 | 普通回调用 thunk + TLS attach；signal 已单独使用固定原子登记桩和安全点派送，不在信号帧运行 libffi/guest |
| 平台耦合 | 当前只宣称 Linux/ELF/x86_64；抽取 OS 边界后再扩平台 |
| 生命周期与嵌入 | close/wait、执行租约、pthread 延迟回调、CtxSlot 与逐实例 ctor/fini 已有；保留进程期代码地址是对任意 native 指针无法撤销的明确边界。后续只可补 safe typed export 或针对具体 native API 的撤销合同，不能宣称容器校验能证明任意 FFI ABI |

## 12. 先行者参考

- [Miri](https://github.com/rust-lang/miri) — RAM 的检查参考实现；InterpCx/Machine/shims/native-lib 的**代码**参考（非心智模型）
- [C++ 抽象机器](https://en.cppreference.com/w/cpp/language/as_if) — as-if 规则与抽象机器概念，本项目脊柱的思想来源
- [Rust opsem team](https://github.com/rust-lang/unsafe-code-guidelines) — 事实 RAM 的内存模型/别名模型出处
- [rustc_codegen_cranelift](https://github.com/rust-lang/rustc_codegen_cranelift) — M5 JIT 参考；[2025-06 unwinding 进展](https://bjorn3.github.io/2025/06/30/progress-report-june-2025.html)
- [cargo script RFC 3424](https://rust-lang.github.io/rfcs/3424-cargo-script.html) · [稳定化 PR](https://github.com/rust-lang/cargo/pull/16569)
- [Rustc Dev Guide: rustc_private/driver](https://rustc-dev-guide.rust-lang.org/rustc-driver/intro.html)
- [evcxr](https://github.com/evcxr/evcxr) — 反面教材（编译器外壳：延迟、状态搬运、不可嵌入）
