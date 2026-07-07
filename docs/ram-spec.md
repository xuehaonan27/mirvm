# RAM-SPEC —— Rust 抽象机器规格（mirvm 的语义契约）

> **状态：草稿，待评审。** 本文定义 **mirvm 实现的那台 Rust 抽象机器（Rust Abstract Machine, RAM）**，
> 是 mirvm 对外的**语义承诺 / 正确性契约**——"事实标准实现"从口号变成可对照的条文。
>
> **本文不是**：Rust 官方形式规范（不存在）；不是从零重造一套操作语义（那是 opsem 团队十年工程 + Miri 的
> 代码）。**本文是**：把"事实 RAM"（MIR 操作语义 + opsem 团队内存模型 + provenance + rustc layout）
> 以**契约形式**钉清，引用权威来源、明确 mirvm 的边界/UB 立场/自由/偏差。实现细节见 §4 内存、
> concurrency-arch.md、frame-abi-bytecode.md——那些说 HOW，本文说 WHAT。

---

## 0. 定位：一台机器，三个实现

Rust 无官方形式规范，但存在一台**事实上的抽象机器**。它有三个实现，实现同一台 RAM：

| 实现 | 立场 | 用途 |
|---|---|---|
| **native codegen**（rustc+LLVM/cranelift） | 生产执行 | 编译成机器码跑 |
| **Miri** | *检查*实现（宁慢勿漏 UB，开满 provenance/aliasing 检查） | UB 检测 |
| **mirvm** | *运行/标准*实现（假设合法、追求快，关检查） | 快速运行 / 事实标准 |

**核心推论**：三者实现同一台 RAM，所以 **"mirvm 输出 == native 输出" 是同源的必然，不是巧合**——
这是 mirvm 差分对拍 native 为何有效的**理论依据**（§8）。mirvm 与 Miri 的差别不在语义，在**质量取向**
（检测 vs 运行）；UB 检测对 mirvm 是可选 QoI，不是身份。

---

## 1. 正确性契约

> **对任何在 RAM 下有已定义行为的程序 P，mirvm 执行 P 的【可观测行为】符合 RAM 允许 P 产生的行为集合。**

三个要点，缺一不可：

1. **只管【可观测行为】**（as-if，§5）：I/O、syscall 效果、volatile、进程退出码、panic 消息。其余内部
   （分配位置、执行 tier、调度）自由。
2. **【有已定义行为】才承诺**：UB 程序 mirvm 不作承诺（§2 UB 级、§6 立场）。
3. **符合【行为集合】而非单一值**：RAM 对很多东西只约束一个**集合**（unspecified / 非确定，§2）；mirvm 产出
   集合中任一即合规，**不必与 native 逐字节相同**（例：地址值、HashMap 迭代序、repr(Rust) 布局、线程调度）。

---

## 2. 定义度四级（"符合"到底是什么意思——本节是契约的严格核心）

RAM 把程序行为分四级，mirvm 对每级的义务不同：

| 级别 | RAM 说什么 | mirvm 义务 | 例 |
|---|---|---|---|
| **well-defined** | 唯一确定的行为 | **必须**产出该行为 | `2+2==4`、`Vec::push` 后 len+1 |
| **unspecified** | 允许的**集合**，实现挑一个 | 产出集合中**任一**即可（**不必同 native**） | repr(Rust) 字段顺序、HashMap 迭代序、`&x as usize` 的具体地址值、未初始化 padding 字节 |
| **non-deterministic** | 允许**多个执行** | 产出**任一合法执行**即可 | 线程调度交错、弱内存序可见性、`thread_rng`、`HashMap` 随机种子 |
| **UB** | **无定义** | **无约束**（假设不发生，不检测；§6） | 数据竞争、越界、use-after-free、读未初始化、违反别名 |

**推论（对差分测试至关重要，§8）**：只有 **well-defined 的可观测输出**能与 native **逐字节对拍**；
unspecified/non-det 输出只能靠**不变式**（如"和为 25"而非"顺序为 …"）或**归一化**（如线程名）比较；
UB 程序**不对拍**（两边都可任意）。

---

## 3. RAM 的组成（五部分 + UB）

每部分给 RAM 的定义 + mirvm 的实现指针。

### 3.1 存储（Storage）

- **分配（allocation）**：互异、对齐、有大小、有生死（live/dead）。分配返回互异、对齐、非空的地址。
- **字节**：每字节有**初始化状态**（init/uninit）；指针大小的字节可携带 **provenance**。
- **指针 = 地址 + provenance**。int→ptr、ptr→int、exposed provenance（Strict Provenance）。
- **别名模型**（Tree Borrows，opsem 团队仍在定）：**定义 UB**（违反别名 = UB），合法程序不违反。
- *mirvm 实现*：**真实地址**（分配基址 = 宿主真址，§4）；**不追踪 per-allocation 元数据**（init mask/
  provenance/bounds 是检查器 overlay，fast machine 不需要，§4）；别名不强制（§6/§7）。

### 3.2 值与布局（Values & Layout）

- 类型如何 **realize 成字节**：size / align / 字段偏移 / 判别式编码 / niche 优化。由 **rustc layout 算法**
  固定，**target-specific**（指针宽度/对齐随平台）。repr(C) 遵 C ABI；repr(Rust) 布局 **unspecified**（§2）。
- 值形态：标量（scalar）、标量对（scalar pair，如 &[T]/胖指针）、聚合（aggregate）。
- *mirvm 实现*：**复用 rustc layout**（target==host / 冻结进字节码，C8/C12）——与 native 逐位一致。

### 3.3 计算（Computation）

- **MIR 操作语义**：place（含 projection）、rvalue、statement、terminator。函数调用、参数传递、返回。
  **unwinding**（panic 沿栈退帧跑 Drop）、**Drop**（含 drop glue、drop 顺序）。
- **const eval**：编译期子集，**同一台机器**（const 求值 = 编译期跑 RAM）。
- *mirvm 实现*：解释 MIR/字节码（tier-0/M4）；**unwind 自实现**（VM 拥有栈帧；模型 A 下走 native 栈 +
  Cranelift landing pad，frame-abi §7）。

### 3.4 并发（Concurrency）

- **内存模型 = C++20 派生**：原子操作 + 序（SeqCst/Acquire/Release/AcqRel/Relaxed）、happens-before、
  synchronizes-with；**数据竞争 = UB**。
- **线程**：`std::thread` 语义（spawn/join/生命周期）；**TLS**（thread_local）。
- *mirvm 实现*：**真 1:1 OS 线程**（C8）；guest 原子 → **宿主原子指令**（真地址，= native codegen 行为，
  弱内存序自然恢复，C2/C3）；引擎不介入 guest 同步（concurrency-arch §4）。

### 3.5 可观测行为（Observable behavior）

- **I/O、syscall 的效果、volatile 访问、进程退出码、panic 输出**——as-if 规则要保持的东西。
- *mirvm 实现*：**真 OS 直通**（read/write/epoll/…真内核 fd，§7/async-stackless.md）；panic→退出码 101 等。

### 3.6 未定义行为（UB）

- RAM **未定义**的程序状态。**符合规范的实现对 UB 不受约束**；*检查*实现（Miri）在此陷入报错；
  *标准*实现（mirvm fast）**假设它不发生**。
- *mirvm 立场*（§6）：**假设合法、不检测**（P3）；guest 的 UB（竞争/越界/UAF）在真实地址下 = **宿主 UB**，
  与 native 一致（C4）。

---

## 4. RAM 的边界（哪里不再是 RAM）

**FFI = 抽象机器的边界。** 这条给"什么在语义内、什么在语义外"一个原则性定义：

- **界内**（解释/编译的 Rust）：实现 RAM 语义。
- **界外**（native 代码：libc、C 库、裸机器码）：**RAM 不建模其内部**，mirvm 只**移交控制权**（FFI 出）
  或**接收控制权**（thunk 入）。native 的内存分配（Native Heap）、native 内部行为**在 RAM 之外**。
- **inline asm**：RAM 内一段**不透明机器码效果**——不是 RAM 计算的一部分，mirvm 只能模拟其效果或函数级
  拦截（§7/C10）。
- **跨边界的 UB**：Rust ABI 规定 unwind 穿 native 帧 = UB → mirvm **abort**（= native）。

含义：**"能在 mirvm 跑 ≈ 能通过 rustc 编译并在 native 跑"**，边界处两实现同样"移交给 native"——一致性在
边界处由"双方都调真 native"保证。

---

## 5. mirvm 的自由（as-if 授权了什么）

只要 §1 的可观测行为契约成立，mirvm 在以下方面**自由**（且已用于设计）：

- **执行 tier**：解释 / 字节码 VM / JIT（C11/C12）——同一 RAM 的不同实现。
- **托管堆分配器**：arena/TLAB、真实地址（§4）——RAM 只要求分配互异/对齐/非空，钱从哪来自由。
- **线程实现**：真 OS 线程 / （tier-0）GIL-over-真线程——只要都实现 §3.4 的并发语义、产出合法执行。
- **调度**：任何产出**合法执行**（§2 non-det）的调度都合规。
- **unspecified 的具体取值**：地址、repr(Rust) 布局、HashMap 序——任挑（§2）。

**不自由的**：well-defined 的可观测行为（必须保持）。判据永远是"**可观测行为变了吗？没变即自由**"。

---

## 6. mirvm 对 UB 与"未定"的立场

### 6.1 不检测 UB（设计选择，非偏差）

mirvm **假设程序合法、不检测 UB**（P3）。这不是"偏差"（偏差是"合法程序上与 RAM 有出入"），而是
**质量取向选择**：把 UB 检测留给 Miri。对**合法程序**，mirvm 完全符合 RAM。

### 6.2 RAM 处于"未定"时，mirvm 天然中立

事实 RAM 在若干处仍**未定**（opsem 团队仍在议，如 Tree Borrows vs Stacked Borrows 的确切别名规则）。
**mirvm 因"不检测"而对这些未定细节天然中立**——那些细节只在**判定 UB** 时才有区别，而 mirvm 不判 UB，
所以**无论 opsem 最终怎么定，mirvm 照跑合法程序不变**。这是"关检查"的一个副产物优点。

### 6.3 guest UB = 宿主 UB

真实地址下，guest 的 unsafe UB（数据竞争/越界/UAF）= mirvm 进程内的宿主 UB，与 native 行为一致（C4）。
**含义**：guest UB / FFI 缺陷 / inline asm 能打穿 VM 自有内存（共享地址空间）→ 崩。但 **safe guest 代码
证明上做不到**（C3），只有 UB/native 缺陷能触发。防护分层（L0 类型系统 / L1 结构隔离 / L2 MPK / L3 checked
模式 / L4 进程 containment）、无免费午餐（真实地址 vs Wasm 式封闭二选一）——详见 concurrency-arch.md §6、
账本 C13。fast 模式信任 guest；不可信/LLM 场景用 L4 沙箱进程 + L1（可选 L3 checked）。

---

## 7. mirvm 声明的偏差（合法程序上与 RAM/native 的已知出入）

诚实列出。每条注明**为何**、对合法程序**是否仍是 RAM 允许的行为**。

| 偏差 | 说明 | 是否 RAM 允许 |
|---|---|---|
| **弱内存序不可见**（tier-0 / GIL） | 串行执行 = 顺序一致，观察不到弱内存重排（C6） | ✅ SC 是 §2 non-det 允许的执行之一；对合法程序正确 |
| **调度确定**（tier-0） | 并发时序 bug 可能不复现（反之亦然） | ✅ 是 non-det 集合中一个确定执行 |
| **getrandom 确定性**（可选，tier-0） | HashMap 种子/rand 可复现 | ✅ 是 non-det 集合中一个取值；`--real-random` 可关 |
| **主线程名 `<unnamed>`**（tier-0） | 非 `main`；panic 消息头不同 | ⚠️ 可观测差异，tier-0 残留，差分归一化；VM tier 修正 |
| **栈溢出深度** | ≈ native（模型 A，frame-abi §9） | ✅ 深度本 unspecified；近似即可 |
| **退出不跑 rt cleanup / print! 残留缓冲**（tier-0） | 行缓冲 flush 无感，print! 残留可能丢 | ⚠️ tier-0 残留，M2+ 修正 |

> 原则：偏差要么落在 §2 的 unspecified/non-det 集合内（对合法程序仍**正确**），要么是 tier-0 的**待修正残留**
> （明确标注、VM tier 消除）。**不接受**"合法程序上产出 well-defined 行为集合之外的结果"。

---

## 8. 与其他 RAM 实现的关系（差分测试的理论）

- **native codegen**：同一 RAM 的另一实现 → **差分对拍的理论依据**。但只能拍 **well-defined 可观测输出**
  （§2）；unspecified/non-det 靠不变式/归一化；UB 程序不拍。
- **Miri**：同一 RAM 的检查实现 → 可作 mirvm 的**第二 oracle**（尤其查 mirvm 自身 bug）；且 mirvm 借鉴其
  shim/intrinsic **代码**（非心智模型，P1）。
- **rustc const-eval**：编译期同一台机器 → const 求值与运行期求值应一致（§3.3）。
- **tier-0（InterpCx）**：建在 rustc const-eval 解释器上 → 天然是"RAM 的一个（慢）实现"，作 M4 自研 VM 的
  **差分 oracle**。

---

## 9. 版本与一致性

- **RAM 随 Rust 演进**（新特性、opsem 决议）。mirvm **锁 rustc 版本**（D9，当前 nightly-2026-07-02），
  故本 RAM-SPEC **对应一个 rustc 版本**；bump rustc 时复审本文。
- **字节码/分发件版本化**（C12）：.mirvm 像"classfile 有版本号"，runtime 匹配或转换。
- **一致性声明**：mirvm vX 对 rustc vY 定义的 RAM，在 §1 契约下一致（偏差见 §7）。

---

## 10. 权威来源（事实 RAM 的出处）

- **MIR 操作语义**：[rustc-dev-guide: MIR](https://rustc-dev-guide.rust-lang.org/mir/index.html)、rustc `rustc_const_eval::interpret`（Miri/mirvm 共用的解释核心）
- **内存模型 / 别名模型 / provenance**：[opsem team / unsafe-code-guidelines](https://github.com/rust-lang/unsafe-code-guidelines)、Tree Borrows、Strict Provenance
- **布局**：`rustc_abi` layout 算法（target-specific）
- **并发内存模型**：C++20（Rust 借用）
- **可执行检查参考**：[Miri](https://github.com/rust-lang/miri)
- **抽象机器 / as-if 概念**：C++ abstract machine（脊柱思想来源）

---

## 附：本文与实现文档的关系

| 层 | 文档 |
|---|---|
| **语义（WHAT）** | 本文 RAM-SPEC |
| 内存实现（HOW） | DESIGN.md §4 |
| 并发实现（HOW） | docs/concurrency-arch.md |
| 帧/字节码/JIT（HOW） | docs/frame-abi-bytecode.md、frame-stack-models.md |
| async（HOW） | docs/async-stackless.md |
| 边界/os（HOW） | DESIGN.md §7、P7 os:: |
