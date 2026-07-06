# mirvm — 设计文档

> 工作代号 `mirvm`（MIR Virtual Machine）。
> 奠基 2026-07-03；2026-07-05 确立当前心智模型（抽象机器 + VM 作者视角）。
> 本文档是项目的**心智模型与设计契约**。改动决策先改这里，再改代码。

---

## 0. 命题（一句话）

**mirvm 是 Rust 抽象机器（Rust Abstract Machine, RAM）的一个事实标准实现，按 JVM 级系统软件的方式构建。**

- 它**实现一台抽象机器**，不是"给 Rust 套个解释器"。正确性以"是否忠实实现 RAM"来定义。
- 它是**运行参考实现**：Miri 是 RAM 的*检查*参考（宁慢勿漏 UB），mirvm 是 RAM 的*运行*参考（假设合法、追求快）。两者实现同一台 RAM。
- 它按 **VM 作者的视角**设计（托管堆、执行引擎分层、OS 线程、加载/链接、JIT），**不是** Miri 的扩展。凡是遇到设计抉择，问的是"一台 JVM 类系统软件会怎么做"，而不是"Miri 怎么做"。

下面几节先立**心智模型**（§1–§3 抽象机器与 VM 架构），再落**具体模型**（§4–§6 内存/线程/执行/边界），最后是**工程与历史**（§7 决策、§8 里程碑、§9 tier-0 实现日志）。

---

## 1. Rust 抽象机器（我们实现的东西）

Rust 没有官方形式化规范，但存在一台**事实上的**抽象机器：rustc 的 MIR 操作语义 + opsem 团队的内存模型（借自 C++20）+ provenance 模型 + rustc 的 layout 算法。Miri 是它的可执行参考。mirvm 立志成为它的**权威、快速的可执行定义**。

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
- **两种线程实现并存**合法——协作式顺序一致执行是 RAM 允许的合法执行之一（§5）。

凡是"我们能不能这么优化"的问题，答案永远回到：**可观测行为变了吗？没变就自由。**

---

## 2. VM 架构（JVM 类比是脚手架）

用 JVM 这台成熟系统软件的结构来锚定 VM 作者视角。每个部件都是"实现 RAM 的某一面"：

| JVM 部件 | mirvm 对应 | 说明 |
|---|---|---|
| 类加载 + 字节码验证 | **rustc 前端**（解析/宏/typeck/borrowck/MIR）+ **惰性单态化** | RAM 的"加载器/验证器"。这是十年工程，我们**复用**不重建。加载 = 取得某个 instance 的 MIR |
| 字节码 | **MIR**（现在）→ **自研紧凑字节码**（M4） | RAM 计算的载体 |
| 托管堆（GC） | **Rust Heap**（托管，arena/TLAB，**不搬迁**，Drop 而非 GC） | RAM 存储的 realize，见 §4 |
| 执行引擎（解释→C1→C2） | **解释 tier → 字节码 VM → Cranelift JIT** | RAM 计算的执行，见 §6 |
| 线程（1:1 OS） | **1:1 OS 线程**（VM tier） | RAM 并发的 realize，见 §5 |
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
3. **REPL/Notebook**（后置 M6）：VM 拥有持久堆，状态持久化天然成立，跨 cell 借用不再是问题。
4. **嵌入式引擎**（后置）：engine 是 library、CLI 是薄壳，这条路从第一天就不被堵死。

**执行模式**（HotSpot 风格，始终可配）：`--engine=interp`（纯解释，java -Xint）/ `mixed`（解释+热点 JIT，默认，落地后）/ `jit`（尽量全编译，-Xcomp，对比测试）。

---

## 4. 内存模型（RAM 存储的实现）

RAM 存储在 mirvm 里 realize 为**三个隔离的部分**（管理与污染维度，非地址空间维度——地址空间只有一个，宿主的）：

| 部分 | 是什么 | 归属 | 关键性质 |
|---|---|---|---|
| **VM 元数据** | provenance 表、MIR/字节码缓存、线程表、layout 表 | **不属于 RAM**，实现私有 | 必须与下面隔离——guest UB（RAM 之外）绝不能污染实现自身 |
| **Rust Heap** | Rust 分配（`__rust_alloc`：Box/Vec/被解释代码的 Rust 分配） | RAM 存储 | VM 向 libc 要的大块连续内存，自管（arena/TLAB，快路径无锁），**托管但不搬迁**（地址钉死，回收靠 Drop 非 GC） |
| **Native Heap** | native 分配（`libc::malloc` 直调、C 库内部分配） | RAM 之外 | 直通真 libc malloc；VM 不追踪其元数据 |

### 一个地址空间，两种代码都能碰两个堆

从 OS 视角，**整个 VM 在一个连续地址空间内**：Rust Heap 是 VM 向 libc 要的大块内存，Native Heap 是 libc 另外给的，都是真地址。**真实地址模式下，内存访问就是裸宿主 read/write——有没有 AllocId 都一样，字节都在真地址上。** 于是 native 代码能写 Rust Heap（zlib 把结果写进 guest 的 Vec，已验证），解释代码也能读写 Native Heap（拿 C 给的指针解引用——合法程序常有，运行时就该让它成立）。

### AllocId 是什么：检查器的元数据 overlay，VM tier 甩掉

**AllocId 不决定"字节在哪"**（永远在真地址）。它是 **Miri 每分配元数据侧表的钥匙**——init mask（字节是否已初始化）、provenance map（哪些区块是指针）、bounds/liveness。**这三样全是"检查"基础设施**，我们经 InterpCx（tier-0）继承。**fast machine（假设合法）在真实地址模式下几乎不需要**：不查 UB→不需 init mask；不做别名检查、指针即地址位→不需 provenance；不查越界/UAF→不需 bounds。

两种"元数据"要分清：

| | 是什么 | fast VM 需要吗 | 键 |
|---|---|---|---|
| **类型级** | layout/字段偏移/判别式/vtable | **必需**（解释 MIR 靠它） | 按**类型**（tcx layout / C8 冻结进字节码），VM 私有表 |
| **每分配** | init mask / provenance / bounds | **不需要**（检查器 overlay） | 按 **AllocId**，tier-0 继承，VM tier 甩掉 |

所以：**裸宿主访问是根本路径，AllocId 元数据是 tier-0 从 Miri 继承的 overlay。** "解释代码读 Native Heap 靠裸访问"不是特例回退——它是 **VM tier 统一模型的预览**（统一裸访问、无 per-allocation 元数据、无 AllocId 间接寻址，正好也是 fib(27) 慢的原因之一）。（当前 tier-0 实现对无 AllocId 指针报错，是 InterpCx 的检查器行为残留，待补裸回退，见 §9。）

分工的动机：`__rust_alloc`（热路径，每个 Box/Vec）进**托管** Rust Heap，为速度/并发/隔离；`libc::malloc`（较罕见、属 native 世界）**直通**真 libc，落 Native Heap——否则 guest `libc::malloc` 的指针交给 C `free` 会崩（Rust-Heap 指针 vs libc free 不匹配）。

### 为什么 Rust Heap 是"托管但不搬迁"（Rust 特有，非照抄 JVM）

- **托管**（拿 JVM 的红利）：RAM 对分配只要求互异/对齐/非空，as-if 允许 VM 自己管。**per-thread arena / TLAB（bump 分配）** 是 JVM 让分配在多核 scale 的招，直接服务生而并发（快路径无锁）。对比"每次 round-trip libc malloc + 建 Allocation + 注册进共享表"——那是解释器做法，是串行化点，慢且反并发。
- **不搬迁**（Rust 现实，与 JVM 分道）：Java 对象可被 GC 移动；Rust Heap **绝不能移动**——ptr↔int、provenance、FFI 直传都依赖地址钉死。所以借 JVM 的**管理骨架**，按 Rust 的**钉地址/无 GC** 改造。这是真正 Rust-native 的设计。

### 真实地址（FFI 与并行的共同地基）

分配的基址 = 其宿主缓冲的真实地址。于是 guest 指针**就是**宿主指针：热路径 load/store 零翻译；FFI 把 guest 缓冲原样传给 native（native 直接读写，见 §7）；并行 tier 原子操作直落宿主地址。这是 §7 FFI 廉价、§5 并行可行的根本前提。

### 隔离保证（鲁棒性）

Rust Heap 与 VM 元数据分池：guest 的 unsafe UB（越界/UAF）打烂的是 Rust Heap，波及不到实现自身（Miri 的 `IsolatedAlloc` 同一动机）。这是 §4 三分的核心收益，也是"标准实现"该有的鲁棒性。（当前 tier-0 尚未分池，见 §9 待办。）

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

所以：**当一个指向解释态函数的指针要逃逸给 native 时，把它 materialize 成真 thunk。** 于是 `thread_start` 有真地址，`pthread_create` 变成**纯直通**，std wrapper 原样工作；`Thread.id` 是**真 pthread_t** → join/as_pthread_t/into_pthread_t/futex/mutex **全部走真 libc、零拦截**。这个 thunk 机制**顺手解决 C→Rust 回调**（qsort 比较器、signal handler 同理）。**Miri native-lib 正是这么做的**（`build_libffi_closure`：Function 分配的地址 = libffi closure）；我们现在的"函数分配泄漏 1 字节占位地址"只在**解释器自己调**时够用，一旦 **native 要调**就升级成 closure。**JIT tier**：thread_start 是真机器码，连 thunk 都不需要，std wrapper 直接通——所以 thunk 是解释/字节码 tier 特有的边界桥。

### 拦截边界（对应 §7）：native 操作几乎拦不住，且不该试

拦截只发生在**解释代码的 foreign-call 边界**。越界即瞎，且**广泛拦截 native 操作是工程灾难**：
- guest（std 或裸 `extern "C"`）调 pthread_create → 纯直通真 libc（`thread_start` 已是真 thunk）。✓
- guest `dlopen` 取真函数指针直接 call → 走 FFI，回调若指向解释态函数亦走 thunk。✓
- C 库内部 spawn 线程 → **永远看不见**，是真 native 线程跑 native 代码，无妨（回调 Rust 时走 thunk）。

### tier 分层：真线程为本，协作式是错误的过渡

| | 真线程模型（本设计） | tier-0 现状（协作式） |
|---|---|---|
| pthread_create | 纯直通真 libc（thread_start = 真 thunk） | 建 GuestThread，假线程复用一条宿主线程 |
| into_pthread_t / 交 C | 真 pthread_t，**能用** | **假值，一用就废——合法程序跑错** |
| join/futex | 真 libc 直通 | 自建等待队列（emulation） |

> ⚠️ 重大修正（2026-07-05，看过 std 源码后）：**协作式 ThreadManager/调度器/futex 队列是 emulation，且被 `into_pthread_t` 判为错误实现（非仅慢）**——一个合法 Rust 程序取出 pthread_t 交给 C 时，协作式没有真 pthread_t。不围绕它设计，是要推倒的。
>
> **tier-0 的正确过渡不是协作式，是 GIL**（真线程 + 全局解释器锁，CPython 式）：真 pthread_create + 蹦床，解释执行被锁串行化，阻塞时放锁。这样 `into_pthread_t` 在 tier-0 也成立，且结构与 VM tier 同构（去锁即并行）——协作式是**扔掉的代码**，GIL 是**踏脚石**。（GIL 作 VM tier 永久架构才是反面教材；作 tier-0 bootstrap 是对的。当前代码仍是协作式，未切 GIL——见 §9 待办。）

### 为什么 VM tier 才能去锁并行

InterpCx 的内存 map、MonoHashMap（RefCell 遍地）不 Sync，蹦床在第二条 OS 线程重入会撞 RefCell。所以"去掉 GIL 的真并行"是 **VM tier 的地基级决策**（引擎须 Sync），见 §7 线程安全三招。

### Rust 三红利（并行 VM 比 JVM 当年容易）

无 GC；内存模型现成（C++20）；safe 代码类型系统保证无数据竞争 → 只需保护**实现自身**状态。

### 诚实偏差（弱内存序）

GIL/协作串行执行下所有内存操作按单一全序（顺序一致），**观察不到弱内存重排**、调度确定 → 时序 bug 可能不复现。SC 是 RAM 允许的合法执行，对合法程序仍正确；声明为偏差。并行程序差分须靠输出不变式（不能逐字节比时序敏感输出）。

---

## 6. 执行引擎（RAM 计算的执行，分 tier）

| tier | 是什么 | 状态 | 定位 |
|---|---|---|---|
| **tier 0 解释** | fast Machine on rustc `InterpCx` | ✅ 已实现 | **bootstrap + 差分 oracle**，不是设计中心。它是 RAM 的一个合法（慢）实现，让我们数周内跑真程序、并为自研 VM 提供对拍基线 |
| **tier 1 字节码 VM** | MIR → 自研紧凑字节码，内联缓存，非泛型依赖原生直调 | M4 | **成为真正的 VM**：生而并发（§5/§7），性能答案。前置硬关卡：RAM 规格文档 + 并发架构 RFC + 原型 spike |
| **tier 2 JIT** | Cranelift 热点编译 | M5 | ≈ cg_clif debug build 性能；`--engine` 开关 |

**tier 0 = bootstrap，不是终点。** InterpCx 是 rustc 的解释基础设施（Miri 也建在其上），它让我们**站在正确的 RAM 语义上快速起步**；但它不 Sync（§5）、AllocId 间接寻址慢（fib(27) 微基准解释 ≈1.6s vs native debug 4ms）。M4 自研字节码 VM 才是 VM 作者要掌控的核心，届时 InterpCx 退居差分 oracle。

**M4 帧栈模型：倾向 A（guest 帧在 native 栈，HotSpot/V8 式）**。详细机制对照见 [docs/frame-stack-models.md](docs/frame-stack-models.md)；帧布局/调用约定/字节码格式的 M4 设计草图见 [docs/frame-abi-bytecode.md](docs/frame-abi-bytecode.md)。理由（greenfield + JIT 硬约束）：JIT 必做且用 Cranelift（方法级），A 下解释帧与编译帧同在 native 栈 → interp↔compiled 廉价适配（i2c/c2i），B 要建拆 VM 帧 + 两栈联合 unwind；B 的看家优势（协程/挂起）因真 OS 线程 + Rust 无栈 async 对我们无关；A 天然栈溢出忠实。关键认识：JIT-VM 里解释器是**冷层**，A"解释器更难写"的代价权重大降。**当前 tier-0 是 B1（错误实现，待推倒）。** 代价：M4/M5 强耦合，帧布局+调用约定须与 Cranelift 共同设计（见 C11）；头号硬骨头 = 混合栈 unwind（frame-abi-bytecode.md §7，M4 前置 spike）。

---

## 7. 抽象机器边界（FFI / intrinsics / native）

**FFI = 抽象机器的边界。** 边界之内实现 RAM 语义；边界之外（native）RAM 不建模，我们只移交控制权。这一条统一了之前所有关于"什么该 shim"的含糊。

一个 extern 调用，按它相对 RAM 的位置分类。**注意：guest 始终被解释，所以这些 foreign 调用我们永远看得见；"直通"指 handler 把活转交给真 OS，而非"看不见"。native 代码内部的同名调用（如 zlib 内部 malloc）是机器码打到真 libc，我们看不见也不该管（RAM 之外）。**

1. **RAM 内建 / 由 handler 服务**：
   - **intrinsics**：RAM 计算的一部分，VM 原生理解（如 JVM 字节码指令）。
   - **分配器**：拦截动机是**归属 + 元数据**。`__rust_alloc`（Rust 分配器，编译器合成、无 MIR，MIR 层的 foreign-call 边界拦截）→ 落**托管 Rust Heap**（速度/并发/隔离），解释器为其建元数据以解引用。`libc::malloc`/`calloc`/`free` 等 native 分配器 → **直通真 libc、落 Native Heap**（否则 guest malloc 的指针交 C free 会崩）。解释器对 Native Heap 指针回退裸宿主访问（§4）。**"一处"= 托管 Rust Heap 的分配器入口；native 分配不进这一处，是另一条真 libc 直通路。**
   - **unwind**：VM 拥有栈帧，panic/catch_unwind 自实现（跨平台无痛）。
   - **线程**（§5）：**不 emulate、不 wrap pthread，用真 OS 线程**。std 已把 pthread 包好；我们唯一要做的是让解释态 `thread_start` 变成 native 可调用（materialize 成真 thunk / libffi closure）——这是 **FFI 的反方向（native→解释）**，不是包 pthread。于是 pthread_create 纯直通、Thread.id 是真 pthread_t、join/into_pthread_t/futex 全走真 libc。同一 thunk 机制**顺手解决 C→Rust 回调**（qsort/signal handler）。tier-0 因引擎不 Sync 退回 GIL-over-真线程（当前代码仍是协作 emulation，待切）。


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
- **P7 OS 交互集中在 `os::` 层（JVM os:: 同构）**：一切触及真 OS / 系统库的东西（线程/futex/文件/时钟/内存/网络/信号/FFI 出入）**集中在一个 `os::` 模块**，是 §7 抽象机器边界的**物理归宿**。解释器/VM 核心只对 `os::` 接口说话，**绝不散调 libc**。平台差异（Linux/macOS/Windows）藏在 `os::` 背后同一接口下（`os::linux::` 等）。三种归宿（直通/内建/合成，见 C10）在此有序组织，而非散落各处。反例 = tier-0 现状（shims.rs 大 match + native.rs + threads.rs 各处散落，M4 重写时收拢）。

**`os::` 模块草图**（M4 落地形态；tier-0 逐步收拢）：

```
src/os/
  mod.rs        — os 抽象接口（trait）+ 三归宿分派；VM 核心只依赖这里
  thread.rs     — create_thread(蹦床)/join/detach/futex/tls（真线程，C8）
  mem.rs        — Rust Heap 分配器(arena/TLAB) / native heap(libc malloc 直通) / mmap
  fs.rs         — open/read/write/close/stat（真 fd 直通）
  time.rs       — clock_gettime/nanosleep（直通）
  net.rs        — socket/epoll/eventfd/timerfd（真内核 fd 直通，async I/O）
  rand.rs       — getrandom（直通）
  process.rs    — exit/abort/env/args
  math.rs       — libm（宿主直算，合成）
  ffi.rs        — 出向: libffi call-out；入向: thunk/closure(native→解释, C8)
  linux/        — 平台特定: syscall 号、struct 布局、weak 符号
```

### 约束账本（M4 开工前必读；每踩一坑追加）

- **C0 抽象机器脊柱**（§1）：正确性契约 + as-if 自由 + FFI=RAM 边界。一切决策的根。
- **C1 生而并发**（§5，第一红线）：VM tier 默认 1:1 真并行；tier-0 因引擎不 Sync 用 GIL-over-真线程过渡（协作式是要扔的 emulation）。依据：JVM 级目标；现代 Rust 天生并发（tokio 多线程默认、rayon、**cargo test 默认并行**）；CPython/GIL 作永久架构是单核时代妥协、反面教材（仅作 tier-0 bootstrap 可取）。
- **C2 并发内存模型**（§4）：三分（元数据私有隔离 / Rust Heap 托管非搬迁 per-thread arena / native heap 外）；原子直落宿主指令；真实地址；引擎状态三态（每线程私有 / 发布后不可变 / 显式同步）。**内存访问 = 裸宿主 read/write（有无 AllocId 都在真地址）；AllocId 是 Miri 的每分配元数据 overlay（init/provenance/bounds），fast machine 不需要，VM tier 甩掉——省掉间接寻址（fib(27) 慢因之一）。类型级元数据（layout/offset/vtable）另说，按类型冻结进字节码（C8），永远需要。**
- **C3 Rust 三红利**（§5）：无 GC、内存模型现成、safe 无竞争。
- **C4 guest UB 立场**：unsafe 竞争/UAF = 宿主竞争/UAF，与 native 一致（fast 立场）；隔离区（保护实现自身）+ 可选 TSan 后置。
- **C5 tier-0 硬约束**：InterpCx 不 Sync → 永远单宿主线程 → 协作式；作为 bootstrap + oracle 存在。
- **C6 弱内存序偏差**（§5）：协作模式 SC 执行、无重排、调度确定 → 时序 bug 可能不复现；对合法程序仍正确，声明为偏差。
- **C7 性能基准**：regex 编译（DFA + Unicode 表）42s 是 1 号基准；VM 评审须给该案例预估收益。
- **C8 真线程架构（§5，看过 std 源码后定，2026-07-05）**：**不 emulate、不 wrap pthread，用真 OS 线程**。std 已 wrap pthread（`thread_start` 是 std 的 extern C fn，`Thread.id` 是真 pthread_t）。VM 唯一要做的是 **FFI 的反方向**：解释态函数指针逃逸给 native 时 materialize 成**真 thunk（libffi closure，Miri `build_libffi_closure` 同款）**。于是 pthread_create **纯直通**（非拦截/wrap），join/into_pthread_t/as_pthread_t/futex/mutex 全走真 libc。同一 thunk 机制**解决 C→Rust 回调**（qsort/signal，之前 defer 的问题）。JIT tier 下 thread_start 是真机器码，thunk 消失。tier-0 不 Sync → GIL-over-真线程过渡（into_pthread_t 成立，结构同 VM tier，去锁即并行）；**协作式是扔掉的 emulation**。引擎线程安全三招：**降低时元数据冻结**（layout/偏移/vtable 烘焙进字节码，运行期不触 tcx）、**降低服务线程**（惰性单态化经 channel 发给专职编译线程，HotSpot compiler-thread 同构，为 M5 后台 JIT 铺路）、Rust Heap per-thread arena。开放问题：thunk 重入的 'tcx 生命周期/Sync 边界。
- **C9 FFI/native 写内存**（§7）：真实地址让 FFI 零编组；一个地址空间两种代码碰两个堆（native 写 Rust Heap；解释器对 Native Heap 指针回退裸宿主访问——运行时可，检查器不可）；值表示不能假设"只有解释器写内存"；libm 逃逸宿主直算通道 VM tier 要保留（intrinsic 化）。
- **C10 边界与拦截（§7，2026-07-05 三次修正）**：拦截只在**解释代码的 foreign-call 边界**，**绝不广泛拦截 native 操作（工程灾难）**。动机——**归属+元数据**（`__rust_alloc`→托管 Rust Heap，MIR 层拦；`libc::malloc`→真 libc→Native Heap 直通）/ **真线程最小介入**（只 pthread_create 插蹦床，余皆真 libc 直通）/ **纯直通**（真资源，handler 转发真 OS）。inline asm 无调用边界，只能模拟或函数级拦。native 内部/裸指针调用看不见，记录为限制。沙箱：OS 级(seccomp)管安全，mirvm 钩子仅虚拟化。
- **C11 帧栈模型 = A（guest 帧在 native 栈，2026-07-05 定，详见 docs/frame-stack-models.md）**：greenfield + JIT 硬约束下选 A（HotSpot/V8 式），非 B（CPython/Lua）。因 Cranelift 是方法级 JIT，A 下解释帧+编译帧同在 native 栈 → interp↔compiled 廉价适配（i2c/c2i），B 要建拆 VM 帧+两栈联合 unwind/backtrace；B 看家优势（协程/栈式挂起）因真 OS 线程+Rust async 无栈对我们无关；A 天然栈溢出忠实。**JIT-VM 里解释器是冷层 → A"解释器难写"代价权重大降，可起步简单（tree-walking），力气花 JIT 集成。** 每 guest 线程用其 OS 线程 native 栈放 guest 帧。**推论：M4/M5 强耦合——帧布局+调用约定须与 Cranelift 共同设计，先于 M4 定（并入并发 RFC/帧约定规格）。** 当前 tier-0 B1 是待推倒的错误实现。重估 B 仅当将来要栈式协程（明确不做）。
- **C12 JIT 后端 + 字节码 + 分发（2026-07-05 定，详见 docs/frame-abi-bytecode.md §7.5）**：
  - **JIT = Cranelift，藏在 `JITBackend` trait 后**（P7 同纪律，copy-and-patch 备选）。为 JIT 而生、≈10× 快于 LLVM 编译、质量≈debug build（正合目标）；**cg_clif 已趟通 MIR→Cranelift+Rust ABI+unwinding**，复用。耦合可控：抽不掉的只有调用约定（=Rust ABI，我们本就用）+ unwind 模型（=Rust 原生 landing-pad，逃不掉），**都非 Cranelift 特有**；不为它牺牲内存/线程/元数据模型。
  - **unwind = 候选 A**（复用 Cranelift landing-pad + Rust personality，因 JIT 定 Cranelift）；候选 B（自研栈行走）兜底。头号 M4 前置 spike。
  - **字节码贴近 MIR**（不下沉 CLIF）→ 解释器与 JIT 共享 MIR 级真理源、复用 cg_clif。**两级结构**：mirvmc（rustc 前端全 check → **Stable MIR/rustc_public + serde** → .mirvm 分发件，= .class/.jar 类比）；运行期"class loading"（按 target 冻结 layout C8 → 解释器寄存器字节码 + 喂 Cranelift，每平台一次缓存）。版本绑定诚实（classfile 版本号式，semver 转换）。
  - **分发格式 = 多 target 打包（定，2026-07-05 用户确认）**：**单产物跑任意 target 对完整 Rust 理论上不可能**（cfg 编译期按 target 剪枝=字面不同的程序 + usize/可观测 layout/const-eval；Java 能因无编译期 cfg/JVM 定 layout/定长基本类型，Rust 三条全违反=语言固有）。**采纳 fat artifact**：mirvmc 对 N 个 triple 各跑前端、打包 N 段（`.mirvm` 容器 = target 索引 + 各段 Stable-MIR），运行期挑匹配段 load → 消费端零工具链、覆盖常见平台、运行期可 JIT。os:: 每平台 build 时选定，与分发格式无关。
  - **迁移承诺**：slaved 操作数区仅 v0，**后续必换 alloca**（真内联 native 栈；Rust 里需 unsafe/crate）。

### 工程决策

- **nightly 锁定 + 定期 bump**：rustc_private API 随 nightly 漂移；`rust-toolchain.toml` 锁日期版本（当前 nightly-2026-07-02 / rustc 1.98.0-nightly），每月 bump；rustc 交互隔离在少数模块。关注 Miri 同步提交作迁移指南。
- **依赖以 `-Zalways-encode-mir` 构建**：rlib 携全部函数 MIR；内容寻址全局缓存（~/.cache/mirvm）。
- **engine 是 library，CLI 是薄壳**：为嵌入与测试留路。
- **借鉴 Miri 代码**（MIT/Apache-2.0，保留 attribution）：shim 结构、intrinsic 清单、native-lib 机制是最佳代码参考——但**仅代码，不是心智模型**（P1）。

---

## 9. 里程碑与 tier-0 实现日志

### 里程碑

- **M0 工具链打通**（✅ 2026-07-03）：rustc_private 驱动编译源文件、定位 entry fn、dump MIR。
- **M1 最小 RAM 实现**（✅ 2026-07-03）：fast Machine on InterpCx，std 程序端到端。差分 5/5（fib、String/Vec、HashMap、panic+exit 101、catch_unwind+Drop）。
- **M2 吃下真实生态**（✅ 2026-07-05）：cargo 依赖图、proc-macro、frontmatter 脚本、.init_array（args/env 转正）、时间/文件/malloc shims。serde_json+rand+regex 与 native 逐字节一致。
- **M2.5 生态补全**（线程/真实地址/FFI ✅ 2026-07-05；corpus 进行中）：
  - ✅ 线程（tier-0 协作实现，§5）：pthread/futex/nanosleep/TLS 析构；差分 12/12。
  - ✅ 真实地址内存（§4 前身）：MirvmAllocBytes 真对齐 + prepared 破指针环。
  - ✅ libffi FFI（§7）：dlsym + 编组 + native 写内存暴露；libz-sys 真 C 库往返与 native 逐字节一致（diff 14/14 + cargo 3/3）。
  - ⏳ corpus 驱动补全（rayon/chrono/clap/anyhow/csv/...），异步生态（epoll 等 = **真直通 handler**，非 emulate；async 本身编译期无栈状态机，引擎零特殊支持，见 docs/async-stackless.md）。
- **RAM-SPEC 文档**（近期，本次奠基的延伸）：把 §1 扩成独立的抽象机器规格，作为 mirvm 的对外语义承诺（"标准实现"的书面契约）。
- **M3 产品面**：daemon（前端增量状态留内存）、agent API（JSON 诊断/超时/内存上限/OS 沙箱）。验收：脚本二跑 <300ms、改一行重跑 <1s。
- **M4 字节码 VM（成为真正的 VM）**：自研生而并发字节码引擎。**前置硬关卡：RAM 规格 + 并发架构 RFC + spike 过 TSan。** 验收：≥5× InterpCx，差分双策略全绿。
- **M5 JIT**：Cranelift 热点，`--engine` 开关。
- **M6 REPL/Notebook**（后置）。**M7+ 嵌入 API**（后置）。

### tier-0 实现日志（如何在 rustc 解释器上 bootstrap 出 RAM 实现）

> 这些是 tier-0（InterpCx）阶段的具体实现与踩坑。M4 自研 VM 会重写执行核心，但**语义结论、边界洞察、性能数据长期有效**。

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
| tier-0 性能天花板 | 它只是 bootstrap；M4 自研 VM 是答案，届时转 oracle |
| FFI C→Rust 回调 | 已知坑（Miri 亦未全解）；单向调用先行，回调明确报错 |
| 异步/tokio（epoll/io_uring/socket） | 都是**真直通真内核 fd**（非 emulate），M3 后写这批直通 handler；async 本身编译期无栈状态机、引擎零特殊支持 |

## 12. 先行者参考

- [Miri](https://github.com/rust-lang/miri) — RAM 的检查参考实现；InterpCx/Machine/shims/native-lib 的**代码**参考（非心智模型）
- [C++ 抽象机器](https://en.cppreference.com/w/cpp/language/as_if) — as-if 规则与抽象机器概念，本项目脊柱的思想来源
- [Rust opsem team](https://github.com/rust-lang/unsafe-code-guidelines) — 事实 RAM 的内存模型/别名模型出处
- [rustc_codegen_cranelift](https://github.com/rust-lang/rustc_codegen_cranelift) — M5 JIT 参考；[2025-06 unwinding 进展](https://bjorn3.github.io/2025/06/30/progress-report-june-2025.html)
- [cargo script RFC 3424](https://rust-lang.github.io/rfcs/3424-cargo-script.html) · [稳定化 PR](https://github.com/rust-lang/cargo/pull/16569)
- [Rustc Dev Guide: rustc_private/driver](https://rustc-dev-guide.rust-lang.org/rustc-driver/intro.html)
- [evcxr](https://github.com/evcxr/evcxr) — 反面教材（编译器外壳：延迟、状态搬运、不可嵌入）
