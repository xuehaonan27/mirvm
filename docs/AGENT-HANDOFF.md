# mirvm 项目交接文档（AGENT-HANDOFF）

> 目的：让新接手的 agent 能无缝继续本项目。信息力求自足——读完本文 + 引用的设计文档，
> 即可接手当前工作（M4.1 施工）。作者：前任 agent，2026-07-07。
> **读法**：先读 §0-§2 建立坐标，再按需深入。所有"为什么"都有出处，不要推翻既有决策而不读其
> 论证；所有"怎么做"都要先跑 `--vm-stats` 拿真实数据，**绝不凭直觉/训练知识写 rustc API**。

---

## 0. 立即须知（环境 / 铁律）

- **工作目录**：`/home/xuehaonan/mirvm`（git 仓库，主分支 main）。
- **工具链锁定**：`nightly-2026-07-02`（rustc 1.98.0-nightly）。路径
  `~/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/`。**月度 bump，别乱升**。
- **构建**：`cargo build --release`（**必须 release**；debug 慢 ~7×）。带 cranelift feature
  首次编译 ~1 分钟。
- **sysroot**：首次运行自动构建带全量 MIR 的 sysroot，缓存在 `~/.cache/mirvm/`（勿删）。
- **Miri 参考**：可克隆到 `/tmp/miri-ref`（易失，可重克隆）。P1 = Miri 是**代码参考**，
  **不是心智模型来源**。
- **rustc 源码**：`~/.rustup/toolchains/.../lib/rustlib/rustc-src/rust/compiler/`（写 lower
  前必 grep 核对 API——本 nightly MIR 有漂移，见 §9）。
- **三条铁律**：① 先调研后动手（跑 `--vm-stats`、dump MIR、grep 源码）；② 每期先出设计文档
  给用户审、完工写经验文档；③ 防静默错值——宁 Trap 带诊断，勿静默返回 0（差分才可信）。

---

## 1. 项目是什么（一句话 + 心智模型脊柱）

**mirvm = Rust 抽象机器（RAM）的事实标准实现，按 JVM 级系统软件构建。** 用户于 2026-07-03
启动。动机：Rust 编译慢拖累内循环 + LLM 写 Rust 准确度高，想要 Rust 脚本化/运行时执行；对
evcxr 四点不满（延迟、状态/借用限制、跑不了完整项目、编译器外壳不可嵌入）。

**它不是**：给 Rust 套解释器、Miri 扩展、玩具。**它是**：像 JVM/HotSpot 那样的生产级 VM。

**心智模型脊柱（2026-07-05 确立，review 一切决策的准绳，详见 `DESIGN.md`）：**

- **一台 RAM，三个实现**：native codegen（跑）/ Miri（检查）/ mirvm（跑）。正确性 = 忠实实现
  RAM；native 是 RAM 的另一实现，故**对拍 native 是有效度量**。as-if 规则 = 一切内部实现
  （分配器/tier/调度）保持可观测等价即自由。
- **VM 作者视角（用户反复强调）**：遇抉择问"JVM 怎么做"，**不问"Miri 怎么做"**。用户批评过
  早期"围着自造的线程语义层设计"是"给 Miri 加扩展而非写 VM"。
- **反 emulation（用户核心偏好）**："能用真 OS 就别 emulate、别 wrap"。有真 OS 原语就直接用
  （epoll/futex/pthread/文件 IO 全直通真内核）；只"shim"真正必须 VM 内部的东西。用户厌恶的
  emulation 特指**过度重实现**（如 tier-0 自建的协作式线程调度器）。
- **真实地址内存**：一个连续地址空间（宿主的），guest 指针 = 真宿主地址。这让 FFI 零编组、
  `into_pthread_t` 成立、mmap 区可访问。代价：与 Wasm 式廉价沙箱封闭不兼容（无免费午餐）。
- **RAM 定义度四级**（`docs/ram-spec.md`）：well-defined（必须匹配 native）/ unspecified
  （挑一个，不必同 native）/ non-det（任一合法执行）/ UB（无约束、不检测）。**差分对拍只拍
  well-defined 可观测输出**。mirvm **不检测 UB**（质量取向，非偏差）。
- **不做**：borrowck、UB 检测、自研前端（复用 rustc 四堵墙）。

---

## 2. 当前状态速览（你在哪 / 下一步）

**已完成**：
- **M0-M2.5（tier-0）**：`src/interp/`，基于 rustc `InterpCx`。std 端到端、cargo 依赖、
  线程（协作式）、真实地址内存、libffi FFI。**tier-0 是弃子**——bootstrap + 差分 oracle，
  不是设计中心，M4 整体推翻。差分 16/16 + cargo 3/3 通过。
- **corpus 五批收口**（`docs/corpus.md`）：22 crate + 3 探针跑真实生态，逼出五处设计票据
  （见 §5.2）。
- **5 个 M4 前 spike 全过**（`docs/spike{1-5}-*.md`）：模型 A 骨架 / i2c-c2i 混合栈=vmctx /
  混合栈 unwind=候选 A / 并发 TSan 零警告=引擎 Sync / 真 Cranelift（P vs R 数据 + eh_frame
  注册后 panic 穿 JIT 帧）。**模型 A 地基全部验证，M4 开闸。**
- **M4.0 地基完成**（`docs/m4-log.md`）：新引擎端到端跑通真实 Rust，gate 9/9。
- **M4.1 调研完成**（`docs/m4.1-design.md` + `docs/m4-debt-map.md`）：设计文档 + 全期债务
  普查，含关键修正（见 §7）。

**下一步 = 开工 M4.1（值与内存）**。前置全齐：设计文档、债务地图、施工顺序、SIMD 决策
（已定 = "B 目标 A 排序"，见 §7.4）。**从施工顺序第 0 步（worklist 闭包扩集 + foreign 三路
骨架）开始**。

> **更新（2026-07-08）**：**M4.1 已完成**（gate1 digest 9/9 == native + M4.1 份内债务清零；
> 经验与遗留见 `docs/m4-log.md` M4.1 条目）。项目已迁移至 `/home/ubuntu/mirvm`（原
> /home/xuehaonan）。
>
> **更新（2026-07-09）**：**M4.2 unwind 已完成**（gate2 九用例 == native：panic 发起/
> 跨帧 Drop/catch/重抛/Assert→真 panic；resume 770 处债务清零；panic hook 打印与退出码
> 101 与 native 一致；经验见 m4-log M4.2 条目——两个自递归陷阱与 track_caller ABI 三处
> 一致性尤其值得读）。
>
> **更新（2026-07-09 晚）**：**M4.3 已完成**（`tests/diff_vm.sh` **全量差分 11/11 非线程
> 用例 == native**——新引擎完整 main 启动链跑 fib/strings/args_env/time_fs/ptr_int/
> hashmap/catch/panic_exit/async×2/ffi_libc；os:: 直通 = CallForeign+dlsym+libffi 通用道
> +denylist+stub 表；128 位算术补全；经验见 m4-log M4.3 条目——weak 符号链接语义与
> Coroutine Aggregate 顶层落位两个修复尤其值得读）。**下一步 = M4.4 真线程（收官之战，
> 过审期——开工前设计文档给用户审！）**：pthread 1:1 直通、thunk 工厂+TLS 边界 attach、
> guest TLS per-thread（单线程物化的 M4.4 义务标注在 lower ThreadLocalRef 处）、原子序
> 映射、signal thunk；gate = threads_* 5 用例 + tier-0 挂死双场景 + TSan + rayon。

**挂起检查点（勿丢，§10）**：vmctx P/R 真负载终裁挂 M5；landing pad/LSDA 挂 M5；fork/atfork
挂 M4 后；预降低 std 发行工件挂 mode B（M4.5 后）。

---

## 3. 目录与文件地图

```
/home/xuehaonan/mirvm/
├── src/
│   ├── lib.rs              # crate 根（rustc_private feature 声明 + box_patterns）
│   ├── main.rs             # bin 薄壳 → cli::main
│   ├── cli.rs              # 驱动：三形态(run/runner/rustc-wrapper) + --engine vm/--vm-call/--vm-stats
│   ├── cargo_shim.rs       # cargo RUSTC_WRAPPER + runner
│   ├── sysroot.rs          # 自动构建带 MIR 的 sysroot（缓存 ~/.cache/mirvm）
│   ├── interp/             # ★ tier-0（弃子，rustc InterpCx 上的 fast machine + 协作调度）
│   │   ├── machine.rs eval.rs shims.rs native.rs threads.rs
│   │   ├── intrinsics.rs helpers.rs addrs.rs alloc_bytes.rs mono_map.rs mod.rs
│   ├── lower/              # ★ M4 加载相（rustc_private 域，tcx 关在这里，永不出境）
│   │   ├── mod.rs          #   lower_program：收集→分配 FuncId→逐 instance 降低→exports 表(+@entry)
│   │   ├── collect.rs      #   collect_and_partition_mono_items → instance 集（种子，见 §7.1）
│   │   ├── frame.rs        #   帧布局冻结（逐 local layout → 对齐 bump）
│   │   └── func.rs         #   逐 instance 降低（MIR→IR）+ Trap-stub 全覆盖
│   └── vm/                 # ★ 执行相 + spike（纯 Rust，零 rustc_private——机械纯度门禁）
│       ├── mod.rs
│       ├── engine/         # ★★ M4 真引擎（M4.0 起）
│       │   ├── ir.rs       #   类型化字节码（Width/Slot/Operand/Rvalue/Stmt/Terminator/FuncBody/Module）
│       │   ├── frame.rs    #   ByteRegion（字节 arena；M4.1 要改 mmap 定容，见 §7.3-F6）
│       │   ├── ctx.rs      #   Shared（发布后只读）+ Ctx（每线程 vmctx）
│       │   ├── interp.rs   #   类型化 interp_frame（标量+溢出对+Assert=诊断退出）+ run_export
│       │   └── stats.rs    #   --vm-stats 调研仪器（Trap 债务直方图 + per-export 可达 BFS）
│       ├── bytecode.rs frame.rs memory.rs interp.rs   # spike 用的冻结基础设施（勿动）
│       └── spike1.rs .. spike5.rs                     # 5 个已过 spike（spike5 门控 cranelift feature）
├── tsan/                   # ★ 独立 crate：#[path] 复用 src/vm，-Zsanitizer=thread 判定引擎 Sync
├── docs/                   # ★ 全部设计文档（见 §3.1）
├── tests/                  # diff.sh(16) diff_cargo.sh(3) corpus.sh spike4_tsan.sh m4_gate0.sh
├── demo/                   # 差分用例；demo/m4/pure.rs(M4.0 gate) demo/m4/digest.rs(M4.1 gate)
└── corpus/                 # corpus 程序 c_*.rs
```

### 3.1 设计文档索引（务必读，本文只是导航）

| 文档 | 内容 | 关键性 |
|---|---|---|
| `DESIGN.md` | 主设计（RAM 脊柱 / 架构 / 内存 / 线程 / tier / 原则 P0-P7 / 账本 C0-C13） | 脊柱 |
| `docs/ram-spec.md` | RAM 语义契约（定义度四级、差分只拍 well-defined） | 高 |
| `docs/frame-stack-models.md` | 模型 A vs B，结论 A（JIT 集成决定性） | 高 |
| `docs/frame-abi-bytecode.md` | M4 帧/ABI/字节码/JIT/分发 + §10 开放问题 + §11 spike 清单 | **最高** |
| `docs/concurrency-arch.md` | 并发 RFC（状态三分、tcx 唯一敌人、TLAB、Spike4 验收） | 高 |
| `docs/vmctx-passing.md` | vmctx 传递（边界 TLS 逼定 / 内部 P vs R 挂 M5，含图） | 高 |
| `docs/async-stackless.md` | async=无栈状态机实证 | 中 |
| `docs/corpus.md` | corpus 五批 + 五票据 | 高 |
| `docs/spike{1..5}-*.md` | 五 spike 经验教训 | 中（背景） |
| `docs/m4-plan.md` | M4 六期计划 + 六决策 D1-D6 + 挂起项 | **最高** |
| `docs/m4.1-design.md` | M4.1 设计（调研+施工顺序+SIMD 决策） | **当前工作** |
| `docs/m4-debt-map.md` | 全期 Trap 债务地图（三架构级发现） | **当前工作** |
| `docs/m4-log.md` | M4 施工日志（每期 gate + 教训） | 追加式 |

内存文件：`~/.claude/projects/-home-xuehaonan-mirvm/memory/mirvm-rust-runtime-project.md`
（超长，是本文的浓缩源）+ `MEMORY.md`（索引）。

---

## 4. 心智模型详解（决策的"为什么"）

这些是用户反复纠正 agent 后固化的，**不要重新讨论，除非用户提**：

- **内存三分（DESIGN §4）**：VM 元数据（私有，须隔离，guest UB 不能污染实现自身）/ Rust Heap
  （RAM 存储，托管非搬迁——借 JVM arena/TLAB 骨架，按 Rust 钉地址/无 GC 改造，真实地址供 FFI）
  / Native Heap（RAM 之外，libc 自管，不追踪）。地址空间只有一个，三分是管理/污染维度。
  `__rust_alloc`→托管 Rust Heap（MIR 层拦）；`libc::malloc`→真 libc 直通落 Native Heap。
- **AllocId 是检查器 overlay**（用户 2026-07-05 戳破）：内存访问 = 裸宿主 read/write，有没有
  AllocId 都在真地址上。fast machine 假设合法 + 真实地址下**不需要 AllocId**，VM tier 甩掉。
  类型级元数据（layout/offset/vtable）另说，按类型冻结进字节码永远需要。
- **线程（DESIGN §5）**：**不 emulate、不 wrap pthread，用真 OS 线程**。std 已 wrap pthread
  （thread_start 是 std 的 extern C fn，Thread.id 是真 pthread_t）。VM 唯一要做的是**补 FFI
  反方向**：解释态函数指针逃逸给 native 时 materialize 成真 thunk（libffi closure）。于是
  pthread_create 纯直通、join/futex/into_pthread_t 走真 libc 零拦截。**tier-0 的协作式
  ThreadManager 是要扔的 emulation**（into_pthread_t 证明它跑错非仅慢）。tier-0 因 InterpCx
  不 Sync 用 GIL-over-真线程过渡。
- **帧模型 A（账本 C11）**：guest 帧放 **native 栈**（HotSpot/V8 式），非独立 VM 帧栈
  （CPython/Lua 式）。因 JIT 硬约束定的：Cranelift 是方法级 JIT，A 下解释帧+编译帧同在 native
  栈→i2c/c2i 廉价、unwind 走一条栈。B 的协程优势因真 OS 线程 + Rust async 无栈对我们无关。
  关键：JIT-VM 里解释器是**冷层**，A"解释器难写"代价权重低。
- **并发（concurrency-arch）**：状态三分——每线程私有 / 发布后只读 / 显式同步。**唯一的敌人
  是 tcx 不 Sync**，这是 tier-0 只能单线程的根本原因；但 tcx 是**加载相**的事，执行相永远
  tcx-free（Rust 单态化静态 → eager 降完全部字节码）→ 引擎 Sync 无 GIL。两运行模式：mode A
  （run from source，有 tcx 关在加载相）/ mode B（run .mirvm 分发，根本无 tcx）。TLAB 直接上
  （mimalloc 结构，非纯 bump——因 Rust 有 individual free）。
- **os:: 模块（P7）**：一切触真 OS/系统库的东西集中一个 `os::` 模块（JVM os:: 同构），三种
  处置：**直通**（epoll/read/socket/futex，绝大多数）/ **VM 内建**（__rust_alloc/thunk/
  intrinsics）/ **合成**（libm 宿主直算）。denylist（pthread_/fork/exec/setjmp/signal 等）绝
  不直通 native。
- **VM 鲁棒性（C13）**：真实地址下 guest UB/FFI/asm 能打穿 VM。分层防御砍到 L1（结构隔离）+
  L3（checked 模式，opt-in）。**Rust 类型系统让 checked 比 Wasm 便宜**——只查 raw 指针解引用。
  slaved/alloca（轴 F）与 fast/checked（轴 S）**解耦**，只在 `GuestMemory::contains(addr)`
  相遇。

---

## 5. 已验证的知识（spike + corpus 的产出，M4 依赖它们）

### 5.1 五个 spike（`docs/spike{1..5}-*.md`，代码 `src/vm/spike{1..5}.rs`）

1. **模型 A 骨架**：interp_frame tree-walking + Call 宿主递归 = guest 帧上 native 栈。真地址
   裸内存（§2.5 白赚）。**教训**：解释器循环借用必须解耦（把 `&'p Program` 复制到局部再取
   body，否则借 self 挡死 &mut region）；u64-word 值不够，真值须带类型/尺寸（→ M4.0 落地）。
2. **i2c/c2i 混合栈**：解释帧↔编译帧廉价互操作（模型 A 核心赌注成立）。**头号发现：再入迫使
   ctx 为裸指针 vmctx，非 Rust &mut**（c2i 再入需 &mut region，&mut 无法表达再入式共享可变；
   安全靠操作数区纪律化栈 + 单线程顺序）。字段级瞬态借用（`&mut (*ctx).region` 非 `&mut *ctx`，
   否则与 &prog 冲突；edition 2024 dangerous_implicit_autorefs 禁隐式 autoref）。
3. **混合栈 unwind（头号硬骨头）**：候选 A 坐实（宿主 Rust panic = 同一平台 unwinder +
   personality）。混合栈传播 + Drop 顺序（内层先，单条 native 栈 unwinder 天然）+ catch +
   跨 FFI abort 全对。**教训**：raise 用 resume_unwind（无 hook 噪声）；catch downcast 区分
   GuestPanic/宿主 panic（宿主 panic 绝不吞）；**JIT 调用约定必须 unwind-capable**（plain
   extern "C" = abort shim，恰给跨 FFI abort）。
4. **并发过 TSan**：8 真宿主线程并行混合执行 + 跨 tier 原子 + 阻塞 IO 活性（corpus §2.1 收束）
   + 并发 unwind，**TSan 零警告=引擎 Sync**。**新引擎义务：解释器执行 guest 原子必须发真宿主
   原子指令**（tier-0 普通读写模拟在真线程下=引擎自身数据竞争）。TSan 通道依赖引擎核心零
   rustc_private（tsan crate #[path] 复用 src/vm）。
5. **真 Cranelift 接入**：i2c/c2i/cc→cc 直调全坐实。**vmctx P vs R 数据**：fib(30) 直调
   R(pinned r15)=5.47ms vs P(显式参)=5.90ms，**R 快 ~8%**（但微基准偏袒 R，寄存器压力未测→
   终裁挂 M5）。**unwind 穿真 JIT 帧**：裸跑 SIGABRT（cranelift-jit 不注册系统 eh_frame，其
   wasmtime-unwinder 与宿主不互操作→**不采**）；eh_frame 自注册后传播成功（create_unwind_info
   → gimli → __register_frame 逐 FDE）。**教训**：写 Cranelift 前必 grep 0.133.1 真源码（API
   有漂移）。

### 5.2 corpus 五票据（`docs/corpus.md`）

跑 22 真实 crate + 3 探针，逼出五处设计需求（每处有 M4 处置）：
- **§2.1 协作调度 vs 真阻塞 syscall**：线程化阻塞服务器（accept→read→write）是主流模式，
  tier-0 协作调度上挂死 → **M4 真线程**（spike4 已收束）。
- **§2.2 inline asm（三张面孔：cpuid 特性检测 / 裸 syscall / 算术原语 div）** → **直接 JIT
  asm 块**（用户定，非模板拦截；虚拟 CPU = 真宿主 CPU）。M4 后/M5。
- **§2.3 guest→内核回调缺 thunk（signal）** → M4 建 thunk。
- **§2.5 检查器 overlay 残留（walkdir/process/mmap 三实例，tier-0 第一号阻塞）** → **甩掉
  AllocId 检查器**（M4 新引擎天生无此问题，真地址裸访问）。
- **§2.6 fork/clone** → os::+atfork，containment 交 seccomp（denylist 不是围栏）。

---

## 6. M4 架构与六决策（`docs/m4-plan.md`）

**总目标**：tier-0 能跑的一切在新引擎可观测一致 → tier-0 退役为 oracle。
**退出判据**：全量差分绿 + 真线程 TSan 干净 + 两个 tier-0 挂死场景通过 + 性能不慢于 tier-0。

**架构硬纪律**：`src/lower/`（加载相，rustc_private 域，tcx 永不出境）/ `src/vm/`（执行相，
纯 Rust 零 rustc_private）。**机械门禁 = tsan harness 构建**（vm/ 漏进 rustc 类型即编译失败）。

**六决策**：
- **D1 eager 降低复用 rustc 单态化收集器**——**已修正**（debt-map §2-A）：collector 是
  codegen/链接视角，跨 crate 非泛型函数不收 → **collector 集合作种子 + 调用点 worklist 闭包
  扩集**（与 D5 fallback 补收同机制，扩集仍在加载相，执行相 tcx-free 不变）。
- **D2 帧局部 = 字节区 + 冻结帧布局**（Place 溶解为帧偏移；slaved v0，alloca 后置）。
- **D3 分配器 v1 = mimalloc crate 后端 + 薄真地址包装**；`__rust_alloc` lower 时改写为引擎
  分配调用，`libc::malloc` 直通不动。
- **D4 fn-ptr = 每 instance 真地址条目表**；逃逸物化 thunk。
- **D5 intrinsics**：有 MIR fallback body 的当普通函数降低（worklist 补收）；
  `must_be_overridden` 的做引擎内建（copy/原子/volatile/数学/discriminant/simd）。
- **D6 M4 纯解释器无 vmctx P/R 问题**（ctx 是 Rust 参数）；**P/R 真负载终裁挂 M5 JIT**（勿丢）。
  M4 只需边界 TLS + attach（M4.4 thunk 工厂）。

**六期**（每期差分 gate）：M4.0 地基（✅ fib）→ **M4.1 值与内存**（当前）→ M4.2 unwind →
M4.3 os::+FFI → M4.4 真线程（收官之战：两 tier-0 挂死场景通过、TSan 指新引擎、rayon 28s→秒
级，**过审期**）→ M4.5 收口切默认。**流程**：M4.0/M4.4 开工前过审，其余简报即行；每期 gate
报告 + 经验记 m4-log.md。

**panic 前端处置（用户 2026-07-07 修订，debt-map §2-B）**：panic_fmt/fmt 家族**照常经
worklist 解释**（native 也跑这套=忠实性）；引擎接管点下移到 std 声明的 extern 边界 = **foreign
三路处置**：①引擎原语（__rust_start_panic=unwind 原语 M4.2 / __rust_alloc M4.1）②guest 导出
符号=链接仿真（panic_impl 等 weak lang item → std 实现，查 lang_items/导出符号表；tier-0
for_each_linked_def 已趟过）③os:: 直通（M4.3）。**特判的不是"panic 是什么"，是"链接器本来
会做什么"**——加载相份内事，不 ad-hoc。

---

## 7. M4.1（当前工作）—— 值与内存

**读全文**：`docs/m4.1-design.md`（设计）+ `docs/m4-debt-map.md`（债务）。此处是提炼。

### 7.1 D1 修正落地（施工顺序第 0 步，最先做）

collector（`src/lower/collect.rs`）给出的是 codegen 视角集合——`std::io::_print`/`panic_fmt`/
`panic_nounwind` 全"未收集"（debt-map §2-A，255+140 处）。**落 worklist 闭包扩集**：lower 遇
不在表里的 callee（非 foreign、非 intrinsic 内建、非 panic 原语）→ 加入待降低队列、分配
FuncId、继续。当前 `lower_program`（`src/lower/mod.rs`）是"先收集全集→逐个降"的静态两遍，改
成"种子入队 + 降低时发现新 callee 入队"的 worklist。同一机制承接 D5 fallback body 补收。

**同时落 foreign 三路解析骨架**（§6 panic 处置）：func.rs 的 Call 解析处已识别 foreign（见
现有 `is_foreign_item` 分支），扩成：①引擎原语表（本期 = __rust_alloc 系）②链接仿真（weak
lang item → std 实现）③未知暂 Trap。

### 7.2 IR 升级：静态槽 → place 求值（本期核心）

债务 #1 是 `Deref` 2752 处（全局）+ Offset 269 + Index。M4.0 的"(off,width) 静态槽"撑不住
（Deref/Index 是运行期地址）。升级为 **place 求值模型**：
```
PlaceExpr { base: Local(帧偏移) | Static(冻结真地址),
            steps: [Deref | Offset(常量字节) | IndexScaled(idx槽,步长)] } → addr
```
帧基址是真地址（见 F6）⇒ 帧内/堆/statics 统一裸地址读写。访问尺寸不再限 ≤8：`Copy{dst,src,
size}`（memcpy）承载聚合/pair 整体搬运，标量路径保留 (width) 快路径。

### 7.3 关键发现（改变施工，均来自实测）

- **F3 ScalarPair 是第二支柱**：非标量债务大头其实是**双标量**（Layout{size,align}、
  Option\<usize\>、&str/&[u8] 胖指针、(T,bool)）。值模型支持 pair（参数/返回/place/常量）即解
  锁分配链与迭代器。
- **F5 statics/重定位设施全现成**（API 已核）：`tcx.eval_static_initializer(DefId)` →
  ConstAllocation；`alloc.provenance().ptrs()` → `SortedMap<Size, CtfeProvenance>`（重定位
  表）；`tcx.global_alloc(id)` → {Memory/Static/Function/VTable}；**`tcx.vtable_allocation`
  直接给现成 vtable 分配**。**两遍法**：遍1 DFS GlobalAlloc 图给每 alloc 分配冻结区真地址
  （先分后填，环安全）；遍2 拷字节 + 遍 ptrs() 写目标真地址 + addend。
- **F6 ByteRegion 必须改 mmap 定容区**：`Ref/RawPtr` 取帧内局部**真地址**（&arr、as_mut_ptr），
  现 `Vec<u8>` 会搬家 → 帧地址终身稳定要求 mmap（GuestMemory 同款）。**这是一切的地基，第 1 步做**。
- **F2 Drop glue 是刚需但只是一个 Call**（正常路径；glue 已收集，`Instance::resolve_drop_glue`
  ——注意本 nightly 是这个名字不是 resolve_drop_in_place）。unwind 路径才是 M4.2。
- **atomic_cxchg 240 处**（Once/Arc 引用计数）比预想更早成高频 → M4.1 就按 spike4 义务直接内建
  成宿主原子，不留单线程假实现。

### 7.4 SIMD/`__m128i` 决策（已定：B 目标 A 排序）

hashbrown SSE2 group 探测（`__m128i`）在 map_digest 可达路径。**无硬难点**（m4.1-design §4.1）：
16 字节值走字节 arena+memcpy，运算是 4-8 个逐字节 lane 操作（各 ~10 行宿主循环，Miri 有参考），
两条通道（simd_* 泛型 / llvm.x86.* foreign）汇入 D5/foreign 表。**决策**：SIMD 最小集作施工
顺序**第 5.5 步**，map_digest gate 保留真 HashMap；降级条件 = 实测尾巴 >10 操作时临时换自写 map、
SIMD 挪 M4.2。

### 7.5 施工顺序（每步有 gate 函数变绿）

0. worklist 闭包扩集 + foreign 三路骨架
1. ByteRegion → mmap 定容真地址区（F6）
2. place 求值 IR + 引擎（Deref/Index/Offset/Ref）→ rawptr_digest、slice_digest 绿
3. ScalarPair 通道 + Transmute + 浮点/128 → float_digest、enum_digest 绿（判别式/Downcast/Aggregate）
4. 常量池 + statics 两遍重定位 → static_digest、string_digest 绿
5. 堆内建 + Drop-as-Call + intrinsic 内建表（atomic_* 直接宿主原子）→ box_digest、vec_digest 绿
5.5 SIMD 最小集 → map_digest 绿
6. gate 脚本 tests/m4_gate1.sh + 回归 + m4-log 条目

### 7.6 M4.1 gate

`demo/m4/digest.rs`（vec/string/map/box/enum/slice/static/rawptr/float digest，返回 u64
校验和）。gate = `--vm-call` 全对（期望值 = 同源 native 编译直跑，脚本内嵌）+ `--vm-stats`
复测九函数可达 Trap=0（resume 豁免）+ 纯度门禁 + spike1-5 + diff 16/16 无回归。

---

## 8. 工作流纪律（用户明确要求，每期照做）

1. **开工前调研**：跑 `mirvm run --engine vm --vm-stats <demo>` 拿真实债务表；`rustc
   -Zunpretty=mir` dump 构造形状；grep `rustc-src` 核 API 签名。**绝不凭直觉/训练知识写
   rustc API**——本 nightly MIR 有漂移（§9）。
2. **出设计文档给用户审**（过审期）或简报（简报期）。设计文档 = 调研数据 + 决策 + 施工顺序 +
   gate 定义 + 风险。
3. **实现**：Trap-stub 全覆盖（不认识的构造就地降 Trap，绝不中止降低；只有被执行的路径须
   trap-free）；防静默错值（非标量返回/参数宁 Trap 勿返 0）。
4. **完工写经验文档**（`docs/m4-log.md` 追加期条目）：gate 结果 + 教训 + 遗留。
5. **每步跑回归**：`bash tests/m4_gate0.sh`（新引擎 gate + 纯度门禁）、`MIRVM=$(pwd)/target/
   release/mirvm bash tests/diff.sh`（tier-0 16/16）、`./target/release/mirvm spike{1..5}`、
   `bash tests/spike4_tsan.sh`（TSan，首次 build-std ~1-2 分钟）。

**`--vm-stats` 是永久调研仪器**（`src/vm/engine/stats.rs`）：Trap 债务直方图 + 分期余额 +
per-export 可达 BFS（经 Call 边）。**已知盲点**：fn-ptr 间接调用无出边、layout 失败的整函数
trap_body 无出边、"未收集"callee 无出边（worklist 落地后前两个仍在）→ 债务读法永远"至少欠
这些"。

---

## 9. 关键陷阱与经验（血泪，务必知道）

- **本 nightly（2026-07-02）MIR 漂移**（M4.0/M4.1 实测，凭记忆必翻车）：
  - `Rvalue::Use(Operand, WithRetag)`——retag 折进 Use（Tree Borrows）；fast machine **忽略
    retag**（别名检查是检查器语义，P3 不检测）。
  - `Rvalue::NullaryOp` 没了；UbChecks 变 **`Operand::RuntimeChecks`**（Ub/Overflow/Contract）
    → 按 session 旗标折 bool 立即数。
  - `EarlyBinder::bind(tcx, v)` 多了 tcx 首参。
  - drop glue = `Instance::resolve_drop_glue`（不是 resolve_drop_in_place）。
  - `Operand` 有 `RuntimeChecks` 变体（match 要覆盖）。
  - MonoItemPartitions 在 `rustc_middle::mono`（不是 mir::mono）；字段 codegen_units/
    all_mono_items；`CodegenUnit::items()` → FxIndexMap；`MonoItem::{Fn(Instance),Static,
    GlobalAsm}`。
  - Cranelift 0.133.1：unwind 走 wasmtime-unwinder feature（非 eh_frame，**不采**）；
    `__register_frame` 逐 FDE（libgcc）vs 整段（libunwind）语义分裂——逐 FDE + CIE 判别字段。
- **CLI 是空格分隔**（`--engine vm` 不是 `--engine=vm`）。
- **借用纪律**（spike2 教训，engine 全靠它）：raw-ptr ctx，字段级瞬态借用（`&mut (*ctx).region`
  非 `&mut *ctx`），绝不跨 call_guest 持有。
- **配额/context**：本次会话 context 爆到 402%、/compact 因配额失败——所以有本文档。新 agent
  接手时 context 是干净的，读文档即可。

---

## 10. 挂起检查点（勿丢，写在 m4-plan.md §7）

1. **vmctx 内部约定 P vs R 真负载终裁** → M5 JIT 接入（spike5 初判 R 快 8%，但微基准偏袒 R）。
2. **landing pad/LSDA**（JIT 帧内跑 drop glue）→ M5（spike5 已验 CFI 传播半边）。
3. **fork/clone os::+atfork** → M4 后。
4. **预降低 std 发行工件**（用户 2026-07-07 提，并入 mode B）：非泛型 std 预降字节码 + 泛型带
   多态 MIR（泛型不可能全预降）；收益=启动只降用户 crate、消费端零 rust-src；落 M4.5 后/M5
   前后，M4 全程 mode A 够用不阻塞。

---

## 11. 命令速查

```bash
# 构建（必须 release）
cargo build --release

# M4 新引擎跑一个导出函数（M4.0/M4.1 gate 入口）
./target/release/mirvm run --engine vm --vm-call 'fib(25)' demo/m4/pure.rs

# 调研仪器：Trap 债务表（每期开工前跑）
./target/release/mirvm run --engine vm --vm-stats demo/m4/digest.rs

# gate + 回归
bash tests/m4_gate0.sh                                    # 新引擎 gate 9/9 + 纯度门禁
MIRVM=$(pwd)/target/release/mirvm bash tests/diff.sh      # tier-0 16/16
./target/release/mirvm spike1  # .. spike5                # spike 回归
bash tests/spike4_tsan.sh                                 # TSan（引擎 Sync）
bash tests/corpus.sh                                      # corpus（生态覆盖，多为预期红）

# tier-0 跑法（弃子，但仍是差分 oracle）
./target/release/mirvm run demo/fib.rs

# dump MIR（调研）
RUSTC=~/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc
$RUSTC --edition 2024 -Zunpretty=mir <file>.rs

# grep rustc 源码核 API（写 lower 前必做）
SRC=~/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/lib/rustlib/rustc-src/rust/compiler
grep -rn '<符号>' $SRC/rustc_middle/src/
```

---

## 12. 给接手 agent 的一句话

模型 A 已被 5 个 spike 验证到底，M4 是"大体量、低未知度"的工程——**风险已排完，剩下是照施工
顺序把 Trap 一类一类消掉，每消一类跑一次 gate**。用户看重：VM 作者视角、真实数据驱动（`--vm-stats`
是你最好的朋友）、每期设计文档过审、防静默错值、不推翻既有决策而不读其论证。下一个动作明确：
**M4.1 施工顺序第 0 步（worklist 闭包扩集 + foreign 三路骨架）**。开工前把 `docs/m4.1-design.md`
和 `docs/m4-debt-map.md` 读完。
