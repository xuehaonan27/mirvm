# mirvm — 设计文档

> 工作代号 `mirvm`（MIR Virtual Machine），随时可改名。
> 创建于 2026-07-03，记录项目奠基时的架构决策。改动决策请更新本文档。

## 1. 一句话定义

一个**拥有自己执行引擎的 Rust runtime**：用真 rustc 做前端（宏展开、类型检查、trait 求解、诊断），
拿到 MIR 之后由自研引擎解释执行（后续增加可开关的 Cranelift JIT 热点编译），
跳过 codegen 和链接，实现"改完即跑"。

对标定位："LuaJIT for Rust"。**不是** evcxr（编译器外壳）、**不是** Miri（UB 检测器）的替代品。
长期目标（2026-07-05 用户明确）：**JVM 级的成熟 runtime**——执行引擎必须"生而并发"，
真并行不是可推迟的实现细节，而是 M4 字节码 VM 的第一设计红线（见 §5.3 约束账本 C1）。

## 2. 目标与优先级（2026-07-03 决策）

按优先级排序，前三项均为 P0，第四项明确后置：

1. **LLM/Agent 脚本执行**：单文件 Rust 脚本快速启动；沙箱与资源限制（超时/内存/syscall 白名单）；
   结构化 JSON 诊断（rustc 自带，免费）。语法对齐即将 stable 的 cargo script frontmatter
   （RFC 3424，单文件内嵌 manifest）。
2. **项目开发内循环加速**：`mirvm run .` 跑完整 cargo 项目，改一行代码亚秒级重跑
   （省 codegen + 链接；依赖 MIR 一次构建全局缓存）。
3. **REPL / Notebook**：交互式后置（M6），但架构为其铺路——解释器拥有堆栈，
   状态持久化天然成立，跨 cell 借用不再是问题。
4. ~~嵌入式脚本引擎~~：暂不紧要。但从第一天起 engine 做成 library、CLI 只是薄壳，
   保证这条路不被堵死。

### 执行模式（用户明确要求，HotSpot 风格）

先把**解释器**做好做对，JIT 之后再加，且始终可配置：

```
--engine=interp   # 纯解释（类比 java -Xint），默认（JIT 落地前唯一模式）
--engine=mixed    # 解释 + 热点 JIT（类比 -Xmixed），JIT 落地后的默认
--engine=jit      # 尽量全 JIT（类比 -Xcomp），用于对比测试
```

## 3. 为什么是路线 A（rustc 前端 + 自研执行引擎）

评估过的三条路线（详细分析见 2026-07-03 会话记录）：

| 路线 | 结论 |
|---|---|
| **A. rustc_private 前端 + 自研执行层** | ✅ 选定。语言 100% 保真，自研含量集中在执行引擎（最有价值的部分） |
| B. rustc + cg_clif JIT 整合 | 备胎/兜底。出货快但核心不是自己的；Windows JIT 不可用、unwinding 仅 Linux |
| C. 完全不依赖 rustc（ra_ap_* 或裸写） | ❌ 否决。前端是十年工程；诊断质量长期弱于 rustc，恰好毁掉"LLM 写 Rust 更准"的红利 |

决定性理由：

- **四堵墙**决定了绕不开 rustc：① 前端（trait solver/类型推断/宏卫生）是十年工程；
  ② proc-macro 是编译期运行的原生代码，纯解释器不成立；③ 泛型跨 crate 单态化，
  依赖的 MIR 也必须能执行；④ std 地基是 unsafe + intrinsics + syscall，shim 工作量巨大。
- 复用 rustc 前端后，**layout / ABI 与原生代码精确一致**（直接用 rustc 的 layout query），
  为后续"非泛型依赖函数原生直调"和 FFI 留下正确性基础。
- "在 mirvm 里能跑 ≈ 能通过 rustc 编译"的保证不丢，这是 LLM 场景的核心价值。

## 4. 架构

```
┌─────────────────────────────────────────────────────┐
│ 产品层: mirvm CLI / daemon / (后置: REPL, 嵌入 API)   │
├─────────────────────────────────────────────────────┤
│ 运行时服务: syscall shims / FFI(libffi) / 线程调度    │
│            / panic-unwind / 沙箱与资源限制           │
├─────────────────────────────────────────────────────┤
│ 执行引擎 (自研核心，分阶段演进):                       │
│   tier 0: fast Machine on rustc InterpCx  (M1)      │
│   tier 1: 自研紧凑字节码 VM + 内联缓存      (M4)      │
│   tier 2: Cranelift 热点 JIT（可开关）      (M5)      │
├─────────────────────────────────────────────────────┤
│ 前端: rustc (rustc_private) → typeck 后的 MIR        │
│   依赖/std: -Zalways-encode-mir 构建，全局缓存        │
├─────────────────────────────────────────────────────┤
│ 输入: 单文件脚本 (cargo script frontmatter) / cargo 项目│
└─────────────────────────────────────────────────────┘
```

### 关键设计决策

| # | 决策 | 理由 |
|---|---|---|
| D1 | **不做 borrow check** | 不影响合法程序的运行语义；留给 CI 的真 rustc。大幅砍范围 |
| D2 | **惰性单态化** | 解释器带着泛型实参直接执行 MIR，不生成代码。这正是省掉 codegen 的地方 |
| D3 | **起步用 rustc 内置 `InterpCx` + 自写 fast Machine** | rustc_const_eval 的解释器核心是通用的，Miri 只是一个"开满检查的 Machine"。我们写"关掉全部 UB 检查、为吞吐调优"的 Machine，数周可跑真程序。它同时是后续自研 VM 的**正确性基线（差分测试 oracle）** |
| D4 | **unwind 自己实现** | 解释器拥有栈帧，panic/catch_unwind 跨平台无痛（cg_clif JIT 做不到的点） |
| D5 | **std/依赖以 `-Zalways-encode-mir` 构建** | rlib 携带全部函数 MIR（Miri sysroot 同款做法）；一次构建，内容寻址全局缓存 |
| D6 | **proc-macro 原生编译执行** | 别无选择；由 cargo 正常构建 proc-macro crate，rustc 前端照常加载 |
| D7 | **FFI 走 libffi 蹦床** | Miri native-lib 模式已趟路（整数/指针参数可用；C→Rust 回调是已知坑，后置） |
| D8 | **线程先做协作式调度** | Miri 同款；真 OS 线程并发解释后置 |
| D9 | **nightly 锁定 + 定期 bump** | rustc_private API 随 nightly 漂移；Miri/clippy/kani 证明可维护。策略：`rust-toolchain.toml` 锁具体日期版本，每月 bump 一次，rustc 交互层集中在少数模块隔离漂移面 |
| D10 | **engine 是 library，CLI 是薄壳** | 为嵌入场景（现在后置）留路；也方便测试 |
| D11 | **Miri 代码可借鉴移植** | MIT/Apache-2.0 双许可，shim 结构、intrinsic 清单尤其值得参考（保留 attribution） |

### 明确的非目标

- 不做 UB 检测（那是 Miri 的工作；我们假设程序合法，追求快）
- 不自研前端、不自研 trait solver
- 不追生产级峰值性能（目标：解释 tier 可用于脚本/测试反馈，JIT tier ≈ debug build）
- 初期不支持 Windows（unwinding/FFI 都以 Unix 优先；macOS 次之）

## 5. 里程碑

- **M0 — 工具链打通**（✅ 2026-07-03）：rustc_private 驱动能编译源文件、定位 entry fn、打印其 MIR。
- **M1 — 最小解释器**（✅ 2026-07-03）：fast Machine on InterpCx；std 程序端到端解释执行。
  验收达成：差分测试 5/5（fib 递归+迭代器、String/Vec/排序/格式化、HashMap（getrandom→SipHash→TLS）、
  panic 消息逐字符一致+退出码 101、catch_unwind+unwind 中 Drop）。实现笔记见 §5.1。
- **M2 — 吃下真实生态**（✅ 2026-07-05，线程与 libffi 明确移入 M2.5）：
  cargo 依赖图（wrapper/runner 拦截）、proc-macro、frontmatter 单文件脚本、
  .init_array 全局构造器（args/env 转正）、时间/文件/pthread-TLS/malloc shims。
  验收达成：serde_json（含 serde_derive）+ rand（dlsym→getrandom→ChaCha）+
  regex（SIMD 标量回退）脚本与 native cargo run 输出逐字节一致；
  cargo 项目模式含程序参数与退出码透传对拍通过。实现笔记见 §5.2。
- **M2.5 — 生态补全**（线程 ✅ 2026-07-05；FFI/corpus 进行中）：
  - ✅ 协作式**线程语义层**（src/interp/threads.rs，按 C8 分层：语义=GuestThread/
    ThreadManager/意图 API，策略=确定性 round-robin + 语句时间片 + 全员睡眠/死锁检测）。
    验收：spawn+join/嵌套、mpsc 多生产者、Mutex 争用(8×500)、Condvar、scoped threads、
    线程 panic→join Err、线程 TLS 析构、sleep/recv_timeout——差分 12/12 与 native 一致。
  - ✅ **真实地址内存改造**（2026-07-05）：分配基址 = 宿主缓冲真实地址
    （MirvmAllocBytes 真对齐分配）；全局分配"预分配缓冲、内容后到"破指针环；
    函数/vtable 泄漏占位地址；TypeId 基址 0。debug 构建下每次 write 校验
    "guest 地址直读 = 解释器视角"不变式。实现笔记见下。
  - libffi 原生 FFI（地基已备好：guest 指针即宿主指针，可直传 native 代码）
  - corpus 驱动补全（rayon/chrono/clap/itertools/anyhow/csv/...），异步生态评估（epoll）

**真实地址内存实现笔记（2026-07-05）**：
- `Box<[u8]>` 只保证 1 字节对齐——真实地址模式必须换自定义 AllocBytes
  （MirvmAllocBytes：按 guest 要求对齐的宿主分配，size 0 也占 1 字节保地址唯一）
- 全局分配的时序难题（指针环 A→B→A、"先要地址还是先要内容"）：Miri native-lib 的
  **prepared 机制**——resolve_addr 时预分配零缓冲取地址存入 prepared 表；
  adjust_global_allocation 材料化时取走同一块缓冲拷入 tcx 字节。地址先于内容存在。
- 关键洞察：**解释器读 vtable 走 tcx 查询（vtable_entries），从不读其内存字节**——
  所以函数/vtable 分配只需唯一占位地址（泄漏 1 字节），不需真实内容。
  并行 VM 时代 vtable 按 C8 冻结进字节码，此占位策略仍然成立。
- 死分配的宿主地址会被复用：反查表按 (addr,id) 双键防错删；基址映射永久保留
  （悬垂指针算偏移用，报错语义与 Miri 一致）。
- **M3 — 产品面（对齐 P0 场景）**：`mirvm run` 单文件脚本（frontmatter 依赖声明）与
  cargo 项目两种入口；常驻 daemon（前端增量状态留内存）；agent API：
  JSON 诊断、超时、内存上限、syscall 白名单沙箱。
  验收：脚本二次运行（缓存热）端到端 < 300ms；改一行重跑 < 1s（中型项目）。
- **M4 — 性能 tier 1**：MIR → 自研紧凑字节码 VM（替换 InterpCx 热路径）、内联缓存、
  非泛型依赖函数原生直调。验收：对 InterpCx 基线 ≥ 5× 提速；差分测试全绿。
  **前置硬关卡：并发架构 RFC + 原型 spike 通过（§5.3 C1）；VM 生而并行。**
- **M5 — JIT tier 2**：Cranelift 热点编译，`--engine=interp|mixed|jit` 开关。
  验收：计算密集 benchmark ≈ cg_clif debug build 的 2× 以内。
- **M6 — REPL/Notebook**（用户决策：后置）：解释器持久堆上的增量求值；跨 cell 借用成立。
- **M7+ — 嵌入 API**（用户决策：暂不紧要）。

### 5.1 M1 实现笔记（2026-07-03）

**结构**：`src/interp/{machine,eval,shims,intrinsics,helpers,addrs,mono_map}.rs`，
engine 在 lib、CLI 是薄壳（D10）。`src/sysroot.rs` 用 rustc-build-sysroot 自动构建
缓存 sysroot（`~/.cache/mirvm/`）。

**M1 已知偏差（直接调 main，不走 `start` lang item）**：
- `std::env::args()` 为空（.init_array 全局构造器未执行；Miri 已有 GlobalCtorState 先例，M2 补）
- 主线程名 `<unnamed>`（native 是 `main`），gettid 恒 1001；差分测试对 stderr 归一化后比较
- 环境变量为空表（environ/getenv shim 返回空/null；M2 可选择透传宿主环境）
- 进程退出不跑 rt cleanup（println! 行缓冲即时 flush，无感知；print! 残留缓冲会丢）
- getrandom 为确定性 xorshift（HashMap 种子可复现；M3 提供 --real-random 开关）

**踩过的坑（后来者须知）**：
- `__rust_no_alloc_shim_is_unstable_v2`：分配前哨兵符号，不在 allocator_shim_contents 里，
  需按 mangle_internal_symbol 单独识别为空操作（Miri 靠 cfg(miri) 的 std 绕过，我们不行）
- panic_impl（`rust_begin_unwind`）是 core 视角的 foreign fn：需要 lookup_exported_symbol
  机制按符号名在全部已链接 crate 找 MIR（同样服务于 __rdl_* 等）
- Linux std 的 getrandom/statx 等走 weak linkage：extern static 的值 = 函数指针；
  用 `ExtraFnVal = Symbol` 提供合成函数指针（Miri DynSym 同款）
- `#[track_caller]`（panic_bounds_check 等）：caller ABI 末尾"假装"有 Location 参数但不真传，
  callee 的 caller_location intrinsic 走栈取——call_function 必须传 with_caller_location
- `assert_inhabited` 系 intrinsic 无 fallback body 且 core 引擎不管：fast machine 直接跳过
- panic 穿出 main = 引擎 UB "unwinding past the topmost frame"（弹根帧前抛出，
  after_stack_pop 拦不到）→ eval_main 里翻译成退出码 101

**性能基线（release 构建的 mirvm）**：
- 脚本热启动（sysroot 已缓存）：`demo/strings.rs` 端到端 **0.24s**（rustc 前端为主）
- fib(27) 纯调用密集微基准（最不利场景）：解释 ≈1.6s vs native debug ≈4ms（数百倍）
  —— InterpCx tier 的已知代价，M4 自研字节码 VM 的主要目标；日常脚本远好于此
- mirvm 自身必须 release 构建（debug 构建慢 ~7×）

**D9 漂移记录（nightly-2026-07-02）**：`MachineStopType` 精简为 Any+Display+Debug+Send；
帧压栈走 `init_stack_frame` + `ReturnContinuation`（旧 StackPopCleanup 没了）；
`write_mir_pretty` 重构为 `MirWriter`；`catch_with_exit_code` 返回 `ExitCode`；
`Linkage` 移到 `rustc_hir::attrs`；throw_* 宏需要 `feature(yeet_expr)`。

### 5.2 M2 实现笔记（2026-07-05）

**cargo 集成**（`src/cargo_shim.rs`，机制移植自 cargo-miri）三阶段：
phase_cargo（注入 RUSTC_WRAPPER=自身 + target.runner + 独立 target/mirvm + 强制 --target host）
→ phase_wrapper（host crate 透传；target 依赖加 MIR sysroot + -Zalways-encode-mir；
最终 bin 不编译，写 JSON"假二进制"+ stub .d）→ phase_runner（读 JSON，用 cargo
原始参数驱动解释会话）。proc-macro 是 host crate 原生编译，前端加载即用，零额外工作。

**frontmatter 脚本**：`---` 围栏内嵌 manifest（RFC 3424 语法），物化到
~/.cache/mirvm/scripts/<路径hash>/，剥离处替换空行保持诊断行号。
无 frontmatter 的单文件仍走零 cargo 快路径。

**M2 新增 shims**：clock_gettime、open/read/close/lseek64/fstat64/stat64/fstatat64/unlink
（fd 与 C 结构直通宿主——target==host 布局精确一致是这批 shim 廉价的原因）、
malloc/calloc/realloc/free（System 分配器直调）、pthread_key_* 四件套（单线程平凡）、
dlsym（已知符号给合成函数指针）、__errno_location（机器内 errno 单元，宿主调用后同步）、
getenv（查 env 表）。weak 符号置 NULL 名单：statx、__cxa_thread_atexit_impl。

**踩过的坑（M2 增补）**：
- 裸 "rustc" 会被 rustup 按 cwd 解析——wrapper 必须无条件用 pinned toolchain 的
  rustc（proc-macro dylib 版本锁死）；rustc-build-sysroot 同理，必须显式传
  `.rustc_version()`，否则缓存哈希随 cwd 乒乓、sysroot 反复重建
- std_detect 的 CPU 特性检测走 CPUID 内联汇编（InterpCx 不支持 asm）：
  在 find_mir_or_eval_fn 按 DefPath 拦截 `std_detect::detect_features` 返回全零
  → memchr/aho-corasick 走标量路径（Miri 靠 cfg(miri) 绕开，干净 std 只能拦）
- statx 有双保险：weak 符号置 NULL 之外，std 还会 raw syscall(332)——回 ENOSYS
- runner 要剥 `--error-format=json --json=...`（cargo 已退场，没人消费 artifact 通知）、
  跳过 CARGO_MAKEFLAGS（指向已消亡的 jobserver）

**性能数据（release mirvm，全缓存热启动）**：
- serde_json 项目端到端 **0.26s**（cargo no-op + 前端 + 解释）
- ecosystem 脚本 **42s**——大头是解释执行 `Regex::new`（DFA 构建 + Unicode 表，
  解释器最不利负载）。这是 M4 字节码 VM / M5 JIT 的头号靶子与基准用例
- mirvm 自身必须 release 构建（debug 慢 ~7×，regex 案例 5min+）

**M1 偏差状态更新**：args/env 已转正（.init_array 构造器真实执行、宿主 env 透传、
getenv 查表）。仍存偏差：主线程名 `<unnamed>`、退出不跑 rt cleanup 与主线程 TLS 析构
（spawned 线程的 TLS 析构 M2.5 起已运行）、getrandom 确定性（--real-random 留 M3）。

**M2.5 线程实现笔记（2026-07-05）**：
- 阻塞 shim 的通用模式：**先写结果再阻塞**——pthread_join 直接写 0；futex_wait 推测性
  写 0（唤醒即成立），超时路径由调度器改写为 -1+ETIMEDOUT（dest 先 force_allocation
  固化成绝对位置，跨线程可写）。IP 已随 NeedsReturn 跳到 ret block，唤醒后直接续跑。
- pthread key 析构：线程根帧返回后、Terminated 前，逐个压 dtor(value) 帧继续跑
  （glibc 轮次语义简化版；不变式：pthread_tls 只存非空值）。主线程不跑（native 同）。
- `core::hint::spin_loop` → `_mm_pause` → `llvm.x86.sse2.pause` foreign 调用：
  shim 成"让出时间片"，语义妥帖（自旋者让路）。
- futex 值检查的原子性由协作调度保证（step 内不可分割），无需真原子读。
- std 的线程结果传递（Packet/Arc + 内部 catch_unwind）全部是被解释的 guest 代码，
  panic→join Err 零额外工作，白捡。

### 5.3 VM 设计约束账本（M4 开工前必读；每踩一个坑追加一条）

> 目的：把踩坑经验系统性转成字节码 VM 的设计输入，防止"设计完再返工"。

- **C1 生而并发（2026-07-05 用户决策，第一红线）**：M4 字节码 VM 必须以 **1:1 真并行**
  （guest 线程 = 宿主线程）为默认执行模式设计；协作式调度保留为**确定性执行模式**
  （`--threads=coop`，agent/复现场景），两种模式共享线程语义层、只换执行策略。
  依据：目标是 JVM 级 runtime；现代 Rust 程序天生并发（tokio 多线程 runtime 默认、
  rayon、**cargo test 默认并行跑测试**——dev-loop 场景绕不开）；CPython/GIL 类引擎是
  单核时代的妥协产物，是反面教材而非参照系。
- **C2 并发内存模型**：guest 原子操作直落宿主原子指令（与 native codegen 相同 → 并行
  tier 的内存行为 = native 行为）；guest 分配背靠**真实宿主地址**（热路径 load/store
  零查表；与 libffi FFI 是同一份改造，M2.5 做时即按并发就绪标准：分配注册表用
  分片锁/无锁结构）；引擎自身状态**三分法**：每线程私有 / 发布后不可变（字节码缓存、
  layout 表）/ 显式同步（分配注册表）。
- **C3 Rust 的三个结构性红利**（并行 VM 比 JVM 当年容易的原因，设计时要吃满）：
  无 GC（所有权/Drop，JVM 最难的并发 GC 问题不存在）；内存模型现成（C++20 模型，
  无需自研 JMM）；safe 代码类型系统保证无数据竞争 → 只需保护引擎自身状态。
- **C4 guest UB 立场（并行 tier）**：unsafe 数据竞争 = 宿主数据竞争，与 native 行为
  一致（fast 语义"假设程序合法"立场不变）；可选 TSan 调试模式后置。
- **C5 InterpCx tier 的定位与硬约束**：rustc interpret 基础设施根本不 Sync
  （RefCell 遍地），该 tier **永远单宿主线程**——作为确定性 oracle 与兜底 tier 存在。
  分层混合（VM 热路径 + InterpCx 兜底）要求两引擎共享内存模型与调用边界。
- **C6 InterpCx tier 并发偏差（弱内存序，防遗忘）**：协作式模式下所有内存操作按单一
  全序执行（顺序一致），**永远观察不到弱内存重排**；调度确定性 → 并发时序 bug 可能
  不复现（反之亦然）。理论上 SC 执行是内存模型允许的合法执行之一（弱序允许而不强制
  重排），故对合法程序仍是正确语义；作为偏差记录并在文档/诊断中向用户声明。
  并行程序的差分测试需依赖确定性模式或输出不变式（不能逐字节比时序敏感输出）。
- **C7 分层执行的性能基准**：regex 编译（DFA 构建 + Unicode 表）42s 案例是 1 号
  基准；VM 设计评审时必须给出该案例的预估收益。
- **C8 1:1 并行执行架构草图（2026-07-05，并发架构 RFC 的种子）**：
  - 分层：**线程语义层永久**（生命周期/join/TLS/panic 传播/futex 语义/线程表），
    协作式调度器亦永久（= 确定性模式）；两者之间以 `ThreadStrategy` 边界隔离——
    语义层发意图（BlockOn/Wake/Spawn/Exit），策略层实现（coop 队列 vs 宿主原语）。
    唯一临时的是"coop 是唯一策略"这个状态。M2.5 的线程差分 corpus 是永久资产
    （VM 的 1:1 实现将在同一 corpus 上验收）。
  - 执行模型：每 guest 线程 = 一宿主线程，各跑 VM 循环；共享 VmShared 按 C2 三分法；
    **VM 自管栈帧**（每线程堆上 frame arena，两种策略共用，guest 深递归不炸宿主栈）。
  - 两个真实地址红利：guest futex 地址即宿主地址 → **futex 直通 SYS_futex**
    （Mutex/Condvar/Once/park 零调度代码）；阻塞 IO 在 1:1 下自然化（协作式的
    mini-reactor 问题只属于 coop 模式）。
  - 引擎状态线程安全三招：**降低时元数据冻结**（layout/偏移/vtable 烘焙进字节码，
    运行期永不触 tcx）；**降低服务线程**（惰性单态化经 channel 发给专职编译线程，
    HotSpot compiler-thread 同构，为 M5 后台 JIT 铺路）；分配注册表分片/无锁。
  - 验证路径：语义层+coop（M2.5）→ 真实地址内存（并发就绪）→ M4 前 spike
    （原型字节码 + N 宿主线程压测原子/注册表/futex，**引擎过 TSan** 为通过标准）
    → RFC 定稿 → M4 施工，线程 corpus 双策略全绿。
  - 开放问题：并行 fast 模式下 guest UAF = 宿主 UAF（C4 延伸，隔离区后置）；
    main 返回时 detached 线程的退出语义（native = 直接退，写进语义层）；
    编译线程不得执行 guest 代码（死锁面）。

## 6. 风险与对策

| 风险 | 对策 |
|---|---|
| nightly API 漂移导致维护负担 | D9 锁定+定期 bump；rustc 交互隔离在 `src/rustc_glue/`；关注 Miri 同步提交作为迁移指南 |
| std shims 工作量失控（最大风险） | 按需实现（跑目标程序缺什么补什么）；大量借鉴 Miri（D11）；M1 只做 write/exit/alloc 最小集 |
| InterpCx 性能天花板（AllocId 间接寻址等） | 它只是 tier 0 基线；M4 自研 VM 才是性能答案；届时 InterpCx 转为差分测试 oracle |
| FFI 回调（C→Rust）难做对 | 已知坑（Miri 同样未解决）；M2 只承诺单向调用，回调场景明确报错 |
| 异步/tokio 生态（epoll/io_uring shims） | 大工程，放 M3 后评估；先支持阻塞式 std |
| cargo script 语法仍在 FCP | 跟踪 rust-lang/cargo#16569，语法完全对齐官方，不自创 |

## 7. 先行者参考

- [evcxr](https://github.com/evcxr/evcxr) — 编译器外壳式 REPL（我们的反面教材：延迟、状态搬运、不可嵌入）
- [Miri](https://github.com/rust-lang/miri) — InterpCx/Machine 用法、shims、intrinsics 的最佳参考实现
- [rustc_codegen_cranelift](https://github.com/rust-lang/rustc_codegen_cranelift) — M5 JIT 的直接参考；[2025-06 unwinding 进展](https://bjorn3.github.io/2025/06/30/progress-report-june-2025.html)
- [subsecond / ThinLink](https://docs.rs/subsecond) — 热补丁与增量链接思路（路线 B 元素，daemon 阶段可借鉴）
- [cargo script RFC 3424](https://rust-lang.github.io/rfcs/3424-cargo-script.html) · [稳定化 PR](https://github.com/rust-lang/cargo/pull/16569)
- [Rustc Dev Guide: rustc_private / driver](https://rustc-dev-guide.rust-lang.org/rustc-driver/intro.html)
- 解释开销数据点：Miri 系解释 ≈ 25×（[Asterinas 论文](https://arxiv.org/pdf/2506.03876)，关检查后可更低）
