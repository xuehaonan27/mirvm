# 并发架构 RFC —— M4 生而并发引擎（历史 RFC）

> 文档状态：**M4 的历史架构 RFC**。tcx-free 执行相、状态三分、1:1 真线程、宿主原子、TSan
> gate 与独立 `os::` 层（2026-07-18/19 E21 闭合）已落地；mode B、hand-rolled TLAB
> （v1 = mimalloc crate 后端，E14）与 checked 模式（E23）仍未实现。实际状态见
> [current-status.md](../current-status.md)，不要把本文所有未来形态当成当前目录结构。

> **原始状态（2026-07-07）：草稿，待评审。** 依据：C1（生而并发，VM tier=N 真 OS 线程无 GIL；tier-0=GIL 过渡）、
> C2（并发内存模型、状态三分）、C3（Rust 三红利）、C4（guest UB 立场）、C8（三招）、
> C11/C12（模型 A、Cranelift、贴近 MIR 的字节码、多 target 打包）。
> 目标：定清 VM tier 如何真并行、哪些状态怎么同步、C8 三招的具体形态、spike 验收（过 TSan）。
> 与 frame-abi-bytecode.md 配套（帧/字节码那半），本文管"并发那半"，两者需共同成立（C11）。
>
> **Spike 4 验收通过（2026-07-07，history/spike4-concurrency-tsan.md）**：8 真宿主线程并行混合
> 执行（i2c/c2i 并发）+ 跨 tier 同址原子 + 阻塞 syscall 活性（corpus §2.1 场景收束）+ 并发混合栈
> unwind，**TSan 全量插桩零竞争警告**。状态三分以 Shared（发布后只读）/Ctx（每线程私有）落地，
> 引擎执行路径零锁。新增引擎义务：**解释器执行 guest 原子必须发真宿主原子指令**（tier-0 的普通
> 读写模拟在真线程下 = 引擎自身数据竞争）。TSan 通道依赖"引擎核心零 rustc_private"（tsan/ harness）。

---

## 0. 命题与唯一的敌人

**命题**：N 个 guest 线程 = N 个真 OS 线程，各自跑解释器（interp_frame 于自己的 native 栈）+ 编译码，
**无 GIL**（VM tier），真并行。

**Rust 白送的简化（C3）**：无 GC（不需并发 GC/safepoint）；内存模型现成（C++20，原子直映硬件）；
**safe 代码类型系统保证无数据竞争** → 我们**只需保护【实现自身】的状态**，不需为 guest 的竞争兜底
（guest 的 unsafe 竞争是 guest 责任，C4，= native 行为）。

**唯一的敌人：`tcx` 不 Sync。** 这是 tier-0 只能单宿主线程的**根本原因**——InterpCx 执行每步都查 tcx
（layout/instance_mir/…），而 rustc 的 `TyCtxt` 是 `!Sync`（arena、interner 非线程安全）。**整个 RFC 的
核心就是把 tcx 赶出并行执行路径。** 靠两件事：①执行期不碰 tcx（元数据冻结，§3.1）；②残余 tcx 访问
confine 到单线程（降低服务，§3.2）。分发模式（run .mirvm）**根本没有 tcx**，最干净（§1.2）。

---

## 1. 执行模型

### 1.1 结构

```
                 VmShared (发布后只读 + 显式同步部分)
   ┌──────────────────────────────────────────────────────────┐
   │ 冻结字节码 BytecodeBody / layout表 / vtable / 常量池        │ ← 发布后只读, lock-free 读
   │ instance→字节码缓存 / instance→编译码缓存 / 线程注册表      │ ← 显式同步
   │ Rust Heap 全局块池                                          │ ← 显式同步
   └──────────────────────────────────────────────────────────┘
      ▲ 各线程共享
  ┌───┴────┐   ┌────────┐   ┌────────┐
  │OS 线程0│   │OS 线程1│   │OS 线程2│  各自私有: native 栈(guest 帧) + 操作数区 +
  │interp/ │   │interp/ │   │interp/ │            TLS + errno + unwind payload + Rust Heap arena
  │compiled│   │compiled│   │compiled│
  └────────┘   └────────┘   └────────┘
```

- 每 guest 线程 = 一 OS 线程，跑 interp_frame（模型 A，帧在其 native 栈）或编译码。
- 无全局锁（VM tier）；线程间只在"显式同步"那格相遇（§2）。

### 1.2 两种运行模式：区别只在"加载"，执行完全相同

**关键框架**：两种模式**共享同一个运行时**（执行引擎、内存、线程、JIT）；区别**只在前半段——如何把源码/产物
变成"字节码 + 冻结元数据"**。之后的执行一模一样。所以把生命周期切成两相：

```
 加载相 (单线程, 在 spawn guest 线程之前)：产出【完整】字节码集 + 冻结元数据
 执行相 (多线程, 【永远无 tcx】)：N 个真 OS 线程跑字节码 + 编译码
```

#### 模式 A：run from source（`mirvm run x.rs`，dev 内循环）

```
 rustc_private 前端(parse/宏/typeck/borrowck/MIR) → tcx
   → 单态化收集器(mono collector, 找全部可达单态化 instance, 与 codegen 同款) [用 tcx]
   → 降低 MIR→字节码 + 冻结 layout/偏移/vtable/drop/调用目标 [用 tcx]
   → 【完整字节码集】→ (可丢弃 tcx) → spawn 线程, 执行
```
- **tcx 存在，但被关在加载相**（单线程，spawn 前）。执行相不碰 tcx，可在加载后**丢弃 tcx**
  （daemon 模式可保留供改代码后增量重降低）。
- 启动成本 = rustc 前端（数秒）——dev 可接受，仍远快于全编译（省 codegen/LLVM/链接）。

#### 模式 B：run .mirvm（`mirvm run app.mirvm`，分发）

```
 (mirvmc 已【离线】做完前端+单态化+Stable MIR 抽取, 序列化进 .mirvm 多 target 产物)
 运行期: 挑匹配 target 段 → 反序列化 Stable MIR → 降低 Stable-MIR→字节码 + 冻结 [无 tcx]
   → 【完整字节码集】→ spawn 线程, 执行
```
- **根本无 tcx**、无 rustc。Stable MIR **自包含**（预单态化体 + layout + 符号），降低不需 tcx。
- 启动 = 反序列化匹配段，快（无前端）。

#### mirvmc 与模式 A 的关系

**mirvmc = 模式 A 加载相的前半段**（前端 + 单态化 + Stable MIR 抽取），只是**不执行、而是序列化成 .mirvm**。
同一套前端/单态化代码两用：跑 → 执行（模式 A），或序列化 → 分发（供模式 B）。

#### 为什么"执行相永远无 tcx"（这条是 Sync 引擎能干净的根本）

**Rust 单态化是静态的**——所有 instance 编译期即可定（mono collector 遍历可达；dyn/函数指针用**固定** vtable/
指针，运行期不产生新单态化，无反射式运行期实例化）。所以能在执行前把**全部可达字节码降完**。**tcx 是加载相
工具，并行执行相永不触碰**。于是"唯一敌人 tcx"（§0）被关进单线程加载相（模式 A）或根本不存在（模式 B）——
**并行执行永远见不到 tcx**，这就是引擎能 Sync 的地基。

**JIT 编译服务（后台、惰性）也无需 tcx**：它把热**字节码**→native（Cranelift），字节码已是 tcx-free 的，
所以连后台编译线程都不碰 tcx。**tcx 的唯一用户 = 模式 A 的加载相。**

#### 对照

| | 模式 A：from source | 模式 B：.mirvm |
|---|---|---|
| 前端 / tcx | 有（关在加载相） | **无** |
| 单态化 | 加载相 eager（用 tcx） | 离线（mirvmc 已做） |
| 降低输入 | 内部 MIR（查 tcx） | 序列化 Stable MIR（自包含） |
| 启动成本 | rustc 前端（数秒） | 反序列化（快） |
| 执行相并发 | tcx-free，与 B 相同 | tcx-free |
| 用途 | dev 内循环 | 分发（消费端零工具链） |
| 类比 | `java Main.java`（源启动器：编译再跑）/ CPython 跑 .py | `java -jar app.jar` / 跑 .pyc |

#### 一个策略选择（eager vs lazy 降低）

上面默认 **eager 降低**（执行前把字节码全降完）→ 执行相 tcx-free，两模式并发相同。**推荐 eager**：字节码降低
很便宜（只是解析偏移/调用，非 JIT 编译；且模式 A 里 rustc 前端成本已占大头）。
- 备选 **lazy 降低**（首次调用某函数才降它，启动更快）：模式 B 无妨（无 tcx，只是并发缓存）；但**模式 A 的
  lazy 降低会把 tcx 拖进执行相** → 需服务线程 confine tcx（§3.2 旧顾虑）。**为避免这个，模式 A 用 eager。**

> 结论：**tcx 是加载相的事，不是执行相的事。** eager 降低把 tcx 彻底关在 spawn 之前；执行永远干净。
> 只有 JIT（昂贵）是惰性/后台的，而它不需要 tcx。

#### 加载相的并行度：from-source 骑 rustc，.mirvm 我们自己控

- **from-source**：加载相走 rustc 前端。rustc 前端并行化仍 nightly 实验（#113349，未 stable）：typeck/
  borrowck/MIR-opt 可并行，但 **parse/宏展开/HIR lowering 仍串行**（当前 -Z threads 省 20-30%）。**mirvm
  免费吃到 rustc 前端的并行度，无论它是多少**——我们不控制、只骑它。
- **.mirvm**：加载相 = Stable-MIR→字节码，**tcx-free**，**这段并行由我们自己控**（不受 rustc 前端串行拖累）。
- **这正是"加载/执行分离"的价值**：执行相并行是**我们设计的**（N 真线程全并行），与 rustc 前端并行度**解耦**；
  rustc 前端并行与否只影响 from-source 加载相快慢，不影响执行相。

#### 平滑过渡原则（借 JVM JEP 330）

`mirvm run x.rs`（from-source）与 `mirvm run app.mirvm`（分发）必须**同程序、同入口、同行为**，无缝切换
——正如 JEP 330 让"源直跑"与"javac 编译后跑"的 launch-class 一致，程序长大切换时同入口照跑。
**注**：mirvm from-source **先跑完整个 rustc 前端**（所有 check 上前）再执行，故**无 JEP 458 那种"错误延迟到
执行期"的缺点**——错误全在执行前暴露。

---

## 2. 状态三分（RFC 的心脏）

每一份引擎状态，**必须**归入三格之一，并按格施加同步：

| 归属 | 内容 | 同步策略 |
|---|---|---|
| **每线程私有** | native 栈（guest 帧）/ slaved 操作数区 / TLS 实例 / errno / unwind payload 栈 / **Rust Heap 线程 arena** | **无同步**（天然私有） |
| **发布后只读** | 冻结字节码 BytecodeBody / layout 表 / vtable / 常量池 / 已解析 instance 元数据 / 加载的 .mirvm 程序 | **发布屏障**（release 建好 → acquire 读），之后 **lock-free 读**；永不改 |
| **显式同步** | instance→字节码缓存（惰性降低）/ instance→编译码缓存（JIT）/ 线程注册表 / Rust Heap 全局块池 | **锁 / 并发结构 / 原子发布**（写少读多） |

**设计准则（M4 每个数据结构都要过这张表）**：新增任何引擎状态，先问它属哪格。放不进"私有"或"只读"的，
必须显式同步且**尽量做成 insert-once/发布式**（写一次、读多次），避免热路径锁。

**guest 的状态不在此表**：guest 的 `static`、堆对象、原子量都在 Rust Heap（真地址），**由 guest 代码自己
同步**（safe Rust 的 static 受 Sync 约束或藏在 Mutex 后；unsafe 竞争是 guest 责任 C4）。引擎只提供内存
（真地址）+ 执行原子指令（§4），**不替 guest 加锁**。

---

## 3. C8 三招落地

### 3.1 招一：降低时元数据冻结（消灭执行期 tcx）

MIR/Stable-MIR → 字节码时，把**一切 tcx 派生数据**解析进 `BytecodeBody`（frame-abi-bytecode.md §5）：
layout/字段偏移/判别式编码/vtable 布局/drop glue instance/调用目标。**执行只读 BytecodeBody，永不触 tcx。**

- 效果：执行路径**线程安全**（无 !Sync 的 tcx）+ **快**（无 per-op 查询，≈ HotSpot resolved constant pool）。
- BytecodeBody 属"发布后只读"格 → 各线程 lock-free 共享读。

### 3.2 招二：降低在加载相 + JIT 编译后台服务线程

**分两件事，都不让 tcx 进执行相**（详见 §1.2）：

- **字节码降低 = 加载相 eager**（spawn guest 线程前）。模式 A 用 tcx（单线程加载相）；模式 B 无 tcx
  （Stable MIR 自包含）。执行相拿到完整字节码集，**永不碰 tcx**。
- **JIT 编译 = 后台服务线程（HotSpot compiler-thread 同构）**。把热**字节码**→native（Cranelift，C12），
  **不需 tcx**（字节码已 tcx-free）。执行线程遇未编译热函数 → 请求编译 / 继续解释，编完原子发布到"编译码
  缓存"（显式同步格）。**同一后台服务机制即 M5 的后台 JIT。**

> 关键不变式（比旧版更强）：**tcx 只出现在模式 A 的单线程加载相；并行执行相与后台 JIT 服务都不碰 tcx。**
> 违反 = tier-0 的病复发。（旧版设想"服务线程在执行相 confine tcx"——现改为 eager 降低把 tcx 关在加载相，
> 更干净；仅当选 lazy 降低才需回到 confine，故模式 A 选 eager，§1.2。）

### 3.3 招三：Rust Heap 线程本地分配器（借 TLAB 概念，结构用 mimalloc 式）

**决定（2026-07-05 用户定）：直接上线程本地分配器（不走"系统 malloc v0"退路）。**

**关键：借 JVM TLAB 的"线程本地 lock-free 快路径"概念，但结构是 mimalloc/snmalloc 式，不是纯 bump。**
原因——**JVM 纯 TLAB-bump 只在有 GC 时成立**（GC 批量回收、从不单独 free，TLAB 只是 bump 指针）；
**我们有 individual free**（每个 Box/Vec drop 都 dealloc），纯 bump 撑不住（会无限涨）。故需 **size-class
free list**（alloc 从 free list 弹 / 新页 bump；free 推回）+ **remote-free 队列**（跨线程 free）。

- **快路径 lock-free**：每 guest 线程从**自己的线程堆**（size-class free list / 新页 bump）分配。
  `__rust_alloc`/`alloc_zeroed` 落这。无锁、无 libc round-trip。
- **跨线程 free**（Arc/Box 跨线程送后异线程 drop——常见）：**remote-free 队列**（mimalloc/snmalloc 式），
  释放交回原线程堆。非热路径。
- **大对象**：超阈值直接走全局块池 / mmap（绕过线程堆）。
- **refill / 全局块池**：线程堆缺页 → 向全局块池要（显式同步，低频）；后端 = mmap 大块。
- **真实地址 / 隔离 / 对齐**：块按 guest 对齐切片；真实地址天然（§4）；guest 内存与引擎元数据分池（§6）。

**落地选择**：可 hand-roll，也可**直接用 `mimalloc` / `snmalloc-rs` 作 Rust Heap 后端**（它们就是这套、
久经考验，snmalloc 尤擅跨线程 free），我们只加薄的真实地址/隔离包装——符合"别重造轮子"。

**从 JVM TLAB 借什么、不借什么**：

| JVM TLAB 经验 | 借？ | 说明 |
|---|---|---|
| 线程本地 lock-free 快路径 | ✅ 核心 | 现代并发 malloc 也是这个 |
| refill-waste 启发式（尾部剩太多→大对象走共享区，不退休大半满 buffer） | ✅ | 避免浪费尾巴，waste limit 自适应 |
| 自适应大小（按线程分配率、目标 refill 次数） | ✅ 借思想 | JVM 绑 GC epoch 重算；我们无 GC → 换触发（周期/refill 计数） |
| 大对象绕过快路径 | ✅ | 已有 |
| filler/dummy 保堆可解析 | ❌ 无 GC→不需要 | **省掉** |
| 预清零（ZeroTLAB） | ❌ 不需要 | Rust `alloc` 返未初始化，只 `alloc_zeroed` 清零 → **比 JVM 快** |
| 分代/晋升/Eden/safepoint retire | ❌ 无 GC | 不需要 |
| **individual free** | ⚠️ JVM 无此问题 | 我们必须处理 → 用 size-class free list（mimalloc），非纯 bump |

---

## 4. 原子与内存模型（C2/C3）—— 引擎不介入

guest 的原子操作**由 guest 负责语义，引擎只执行硬件原子指令**：

- 解释器：`atomic_*` op 用**宿主原子指令**读写**真地址**（host `AtomicU*` / 内联原子）。
- 编译码：Cranelift 发原子指令 + fence。
- 两者都在真地址上 → 跨真 OS 线程**天然正确**（= native 行为）。Rust 的 C++20 内存模型现成，直映硬件（C3）。

引擎**不同步 guest 原子访问**（那是 guest 级同步）；引擎只保证"原子 op 编译/解释成真原子指令"。

---

## 5. 线程生命周期与注册表

- **create**：只拦 pthread_create 插蹦床（C8）；`Thread.id` 真 pthread_t → join/futex/into_pthread_t
  全走真 libc（frame-abi §8）。新线程 = 真 OS 线程，分配其私有状态（arena/操作数区/TLS）。
- **线程注册表**（显式同步格）：登记活 guest 线程（ID 分配、shutdown、诊断）。建/销加锁。
- **attach（JNI AttachCurrentThread 类比）**：**C 库自己创建的线程回调 guest** 时，thunk 发现该 OS 线程
  不在注册表 → 先 **attach**（为它分配每线程状态、登记），再 interp_frame。detach 时清理。
- **shutdown**：main 线程返回 = **进程退出**（native 语义）；detached 线程随进程消亡。

---

## 6. 隔离 / VM 鲁棒性（C4/C13）——guest UB / FFI / asm 会不会打穿 VM？

**威胁**：真实地址模式下 guest 与 VM 共享一个地址空间（§4），故 guest 的 **unsafe UB**（野指针/UAF/越界）、
**FFI/C 库缺陷**、**inline asm** 能写到 **VM 自有内存**（元数据/解释器/字节码/别的线程栈）→ 崩或静默损坏。
**但 guest 的 safe 代码【证明上】做不到**（Rust 类型系统 C3）——只有 UB 或 native 缺陷能触发，这是绝大多数
代码的非威胁。

**根本张力**：真实地址（为 FFI 零编组 + native 保真而选）与 Wasm 式廉价内存封闭**不兼容**——Wasm 每次访问
bounds-check 到一块线性内存，而 Rust guest 用真指针=真地址。**二者得其一，无免费午餐**。

**分层防御**（2026-07-05 用户定：砍 L2/L4，聚焦 L1+L3）：

| 层 | 防什么 | 状态 |
|---|---|---|
| **L0 类型系统**（白送） | safe guest 代码碰不到 VM 内存 | ✅ 天然，覆盖绝大多数 |
| **L1 结构隔离**（做） | VM 内存 vs guest 内存分池、放已知地址区 + guard page | ✅ 做；且让 L3 的 region check 退化成单次范围比较 |
| **L3 checked 模式**（opt-in，不可信/LLM 用） | 每 raw 解引用前 region check → 野写在损坏前拦 | 见下 |
| ~~L2 MPK/PKU~~ | — | ❌ 太 arch-specific（x86 专属），降级为 L3 的可选加速器 |
| ~~L4 进程沙箱~~ | — | ❌ out of scope（用户定，不管沙箱） |

**L3 checked 模式详解**（核心：Rust 类型系统让它比 Wasm 便宜得多）：

- **JIT 能插检查**：**我们做 MIR/字节码→CLIF 降低，Cranelift 只编译我们给的 CLIF** → checked 模式在降低时
  往 CLIF 插 region-check（load/store 前 compare+branch）；fast 模式不插（= native codegen）。机器码**不脱离
  掌控**。
- **只查 raw 解引用**：safe 引用访问（`*r`, `r:&T`）无 UB 时**证明上有效 → 不查**；**只有 raw 指针解引用
  （unsafe）可能野 → 只查这些**（MIR 按指针类型区分）。良好代码绝大多数是安全引用 → 检查点极少。**Wasm
  查一切，我们只查 raw 解引用**——总开销靠此压下。
- **检查 = region check**（`addr ∈ guest 内存区`）：compare+branch、predicted-taken、只在 raw 解引用、几乎总
  通过（仅真野指针 fail）。L1 让它成单次范围比较。
- **借 JVM**（[implicit null check](https://shipilev.net/jvm/anatomy-quarks/25-implicit-null-checks/)、[uncommon trap](https://shipilev.net/jvm/anatomy-quarks/29-uncommon-traps/)）：静态检查消除（证明 raw 指针来自已知分配+有界偏移→删检查，BCE 同理）；deopt/投机+profile
  驱动（M5）；implicit-trap（guard page）**对我们较难**——guest 内存散（堆 arena+native 栈+statics），非
  有界区，正是 [Wasm Memory64](https://github.com/WebAssembly/memory64/issues/3) 的问题（64 位真指针下 guard-page 招失效），故主用显式 region check。
- **Wasm 界**：guard-page 消除近零成本但**仅对 32 位 offset guest**；mirvm 真 64 位指针比 Memory64 还糟，
  guard-page 用不上——但 Rust safe/unsafe 区分让检查点本就少，靠此而非 guard-page 压开销。
- **诚实界**：只查 raw 解引用会漏"unsafe 把野地址洗进 &T 再解"（要引用级校验=Miri 全量，慢）；但野写几乎
  都走 raw 指针，故性价比高。checked 是个谱：lite（raw 解引用，便宜，抓大多数）→ full Miri（全量，慢）。
- **Model-A 相互作用（记）**：**slaved 操作数区**让 guest 局部在已知区、与 VM native-栈帧分开 → region check
  便宜；**alloca**（frame-abi 承诺的后续迁移）让 guest 局部内联 native 栈、与 VM 状态交错 → region check 难。
  → **checked 模式青睐 slaved 区**。**解耦要求（用户定）：帧局部存储（slaved/alloca，轴 F）与安全模式
  （fast/checked，轴 S）是两根【正交轴】，实现【不得耦合】**——只在 `GuestMemory::contains(addr)->bool`
  谓词处相遇（fast 不调 / checked 调；FrameStorage 提供，slaved=廉价范围比较、alloca=较贵需 per-alloc 追踪）。
  **暂定配对 alloca+fast / slaved+checked 是默认配置、非 hardwire**，任意组合可编译运行。同 JITBackend/os:: 纪律。
- **Profile**：checked 做成 **opt-in**（fast 模式无检查 ≈ native 速度；checked 供不可信/LLM）；解释器/JIT
  加不加检查的速度 delta 是明确的 profile + 优化目标（BCE/deopt 压）。

**关键不对称**：编译码只碰 guest 内存 → 检查可插进其 CLIF；解释器替 guest 执行访问 → 检查插进解释器的
raw-deref 处理。两 tier 都能查，只是插入点不同。

**按用途**：可信 dev/自己项目 = **fast 模式 + L1**（guest UB 自己 bug，= native）；不可信/LLM/agent（P0）=
**checked 模式（L3）+ L1**。**底线**：真实地址与"Wasm 式廉价封闭"不兼容、无免费午餐，但 Rust 类型系统让
checked 模式的开销远低于 Wasm（只查 raw 解引用）。与 §7 "OS 级沙箱管安全"同源（那防伤宿主，这防伤 VM 自身）。

---

## 7. tier-0 → VM tier 迁移

| | tier-0（过渡，待建） | VM tier（目标） |
|---|---|---|
| 线程 | 真 pthread + 蹦床（真 pthread_t、into_pthread_t 成立） | 同 |
| 执行 | **GIL-over-真线程**：解释执行加全局锁串行化，阻塞前（futex/join/FFI/降低）放锁 | **无 GIL**，真并行 |
| tcx | GIL 下单线程访问 tcx（安全） | 不碰 tcx（§3.1/3.2） |
| 结构 | 与 VM tier **同构**（去掉 GIL 即并行） | — |

- **GIL 是踏脚石不是终态**：它让 into_pthread_t 等在 tier-0 也成立（真 pthread_t），且结构同 VM tier；
  **协作式调度是要扔的 emulation**（用户判定：into_pthread_t 证明其跑错）。
- 去 GIL 的前提：状态三分（§2）落实、tcx 赶出执行路径（§3.1/3.2）、Rust Heap 并发化（§3.3）。

---

## 8. Spike 验收（C8 gate，M4 前置）

**目标**：证明"N 真 OS 线程各跑 interp_frame，共享发布后只读字节码 + per-thread arena + 显式同步缓存"
**在引擎自身层面无数据竞争**。

- **负载**（guest 程序，跑在真线程上）：原子计数器、mpsc 生产消费、Mutex 8×N 争用、Arc 共享求和、
  scoped threads。
- **通过标准**：**引擎自身**（VmShared、缓存、注册表、arena/块池、发布协议）**过 ThreadSanitizer**。
  - **明确排除**：guest 程序自身的 unsafe 数据竞争**不在** TSan-clean 承诺内（那是 guest 责任 C4，且合法
    guest 程序 safe 代码无竞争 C3）。TSan 只针对**引擎实现自身的状态**。
- **同时**：输出与单线程/tier-0 差分一致（对输出确定的负载逐字节；时序敏感的靠不变式）。

这是 §5.3 账本里"引擎过 TSan"的具体化，也是 C1"去 GIL 前的硬关卡"。

---

## 9. 开放问题 / spike 清单

1. **发布协议**：instance→字节码/编译码缓存的 insert-once + 无撕裂发布。并发 HashMap（如 dashmap 式）+
   原子发布 vs RwLock（正确优先）。先 RwLock，profile 再优化。
2. **TLAB 分配器细节（已定上 TLAB，§3.3）**：arena chunk 大小/大小类、remote-free 队列的具体结构、
   大对象阈值、arena 在线程退出时的回收。照搬 mimalloc/jemalloc 结构。
3. **thunk attach 的每线程状态分配**：C 创建线程首次回调时 attach 的开销与生命周期（detach 时机）。
4. **隔离强度**：Rust Heap 与引擎元数据分池的具体布局；要不要 guard page / 单独 mmap 区。
5. **GIL 放锁点**：tier-0 GIL 在哪些点放锁（futex_wait/join/阻塞 FFI），避免"持锁阻塞"死锁。
6. **模式 A 的 eager 降低成本**：若"全量降字节码"在大程序上启动偏慢，再评估 lazy+tcx-confine（§1.2/§3.2），
   但优先保 eager（执行相 tcx-free）。

---

## 10. 与 frame-abi-bytecode.md 的接口（共同成立，C11）

- 每线程 native 栈放 guest 帧（模型 A）+ 每线程 slaved 操作数区 = §2"每线程私有"格。
- BytecodeBody（冻结元数据）= §2"发布后只读"格；lowering（§3.1）产出它。
- JITBackend/Cranelift 编译码缓存 = §2"显式同步"格；JIT 服务线程（§3.2）产出。
- unwind（frame-abi §7 候选 A，Cranelift landing pad）在**每线程 native 栈**独立进行，无跨线程同步。
- 蹦床/thunk（frame-abi §8）= §5 create/attach。

两文档合起来 = M4 引擎的完整地基：帧/字节码/JIT（frame-abi）+ 并发/状态/生命周期（本文）。
