# 冷启动调研：源码 → 引擎 IR 加载相解剖（轨 C／M6 片3 前置调研）

2026-07-14。背景：用户裁定 **M5.3（方法级 JIT）Pending**——冷启动（尤其 lower 相）是当前一等问题，
先调研后施工，本轮**只测量与调研，不写产品代码**。本文 = 实测解剖 + rustc-src 先例调研 +
杠杆清单与建议施工顺序（供裁定）。

测量环境：AMD EPYC 7773X（Zen 3），release 二进制，nightly-2026-07-02，页缓存热。
方法与仪器见 §1；30-demo 原始账本见附录 A。

## 0. 结论速览

1. **单文件脚本冷启动 ~430ms 墙钟里，lower 占 ~310ms（72%）**；frontend 仅 15–37ms，engine 多为
   个位数 ms，启动仪式 ~55ms（§2.4）。冷启动的敌人不是 rustc 前端，是我们自己的加载相。
2. **lower 是近常数的"std 税"**：几行代码的 fib 也要急切降低 3027 个 instance。lower 耗时与
   instance 数完全线性——**~0.10ms/instance**（fib 3027→299ms；ecosystem 13209→1272ms）。
3. **钱花在 rustc 机器，不在我方代码**：lower 相 perf 归因 = rustc 查询/interning 27.5% +
   rmeta 解码 10.5% + 内核（页错误/清零，是前两者的代谢）34.9% + malloc 9.6%，
   **mirvm 自身发射代码仅 6.2%**。优化发射代码无意义；要么少碰 rustc（V3/V4），要么并行碰（V5）。
4. **执行集 ≪ 降低集**（懒降低的定量依据）：fib 实际执行 230/3027（7.6%），ecosystem
   3840/13209（29%）——急切降低做了 **3–13× 的多余工作**（§2.3 表）。
5. **native 对照组**：rustc 全编译+链接同文件只要 108–202ms（其中 link ~60ms、LLVM codegen ~25ms）。
   native 赢在**预编译 std**——它只补用户增量；mirvm 每次冷跑从 rmeta 里重造整个 std 闭包。
6. **cargo 形态全冷 10.6s = 7.7s 依赖构建 + 2.9s runner**。依赖今天跑的是**完整 codegen+link**
   （`ar t` 实证每个 target rlib 都带 `.rcgu.o`），而 mirvm 只消费其中的 rmeta MIR——目标码纯白烧。
   cargo-miri 有现成剪枝先例（§4.1）。
7. **L2 复核有效**：ecosystem 干净环境下 store 45ms / warm 命中 cache-load 97ms（total 3.0s→1.5s，
   与 M6 片2 账本一致）。但调研顺带抓到三个此前不可见的真问题（§5）：runner 环境化石化、
   空 stub `.d` 让假二进制永不重录、diff_cargo 无 warm 复跑维度。
8. 杠杆排序（§6/§7）：小件 V1（sysroot 仪式 stamp 化，每跑 −40~55ms）与 V2（依赖 codegen 剪枝，
   先例两路线见 §4.1）可先行；**决定性杠杆是 V3 懒降低**（脚本 lower 300→~40ms 量级），但它与
   M5.3 JIT 的"首调触发 per-fn 物化"是同一套机制，应一次定型（ABI 一次定型的教训）；V4 std
   预降低底座吃 V3 的机制红利，与模式 B 同一条设计线。rustc-src 调研另有两个**证伪**：
   `-Zmir-opt-level=0` 非杠杆（§4.5）、`-Cincremental` 有 finalize 坑且脚本路径收益小（§4.4）。

## 1. 测量方法与仪器

- 相位账本：`MIRVM_TIMING=1`（M6 片1），`MIRVM_NO_IR_CACHE=1` 保证冷路径；每 demo 冷跑两次取快者。
- perf：`perf record -F 1999`（sudo 临时 `perf_event_paranoid=1`，测后还原 4）；release 二进制自带
  符号表，按 DSO/符号正则聚成桶。
- **执行集计数**：interp 帧入口临时探针（`MIRVM_EXEC_PROBE` 门控计 distinct FuncId）——
  量完即 `git checkout` 回滚，未提交，不属于测试基建。
- **store 拒因探针**（`MIRVM_CACHE_DEBUG`）同上，已回滚。
- frontend 等价复现：mirvm 不透传 `-Z` 旗标，用同 sysroot/同 edition 的裸
  `rustc --emit=metadata -Ztime-passes` 复现到 after_analysis 为止的等价工作。
- 复原确认：探针回滚、污染缓存清洗后 diff_cargo 5/5 全绿；工作树 pristine。

## 2. 单文件形态解剖

### 2.1 相位分布（30 个非 cargo demo）

| 相位 | min | 中位 | max | 备注 |
|---|---|---|---|---|
| frontend | 15ms | 20ms | 37ms | parse+expand+typeck+borrowck；最大单 pass（typeck）4ms |
| **lower** | **297ms** | **312ms** | **354ms** | **近常数——std 税；用户代码只贡献边际量** |
| engine | 1.7ms | ~5ms | 91ms | 尾部 = recursion_deep 91 / threads_time 86（含 sleep）/ time_fs 74 |
| total（进程内） | 330ms | 356ms | 468ms | |
| wall（墙钟） | 398ms | 428ms | 546ms | wall−total ≈ 65–75ms = 启动仪式（§2.4） |
| 对照：rustc 全编译 | 108ms | 131ms | 202ms | **native 编译+链接比我们的 frontend+lower 快 ~2.5×** |

### 2.2 lower 相 perf 归因（fib 冷跑 ×5，4994 样本）

| 桶 | 占比 | 内容 |
|---|---|---|
| kernel | 34.9% | 页错误/页清零/mmap——查询与解码的内存代谢 |
| rustc 查询/ty | 27.5% | intern_ty、查询缓存、layout、const eval |
| rmeta 解码 | 10.5% | decode_span、Ty/MIR Body Decodable（std 全量 MIR 逐体解出） |
| malloc/memcpy | 9.6% | |
| **mirvm lower 本体** | **6.2%** | 发射/重定位/frame 布局（layout_of 包装 1.3% 为最大单项） |
| 符号名计算 | 2.6% | v0 mangling print_def_path（exported_defs 表 + foreign 解析） |
| frontend passes | 2.7% | |

ecosystem 同形状（engine 43% 之外：查询/ty 21.8%、kernel 13.2%、rmeta 3.5%、mirvm-lower 2.5%）。

### 2.3 频次结构：降低集 vs 静态可达 vs 实际执行

| demo | 急切降低 | 静态可达（stats BFS 经 Call 边） | **实际执行**（探针） | 执行/降低 |
|---|---|---|---|---|
| fib | 3027 | 361 | **230** | 7.6% |
| strings | 3135 | — | 323 | 10.3% |
| hashmap | 3291 | — | 370 | 11.2% |
| threads_channel | 3467 | 634 | 493 | 14.2% |
| time_fs | 3067 | — | 361 | 11.8% |
| ecosystem | 13209 | — | 3840 | 29.1% |

线性模型：**T_lower ≈ 0.10ms × N_lowered**（fib 0.099、ecosystem 0.096，惊人一致）。
频次是主项：把 N 从"闭包全集"降到"执行集"= 7–13×（脚本）/ 3.4×（生态）的削减上界。

### 2.4 启动仪式（wall − total ≈ 55–75ms，**warm 也在付**）

- 裸进程（`mirvm` 无参数）：13ms——exec + librustc_driver 动态链接的地板。
- `ensure_sysroot()` **每次运行** spawn 两个子进程：`rustc --print sysroot`（15ms）+
  `rustc -vV`（13ms，rustc_version），再加 rustc-build-sysroot 的哈希/stat 检查 ≈ 合计 40–55ms。
- 实测 warm 命中 fib：墙钟 85ms，进程内仅 31ms（cache-load 29.8 + engine 1.4）——
  **UX 上一半以上的 warm 延迟是仪式**。修法显然（stamp 化，V1）。

### 2.5 native 对照组解剖（`-Ztime-passes`，fib）

全编译 105ms：link_crate 60ms、codegen+LLVM 25ms、mono collector 走图 10ms、前端 ~20ms。
native 的"std 税"在安装时一次付清（预编译 rlib 目标码），每次编译只付用户增量 + 链接。
这正是 V4（std 预降低底座）对映的物理事实。

## 3. cargo 形态解剖（ecosystem：rand + regex + serde_json 及传递闭包）

| 段 | 耗时 | 构成 |
|---|---|---|
| 全冷墙钟 | **10.6s** | 材料化 + cargo 解析 + 依赖构建 + runner |
| └ 依赖构建 | **~7.7s** | 22 个 target rlib + host 侧 proc-macro（syn/quote/proc_macro2） |
| └ runner 会话 | 2.9s | frontend 72ms / **lower 1272ms** / engine 1514ms |
| 无重建复跑 | 3.0s | cargo 指纹检查 ~150ms + runner 2.9s |
| L2 warm 命中 | **1.65s** | cache-load 97ms + engine 1.4s（engine = M5.3 的地盘） |

**依赖构建的白烧实证**：wrapper 对 target 依赖原样透传 cargo 的
`--emit=dep-info,metadata,link`（只加 `--sysroot` + `-Zalways-encode-mir`，cargo_shim.rs:214-220），
每个 rlib 都含 `.rcgu.o` 目标码（`ar t libunicode_ident-*.rlib` 实证），合计 ~100MB。
mirvm 只消费 rmeta 里的 MIR。**LLVM codegen + 归档对 mirvm 是纯废热**——除 proc-macro/build.rs
（host 侧，真执行，必须真编译）。剪枝先例见 §4.1。

## 4. rustc-src 先例调研

来源纪律：Q3–Q7 直接取自 pinned 源树（`~/.rustup/toolchains/nightly-2026-07-02-*/lib/rustlib/
rustc-src/rust/compiler`，file:line 精确）；**miri 不在 pinned rustc-src 里**（只随
compiler/library 发行），Q1/Q2 取自上游 rust-lang/rust@`4c9d2bfe`（本 nightly 的精确
commit，`rustc -vV` 核对），行号近似——施工前需按该 commit 复核原文。

### 4.1 cargo-miri 的依赖构建剪枝（V2 的先例）

**不是 `--emit` 手术，是空转 codegen backend**：miri 以 in-process rustc driver 接管 target
依赖编译（`MiriDepCompilerCalls::after_analysis`），dummy backend 不产目标码，但仍执行
`let _ = tcx.collect_and_partition_mono_items(());`（注释：复现常规构建的 post-mono
const-eval 报错面）；产物写 stub 满足 cargo。MIR 进 rmeta 靠 `-Zalways-encode-mir`
（MIRI_DEFAULT_ARGS）。

**pinned 树侧的机制依据**（rustc_metadata/src/rmeta/encoder.rs:1087,1120-1124，
`should_encode_mir`）：check 型构建默认**跳过** optimized_mir 编码，除非
`always_encode_mir`——所以 metadata-only 路线（`--emit=dep-info,metadata`，即 cargo check
形态）配上我们已在传的 `-Zalways-encode-mir` 同样能拿到全量 MIR。

mirvm 的两条施工路线（构建期裁定）：
- **A（miri 同构）**：wrapper 对 target 依赖改走 in-process driver + 空 backend——保留
  post-mono const-eval 报错面，改动大（phase_wrapper 现在是 exec 真 rustc）；
- **B（check 同构）**：wrapper 改写 `--emit=dep-info,metadata`——改动一行级，风险面 =
  cargo 对产物存在性的校验（cargo 自身的 pipelining/check 已消化 rmeta-only 依赖）+
  丢失依赖 crate 的 post-mono 报错（本就有 runner 会话兜底）。

### 4.2 miri 的会话旗标（MIRI_DEFAULT_ARGS）

`-Zalways-encode-mir` + `-Zmir-opt-level=0` + 关 CheckAlignment/CheckNull/CheckEnums
（miri 自己做 UB 检查、要更好诊断）+ 关 ReferencePropagation/GVN（别名模型不兼容）。
mirvm 的取舍不同：我们对齐 native 语义而非 UB 侦测，check* pass 应保留（native 同款）。

### 4.3 `-Zthreads` 并行前端（V5 可行性）

- rustc_session/src/options.rs:2778：默认 None（顺序编译器行为）；`=0` →
  `available_parallelism()`；**`=1` 塌缩回 None**（options.rs:1095-1112，仅 >1 启用同步）；
  上限 256。
- **运行期开关，发行 dylib 内建**：rustc_interface/src/interface.rs:388-390
  `set_dyn_thread_safe_mode(threads.is_some())` + rustc_data_structures/src/sync.rs:73-102
  `DYN_THREAD_SAFE_MODE`——不是编译期 cfg。rustc_private 嵌入方直接传 `-Zthreads=N` 即可。
- 风险：`-Z` 不稳定面；util.rs:223-244 死锁处理器 = **abort 进程**（查询环未解出时）。
- 对 mirvm 的含义：`-Zthreads>1` 使 tcx DynSync → 我方 worklist 才有并行化的前提；
  只传旗标不改 lower 仅提速 frontend（15–72ms，边际）。

### 4.4 `-Cincremental`（考察结论：机制可用但有坑，脚本路径非杠杆）

- red-green 复用机制齐全（rustc_incremental/src/persist/{load,save}.rs）；
  `TyCtxt::finish()` 在 after_analysis 返回 Stop 后仍执行（rustc_interface/src/passes.rs:1046）
  → staging dep-graph + 查询缓存**会写**。
- **坑**：会话目录 finalize（`s-*-working` → `s-*-{SVH}` rename）只在 codegen/link 路径调
  （rustc_interface/src/queries.rs:115）；下次加载只认 finalized 目录（fs.rs:529,551-552）
  → **纯 after_analysis 运行写了缓存却永不复用**，除非 mirvm 自己调
  `rustc_incremental::finalize_session_directory`（pub，persist/mod.rs:13）。
- 收益评估：脚本路径 frontend 仅 15–37ms，增量能省的就这一段的一部分（std 体解码属
  extern rmeta，不走本地增量缓存）——**非当前杠杆**；未来"大用户 crate 编辑-重跑"场景再启。

### 4.5 mir-opt-level（考察结论：非杠杆，假设证伪）

- 默认（session.rs:619-624）：`-Copt-level=0` ⇒ **mir_opt_level=1**；优化构建 ⇒ 2。
- 重 pass（Inline/GVN/SROA/JumpThreading/DestProp/RefProp…）全部 gate 在 **≥2**——
  debug 默认下本来就不跑；level 1 仅剩廉价 CFG/local 清理（InstSimplify/SimplifyCfg/
  CopyProp 等）。
- `optimized_mir` 的大头 `mir_drops_elaborated_and_const_checked`（drop 展开+const 检查，
  lib.rs:813-847）**不受 mir_opt_level 控制**，无法跳过。
- 结论：`-Zmir-opt-level=0` 至多省掉 level-1 清理 pass 的零头，且让引擎吃更啰嗦的 MIR
  （解释更慢）——从杠杆表剔除。

### 4.6 rmeta MIR 惰性解码（线性模型的源码依据）

- extern optimized_mir 提供者 = LazyTable 逐 DefId O(1) 定位 + 单体解码
  （rmeta/decoder/cstore_impl.rs:239 + rmeta/table.rs:519-537）；元数据 **mmap**
  （locator.rs:935-951，"only a small fraction of it is read"）；每 rlib 急切解码面有界
  （CrateRoot/trait_impls/def_path_hash_map 等头部，decoder.rs:91-150）。
- 含义：**加载成本 ∝ 触达的 DefId 集**，不 ∝ std/依赖总量——§2.3 的 0.10ms/instance
  线性模型有源码级解释；V3 懒降低把"触达集"从闭包全集缩到执行集时，rmeta 解码、
  查询、interning 全链条一起缩。

（`-Ztime-passes`/`-Zself-profile` 本 nightly 均在：options.rs:2782/2704——本轮已用前者。）

## 5. 调研顺带抓到的三个真问题（记录在案，非本轮修）

**P1 runner 环境化石化**：wrapper 把**整份构建期环境**录进假二进制 JSON
（`write_fake_outputs` 的 `env: std::env::vars().collect()`），runner 回放时逐个 `set_var`。
编译语义变量（`env!()`/CARGO_*）确应取录制值，但 **MIRVM_\* 引擎控制旋钮被一并化石化**：
实证——测量期的 `MIRVM_NO_IR_CACHE=1` 被录入，此后每次运行（不带该变量）都被回放重设，
**L2 对该项目永久旁路**，且 stderr 无任何迹象。同理 MIRVM_TIMING 化石化会永久污染 stderr
（破坏 native 差分）。修法：runner 回放时跳过 MIRVM_\* 前缀（或白名单编译语义变量）。

**P2 空 stub `.d` 让化石永生**：write_fake_outputs 写空 dep-info（原意：阻止 cargo 每次重建），
副作用是 cargo 没有 bin 的源文件清单——**源码编辑永不触发假二进制重录**，录制环境/参数只随
Cargo.toml/参数变化更新。单独看无害（runner 每次从磁盘现读源码，L2 清单也盖真源文件的戳），
但与 P1 复合 = 化石洗不掉。修法：stub `.d` 如实列出源文件（rustc `--emit=dep-info` 或按 targets 推导）。

**P3 diff_cargo 无 L2 warm 复跑维度**：M6 片2 给 diff.sh 加了 warm 一致性校验，cargo 形态没加——
runner 缓存路径零 gate 覆盖（本次污染没有任何 gate 能抓到）。修法：diff_cargo 补第二跑对比
（既有 gate 通道的维度扩展，符合基建预算纪律）。

## 6. 杠杆清单

| # | 杠杆 | 机制 | 实测依据 → 预估收益 | 成本/风险 |
|---|---|---|---|---|
| V1 | sysroot 仪式 stamp 化 | 首次检查后落 stamp（键 = toolchain 路径+版本+build_id），后续跑读 stamp 免 spawn | §2.4：**每跑 −40~55ms，冷热皆得**；warm fib 墙钟 85→~45ms | 小；stamp 失效面（toolchain 原地升级）用 rustc 二进制 (size,mtime) 盖戳兜住 |
| V2 | 依赖 codegen 剪枝（=D9d） | 两路线（§4.1）：A miri 同构（in-process driver+空 backend，保 post-mono 报错面）/ B check 同构（wrapper 改写 `--emit=dep-info,metadata`，一行级）；proc-macro/build.rs 不动 | §3：7.7s 依赖构建中砍掉 LLVM+link 份额（估 **−40~60%**，施工时以 cargo --timings 复核）。**施工实测（S2，2026-07-14）：预估被证伪**——wall ≈0（128 核上依赖 codegen 本在关键路径外并行），真实收益 = CPU −12%（27.3→24.0 CPU 秒）、磁盘 −60%（target rlib 100→40MB）、少 22 次 exec；低核机器 wall 收益按 CPU 差推算 | 中；B 路线风险 = cargo 产物校验面 + 依赖 crate post-mono 报错推迟到 runner 会话 |
| V3 | **懒降低**（per-fn lazy lower） | 急切只降入口链；其余 FuncId 挂 lower-stub，首调触发单函数降低（Trap-stub 协议的能力延伸） | §2.3：N 从闭包全集→执行集，**脚本 lower 300→~40ms 量级、ecosystem 1272→~430ms**（+首调延迟摊入运行期） | 大；vtable/fn-ptr 槽需 stub 化；L2 语义变为"运行后累积快照"；**与 M5.3 JIT 首调物化是同一套管线，应一次定型**。**终裁（2026-07-15，m5.3-design J2）：不建**——顶撞 D3 tcx 边界与 L2 洁净快照契约，S4 后脚本收益仅余 ~25ms；标的改由 S3′ 依赖成像吃掉（重启触发器见 m5.3-design §3.1） |
| V4 | std 预降低底座（base image） | 程序无关的 std 闭包一次降低成共享底座（L2 分层：base+delta；JVM CDS base archive 对映），冷跑只降用户增量 | §2.2/2.5：native 的"安装时付清 std 税"同构；冷 lower → 用户增量成本 | 大；跨模块 FuncId 链接/去重/固定基址分区设计；与模式 B（.mirvm 包）同一条设计线 |
| V5 | 并行 lower | `-Zthreads>1` 使 tcx DynSync（运行期开关已证实，§4.3），我方 worklist 再 rayon 化 | §2.2：加载相 CPU 密集 → 理论 ÷ 核数；EPYC 128 核环境收益显著 | 中；`-Z` 不稳定面 + 死锁处理 = abort + 我方 Linker 状态（ids/frozen/queue）分片设计 |
| V6 | cache-load 零拷贝 | mmap + 惰性逐函数解码（配 V3 自然获得） | fib 30ms / eco 97ms 的反序列化 → 常数级 | 小-中；postcard 整包 → 分段布局 |
| — | engine 1.4–1.5s | M5.3 方法级 JIT | 本轮不动（用户已裁定 Pending，等开工令） | — |

相互作用：V3/V4 都砍频次——V4 砍得最干净（std 税→0）但工程最大；V3 把 L2 从"洁净快照"
变成"累积剖面缓存"，语义上更接近 JIT 世界，两者应与 M5.3 分层设计一次对齐。V1/V2/P 修缮
独立小件，不与任何大件耦合。V5 与 V3 相互削弱（懒降低后可并行的量变小），二选一或 V3 优先。

## 7. 建议施工顺序（供裁定）

1. **S1 小件包**：V1 仪式 stamp + P1/P2/P3 修缮（含 diff_cargo warm 维度）。半天级，
   当下每一跑都受益，且把本次发现的缓存盲区上锁。
2. **S2 = V2 依赖剪枝**：cargo-miri 同构改 wrapper emit，ecosystem 全冷 10.6s→估 5–7s。
   独立于引擎，风险面已有先例趟路。
3. **S3 = V3 懒降低**：脚本冷启动的决定性杠杆——但**先与 M5.3 JIT 出联合分层设计**
   （解释 tier 的 lower-stub 与 JIT tier 的首调编译是同一根首调触发管线），再动工。
4. **S4 = V4 底座**：吃 S3 的机制红利，与模式 B/.mirvm 包同期设计（M5.3 后，D9f④ 原位）。

## 附录 A：30-demo 冷启动账本（release，冷跑两次取快者，ms）

| demo | frontend | lower | engine | total | wall | rustc 对照 |
|---|---|---|---|---|---|---|
| args_env | 16.9 | 310.7 | 5.0 | 346.6 | 420 | 131 |
| asm_extras_probe | 14.6 | 309.4 | 3.3 | 344.0 | 407 | 120 |
| asm_probe | 19.8 | 299.2 | 1.7 | 335.3 | 406 | 150 |
| async_hand | 19.5 | 303.2 | 2.9 | 340.2 | 410 | 128 |
| async_suspend | 20.9 | 314.6 | 4.3 | 355.8 | 438 | 138 |
| atomic_order_probe | 23.1 | 319.0 | 11.4 | 368.4 | 439 | 172 |
| catch | 17.9 | 319.4 | 5.7 | 357.7 | 425 | 113 |
| ffi_libc | 15.3 | 308.1 | 3.1 | 341.4 | 407 | 111 |
| fib | 15.7 | 299.0 | 3.1 | 330.6 | 398 | 124 |
| float_wide_probe | 24.6 | 317.3 | 7.3 | 362.9 | 431 | 141 |
| fork_exec_probe | 19.2 | 306.7 | 22.0 | 362.1 | 433 | 129 |
| hashmap | 19.6 | 329.3 | 3.8 | 367.6 | 441 | 190 |
| intrinsic_probe | 28.4 | 320.8 | 6.3 | 369.4 | 440 | 135 |
| nested_dst_probe | 24.2 | 298.7 | 2.3 | 339.0 | 406 | 129 |
| panic_exit | 15.1 | 303.2 | 1.9 | 333.7 | 403 | 111 |
| ptr_int | 18.9 | 307.2 | 3.0 | 346.2 | 414 | 121 |
| recursion_deep | 19.4 | 313.9 | 90.8 | 440.4 | 505 | 148 |
| signal_probe | 17.4 | 310.6 | 3.7 | 346.1 | 413 | 108 |
| simd_probe | 37.1 | 329.2 | 15.8 | 397.9 | 465 | 191 |
| strings | 21.0 | 320.7 | 4.2 | 360.6 | 450 | 167 |
| threads_channel | 24.0 | 354.4 | 5.6 | 400.8 | 475 | 202 |
| threads_panic | 26.1 | 333.0 | 5.0 | 379.2 | 467 | 178 |
| threads_spawn | 23.4 | 344.1 | 9.6 | 397.9 | 472 | 189 |
| threads_sync | 23.6 | 318.9 | 17.6 | 374.7 | 451 | 183 |
| threads_time | 23.5 | 342.8 | 85.7 | 467.9 | 546 | 172 |
| time_fs | 19.9 | 304.8 | 74.1 | 415.7 | 487 | 136 |
| track_caller_fn_ptr | 14.6 | 312.5 | 4.3 | 346.4 | 421 | 114 |
| u128_switch | 16.3 | 297.4 | 2.7 | 329.6 | 400 | 108 |
| volatile_wide | 22.3 | 308.9 | 1.8 | 347.7 | 418 | 122 |
| wide_int_probe | 16.9 | 319.9 | 4.5 | 356.5 | 428 | 111 |
