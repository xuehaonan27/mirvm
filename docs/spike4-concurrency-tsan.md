# Spike 4：并发（真线程引擎过 TSan）—— 经验与教训

> 状态：**通过**（2026-07-07）。`mirvm spike4` 4 用例全 PASS；**TSan 全量插桩下零竞争警告**
> （concurrency-arch.md 的 RFC 验收标准）；spike1-3 + tier-0 diff 16/16 无回归。
> **4-spike 计划就此收官：模型 A 地基（骨架 / 互操作 / unwind / 并发）全部验证，可进 M4。**

## 1. 建了什么

- `src/vm/spike4.rs`：**Shared/Ctx 分裂**（状态三分落地）+ spike3 协议的 interp（多加
  `AtomicAdd`）+ 编译帧替身（fib / 原子循环 / 阻塞 IO / unwind）+ 4 用例 +
  `run_cases()` 双入口（CLI 与 TSan harness 共用）。
- **`tsan/` 独立 harness crate**：mirvm 主二进制链接 rustc_private 动态库（未插桩 + 自带
  分配器）不能整体上 TSan；`src/vm` 是纯 Rust → 用 `#[path = "../../src/vm/mod.rs"]`
  同源复用，`-Zsanitizer=thread` + `-Zbuild-std`（插桩 std）构建。`tests/spike4_tsan.sh`
  包判定：退出码 0 且零 `WARNING: ThreadSanitizer`。
- 共享 `bytecode.rs` 增 `Rvalue::AtomicAdd`（spike 速记；真字节码中原子按 MIR 形状是
  intrinsic **调用**——耐久的是下面 §3.1 的引擎义务，不是这个拼写）。

## 2. 验证了什么（4 用例 + TSan）

| # | 用例 | 结果 |
|---|---|---|
| A | 8 线程 × 混合 fib(22)（kinds=[Interp,Compiled] 互递归，i2c/c2i 并发发生） | ✅ 全部 =17711 |
| B | 跨 tier 原子计数：4 解释线程（`AtomicAdd` 字节码）+ 4 编译线程（`fetch_add`）同一真地址 | ✅ total=400000 |
| C | **阻塞 IO 活性**：guest A 解释帧→编译帧→真 `read(2)` 阻塞；guest B 延时后写 | ✅ A 收到 42 |
| D | 8 线程并发混合栈 unwind（per-thread panic→Drop→catch） | ✅ 逐线程 (777,[102,101,100]) |
| — | **TSan 全量插桩**跑上述全部 | ✅ **零警告** |

三个主张全部坐实：
1. **引擎 Sync、无 GIL**：共享只读程序 lock-free 读 + per-thread 执行态，TSan 判定引擎
   自有状态零竞争。引擎执行路径**一把锁都没有**。
2. **corpus §2.1 收束**：用例 C 与 tier-0 上**挂死**的 `c_blocking_io` 同构——真线程引擎上
   阻塞 syscall 只挡自己那条线程，程序完成。corpus 第一号并发票据在骨架上兑现。
3. **原子 = 宿主原子指令、跨 tier 互操作**：解释线程与编译线程对同一 u64 真地址并发 RMW，
   计数精确——真实地址模型下两个 tier 的原子都是宿主原子，天然互操作。

## 3. 教训与发现

### 3.1 引擎义务（新）：解释器执行 guest 原子必须发真宿主原子指令

tier-0 把 `atomic_*` 实现为普通读写（协作单线程下合法，"原子性由调度保证"）。**真线程引擎
不行**：解释器替 guest 执行原子操作时，若用普通 load/store，那是**引擎自身**在 guest 原子
位置上的数据竞争——TSan 直接抓引擎（不是 guest）。骨架实现：`AtomicU64::from_ptr(真地址)
.fetch_add(SeqCst)`。M4 的 atomic intrinsic 降低必须逐条映射到宿主原子（含 ordering 映射，
骨架只做了 SeqCst）。这是 tier-0 没有、M4 必须有的一条**引擎义务**，本 spike 把它定为正式产出。

### 3.2 状态三分的骨架落地检验了 RFC 的形状

`Shared { prog, kinds }`（发布后只读格）+ `Ctx { shared: *const, region, drop_log }`
（每线程私有格）。两点验证：
- **类型系统层**：Shared 是纯不可变数据 → 自动 Sync → `&Shared` 跨 scoped 线程**编译通过**
  ——"执行相 tcx-free ⇒ 引擎 Sync"（C8）不是口号，是骨架里一行 `sc.spawn(move || …&shared…)`
  能过 borrowck 的事实（骨架根本没有 tcx = 模式 B 运行形态）。
- **运行时层**：TSan 复核零竞争。
- 本 spike 没用到"显式同步"格（无惰性降低/JIT 缓存）——那一格随 M4 的加载相进来。

### 3.3 `Ctx.shared` 用裸指针而非 `&'s`（vmctx 纪律的又一次胜利）

若 `Ctx<'s> { shared: &'s Shared }`，则 `CompiledFn = extern "C-unwind" fn(*mut Ctx<'s>,…)`
要背 HRTB 生命周期（fn 指针 + 不变的 `*mut` = 类型体操）。裸指针 `*const Shared` 一刀切断，
且与 vmctx-passing.md 的纪律一致（编译码经裸指针够到执行态；生存期由 `thread::scope` 保证）。
**vmctx 就是裸指针世界，越早接受越干净。**

### 3.4 TSan 工程路径：rustc_private 迫使 harness 独立

对 mirvm 整体上 TSan 不可行（rustc 动态库未插桩 + jemalloc 与 TSan 分配器拦截冲突）。
`src/vm` 纯 Rust 的红利在此兑现：`#[path]` 同源复用零分叉，独立 crate 全量插桩
（`-Zbuild-std` 连 std 一起，rust-src 组件本就在位）。**M4 保持"引擎核心零 rustc_private"
就能永远保有这条 TSan 通道**——这是把"引擎是 lib"（D10）往前推的一个实际理由。

### 3.5 unwind 机器天然每线程（case D 白拿）

panic/personality/landing pad 本就是 per-thread 机制，加上 drop_log/region 在 per-thread
Ctx 里，8 线程并发 unwind 互不干扰、TSan 静默——**没有为并发 unwind 写一行代码**。
模型 A 的"unwind 走 native 栈"在并发下的正确性是继承来的，不是实现来的。

## 4. 对 M4 的输入

- **4-spike 全过 → 模型 A 地基验证完毕**，M4 正式实现开闸。骨架里可直接沿用的形状：
  Shared/Ctx 分裂、call_guest dispatch、CleanupGuard 协议、字段级瞬态借用纪律、
  "解释器发真宿主原子"义务。
- M4 挂起检查点（已记）：真 Cranelift 数据回访 vmctx 内部约定 + LSDA 发射复核。
- 骨架未覆盖、M4 要补的并发面：并发 TLAB 分配器（mimalloc 结构，已定）、guest 级
  pthread_create→thunk 生成、原子 ordering 全映射（骨架仅 SeqCst）、"显式同步"格
  （惰性降低/JIT 缓存的 insert-once 发布）。

## 5. 复现

```
cargo build --release
./target/release/mirvm spike4        # 功能：4 用例全 PASS
bash tests/spike4_tsan.sh            # TSan 判定：零警告（首次 build-std 约 1-2 分钟）
./target/release/mirvm spike1 && ./target/release/mirvm spike2 && ./target/release/mirvm spike3
MIRVM=$PWD/target/release/mirvm bash tests/diff.sh   # 16/16
```
