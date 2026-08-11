# vmctx 传递机制：编译码（与回调边界）如何够到 VM 执行态

> 文档状态：**保留的机制比较与决策历史；终裁已落笔（2026-07-21，本文 §7）**。
> 2026-07-07 原文在 Spike 2 后比较 P/T/R；2026-07-11 的分层结论（m5-design D5）与
> 2026-07-21 的终裁落笔（T 骨架生产定稿 + 复测双触发器 = E6 进场 / 多 Engine 立项，
> decision-history §7.20）为准：native→guest 边界是 TLS + lazy attach（生产）；编译码
> 零 ctx 站点实证；P 不再是生产候选。完整时间线与重开条件见
> [decision-history.md](../decision-history.md)。原文保留三案论证。
>
> 2026-07-07 时的**结论先行**：
> - **边界机制已被逼定**：FFI 逃逸指针 / native 回调 / 信号处理器的入口，必须**按当前线程查找执行态
>   （TLS）+ 惰性 attach**——这不是偏好，是被"信号在任意线程跑 + 执行态每线程一份"逼死的（§1.3）。
> - **当时的开放项**：编译码之间传 ctx 用显式 vmctx 首参（Wasmtime 式）还是 pinned 寄存器
>   （HotSpot 式），二者都可行（Cranelift 均支持），是性能/工程细节（§5.2）。
> - **skeleton（spike2）用显式首参 (a) 是对的**——手写验证适配器最干净；但它不是 M4 终选。
> - Spike 3（unwind）与本问题**正交**，不受影响。

---

## 0. 问题定义：谁需要 ctx、mirvm 的压力有多大

三种帧同在一条 native 栈上（模型 A，Spike 2 已验证），只有一份每线程执行态：

```
 ┌──────────────┐   i2c    ┌──────────────┐  FFI 直通   ┌────────────────┐
 │   解释帧      │ ───────► │    编译帧     │ ──────────► │   native 帧     │
 │ interp_frame │ ◄─────── │  (JIT 产物)   │ ◄────────── │ (libc / C 库)   │
 └──────┬───────┘   c2i    └──────┬───────┘   回调(?)    └────────────────┘
        │ 显式 *mut Ctx 参         │
        │（宿主代码，随便传）        │ ← 靠什么够到？＝本文主题
        ▼                         ▼
 ┌───────────────────────────────────────────────────────┐
 │ Ctx（vmctx，**每 guest 线程一份**执行态；指向共享只读程序）│
 │   TLAB 分配指针 │ 操作数区 │ dispatch/kinds │ panic 态   │
 └───────────────────────────────────────────────────────┘
```

- **解释帧**：宿主代码，显式传 `*mut Ctx` 参数即可（Spike 2 已定形），无问题。
- **native 帧**：定义上不知道 ctx 存在——这正是"逃逸/回调"问题的来源。
- **编译帧**：本文主题。它需要一个机制拿到 ctx。

**mirvm 特殊性（压力远小于 Wasm）**：mirvm 用真实地址（C2），guest 访存编译成**裸 load/store**、
静态量/vtable 是**冻结的真地址常数**、compiled→compiled 是**直接 native call**——全都不经 ctx。
Wasmtime 每次线性内存访问都要经 vmctx 拿内存基址，我们不用。编译码真正碰 ctx 的只有：

| 场景 | 频率 |
|---|---|
| TLAB 分配（bump 指针在每线程执行态里） | 热（每个 Box/Vec 增长） |
| c2i：调一个还在解释态的 callee | 冷→热身期常见，JIT 链接后消失 |
| panic/unwind 簿记、guest 栈界检查 | 低频 / 入口级 |
| checked 模式的 `GuestMemory::contains`（C13） | 仅 checked 模式 |

**推论**：大多数纯计算函数**根本不需要 ctx**。"把 vmctx threading 过每个函数"（Wasm 的默认姿势）
对 mirvm 是在给所有人收税、供少数人用——这从一开始就削弱了方式 (a) 的地位。

---

## 1. 决定性约束：不是性能，是 FFI 正确性

三种机制的性能差异是二阶的（几条指令）。**一阶判据是这两条**：

### 1.1 mirvm 的 FFI 命根子：guest 函数指针必须是 plain-C 可调的

`into_pthread_t`、qsort 比较器、signal handler、C 库回调——mirvm 的 FFI 设计（DESIGN §7、C2）
要求逃逸出去的 guest 函数指针**能被 native 按普通 C 签名直接调**，零编组、零包装。
Wasmtime 敢把 vmctx 做成显式首参，是因为 Wasm 根本没有"裸函数指针按 C ABI 逃逸给 native"这回事
（一切经 Wasmtime API）。mirvm 恰恰相反——这是我们与它分道的根本原因。

### 1.2 ctx 是每线程的（并发架构，concurrency-arch.md）

M4 下 Ctx = **每 guest 线程一份**执行态（操作数区、TLAB 都是线程私有），指向共享只读程序。
所以"把 ctx 缝进 thunk"这种捕获式方案，捕获的是**创建时那条线程**的 ctx——回调若发生在别的线程，
用错线程的操作数区/TLAB = 数据竞争/腐坏，**原理性错误**，不是慢。

### 1.3 信号处理器把问题钉死

signal handler **在接收信号的任意线程上跑**（POSIX）。guest 注册的 handler 被投递时：

- 捕获式 thunk（缝死一个 ctx）→ 大概率错线程 → 错。
- 保留寄存器 → 信号打断时寄存器是被打断代码的任意值 → 错。
- **唯一正确的做法：入口处按"当前线程"查找执行态 = TLS 读**（空则惰性 attach）。

**所以边界机制没有选择余地**：凡是 native/内核可以任意线程调进来的入口（逃逸指针、回调、信号），
prologue 必须做 TLS 查找。剩下的唯一自由度是**编译码内部**怎么传。

---

## 2. 方式 (a)：显式 vmctx 首参（Wasmtime / JNI-JNIEnv / lua_State 式）

每个编译函数的真实签名 = guest 签名 + 一个隐藏首参：

```
interp_frame(ctx, …)
    │  i2c: call f(ctx, a0)           ── ctx 装进首参寄存器 rdi
    ▼
compiled_f(ctx, a0)                   ── 签名 = guest 签名 + 1 个隐藏首参
    │  c2i: call call_guest(ctx, g, …)
    ▼
interp_frame(ctx, …)                  ── 再入；ctx 一路线性传递，永不落地
```

**优点**：内部取 ctx 最快之一（就在寄存器里）；无全局状态；Spike 2 用它验证了适配器模型。

**致命伤（对 mirvm）**：逃逸指针签名错位——

```
native 以为:   cmp(a: *T, b: *T) -> i32             ← plain C
编译码实际:    cmp(ctx, a, b)     -> i32             ← 多一个隐藏首参！

唯一补救 = 捕获式 thunk（运行期造小段码，缝一个 ctx 进去）:

qsort ── call thunk(a,b) ──► ┌ thunk: rdi ← 〈捕获的 ctx〉┐ ── jmp ──► cmp(ctx,a,b)
                             └ (libffi closure)         ┘
```

- **每个**逃逸指针都要造 thunk（qsort/pthread/signal 全包一层）——正好破坏"真指针直传"；
- thunk 捕获的是创建线程的 ctx → 跨线程回调/信号**原理性错误**（§1.2/1.3），除非 thunk 里
  也做 TLS 查找——那 (a) 就退化成"边界靠 TLS"，显式参只剩内部意义。

**先例**：Wasmtime（vmctx）、LuaJIT/Lua C API（`lua_State*`）、JNI 的 C API（`JNIEnv*` 就是
显式 ctx 参——但 JNI 是"专用 API 边界"，不是 plain-C 逃逸）。

---

## 3. 方式 (b)：thread-local（TLS）

```
guest 线程启动（或外来线程 attach）时，设一次:
    TLS[CUR_CTX] = &本线程 Ctx

interp_frame(ctx, …)                  ── 宿主代码照旧显式传参
    │  i2c: call f(a0)                ── 纯 guest 签名，无隐藏参
    ▼
compiled_f(a0)
    │  需要 ctx 的点（分配 / c2i / panic 态）:
    │      ctx = TLS[CUR_CTX]         ── 一条 fs 段寻址 load
    │  c2i: call call_guest(ctx, g, …)
    ▼
interp_frame(ctx, …)
```

**逃逸与回调 = 零处理**，签名本来就是 plain C；且天然拿对线程：

```
qsort ── call cmp(a,b) ──► compiled_cmp:
                              ctx = TLS[CUR_CTX]   ← 同线程回调 ⇒ 正是当前线程执行态 ✓

signal（任意线程投递）──► handler:
                              ctx = TLS[CUR_CTX]   ← 自动是**接收信号那条线程**的 ctx ✓
```

### 3.1 外来线程问题 → 惰性 attach（JNI AttachCurrentThread 同款）

```
坑：C 库自己的工作线程（mirvm 从没见过这条线程）回调 guest：

  第三方线程 ── call guest_cb(…) ──► 入口: ctx = TLS[CUR_CTX] = NULL ✗
                                       │
                                       ▼ 惰性 attach
                                     建本线程执行态（操作数区/TLAB）→ 设 TLS → 继续
```

JVM（`AttachCurrentThread`）和 Go（cgo 回调的 `needm`）都有完全同款的机制。注意**任何方案都
逃不掉 attach**（(a) 的 thunk 在外来线程上同样要建执行态）——TLS 只是让"要不要 attach"变成一次
自然的空检查。1:1 真线程模型让 attach 便宜：任何 OS 线程挂上一份执行态就能跑 guest。
attach 逻辑归 `os::thread`（P7）。

### 3.2 JIT 代码生成注记（成本的真相）

TLS 读的贵贱**完全取决于 TLS model**：
- ctx 变量住在 mirvm 宿主自身（主程序/其 lib 的 `#[thread_local]` 静态量）→ **initial/local-exec**
  → 一条 `mov rax, fs:[tpoff]`，≈普通 L1 load。tpoff 在进程加载后恒定，**宿主在 JIT 时把这个常数
  递给代码生成**即可（不走 Cranelift `tls_value` 的 general-dynamic 路径——那才是贵的
  `__tls_get_addr` 调用）。
- 兜底方案：编译码 `call mirvm_ctx()`（宿主 3 条指令的 helper）——每个用点多一次 call，
  对"入口一次"的频率无所谓。
- 细字：若将来 mirvm 引擎作为 dlopen 插件嵌入（M7+ 嵌入 API 情景），宿主自身的 TLS 才会掉进
  dynamic model，届时再议。

且频率可以压到**每激活至多一次**：函数体内 ctx 是循环不变量（一个帧激活从生到死在一条线程上），
JIT 在入口 load 一次、寄存器携带整个函数体——这其实就把 (b) 在函数内部退化成了 (a)，见 §5。

---

## 4. 方式 (c)：保留寄存器（HotSpot r15 / Go g 式）

```
JIT 码全程把 r15 钉为 ctx（regalloc 不许分配它）:

compiled_f:      ctx ≡ r15，取用零指令               ← 最快
    │ call native        （SysV：r15 是 callee-saved）
    ▼
native 帧:       push r15 … 自由把 r15 当临时用 … pop r15; ret
    │                    └── 返回前会恢复 ✓（compiled→native→返回 没事）
    │ 但 native 半路回调 guest：
    ▼
compiled_cb:     读 r15 = native 的临时值 = 垃圾 ✗✗
```

**HotSpot 为什么活得下来**：JNI 边界**不是 plain C**——Java→native 显式传 `JNIEnv*`，native 回调
Java 必须经 JNIEnv 函数表，回调路径从 `JNIEnv*` 恢复线程指针、重建 r15，**不靠寄存器幸存**。
即 HotSpot = 内部 (c) + 边界 (a)（JNIEnv 走私通道）。mirvm 的 FFI 目标恰是**没有走私通道的
plain C**（§1.1）⇒ (c) 的边界破洞对我们无法自愈，**必须**配 TLS 边界恢复。

**作为内部机制**仍可行：Cranelift 有 pinned-reg 支持（`enable_pinned_reg`，x86-64 上即 r15），
SpiderMonkey 用它。代价：全程征用一个 callee-saved 寄存器 + arch-specific 心智负担。

**先例**：HotSpot（r15 = JavaThread*）、Go（g 寄存器 + cgo `needm`）、Erlang BEAM（process 指针
寄存器）。共同点：**都配了边界重建机制**——没有谁裸靠寄存器过 FFI。

---

## 5. 结论形态：混合 = 边界 TLS（被逼定）+ 内部快约定（二选一）

§1 已把边界钉死；§0 说明内部用点稀少。合起来就是 HotSpot 多入口思想（verified-entry /
c2i-adapter 的同构）：**每个编译的 guest 函数两个入口**——

```
                ┌──────────────────────────────────────────────┐
逃逸给 native ──►│ f_boundary:   ← C-ABI 边界入口（plain C 签名） │
的是这个地址     │    ctx = TLS[CUR_CTX]（空则惰性 attach §3.1）   │
                │    jmp f_fast(ctx, args…)        ← tail-call  │
                ├──────────────────────────────────────────────┤
内部直接调用 ───►│ f_fast(ctx, args…):   ← 快入口（内部约定）      │
用这个          │    …函数体：ctx 已在寄存器，需要就用，             │
                │    compiled→compiled 直接 call 别人的 f_fast，  │
                │    ctx 随寄存器继续传…                          │
                └──────────────────────────────────────────────┘

compiled→compiled : 全程 fast 入口，零 TLS
native→guest 边界 : 一次 TLS 读（+ 首次 attach）
信号 handler      : 同边界入口，天然拿对线程
```

### 5.1 thunk 范围收窄（重要红利）

| 逃逸的是 | (a) 纯显式参 | 混合（边界 TLS） |
|---|---|---|
| **编译态** guest fn | 需捕获式 thunk（且跨线程错） | **零 thunk**——f_boundary 本身就是 plain-C native 码 |
| **解释态** guest fn | 需 thunk | 仍需 thunk（它没有机器地址——这本来就是 thunk 的本职），thunk 的 prologue 与 f_boundary 共用同一段 TLS/attach 逻辑 |

即：thunk 从"每个逃逸指针都要"收窄回它的本职（给解释态函数一个机器地址），corpus §2.3 的
signal-thunk、pthread start_routine thunk 都落在这条既有路径上，且**热身后**（函数被 JIT）逃逸
指针可以直接给 f_boundary 地址、thunk 消失——与 frame-abi §8 "JIT tier thunk 消失"的既有判断吻合。

### 5.2 内部约定的最后一个自由度（历史比较；已由 M5 D5 分层结论替代）

| | 内部 = 显式 vmctx 参（Wasmtime 式） | 内部 = pinned r15（HotSpot 式） |
|---|---|---|
| 取 ctx | 首参寄存器 | 零指令 |
| 调用点 | 每个 call 多装一个参 | 无 |
| 寄存器压力 | 占一个**参数**寄存器（可被 regalloc 复用） | **全程**征用一个 callee-saved |
| 不需要 ctx 的叶函数 | 仍被 threading 收税 | 零税 |
| Cranelift 支持 | 平凡（就是个参数） | `enable_pinned_reg` |
| arch 依赖 | 无 | 高（每 arch 选寄存器） |

二者都正确（边界已由 TLS 兜住），差异是纯性能/工程，**等 Spike/M4 有了真 Cranelift 管线再拿数据定**。

**Spike 5 初判数据（2026-07-07，history/spike5-cranelift-adapters.md）**：两变体都在真 Cranelift 上
实现并全对（`enable_pinned_reg`/`get/set_pinned_reg` 开箱即用；R 的 f_boundary 按 §5 图实现：
save→set→call fast→restore，宿主 callee-saved 语义保持，重入幂等）。fib(30) 直接调用微基准：
**R（pinned r15）5.47ms vs P（显式参）5.90ms——R 快 ~8%**；vcode 坐实 P 的 threading 税
（每帧 ctx 进 callee-saved + 每调用点重装）、R 内部直调零 ctx 搬运。初判 R 无一处输于 P 且
多入口结构已实证；**终裁仍留 M4 真负载**（非代表性微基准，勿过度解读）。

---

## 6. 对比总表与先例

| 维度 | (a) 显式参 | (b) TLS | (c) pinned 寄存器 | 混合（TLS 边界 + 快内部） |
|---|---|---|---|---|
| 编译码内取 ctx | 寄存器 | 一条 fs-load（可入口一次+携带） | 零指令 | 内部=寄存器 |
| 逃逸 fn ptr 签名 | ✗ 错位 → thunk 遍地 | ✓ plain C | ✓ plain C | ✓（f_boundary） |
| native→guest 回调 | ✗ 必须 thunk | ✓ 直通 | ✗ r15 是垃圾 | ✓ |
| 信号（任意线程） | ✗ 捕获式原理错 | ✓ 天然对 | ✗ | ✓ |
| 外来线程 | 也要 attach | TLS 空检查 → 惰性 attach | 也要 attach | 同 (b) |
| 每线程 ctx（§1.2） | thunk 捕获错线程 | 天然 per-thread | — | 天然 |
| 先例 | Wasmtime / lua_State / JNIEnv | V8 `Isolate::GetCurrent` / CoreCLR | HotSpot r15 / Go g / BEAM | HotSpot 多入口（verified/adapter entry） |

**没有任何一个生产 VM 裸靠单一机制过 FFI 边界**——HotSpot=寄存器+JNIEnv 走私，Go=寄存器+needm，
V8=寄存器缓存+TLS。mirvm 因为 plain-C FFI 没有走私通道，边界只剩 TLS 一条路，反而把设计空间收敛了。

---

## 7. 决策状态（2026-07-07 原状态 + 后续更新）

> **2026-08-10 更新（多 Engine 触发器命中）**：终裁选择 T，即线程局部的
> “当前 Engine”身份。每个 Engine 持有自己的 `Arc<Shared>`，宿主线程的执行态表按
> Engine id 保存 `Ctx`；边界进入时激活对应项，嵌套返回时恢复外层。旧的进程级
> `SHARED` 和 JIT 单 worker 已拆成每 Engine 状态。
>
> 这次选择不是性能判断。R（保留寄存器）能让已选中的 ctx 读取更快，却不能说明同一
> 线程此刻进入了哪个 Engine；多实例身份仍然必须先由 T 建立。R 因而只保留为 E6
> 分配/TLS 快路径进入编译码后的性能候选，不能替代 T。双 Engine 独立 ctx、嵌套激活
> 恢复和 Engine 析构注销已有单元回归。回调撤销、长寿命宿主线程上的 TSD 回收和稳定
> 嵌入 API 仍属于 E22 后续边界。

> **2026-07-21 更新（T3/M5.5 终裁落笔）**：T 骨架的生产实证与复测触发器定稿。
>
> **T 骨架生产实证**（T1 战役全链落地后）：准入三表（stmt/rvalue/terminator）
> 穷尽的今天，编译码触碰每线程执行态的站点**仍为零**——格①（GC/safepoint/
> TLAB bump/栈检查/线性内存）是架构承诺结构性不存在；格②（FFI/c2i/catch/
> panic 簿记）语义上就是助手形状，ctx 获取（SHARED + TLS attach，M4.4 被逼定
> 的边界机制原样）摊销在助手重量级内；格③（分配/TLS 内联）未进场，编译码
> 零站点。fast 签名 = 纯 guest 签名（CalleeAbi 全形态，T1-a），`get_ctx()`
> 单缝 = SHARED 静态 + 边界 TLS attach——没有为任何假想负载预付寄存器租金。
>
> **计量基线**（`MIRVM_JIT_STATS=1` 助手频度统计，12 桶原子计数 + atexit
> dump，片1 `05021d2`；计时为 JIT-on 墙钟）：
> - fib(32)：61–80ms（≤80ms 硬门）；**全空桶**——数值内核零 ctx 站点直证。
> - rayon（corpus 条目）：冷 506ms / 热 189ms；`tls_ref=1392、c2i=225467、
>   alloc=0`——真实并行负载格③仍为零（分配未进编译码）。
> - unwind_probe（30000 迭代 panic 密径）：`alloc=118529、tls_ref=268212、
>   c2i=3.59M、call_terminate=2.97M`——格② 密度的极端形态；这些调用本身
>   就是助手，T/R 之差不作用于它们。
> - corpus per-crate 分布：见 decision-history §7.20（T3 片2 全量跑批）。
>
> **复测触发器（双闸，用户 2026-07-21 裁定并列；先到先裁）**：
> 1. **E6 进场**——分配快路径内联 / guest TLS 快路径内联进编译码立项时，
>    以该负载复测 T vs R（对照基线 = 本节数据 + spike5 §5.2 的 ~8%）。
> 2. **多 Engine 嵌入立项**——SHARED 是进程级单例，daemon/mode B/库化的
>    多实例场景它是真 blocker；彼时 vmctx（显式参或 TLS 镜像）非做不可，
>    与性能无关，直接重开终裁。
>
> **T→R 升级路径**（ABI 兼容单开关，spike5 已验形状）：(a) `enable_pinned_reg`
> ISA 旗；(b) `get_ctx()` 降低 TLS load → `get_pinned_reg`；(c) 边界入口
> save/set/restore。翻转粒度 = 整个 JIT 代码缓存按新制度重编——免费：代码
> 缓存进程内易失，mode B 分发字节码非机器码，无跨进程兼容面。诚实条款：
> 分层 ≠ 同进程逐函数混用（T 码体内 r15 是普通 callee-saved 临时、R 码
> 假设 r15≡ctx，跨制度调用需包装）。
>
> **挂载点评审**（T3 裁定，均不动）：CallIndirect 内联缓存、LSDA 存储升
> JIT data object——当前无负载区分其价值，保留 E7 优化项池，随触发器复测
> 时一并重估。
>
> 以下 2026-07-11 更新与原决策现场保留备查。

> **2026-07-11 更新**：M5 D5 将选择改写为 T 骨架 + R 兼容缓存层。T/R 共享纯 guest fast
> 签名与 TLS 边界；只有 `get_ctx()` 降低、pinned-reg 开关和边界 save/set/restore 不同。
> 分配或 guest TLS 内联进入编译码后，才以该负载复测是否启用 R。以下条目保留 2026-07-07
> 决策现场，其中“待 M4 定”已是 historical。

- **已定**：边界（逃逸/回调/信号/外来线程）= TLS 按当前线程查找 + 惰性 attach（归 `os::thread`，P7）。
  被 §1 三条约束逼定，无备选。
- **已定**：skeleton/Spike 阶段用 (a) 显式参——手写验证最干净，Spike 2 已用它验证适配器模型；
  Spike 3（unwind）正交，继续用 (a)。
- **待 M4 定（已有 Spike 5 初判数据）**：内部约定 = 显式 vmctx 参 vs pinned r15（§5.2）。
  Spike 5 用真 Cranelift 两者都实现并全对，**R 快 ~8% 且多入口结构（f_boundary）已实证**；
  终裁留 M4 真负载。
- **开放**：attach 的生命周期语义（外来线程的执行态何时回收——JNI 要求显式 Detach，Go 用
  m 池化；与线程退出钩子/TLS 析构交互）；f_boundary 入口与 unwind info 的交互（Spike 5 已验
  CFI 半边：JIT 帧 eh_frame 注册后宿主 panic 正确穿过，含 R 的 pinned 入口）。

关联：frame-abi-bytecode.md §10.7（开放问题挂点）、§8（thunk 重入）、concurrency-arch.md
（每线程执行态）、corpus §2.3（signal 需 thunk——本文 §5.1 收窄其范围）、
spike2 文档 §3（三选初判——本文取代之，结论更新为"边界被逼定 TLS"）。
