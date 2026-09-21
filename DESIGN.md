# mirvm — 设计文档

> 工作代号 `mirvm`（MIR Virtual Machine）。
> 奠基 2026-07-03；2026-07-05 确立当前心智模型（抽象机器 + VM 作者视角）。
> 本文档是项目的**长期心智模型与设计契约**，不是阶段进度表。当前实现、已知缺口和下一步见
> [docs/current-status.md](docs/current-status.md)；未解决债务见
> [docs/open-issues.md](docs/open-issues.md)，frame/vmctx 等契约见 [docs/designs/](docs/designs/)。
> 文中只描述当前设计；实现进度与历史都不在这里。

---

## 0. 命题（一句话）

**mirvm 是 Rust 抽象机器（Rust Abstract Machine, RAM）的一个事实标准实现，按 JVM 级系统软件的方式构建。**

- 它**实现一台抽象机器**，不是"给 Rust 套个解释器"。正确性以"是否忠实实现 RAM"来定义。
- 它是**运行参考实现**：Miri 是 RAM 的*检查*参考（宁慢勿漏 UB），mirvm 是 RAM 的*运行*参考（假设合法、追求快）。两者实现同一台 RAM。
- 它按 **VM 作者的视角**设计（托管堆、执行引擎分层、OS 线程、加载/链接、JIT），**不是** Miri 的扩展。凡是遇到设计抉择，问的是"一台 JVM 类系统软件会怎么做"，而不是"Miri 怎么做"。

下面几节先立**心智模型**（§1–§3 抽象机器与 VM 架构），再落**具体模型**（§4–§7
内存/线程/执行/边界），最后是**工程部分**（§8 设计原则、§9 不是什么、§10 风险、§11 先行者参考）。

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
native 栈。调用活动与局部存储是两条正交轴。完整 A/B 选择理由与重开条件见
[docs/designs/frame-abi-bytecode.md](docs/designs/frame-abi-bytecode.md)。

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
   - **signal**：传统异步 signal 不走 libffi closure。每次 guest 注册物化固定 22 字节 RX 桩，
     内核帧只做固定 TLS 读取与原子登记（不加锁、不分配、不执行 guest、不展开）；进程定向事件进
     注册 Engine 的 inbox，`SI_TKILL` 线程定向事件进目标 pthread 的逐注册代际 cell，二者都在安全
     点以全新 activation 执行 handler。close 的 owner 链恢复与等待、线程退出收口、旧桩回装的
     状态 70 失败、以及查询/`oldact` 反译 guest 地址，合同见
     [modeb-mirvmar-design.md](docs/designs/modeb-mirvmar-design.md) 与
     [c-unwind-contract.md](docs/designs/c-unwind-contract.md) C14；拒绝边界见 open-issues R1/R21。


2. **纯直通（FFI 到系统 libc）**——真资源、不涉及解释态实体：open/read/clock/getrandom、数学函数（libm）。**用系统 libc**（`dlsym(RTLD_DEFAULT)` + 按 `-l` 指令 dlopen）；target==host 保证 ABI 逐位一致。这批现在是手写 shim（历史包袱），neat 的终态是通用直通通道——但那是 polish，不急。

3. **inline asm**——不是 call，是嵌在函数中间的不透明机器码，无符号无调用边界。**永远无法在一处代理**，只能：模拟具体模板，或按名拦截外层函数（我们对 sqrt/std_detect 就是后者）。VM 编码调用时须知：foreign call 有边界，inline asm 没有。

### native 写 guest 内存

zlib 这类 C 库 FFI 进去后是真机器码，其内部的 malloc/memcpy 打到真 libc，**我们不拦也无需拦**（在 RAM 之外）。边界就是那一次 FFI 调用：我们只管**递给它的**缓冲（真实地址让 native 直接写回，解释器看得见）。Miri 同款限制：native 自 malloc、返回指针指望被解释代码解引用 → 失败（无元数据）；算力型 C 库不这样，无痛。

### 沙箱（安全 vs 虚拟化）

安全的 choke point 是**进程级 OS 沙箱**（seccomp-bpf/namespaces 罩住 mirvm 进程）——给 native 程序用了几十年的机制，不管调用走 shim 还是 FFI，syscall 都在内核那道关被拦。**mirvm 不重造安全拦截。** mirvm 层钩子只用于 OS 沙箱表达不了的**虚拟化**：假文件系统、路径重定向、资源计费、区分"guest 调 open"vs"解释器自己读缓存"。即：默认直通 + OS 沙箱管安全 + mirvm 钩子按需虚拟化。

---

## 8. 设计原则

### 原则（从抽象机器脊柱推出）

- **P0 实现 RAM，用 as-if 换自由**：正确性 = 忠实 RAM；一切内部实现（分配器、tier、调度）只要保持可观测等价即自由。
- **P1 VM 作者视角**：遇抉择问"JVM 类系统软件怎么做"，不问"Miri 怎么做"。Miri 是检查参考、shim/intrinsic 的代码参考，但**不是心智模型来源**。
- **P2 复用前端，自研引擎**：绝不重建 trait solver/类型系统/前端（RAM 的加载器）；执行引擎全自研（RAM 的执行器）。
- **P3 不做 borrowck / 不做 UB 检测**：那是前端与 Miri 的职责；我们假设程序合法、追求快。
- **P4 边界即 RAM 边界，绝不广泛拦截 native**：界内（解释代码的 foreign-call 边界）实现语义，界外（native 代码、裸函数指针 call）我们看不见也**不试图拦截——那是工程灾难**。真实地址让边界"软"而廉价（两种代码共享一个地址空间，见 §4）。
- **P5 不 emulate，用真 OS**：能用真 OS 原语（线程/futex/文件/时钟）就直接用，只在 foreign-call 边界"搞最小的一点"（如 pthread_create 插蹦床）。emulate 一套机制（如自建线程调度）是反模式——既慢又常常对合法程序跑错（如 into_pthread_t）。真并行是默认实现；协作式调度是被否决的 emulation，不是可回退的方案。
- **P6 Unix 优先**：unwinding/FFI 都 Unix 优先，macOS 次之，初期不支持 Windows。
- **P7 OS 交互集中在 `os::` 边界（2026-07-18 已物理收口）**：一切触及真 OS /
  系统库的东西都经 `src/os/` 原语层——**非 os 域的 `libc::` 触点已机械清零**
  （grep 门禁，仅剩注释；spikes 冻结原型不在门禁内）。草图中的多数文件
  （fs/time/net/rand/math/ffi）从未成为独立触点——全 syscall 族经
  `os::process::syscall` 变参单点直通、FFI 在 `vm/ffi.rs`（业务）调
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

src/os_arch/           — 同时属于「某内核 + 某 CPU」的知识，OpenJDK os_cpu 同款
  mod.rs        — 边界契约 + pair 选定（目录名 <os>_<arch>）+ pair 必须提供的子系统清单
  linux_x86_64/
    addrspace.rs — 固定基址布局（cacheability 结构由引擎要求，数值取自本 pair 的地址空间）
    signal.rs    — SA_RESTORER/restorer、裸 rt_sigaction 布局、信号入口 stub、ucontext 寄存器下标
    thread.rs    — 裸 futex wait/wake、用户地址宽度
```

  guest 语义裁决（fork 守卫、信号白名单、sigaction 改拷贝文案）仍在引擎业务侧，
  os:: 只供原语——OpenJDK `os::` 同款纪律；需要「一个内核与一个 CPU 同时成立」的知识时，
  经 `os_arch::<子系统>` 这一条路径取，os:: 与 arch:: 都不重复声明它。

### 工程决策

- **nightly 锁定 + 定期 bump**：rustc_private API 随 nightly 漂移；`rust-toolchain.toml` 锁日期版本（当前 nightly-2026-07-02 / rustc 1.98.0-nightly），每月 bump；rustc 交互隔离在少数模块。关注 Miri 同步提交作迁移指南。
- **依赖以 `-Zalways-encode-mir` 构建**：rlib 携全部函数 MIR；内容寻址全局缓存（`$HOME/.mirvm`，`MIRVM_HOME` 改址）。
- **engine 是 library，CLI 是薄壳**：公开的安全加载入口只有 `Package::load`；从可信包
  建 Engine 的 `Package::instantiate` 仍是 `unsafe`。既有 Engine 上的 `run_main`、状态、
  `close`/`wait_closed` 是 safe，执行出口用 `RunOutcome`/`RunErrorKind` 区分正常返回、guest
  panic、关闭和引擎错误。内部 `Shared` 不公开；手工 Module 与无类型 raw export 收在
  `vm::raw` 的 unsafe 面。完整 safe typed export API 仍未建立。
- **借鉴 Miri 代码**（MIT/Apache-2.0，保留 attribution）：shim 结构、intrinsic 清单、native-lib 机制是最佳代码参考——但**仅代码，不是心智模型**（P1）。

---

## 9. mirvm 不是什么

- **不是 Miri**：Miri 是 RAM 的*检查*实现（宁慢勿漏 UB）；mirvm 是*运行/标准*实现（假设合法、追求快）。同一台 RAM，不同质量取向。UB 检测对 mirvm 是可选 QoI，不是身份。
- **不是 evcxr**：evcxr 是编译器外壳（每 cell 走 rustc+链接）；mirvm 有自己的执行引擎。
- **不重建 rustc 前端 / trait solver**：那是 RAM 的加载器，复用。

## 10. 风险与对策

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

## 11. 先行者参考

- [Miri](https://github.com/rust-lang/miri) — RAM 的检查参考实现；InterpCx/Machine/shims/native-lib 的**代码**参考（非心智模型）
- [C++ 抽象机器](https://en.cppreference.com/w/cpp/language/as_if) — as-if 规则与抽象机器概念，本项目脊柱的思想来源
- [Rust opsem team](https://github.com/rust-lang/unsafe-code-guidelines) — 事实 RAM 的内存模型/别名模型出处
- [rustc_codegen_cranelift](https://github.com/rust-lang/rustc_codegen_cranelift) — M5 JIT 参考；[2025-06 unwinding 进展](https://bjorn3.github.io/2025/06/30/progress-report-june-2025.html)
- [cargo script RFC 3424](https://rust-lang.github.io/rfcs/3424-cargo-script.html) · [稳定化 PR](https://github.com/rust-lang/cargo/pull/16569)
- [Rustc Dev Guide: rustc_private/driver](https://rustc-dev-guide.rust-lang.org/rustc-driver/intro.html)
- [evcxr](https://github.com/evcxr/evcxr) — 反面教材（编译器外壳：延迟、状态搬运、不可嵌入）
