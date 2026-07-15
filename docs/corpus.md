# corpus 驱动补全（M2.5）

> 文档状态：**2026-07-05 的历史调研快照**。它保存边界发现，不是当前通过清单；M5.0 已推进
> inline asm 前沿，signal 也曾被退出码 oracle 假判为绿。当前可信边界见
> [current-status.md](current-status.md) 和实际测试脚本。

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
  **后续覆写（2026-07-12）**：`c_backtrace` 已确认该边界。宿主 unwinder 不能产生
  guest frame/IP，当前以 `_Unwind_Backtrace` 精确原因的 XFAIL 明确拒绝，而不是伪造回溯。

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

### 第三批：socket 网络（2026-07-05；2 ✅ / 1 危险探针挂死）

- **net_tcp**（纯 std）· loopback TCP 往返：bind/listen/connect/accept/send/recv/close · ✅（与 native 逐位一致）
  经验：**std::net 用 libc 直接实现（不走 rustix）**→ socket syscall 全经 dlsym 直通真内核，无 asm 问题（对比 tempfile 走 rustix 撞 asm）。单线程 + **write-before-read 排序**：connect 靠内核 loopback 握手进 backlog 自完成、accept 从 backlog 立即返回、read 时数据已在接收缓冲——**每个阻塞调用都靠内核自释放，不依赖另一 guest 线程**，故协作调度下能跑。TCP ping/pong 往返正确。

- **net_udp**（纯 std）· loopback UDP：socket(SOCK_DGRAM)/bind/sendto/recvfrom · ✅（与 native 逐位一致）
  经验：send-before-recv，数据报在 recv 前已入队。同样单线程 + 内核自释放。

- **net_echo_threaded**（危险探针，不入自动批）· 线程化 TCP echo server · ❌ **挂死（timeout）**
  经验：**§2.1 的网络形态确认**，而且这是**最普遍的真实模式**——`client.read_exact` 阻塞等 server 线程 echo，但协作调度器冻结在真 `read()` syscall 里，server 线程永远跑不起来。**惯用的阻塞式线程网络服务器（accept→read→write 在独立线程）在协作 tier-0 上根本跑不了**（见 §2.1 升级）。

补丁记录（第三批）：无（socket 全走 std libc 直通，无缺口）。

### 第四批：进程 / 信号（2026-07-05；0 ✅ / 2 ❌，两个都是清晰的 denylist/边界处置信号）

- **c_process**（纯 std）· `std::process::Command` 跑真子进程（echo / true / false）· ❌ **两层：弱符号（已补）→ §2.5 检查器残留**
  经验：**std 选了 `posix_spawn`（不是 fork+exec）**。
  ① 先撞**弱 extern static `pidfd_spawnp`**：std 用 `weak!` 探测新 glibc 的 pidfd 生成符号。mirvm 有弱符号机制（把可选符号置 NULL 让 std 走回退，如 statx/__cxa_thread_atexit_impl），`pidfd_spawnp` 缺在白名单——**琐碎缺口，已加进 NULL 表** → std 回退到不带 pidfd 的普通 posix_spawn。
  ② 补后撞 **§2.5 检查器残留**：`glibc_version()` 对 `gnu_get_libc_version()` 返回的 native 字符串做 `CStr::from_ptr`→strlen，读无 AllocId 的真地址指针被 InterpCx 内存检查器判 `DanglingIntPointer(MemoryAccess)`。**与 walkdir 同一根因**（native 返回指针访问）——§2.5 是反复出现的类级阻塞。
  处置：**子进程 = 真 OS 进程**（posix_spawn 内部 clone+exec，guest 从不直接调 fork；execs 立即发生，mirvm 副本子进程瞬间变成目标二进制，无解释器在子进程跑）。§2.5 根治后即通。

- **c_signal**（libc）· 注册信号处理函数 + raise · ❌ **`signal` denylist（需 thunk，设计信号）**
  经验：`libc::signal` 撞 denylist（native.rs 明列 signal/sigaction/raise 绝不直通）。**根因**：handler 是 guest `extern "C" fn`（解释代码、无机器地址），内核信号投递需要**真机器地址**——这是 **FFI 反方向 / thunk** 问题，和 pthread_create 的 start_routine、qsort 回调同一类（把解释态函数指针 materialize 成真 libffi closure）。tier-0 未建 thunk → 故意 denylist。**验证了 signal 上 denylist 的理由**；M4 建 thunk 后同一机制解决。

进程/信号批的元结论：**denylist 的处置在此得到具体验证**——`fork`/`exec`/`clone` 挡 VM 模型破坏（std 主动避开、改用 posix_spawn=真子进程，OK），`signal` 挡 thunk-未建的 guest→内核回调。两者都不是"缺 shim"，是明确的边界纪律。

### 第五批：mmap（2026-07-05；0 ✅ / 1 ❌，§2.5 第三实例）

- **c_mmap**（memmap2）· file-backed + 匿名内存映射 · ❌ **§2.5 检查器残留（第三实例）**
  经验：**mmap/munmap 直通本身成功**（`map_mut` 返回、映射建出来了）——挂在 guest **访问**映射区：`mmap[0..5].copy_from_slice(...)` → `copy_nonoverlapping` 写内核映射区，被 InterpCx 判 `DanglingIntPointer(MemoryAccess)`。mmap 区是内核在真地址给的映射、**不是 mirvm 分配（无 AllocId）**，检查器拒。mmap 是 §2.5 **最典型、最重要的实例**——"guest 合法访问、mirvm 没分配的真地址区"（分配器 / 内存映射文件 / 共享内存 / JIT 代码全靠它）。**三个不同 crate（walkdir/process/mmap）收敛 §2.5 → 它是 tier-0 第一号阻塞。**
### 定向探针（不入自动批，会故意挂）

- **c_tokio_mt.rs** · 多线程 tokio · 证伪 §2.1 初判（不死锁）。
- **c_blocking_io.rs** · socketpair 阻塞读 · **挂死**——隔离出 §2.1 真实危险（无超时真 syscall 等另一 guest 线程）。
- **c_net_echo_threaded.rs** · 线程化 TCP echo server · **挂死**——§2.1 网络形态，惯用阻塞式线程服务器模式（见 §2.1 升级）。

## 2. 逼出的设计票据

### 2.1 协作调度 vs 真阻塞 syscall（并发模型）

tokio 通过靠的是：未知外部函数（`epoll_create1`/`epoll_ctl`/`epoll_wait`/`eventfd`）不在 shim 白名单、不在 native denylist → **dlsym 直通真 libc/内核**（`src/interp/native.rs`）。而协作调度器把所有 guest 线程多路复用到**一条真解释器线程**上。

**初判（错）**：多线程运行时 worker 会各自阻塞在真 `epoll_wait` 上互等 → 死锁。**实测证伪**：多线程 tokio（4 worker + 定时器负载）正确跑通、不死锁。corpus 纠正了这个粗糙假设。原因：
- worker 空闲 park 走的是**模拟 futex**（`emulate_futex`，非真 syscall），协作调度器能正确阻塞/唤醒；
- 持 IO driver 的 worker 调 `epoll_wait` 时，定时器轮给它**有限超时**（sleep 1ms）→ 自释放 → 放行调度器。

**真实危险（钉死）**：guest 线程在**无超时**的真阻塞 syscall 上、且其释放**依赖另一 guest 线程推进**时才死。隔离验证（`corpus/c_blocking_io.rs`）：两个 std 线程，主线程在 socketpair 上 `read_exact`（真 `read()` 直通），另一线程负责写——主线程阻塞整条真线程 → 写线程永远跑不起来 → **挂死（timeout）**。

→ **票据**：dlsym 直通阻塞 syscall **仅当调用能自释放**（有限超时，或等待事件来自 guest 之外的真外部 IO/真定时器）才安全；**释放依赖另一 guest 线程推进时即死**。tokio 能活是因为它精心围绕超时 + 模拟 futex park 构建，**不是模型本身健全**。这是 M4 真 1:1 线程的核心必要性（账本 C8）——每个 guest 线程是真线程，阻塞 syscall 只挡自己，别人在别的核上跑。

**升级（第三批 socket 网络证实）**：这个"窄"危险条件其实是**最普遍的真实模式**。惯用的阻塞式线程网络服务器——server 线程 `accept()→read()→write()`、client 线程连接/收发——正是"guest 线程 A 阻塞在无超时真 syscall 上等 guest 线程 B"。`net_echo_threaded` 探针在 `client.read_exact` 挂死。所以协作 tier-0 **根本跑不了标准的阻塞式线程网络服务**（一大类真实程序）；能跑的只有单线程有序 IO（`net_tcp`/`net_udp`：靠内核 loopback 自释放）或 async 事件循环（tokio：模拟 futex + 超时）。危险从"边角情况"提升为"主流服务器模式"——M4 真线程对服务器类负载是刚需，不是优化。

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

**denylist 各条目的处置在第四批得到具体验证**：
- `fork`/`vfork`/`clone`/`exec*` 挡的是"复制/替换解释器进程"（一 fork 就两份 mirvm）。corpus 证实 std 的 `Command` **主动避开**它们、优先 `posix_spawn`（不在 denylist）——posix_spawn 内部 clone+exec 原子完成、guest 从不直接调 fork、子进程瞬间 exec 成目标二进制，**子进程 = 真 OS 进程**，正确处置。denylist 拦裸 fork、放行 posix_spawn 的区分是对的。
- `signal`/`sigaction`/`raise` 挡的是 **guest→内核回调缺 thunk**：handler 是解释代码无机器地址，内核信号投递要真地址。这与 pthread_create start_routine、qsort 回调同属 **FFI 反方向 / thunk** 类（materialize 解释态函数指针成真 libffi closure）。tier-0 未建 thunk 故 denylist；M4 建 thunk 后同一机制一并解决 signal / C→Rust 回调。

### 2.4 协作调度性能（tier-0 弃子实证）

rayon 用 28s：按语句时间片 round-robin 多路复用 work-stealing 池跑 10 万次并行迭代。正确性没问题，但坐实协作模型只能做对拍基底，**性能要真线程 + JIT**。非设计票据，是 tier-0 是弃子的又一实证。

### 2.5 检查器 overlay 残留：native 缓冲的指针算术（walkdir，已知待补）

walkdir 遍历目录时 `UndefinedBehavior(DanglingIntPointer{ InboundsPointerArithmetic })`：`readdir` 返回的 dirent 缓冲来自 native（真地址、无 AllocId），std 对它做 `d_name` 偏移指针算术，**InterpCx 的 inbounds 检查器把无 provenance 的真地址指针算术判成 UB**。

这不是新设计信号，是**已记决策的具体实例**：fast machine 仍带着 rustc InterpCx 的检查器 overlay，而账本 C2/C4 已定 **VM tier 甩掉 AllocId/检查器 overlay**（真地址下裸宿主访问，有没有 AllocId 都在真地址上）。是一整类——任何对 native 返回缓冲（dirent / `readdir` / 某些 libc 返回结构）做指针算术的 guest 代码都会撞。

**复现率（第四、五批新增实例）**：c_process 的 `glibc_version()` 读 `gnu_get_libc_version()` 返回的 native 字符串（`CStr::from_ptr`→strlen）、c_mmap 写内核 mmap 映射区（`copy_nonoverlapping`）都撞同一个 `DanglingIntPointer(MemoryAccess)`。**三个不同 crate（walkdir 经 readdir、process 经 glibc_version、mmap 经内核映射）收敛到同一根因**——native/内核给的真地址区（无 AllocId、不在分配表）被 InterpCx `ptr_get_alloc` 反查不到 → 判 UB。**§2.5 是 tier-0 第一号阻塞**，其中 mmap 最典型（分配器/内存映射文件/共享内存/JIT 代码都靠"访问自己没分配的真地址区"）。

根因机制：`Prov::Wildcard`（int2ptr）指针靠 `by_addr` 反查表定位分配，但 native/内核内存从未进过分配表 → 查不到。**核心难点：检查器要 bounds（size），而 native 区没有已知 bounds**（mmap 多大、dirent 缓冲多长、libc 串多长，mirvm 都不知道）。所以修法只能是**对无 AllocId 指针放弃 bounds 检查、直接裸宿主 read/write 请求的字节**（fast machine 本就假设程序合法、不该查）——但这要 hook InterpCx 的内存访问路径（`get_ptr_alloc`/`Allocation` 层）在"查不到分配"时改走 `ptr::copy` 裸访问，是**结构性改动，与框架相抵**，正是 M4 clean-slate 甩掉 overlay 要根治的（retrofit 进 tier-0 = 逆着 InterpCx 打）。

→ tier-0 层面记为残留（与"主线程名/rt cleanup/getrandom 确定性/内存未分池"同类）；M4 根治，且因三实例收敛，是 M4 高优先。

### 2.6 fork / clone 的真实处置（denylist 不是终局，也不是围栏）

进程批里 denylist 挡住了 `fork`，但用户点出关键：**你保证不了 guest 不直接写 `libc::fork()`——那怎么办？** 甚至更进一步：即便 denylist 了 `fork` 符号，还有别的路进内核。当前实测：`libc::fork` 撞 denylist、裸 `syscall(SYS_clone=56)` 撞 syscall 白名单——**此刻都被拒**，但这是**偶然**（asm 现在不支持、syscall 走白名单）。真正的逃逸口是 **asm-JIT 落地后**：rustix 用 `syscall` **指令**（asm）发 clone，JIT 直达内核、`os::` 拦不住（见 §2.2/§2.3）。所以：

**(a) denylist 不是围栏，只是"命名符号路径的尽力防护 + 未建信号"。** 真正的 fork 拦截只能在 **OS 级（seccomp 过滤 clone）**——与"沙箱是 OS 的事"一致。想在 mirvm 层"禁止 fork"是拦不住的。

**(b) 终局不是"禁"，是"真的支持 fork"。** 从 VM 作者视角，关键是三分：
1. **fork + 立即 exec（99% 场景，含 posix_spawn）**：**安全**。子进程那份"坏掉的 VM"状态无所谓——exec 立刻用新程序镜像替换掉它。已通过 posix_spawn 支持；裸 `fork`+立即 `exec` 同理。
2. **fork-alone，单线程 guest**：**可做到正确**。宿主 `fork()` 靠 COW 复制整个地址空间——**因为 mirvm 用真实地址，guest 的 Rust 堆被内核 COW 正确复制（真实地址模型再次白赚）**；子进程只有一个线程，VM 状态天然一致。只需 **atfork 修复**：把调度器重置到幸存线程、重置 VM 内部锁、重启后台线程（JIT/分配器）。
3. **fork-alone，多线程 guest**：**本质脆弱——但 native 也一样脆**。POSIX 明文：多线程进程里 fork 与 exec 之间只准 async-signal-safe 操作（别的线程持的锁在子进程里永久锁死）。native Rust 程序这么写本就在 UB 边缘。mirvm 只需**匹配 native 的可观测行为**（差分哲学）——native 脆我们不必更强，"尽力/可能坏"是可接受的，与 native 一致。

**(c) 机制**：把 fork 走 `os::`，配 `pthread_atfork` 式纪律（JVM 的 os:: 正是这么管 fork 的）——宿主 `fork()` + 子进程侧 VM 重置。真实地址模型让 **guest 数据 COW 白赚正确**，要修的只有 **VM 自有的元数据/线程**（每个多线程运行时对 fork 的同一难题，有已知的部分解）。

**(d) JVM 对照**：JVM 干脆**完全不支持 fork-alone**——`Runtime.exec` 永远是 fork+exec、子进程绝不回到 JVM。mirvm 可以采同样的保守默认（只支持 fork+exec），但真实地址的 COW 优势让 mirvm 有条件对**单线程 fork** 做得比 JVM 好。

结论：tier-0 denylist `fork` 是**临时**（未建 atfork/VM-重置），非终局。M4 的 `os::` 按上面三分处置；containment 交给 seccomp，不假装 denylist 是安全边界。

## 3. 通过项验证了什么（架构确认）

- **async-stackless 成立**：tokio current_thread 运行时（定时器 + mpsc + spawn 任务）跑出正确结果（1530/15 msgs），无需引擎特殊支持——任务是编译器降解的无栈状态机，poll 驱动（见 designs/async-stackless.md）。
- **协作 + 模拟 futex 是合法 SC 执行**：rayon / crossbeam 正确跑通，说明良好同步的并发程序在 round-robin 上产出合法的顺序一致执行——这是 tier-0 作对拍基底的价值。但它把一切串行化，**验证不了真并行**（弱内存、数据竞争、真并行推进）——正是 M4 必补。
- **真 OS 原语直通可用**：epoll/eventfd/getrandom/文件 IO/**socket 网络** 经 dlsym/shim 直达内核，无需模拟——印证"有真 OS 就直接用"。std::net 用 libc（非 rustix），TCP/UDP loopback 往返与 native 逐位一致。
- **unsafe/布局/别名在解释器下正确**（第二批）：smallvec 的 union 式 `MaybeUninit` 内联存储、bytes 的 Arc 原子 refcount + 零拷贝切片别名、petgraph 的 arena 索引——纯计算类 unsafe 全走通，问题只在 OS/asm 边界。

## 4. 下一批候选（去撞更多边界）

已跑五批（22 crate + 3 探针）：14 通过。真实阻塞收敛到五处，每处都有明确 M4 处置：**inline asm（§2.2 三张面孔）→ JIT asm 块**、**协作调度 vs 阻塞 IO（§2.1，线程化服务器主流模式）→ 真线程**、**检查器 overlay 残留（§2.5，walkdir+process+mmap 三实例，tier-0 第一号阻塞）→ 甩掉 AllocId 检查器**、**guest→内核回调缺 thunk（§2.3，signal）→ M4 建 thunk**、**fork/clone 处置（§2.6）→ os:: + atfork，containment 交 seccomp**。下批候选（边际递减，多为上述类的重复实例）：

- **更多 RustCrypto**（aes/chacha20）：预期同 §2.2 cpuid，确认家族一致——低边际，可略。
- **proc-macro 重度**（syn/quote 作为**依赖被使用**时的运行期，非展开期）：确认运行期确实不碰 proc-macro。
- 或转向：挑一个 §2.5 实例做 tier-0 裸访问 spike，或转 M4 前骨架 spike。

## 5. 真实项目三维差分扩编（2026-07-15；批1 13 个 + 批2 14 个）

> 背景：M5.4b 收尾期一个错值级 miscompile（analyze_frame 只记基址漏区间）在合成
> 门禁全绿下潜伏了整片 M5.4a，最终由真实项目（regex capture drop 链）炸出——
> 用户据此裁定 corpus 从"exit-code 冒烟"升级为 **三维逐字节差分**：
> **mirvm 默认 / native cargo run / MIRVM_JIT_THRESHOLD=1**，stdout/stderr/exit
> 全部逐字节一致才算绿（driver 确定性纪律：定种、BTree 序、浮点 to_bits、stderr 真空）。
> 复跑法：每个 driver 在 `corpus/c_*.rs`，三维命令见其文件头注释与 m5-log M5.4b 节。

### 批1（13 个；10 绿 / 2 FRONTIER / 1 路径探针）

- **绿**：serde_json（Pair 返回密集迭代器）、serde_yaml（unsafe-libyaml 纯 Rust
  移植）、rand_det（rand 0.9 改名 API）、flate2（miniz_oxide raw deflate；
  gz/zlib 原生 API 撞下述 FRONTIER 改手工容器等价覆盖）、brotli、argon2
  （内存硬）、ed25519（dalek u128 域算术；用官方 serial backend 绕下述
  avx512ifma）、p256（RFC6979 定向量锚点）、syn_parse（递归类型 + drop glue 重）、
  hickory（DNS codec；ring 撞 bug② 后的替换项）、unicode_tables（大表四件套）。
- **FRONTIER（锁定 expected-red）**：c_aes_gcm（`llvm.x86.aesni.*`/`pclmulqdq.*`
  未内建，aes/ghash 运行期探测无 force-soft 退路）、c_png_round
  （`llvm.x86.avx2.psad.bw` 未内建，simd-adler32/fdeflate 处处必经）。
- **c_serde_json**：三维对拍机制的路径探针（materialize → script_dir → cargo run -q）。

### 批2（14 个；全绿，2 个 FRONTIER 绕行记录）

- wasmi（**VM-in-VM**：wat 模块调用/memory/global/宿主回调/trap 四类）；
  boa_js（纯 Rust JS 引擎大物：45 片段全语义面；JIT 队列 5562 函数/发布 1862，
  输出仍逐字节一致——语义零依赖 JIT 的实证）；
  tiny_skia（标量路径 2D 光栅化 32 轮，像素 FNV hash 三路一致——浮点重场景
  JIT 与解释器无分歧）；zip_arch（Stored+deflate；crc32fast **≥128B 单块**必撞
  pclmulqdq，64B 分块合法绕行；实证 zip deflate 走 raw 不碰 simd-adler32）；
  rust_decimal（96 位定点）；rustfft（标量路径全绿；默认 avx 撞
  `llvm.x86.avx2.gather.q.pd.256` = FRONTIER，且运行期探测致两路径 1-ulp 分叉
  对拍本无意义，钉 default-features=false）；roaring / bitvec（指针打包别名边界）/
  compact_str（niche 24B 内联临界）；nom_parse / comrak_md（全扩展 CommonMark）/
  fst_build（自动机）；jieba_cut（钉 =0.10.0：0.10.2 的 bytecount 依赖撞
  `llvm.x86.sse2.psad.bw`；另避 jieba-macros 0.10.1 semver 破洞）；
  spade_delaunay（robust 精确谓词：共圆精确零 / 1e-13 近共线 / 1ulp 扰动
  全逐比特一致）。

### 扩编撞出的两个产品 bug（均已修复）

- **bug① 缓存污染**（`718dac5`）：A2 split 的 fn_addrs 按【值域】分拆，S4 补建
  条目（底座 fn 在 deps 降低期于 image 冻结区补建 fn 条目）被留在建者 delta——
  消费方装载同一 image 后其静态烘焙的补建地址在运行期反查表无登记 → 间接调用
  abort「不是已知 fn 条目」（三个 driver 独立撞见；负对照 edit_rand v2-v6 五连崩
  同址 0x6a0000001630/core::fmt::write）。**修复 = 按【地址域】分拆**；负对照
  45 跑 5 崩 → 修复后 45 跑 0 崩。
- **bug② ring 整 crate lower panic**（`cb09b5b`）：fn 体内 extern fn item 作
  fn 指针实参 → 取址路径不判 `is_foreign_item` 直取 optimized_mir → rustc query
  panic。**修复 = foreign_fn_entry_addr（native 链接器语义真符号地址）+
  elfsym.rs（.symtab 兜底，ring 的 -fvisibility=hidden 归档符号）**。ring
  SHA-256 三向量与 native 逐字节一致。

### M5.x intrinsic 内建欠账队列（按证据密度排序）

| intrinsic | 撞它的真实 crate |
|---|---|
| `llvm.x86.avx2.psad.bw` / `llvm.x86.sse2.psad.bw`（`_mm(256)_sad_epu8`） | png/fdeflate、simd-adler32（→flate2 gz/zlib）、jieba-rs 0.10.2 bytecount |
| `llvm.x86.pclmulqdq.*` | crc32fast ≥128B 单块（→zip/flate2 gz）、aes-gcm 的 ghash/polyval |
| `llvm.x86.aesni.*` | aes/aes-gcm（运行期探测无 force-soft） |
| `llvm.x86.avx512.vpmadd52*`（IFMA） | curve25519-dalek 默认 simd backend |
| `llvm.x86.avx2.gather.*` | rustfft 默认 avx（GoodThomas/Rader 路径） |

共性机理：guest cpuid 直通宿主 → 运行期派发选中硬件路径 → 未内建 intrinsic 降
Trap。处理口径：内建进 lower 的 llvm.x86 内建表（M5.2 的 pshufb/sha256 先例；
属 M5.4d SIMD 或独立 M5.x 片）。内建一个解锁一片真实 crate（png/jieba-0.10.2/
flate2 原生容器/crc32fast 整块/aes-gcm/dalek 默认路径/rustfft-avx）。

### gate 接线（2026-07-15）

- gate5 corpus 段扩到 57 个程序（新增 24 绿 + aes_gcm/png_round 双 expected-red
  ——red_pattern 锁定诊断，内建后 XPASS 强制转绿；ed25519 段内注入官方
  serial-backend env）。jieba_cut 全绿但单跑 77-89s 贴 timeout，留 corpus.sh。
- corpus.sh 默认清单同步扩编（timeout 600 容纳 jieba）。
- 三维逐字节差分在 driver 创建时强制执行；gate 内为 exit-code + oracle 级
  （native 逐字节维的冷构建成本不进 gate）。

### 批3（14 个；8 绿 / 4 FRONTIER / 2 产品 bug 实锤）

- **绿**：smoltcp_tcp（纯 Rust TCP/IP 栈：loopback echo 全状态机 + 手工时钟 +
  codec 畸形 13 条）、snow_noise（Noise_XX/NKpsk0 ChaChaPoly 全 transcript 锚定；
  poly1305 avx2 撞 `llvm.x86.avx2.permd` 用官方 `--cfg poly1305_force_soft` 绕——
  与 ed25519 serial env 同型先例；curve25519-dalek 默认 simd backend 全程安然）、
  statrs_stats（0.18 分布族/检验全 bits 锚）、rkyv_zero（零拷贝 + bytecheck
  校验路径；validator 错误文案内嵌裸地址，归类打印）、qr_round（qrcode+rqrr
  闭环，RS 纠错 5×5 翻转仍解出）、fatfs_img（钉 =0.3.6——0.4 从未发布；
  chrono 壁钟炸弹用 default-features=false + 固定 TimeProvider 拆）、
  geo_ops（0.29 robust/i_overlay 谓词全 bits 一致）、rhai_script（meta 解释器：
  闭包/宿主注册/11 种错误变体/资源上限；ahash runtime-rng 不打印哈希序集合）。
- **FRONTIER（锁定 expected-red）**：gix_pure（git loose object 必经 zlib →
  simd-adler32 `sse2.psad.bw`；三条 zlib 路线排查全记录）、lz4_snap（snap frame
  层 crc32c 撞 `llvm.x86.sse42.crc32.*` 新族；lz4_flex 全线 + snap raw 绿）、
  calamine_xlsx（rust_xlsxwriter/calamine 双 crate 经 zip entry CRC 撞
  crc32fast ≥128B 单块 pclmulqdq，块长封死在 crate 内部不可绕）、
  rusqlite_db（native_archive 闭包策略：libsqlite3.a 的 FTS5 引 libm `log`，
  `-z defs` 整档链接拒——闭包检查未计 libm；exit 101，red_code 机制因此
  从写死 70 扩为按程序可配）。
- **产品 bug 实锤**：
  - **track_caller fn_span**（`7dc3b31` 已修）：方法调用点 Location 取
    整个调用表达式 span（lo=接收者）而非 rustc 的 fn_span（被调名段）——
    redb TableAlreadyOpen 错误串行列分叉实锤（269:18 vs 269:20；链式多行
    连行号都偏）。凡方法调用点 unwrap/expect 的 panic 头全偏。修复后
    redb_kv 三维转绿并入 gate。
  - **zstd 静默换库**（已修，待主线收录提交）：hidden-visibility 归档（.dynsym
    空）符号解析 dlsym(RTLD_DEFAULT) 优先于归档 .symtab 兜底 → guest zstd 被
    绑到宿主 libLLVM 内嵌 zstd（dfast 策略 level 3/4 输出不同，len=915 vs 918）；
    reject_symbol_ambiguity 用 nm --dynamic 对空导出表失效。**native 链接器
    语义：静态归档成员的定义在链接期绑定，guest 自己的库永远赢过全局
    命名空间**——修法 = 兜底表只收 .symtab−.dynsym 的 hidden 符号并在三处
    解析点（FfiState::resolve / fn-ptr 取址 / extern static）先于 dlsym 全域
    查询；dynsym 可见面维持原序（物化期碰撞拒绝仍兜底）。修复后 c_zstd_stream
    三维逐字节转绿，ffi_zlib/blake3/ring 回归无损。
- **另发现（欠账类，未立项）**：①**thunk 盲区**——flate2 的 C-libz 后端把
  Rust allocator fn-ptr（zalloc/zfree）嵌进 z_stream **结构体**传给 libz，
  libz 回调时宿主跳进 guest 数据地址静默 SIGSEGV 无诊断（thunk 机制只覆盖
  显式 fn-ptr 实参，结构体内嵌回调是盲区；LD_PRELOAD 实锤 si_addr==rip 落在
  delta 冻结域 rw 非可执行）。②**rusqlite 的 libm 闭包缺口**（见上
  FRONTIER——闭包检查应纳入 -lm 或白名单系统库进 DT_NEEDED）。

**M5.x intrinsic 欠账队列追加**（按批3 证据）：`llvm.x86.sse42.crc32.*`
（snap frame、任意 crc32c 用户——实现成本低，单指令语义）、
`llvm.x86.avx2.permd`（poly1305 avx2；一行 shuffle 语义）。
