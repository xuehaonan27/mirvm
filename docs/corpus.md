# corpus 驱动补全（M2.5）

## 0. corpus 是什么

corpus = 一组**真实生态 crate 的最小驱动程序**，逐个在 tier-0 mirvm 上跑，用来**发现真实代码对抽象机 / VM 边界提出的要求**。

它不是"给 tier-0 刷通过率"——tier-0（rustc `InterpCx` + 协作调度）大概率整体推翻。corpus 的产物是**信号**，每个失败归类成两种之一：

- **设计信号（design-signal）**：暴露 RAM 语义、VM 边界、`os::` 处置、并发模型上的真实需求。→ M4 设计票据，必须记。
- **琐碎缺口（trivial-gap）**：只是某个 libc/intrinsic 直通还没接，M4 架构本来就覆盖。→ 低信号，可为"解锁探针"顺手补，但不投入打磨。

判据：*这个失败是在告诉我们抽象机/边界要处理什么，还是只是 tier-0 恰好没接一个 M4 已规划的通路？*

跑法：`bash tests/corpus.sh`（全量）或 `bash tests/corpus.sh <name>...`（子集）；release 二进制，程序在 `corpus/c_*.rs`。每个 crate 的 stdout/stderr 落 `/tmp/corpus-out/<name>.{out,err}`。

## 1. 逐 crate 记录

每个 corpus 单独记"跑了什么子系统 / 结果 / 学到什么"，不做笼统汇总。

### 首轮（2026-07-05；10 项，9 ✅ / 1 ❌）

- **itertools 0.13** · 纯迭代器组合子（chunks / chunk_by / cartesian_product / dedup / partition）· ✅
  经验：无 OS 边界，纯计算如期通过。唯一波折是测试程序自身的 `partition` 类型推断歧义（`&i32` vs `i32`），rustc 前端就报 E0283——与 mirvm 无关。教训：corpus 程序须先能 native 编译，否则失败信号是假的。

- **anyhow 1** · 错误链 / `Context` / `?` / `e.chain()` · ✅
  经验：默认**不抓 backtrace**（要 `RUST_BACKTRACE` + std backtrace 特性），所以没触及 `_Unwind_Backtrace` / 符号化路径。错误 trait object 装箱、`?` 传播、链遍历都正常。若开 backtrace 会撞新边界（待记）。

- **chrono 0.4** · 日期算术（`NaiveDate` + `Duration`，不调 `now()`）· ✅
  经验：纯日期运算通过。刻意避开 `Utc::now()` / `Local`（会拉 `clock_gettime` 实时钟 + `localtime_r` 时区库）——用 `Naive` 保持确定性 + 纯。闰年 / ordinal / 周几全对。

- **indexmap 2** · 插入序 map/set + hashbrown SwissTable · ✅
  经验：默认 `RandomState` 播种走 `getrandom`（已直通）→ hash 表能建。插入序保持正确、`get_index_of` 对。SwissTable 的 SIMD 探测在解释器下走的是可移植路径（未撞 asm）。

- **clap 4 (derive)** · 参数解析 + `#[derive(Parser)]` · ✅
  经验：**derive 宏在前端（编译期）展开，mirvm 运行期完全不碰 proc-macro**——这是关键区分（proc-macro 是 rustc 前端的事）。`parse_from` 显式给 argv 避开真实 argv；clap 探环境色彩（`getenv`）已覆盖。

- **csv 1 + serde** · CSV 解析（`&[u8]` 状态机）+ serde 反序列化 · ✅
  经验：serde derive（前端）+ csv 读取器 + f64/u64 字段解析全通。纯内存字符串，未碰文件 IO。

- **crossbeam 0.8** · `thread::scope` + MPMC `channel` · ✅
  经验：**首个证明协作调度器 + 模拟 futex 能跑真并发 crate 的数据点**。4 生产者 + 主消费者，100 条消息全收、求和正确。scoped 线程的 join 屏障、通道的 park/unpark 都在 `emulate_futex` 上正确工作。

- **rayon 1** · work-stealing 数据并行（par_iter / par_sort / reduce）· ✅（补 `volatile_store` 后）
  经验：先撞 `volatile_store` intrinsic 缺失（rayon 的 latch/pool 用 `write_volatile`）——琐碎缺口，协作单线程下补成普通写即可。补后**整个 work-stealing 池在协作调度器上跑出正确结果**（平方和 / 排序 / 10! 全对）。但 **28s**（§2.4 性能信号）。数据点：work-stealing 也是合法 SC 执行。

- **tokio 1 (current_thread)** · async 运行时 + 定时器 + mpsc + spawn · ✅
  经验：`epoll_create1`/`epoll_ctl`/`epoll_wait`/`eventfd` 不在 shim/denylist → **dlsym 直通真内核**（§3）。单 guest 线程，`epoll_wait` 靠定时器超时返回。结果 1530/15 msgs 正确——**同时验证 async-stackless（任务是无栈状态机）+ os:: 直通**。

- **tokio 1 (multi_thread, 4 worker)** · 多 worker 运行时 · ✅（补 `pthread_setname_np` + `powf64` 后）
  经验：依次撞 `pthread_setname_np`（denylist 太粗，§2.3）、`powf64` intrinsic（worker EWMA 统计）——都琐碎。补后**正确跑通、不死锁**（0²+…+7²=140）。**证伪了"多线程 tokio 会死锁"的初判**（§2.1）：worker park 走模拟 futex，epoll_wait 有定时器有限超时。

- **blake3 1** · SIMD 密集哈希 · ❌ **`__cpuid` 内联汇编**
  经验：`blake3 → cpufeatures → core_arch::x86::__cpuid` 是 inline asm，解释器跑不了（错误来自 rustc 内核）。**设计信号 §2.2**。整个 RustCrypto/cpufeatures 家族都会撞这——sha2 用来验证这一点（见下批）。

补丁记录（解锁探针 / 通用琐碎缺口，非打磨；差分回归 16/16 无损）：
- `volatile_store` / `volatile_load` intrinsic（`intrinsics.rs`）：协作式单线程下等同普通读写。为拿 rayon 真实并发信号。
- `pthread_setname_np` shim（`shims.rs`）：良性线程元数据，存名字、VM 内部处置（`pthread_` denylist 太粗，见 §2.3）。
- float 数学 intrinsic 桥接（`powf64`/`sqrtf64`/… → 复用 `emulate_libm`，`intrinsics.rs`）：**普遍缺口**（任何数值代码都碰），一次性接掉一整类。与 native 逐位一致。

### 第二批（2026-07-05；7 项，3 ✅ / 4 ❌）

- **smallvec 1** · 小向量优化：内联存储 ↔ 溢出到堆、retain、shrink_to_fit · ✅
  经验：`SmallVec<[i32; 4]>` 的 union 式 `MaybeUninit` 内联存储、越阈值溢出到堆、收缩回内联全对。**unsafe 未初始化存储在解释器下正确**。

- **bytes 1** · `BytesMut → Bytes` freeze、Arc 支撑的共享缓冲、零拷贝切片、大端 get_u32 · ✅
  经验：Arc 原子 refcount（`clone` 共享底层）+ 零拷贝 `slice`（别名同一 buffer）+ `Buf::get_u8/get_u32` 全对。**原子引用计数 + unsafe 指针别名正确**。

- **petgraph 0.6** · `Graph` + Dijkstra 最短路 · ✅
  经验：arena 式节点/边索引、图构建、Dijkstra 算法全对（A→D=2）。复杂 ownership 的纯计算走通。

- **num-bigint 0.4** · 任意精度整数（阶乘 / modpow / 平方）· ❌ **inline asm（`div_wide`）**
  经验：先撞 `float_to_int_unchecked` intrinsic（琐碎，已补）——num-bigint 用 f64 估位数再转 int。补后撞**内联汇编**：`biguint::division::div_wide` 用 `div` 指令做 128/64 位宽除法优化。→ §2.2 类（asm 的**第三张面孔：算术原语**）。tier-0 红，M4-JIT 归宿。

- **tempfile 3** · 临时文件创建/写/元数据/reopen 读回 · ❌ **inline asm（rustix 裸 syscall）**
  经验：撞**内联汇编**，但源头是 **rustix**：`rustix::backend::linux_raw::arch::x86_64::syscall2` 用 `syscall` 指令**直接发裸系统调用、绕过 libc**（tempfile 经 rustix 做 fstat）。→ §2.2 类（asm 的**第二张面孔：裸 syscall**）。**重要**：rustix 的 linux_raw 后端是 Linux 默认，大量现代 crate 走它；裸 syscall-via-asm **完全绕过 os:: 命名符号边界，mirvm 层无法拦截**——印证"沙箱是 OS 级(seccomp)的事"（见 §2.2、§7）。

- **walkdir 2** · 目录递归遍历 · ❌ **检查器残留（非设计信号，是 tier-0 已知待补）**
  经验：`UndefinedBehavior(DanglingIntPointer{ msg: InboundsPointerArithmetic })`，发生在 `std::sys::fs::unix::ReadDir::next` 遍历 `readdir` 返回的 dirent（走 `d_name` 偏移）。这是 **InterpCx 检查器 overlay 对无 AllocId 的 native 真地址指针误报**——正是"VM tier 甩掉 AllocId 检查器"决策（账本 C2/C4）要解决的。是一整类（任何对 native 返回缓冲做指针算术的代码）。见 §2.5。

- **sha2 0.10** · SHA-256（RustCrypto）· ❌ **inline asm（`__cpuid`）**
  经验：如预测，`sha2 → cpufeatures → __cpuid` 检测 SHA-NI 扩展。→ §2.2 类（asm 的**第一张面孔：cpuid 特性检测**）。**坐实 §2.2 是一整类**：整个 RustCrypto/cpufeatures 家族（sha2/blake3/aes/…）都走这。

补丁记录（第二批）：
- `float_to_int_unchecked` intrinsic（`intrinsics.rs`）：fast machine 假设在界内，宿主 as 转换 + 按 dest 位宽截断。通用琐碎缺口（任何 f→int 未检查转换）。
### 定向探针（不入自动批，`c_blocking_io` 会故意挂）

- **c_tokio_mt.rs** · 多线程 tokio · 证伪 §2.1 初判（不死锁）。
- **c_blocking_io.rs** · socketpair 阻塞读 · **挂死（timeout）**——隔离出 §2.1 真实危险（无超时真 syscall 等另一 guest 线程）。

## 2. 逼出的设计票据

### 2.1 协作调度 vs 真阻塞 syscall（并发模型）

tokio 通过靠的是：未知外部函数（`epoll_create1`/`epoll_ctl`/`epoll_wait`/`eventfd`）不在 shim 白名单、不在 native denylist → **dlsym 直通真 libc/内核**（`src/interp/native.rs`）。而协作调度器把所有 guest 线程多路复用到**一条真解释器线程**上。

**初判（错）**：多线程运行时 worker 会各自阻塞在真 `epoll_wait` 上互等 → 死锁。**实测证伪**：多线程 tokio（4 worker + 定时器负载）正确跑通、不死锁。corpus 纠正了这个粗糙假设。原因：
- worker 空闲 park 走的是**模拟 futex**（`emulate_futex`，非真 syscall），协作调度器能正确阻塞/唤醒；
- 持 IO driver 的 worker 调 `epoll_wait` 时，定时器轮给它**有限超时**（sleep 1ms）→ 自释放 → 放行调度器。

**真实危险（钉死）**：guest 线程在**无超时**的真阻塞 syscall 上、且其释放**依赖另一 guest 线程推进**时才死。隔离验证（`corpus/c_blocking_io.rs`）：两个 std 线程，主线程在 socketpair 上 `read_exact`（真 `read()` 直通），另一线程负责写——主线程阻塞整条真线程 → 写线程永远跑不起来 → **挂死（timeout）**。

→ **票据**：dlsym 直通阻塞 syscall **仅当调用能自释放**（有限超时，或等待事件来自 guest 之外的真外部 IO/真定时器）才安全；**释放依赖另一 guest 线程推进时即死**。tokio 能活是因为它精心围绕超时 + 模拟 futex park 构建，**不是模型本身健全**。这是 M4 真 1:1 线程的核心必要性（账本 C8）——每个 guest 线程是真线程，阻塞 syscall 只挡自己，别人在别的核上跑。

### 2.2 内联汇编 → 直接 JIT asm 块（决策：虚拟 CPU = 真宿主 CPU）

blake3 → `cpufeatures` → `core_arch::x86::__cpuid` 是内联汇编；解释器无法执行 asm（错误来自 rustc 内核，非 mirvm）。

MIR `TerminatorKind::InlineAsm` 结构（实测）：
```
asm!("add {0}, 5", inout(reg) copy _1 => _2, options(PURE | NOMEM | NOSTACK)) -> [return: bb1, unwind unreachable]
```
= { template 片段, operands（寄存器类 + in/out place）, options, targets, unwind }。曾考虑三条路（A 模板拦截 cpuid/rdtsc / B 把 asm 块 JIT / C 拒绝）。

**决策（用户 2026-07-05）：直接走 B——把 asm 块 JIT。** 理由：asm 块**本身就是机器码**，去"模板匹配 + 合成语义"（A）是在重新解释一个已经是机器码的东西，既不通用（只认识 cpuid/rdtsc）又脆。B 完全通用（任意 asm）、且是 cg_clif/Cranelift 的既有能力（Cranelift 有 inline-asm 降低），M4 直接复用。

关键认识：**asm 块是最小的 JIT 原语，各 tier 同一机制**——拿 template + operand 寄存器绑定，生成"把输入装进指定寄存器 → 执行 asm → 取出输出寄存器"的一小段机器码，调用它。M4-JIT 里它就是整函数 Cranelift 编译的一部分；即便 tier-0 解释器，也可以对单个 `InlineAsm` terminator 汇编+调用（一个自足小 JIT，不需要方法级 JIT）。

推论（**虚拟 CPU = 真宿主 CPU，不做特性虚拟化**）：asm 在真 CPU 上跑 → `cpuid` 返回**真宿主特性**。于是不存在"选特性位集"的决策（§ 早先的"tier-0 报保守基线"想法作废）。好处：差分对拍在同一宿主上 mirvm-JIT 与 native 看到**相同**的 CPU 特性 → 自然一致（不再是 unspecified）。

正交的下游问题（**与 asm 决策分开**）：cpuid 报了 AVX512 → blake3 走 AVX512 路径 → 调 `_mm512_*` **SIMD intrinsic**（LLVM intrinsic，不是 asm）。这些在 M4-JIT 下由 Cranelift 按宿主特性发真 SIMD（正确 + 快）；在 tier-0 解释器下要么实现 `simd_*`/`llvm.x86.*` 一族、要么就让 blake3 在 tier-0 保持红。**tier-0 是弃子，选后者**——inline asm 与 SIMD intrinsic 都留给 M4-JIT，不为弃子建一套 asm-JIT + SIMD 解释层。所以：blake3/sha2 在 tier-0 红是预期的，M4-JIT 才是它们的归宿。

**corpus 实证（asm 的三张面孔，强化"直接 JIT asm 块"的决策）**：两批 17 crate 里，inline asm 是最主要的真实阻塞，且以三种完全不同的形态出现——
1. **CPU 特性检测**（`cpuid`）：blake3 / sha2 经 cpufeatures；
2. **裸 syscall**（`syscall` 指令，绕过 libc）：tempfile 经 rustix linux_raw 后端；
3. **算术原语**（`div` 做 128/64 宽除法）：num-bigint `div_wide`。

模板拦截（A）要认识全部三类是无望的（尤其算术 asm 千变万化）；**asm-JIT（B）把三类统一处理**——把块汇编成机器码原样跑。这正是"asm 已经是机器码、直接 JIT"的价值。附带确认 §2.3 的边界洞察：rustix 的裸 syscall-asm **没有命名符号可拦**，mirvm 层拦不住，只有 OS 级（seccomp）能拦。

### 2.3 dlsym 直通边界（`os::` 处置验证）

现状派发：**shim 白名单 → 未命中 → dlsym 直通（RTLD_DEFAULT）**，另有 **denylist**：`pthread_*`/`exec`/`setjmp`/`longjmp`/`__cxa_`/`fork`/`vfork`/`clone`/`exit`/`signal` 绝不直通（会绕过 VM 的线程/进程/控制流模型）。

→ corpus 验证了 P7 `os::` 的形状是可行的：**默认直通 + denylist（必须 VM 内部的）+ 合成（statx→ENOSYS 之类）**。denylist 的内容与理由正是"VM-internal 处置"的清单。M4 的 `os::` 模块把这套从"散落 + 隐式 dlsym 兜底"收敛成显式登记。

**与 P7 单一收口的张力（rustix 揭示）**：`os::` 收口假设 OS 交互都经**命名符号**（libc 函数）。但 rustix 的 linux_raw 后端用 `syscall` **指令**直接进内核，没有符号——asm-JIT 决策下它直接跑到真内核，`os::` 拦不住。后果：若 mirvm 日后想在 `os::` 边界**虚拟化/重定向** OS 资源（虚拟 FS、资源限额等），rustix 系 crate 会绕过。两条出路：OS 级 seccomp 拦（与"沙箱是 OS 的事"一致），或对 `syscall` 这个特定 asm 模式**破例**路由回 `os::`（此处 asm-JIT 的"统一处理"与 P7 收口冲突，需权衡）。记为 M4 `os::` 设计的已知张力。

### 2.4 协作调度性能（tier-0 弃子实证）

rayon 用 28s：按语句时间片 round-robin 多路复用 work-stealing 池跑 10 万次并行迭代。正确性没问题，但坐实协作模型只能做对拍基底，**性能要真线程 + JIT**。非设计票据，是 tier-0 是弃子的又一实证。

### 2.5 检查器 overlay 残留：native 缓冲的指针算术（walkdir，已知待补）

walkdir 遍历目录时 `UndefinedBehavior(DanglingIntPointer{ InboundsPointerArithmetic })`：`readdir` 返回的 dirent 缓冲来自 native（真地址、无 AllocId），std 对它做 `d_name` 偏移指针算术，**InterpCx 的 inbounds 检查器把无 provenance 的真地址指针算术判成 UB**。

这不是新设计信号，是**已记决策的具体实例**：fast machine 仍带着 rustc InterpCx 的检查器 overlay，而账本 C2/C4 已定 **VM tier 甩掉 AllocId/检查器 overlay**（真地址下裸宿主访问，有没有 AllocId 都在真地址上）。是一整类——任何对 native 返回缓冲（dirent / `readdir` / 某些 libc 返回结构）做指针算术的 guest 代码都会撞。

→ tier-0 层面可"待补"（让 fast machine 对 wildcard/无 AllocId 指针跳过 inbounds 检查、回退裸访问），但本质由 M4 甩掉 overlay 根治。当前记为 tier-0 残留（与"主线程名/rt cleanup/getrandom 确定性/内存未分池"同类）。

## 3. 通过项验证了什么（架构确认）

- **async-stackless 成立**：tokio current_thread 运行时（定时器 + mpsc + spawn 任务）跑出正确结果（1530/15 msgs），无需引擎特殊支持——任务是编译器降解的无栈状态机，poll 驱动（见 async-stackless.md）。
- **协作 + 模拟 futex 是合法 SC 执行**：rayon / crossbeam 正确跑通，说明良好同步的并发程序在 round-robin 上产出合法的顺序一致执行——这是 tier-0 作对拍基底的价值。但它把一切串行化，**验证不了真并行**（弱内存、数据竞争、真并行推进）——正是 M4 必补。
- **真 OS 原语直通可用**：epoll/eventfd/getrandom/文件 IO 经 dlsym/shim 直达内核，无需模拟——印证"有真 OS 就直接用"。
- **unsafe/布局/别名在解释器下正确**（第二批）：smallvec 的 union 式 `MaybeUninit` 内联存储、bytes 的 Arc 原子 refcount + 零拷贝切片别名、petgraph 的 arena 索引——纯计算类 unsafe 全走通，问题只在 OS/asm 边界。

## 4. 下一批候选（去撞更多边界）

已跑两批（17 crate + 2 探针）：9+3=12 通过，主要阻塞收敛到 **inline asm（§2.2 三张面孔）** 与 **检查器残留（§2.5）** 两处。下批去撞尚未覆盖的边界：

- **真 socket 网络**：`std::net::TcpStream` 本机自连（注意 §2.1 危险——阻塞读若等 guest 内部推进会挂，需带超时或用非阻塞）。
- **进程/信号**：`std::process::Command`（fork/exec 在 denylist，验证 VM 内建处置）、signal handler。
- **mmap 类**：memmap2（`mmap`/`munmap` 直通 + guest 对映射区访问）。
- **更多 RustCrypto**（aes/chacha20）：预期同 §2.2 cpuid，确认家族一致——低边际，可略。
- **proc-macro 重度**（syn/quote 作为**依赖被使用**时的运行期，非展开期）：确认运行期确实不碰 proc-macro。
