# 并行前端调研：rustc `-Zthreads` 上游进展 + V5 改动面评估与施工路径（D7 研判）

2026-09-19。背景：D7 挂账两件事——frontend 相成本（eco ~143ms / ripgrep ~730-800ms）
无杠杆认领；V5（`-Zthreads` 并行 lower）在 V3 不建后无人重启。本文四件事：
①上游并行前端最新进展调研（网络来源，逐条带链接）；②在当前代码库上的改动面评估；
③**设计取舍总表**（每个决策点：选项/建议/理由/代价）；④**分阶段施工路径**
（每步带验收口径与回退面）。只调研与评估，不含产品代码改动。

上游事实截止 2026-09-19；代码事实按当日工作树核对（file:line 会漂移，施工前复核）。
既有本地调研（[coldstart-research.md](coldstart-research.md) §4.3：运行期开关证实、
`=1` 塌缩 None、上限 256、死锁处理器 = abort）仍然有效，本文不重复其论证。

## 0. 结论速览

1. **上游在 2026 年明显提速**：稳定化策略 MCP 已被 compiler team 接受，最后一批已知
   死锁与常见 crash 已修，并行 UI 测试套件已在上游 CI 强制运行，`--jobs` 族选项已合并，
   **"nightly 默认 2 线程前端"的 PR 正在评审（2026-09-16 提交）**。稳定版预期 2027。
2. **对 mirvm 立即可用**：`-Zthreads` 是发行 dylib 内建的运行期开关，嵌入方传旗标即可；
   pinned `nightly-2026-07-02` 晚于两大正确性修复（#143035 死锁、#151509 增量竞争）的
   合并时点（施工前按 pinned 源树复核在位）。
3. **`-Zthreads` 只加速 analysis 相（typeck/borrowck 逐体并行），不加速我方 lower**：
   我方串行 drain 里的查询调用不会因此并行，反而要付 DynSync 分片锁开销（量级待实测，
   有反向风险）。它的主战场是 ripgrep 类大 crate 的 730-800ms frontend 相；
   脚本路径（frontend 中位 20ms）收益边际。
4. **改动面分三轴**（§3，取舍见 §4，施工见 §5）：
   A（`-Zthreads` 注入）= 单 helper + 4 站点 seam + 键外注入 + 确定性 gate，天级；
   B（并行预取，V5-lite）= rustc 池原语预热查询缓存，Linker 零改动，攻 lower 的
   ~75-80% 查询/解码份额，天-周级；
   C（V5 完全体，全并行 lower）= 符号化发射 + 波前确定性合并，周-月级，
   **仅当 B 落地后 profile 证明串行残余仍是账面主项才立项**（Amdahl：B 后残余 ~25%）。
5. **反向风险先于收益到达**：上游 #162848 合并后，bump pin 会让所有进程内会话
   **被动**变 2 线程——底座/deps image 字节确定性与 stderr 差分可能无预警变红。
   §5 阶段 1 的"恒注入、默认钉 1"设计顺带把这个口子焊死，与是否启用并行无关，
   **建议无论 V5 是否立项都先做阶段 1 的防御半件**。

## 1. 上游进展（截至 2026-09-19）

### 1.1 组织与路线

- 总追踪 [rust#113349](https://github.com/rust-lang/rust/issues/113349)（仍 open）；
  月报改发 [goals#121](https://github.com/rust-lang/goals/issues/121)，讨论在 Zulip
  `#t-compiler/wg-parallel-rustc`。
- 项目目标从 SparrowLii 交棒 **petrochenkov**（[2026 目标](https://goals.rust-lang.org/2026/parallel-front-end.html)，
  "Fast Builds" 旗舰路线）；wg-parallel-rustc 重建为正式 working area。
- **稳定化策略 MCP [compiler-team#1005](https://github.com/rust-lang/compiler-team/issues/1005)
  于 2026-07-08 被接受**（major-change-accepted）。分阶段：并行 UI 套件上 CI →
  稳定 `-j/--jobs`（限制全部并行度，配 jobserver）→ `--deterministic`/`--reproducible`
  显式开关（初版实现 = 关前端并行）→ **nightly 默认开启（限 2 线程）收集反馈 ≥3 个月** →
  稳定化 RFC/FCP。按此节奏稳定版最快 2027。

### 1.2 正确性与测试（能不能用）

- 最后一批已知死锁由 [#143035](https://github.com/rust-lang/rust/pull/143035) 修掉
  （work-stealing 只在主循环做；rustc-rayon 并入 rust 树成 `rustc_thread_pool`）。
- 最后一类常见 crash（增量编译 dep-graph 节点 green/red 并发着色竞争）由
  [#151509](https://github.com/rust-lang/rust/pull/151509) 修掉。
- MCP 原文：**没有任何"并行模式编译结果错误"的已知 issue**；剩余 4 个 ICE 全部只在
  query cycle 错误之后触发（mirvm 会话遇 query cycle 本就是编译错误停机路径）。
- 整个 UI 套件（2 万+ 程序）支持并行模式运行（[#153801](https://github.com/rust-lang/rust/pull/153801)），
  上游 CI 先非阻塞（[#158307](https://github.com/rust-lang/rust/pull/158307)）后
  **blocking**（[#159833](https://github.com/rust-lang/rust/pull/159833)）；
  历史 issue 的复现测试批量入库（[#154354](https://github.com/rust-lang/rust/pull/154354)）。

### 1.3 选项与默认值（2026-08/09 动态）

- `-j/--jobs` 及 `--jobs-frontend/-backend/-linker` 已合并
  （[#159675](https://github.com/rust-lang/rust/pull/159675)、
  [#160697](https://github.com/rust-lang/rust/pull/160697) 2026-09-16 合并，milestone 1.100）：
  `--jobs-frontend=N` = 精确 N 线程，`--jobs=N` = 默认线程数但不超过 N。
- **[#162848](https://github.com/rust-lang/rust/pull/162848) "Use 2 parallel frontend
  threads by default on the nightly and dev channel"**（Kobzol，2026-09-16 提交，
  评审中）——对 mirvm 是被动风险源（§6 bump 检查单）。
- 可复现性修复持续：并行下 DefId 生成序
  （[#162580](https://github.com/rust-lang/rust/pull/162580) 已合、
  [#162555](https://github.com/rust-lang/rust/pull/162555) 在审，根 issue
  [#162202](https://github.com/rust-lang/rust/issues/162202)）。上游立场：可复现性
  **不阻塞稳定化**，用 `--reproducible` 显式开关兜底（初版 = 关前端并行）。

### 1.4 性能面（对预期的校准）

- 上游口径：整体编译墙钟约 **20~30%** 缩减（8 线程量级）；>16 线程数据竞争明显。
- **parsing / 宏展开 / name resolution 仍串行**，列在 2026 目标的远期工作；并行化的
  是 query 化的 analysis（typeck/borrowck/MIR building 逐体 `par_hir_body_owners`）。
  borrowck 约束收集并行化有人在试（[#162485](https://github.com/rust-lang/rust/pull/162485)，
  实验性 draft）。
- 含义：mirvm 的 `after_analysis` 形态（无 codegen）里 analysis 占比高于普通编译，
  `-Zthreads` 对 frontend 相的相对收益**可能高于**上游整编译口径——待实测。

## 2. mirvm 侧现状重述（与 coldstart 时代的账目差异）

S4 底座 + S3′b 依赖成像 + L2 之后，lower 的 std/deps 大头已被 image 化吃掉
（脚本纯冷 385→104ms；eco 冷 924→热 66ms）。当前挂账（D7）：

| 段 | 量级 | `-Zthreads`（轴 A）可及性 | V5（轴 B/C）可及性 |
|---|---|---|---|
| 大 crate runner frontend 相 | ripgrep ~730-800ms / eco ~143ms | **主战场**（analysis 逐体并行） | 不可及 |
| 脚本 frontend 相 | 15-37ms | 边际，且付线程池起建 | 不可及 |
| 冷跑 delta lower（bin 自身 mono 闭包） | 随 bin 体量；eco 时代 ~0.10ms/instance | **不可及**（串行 drain 不受益，反付锁开销） | 主战场 |
| 一次性 image 构建会话（base/deps/L2 miss） | 一次性，300ms~秒级 | analysis 份额可及 | lower 份额可及 |
| cargoless dep 单元编译 | 已按 crate 图子进程并行 | 单 crate 内可叠加（超订，T4） | 不可及 |

lower 相成本结构（coldstart §2.2，量的分布至今无理由改变）：rustc 查询/interning
27.5% + rmeta 解码 10.5% + 内核页错误（前两者代谢）34.9% + malloc 9.6% =
**~75-80% 是逐 instance 的只读查询侧**；mirvm 发射本体仅 6.2% + 符号名 2.6%。
这个结构决定了 B（并行预取查询）吃大头、C（并行发射）只吃零头。
lower 的查询面已核对为三族：`tcx.instance_mir`（rmeta 逐体解码，func/mod.rs:1726）、
`tcx.layout_of`（frame.rs:56 起全发射路径高频）、`tcx.symbol_name`（linker 全域）。

## 3. 改动面评估

### 3.1 轴 A：`-Zthreads` 注入（不动 lower）

**机制**（coldstart §4.3 已证）：运行期开关，`rustc_args` 追加 `-Zthreads=N` 即可；
`=1` 塌缩回顺序模式（因此"恒注入、默认 1"在当前 pin 上是语义空转，恰好构成
对 #162848 的前瞻免疫——见 T2）。

**a) 注入 seam 的精确时序**。`run_driver`（cli/driver.rs:625）的执行序：
① DiagnosticRouter 起 → ② `baseimage::ensure()` + `depsimage::try_load(&rustc_args)`
（键 = extern 路径内容戳，:662）→ ③ `ircache::lookup(&rustc_args)`（L2 warm 直跑 VM，
不进 rustc，:672）→ ④ `callbacks.rustc_args = rustc_args.clone()`（L2/pack 键材料快照，
:732）→ ⑤ `run_compiler(&rustc_args)`（:741）。
**注入点 = ④ 与 ⑤ 之间**：给 ⑤ 一个追加了 `-Zthreads` 的独立副本。①②③④ 全部
用干净 args，键、deps 预键、header 回放比对天然不受污染；after_analysis 内
`ircache::store`/`depsimage::store_and_wrap` 走 `self.rustc_args`（干净快照），闭环。
四个站点同构：`run_driver`、`pack_driver`（driver.rs:46）、`run_dep_compiler`
（cargo.rs:219，恒钉 1，见 T4）、base build（baseimage.rs:521）。

**b) 线程安全审计结论（已逐点核对，均无需改动）**：
- `TRACK_DIAGNOSTIC` 双钩子链（driver.rs:63-164）：`AtomicRef` + 无状态函数 +
  `SESSION_WARNINGS` 原子计数；rustc 侧 emit 在 `DiagCtxt` 锁内。计数语义
  （告警拒 L2）与序无关。
- 诊断 router（diagnostics.rs:28 `ACTIVE: Mutex`）与 capture emitter（装入 psess，
  DiagCtxt 内序列化）：并发安全。
- `Callbacks` 本就要求 `Send`；callback 在 rustc 池线程上跑，lower 内 dlopen/cc
  子进程调用与线程无关。引擎相在 `run_compiler` 返回（池已 join）后启动，零交互。
- cargoless 调度并行是**子进程级**（driver/build.rs spawn self_exe），与进程内
  `COMPILER_SESSION` 互斥（cli/mod.rs:23）不冲突。
- P1（runner 环境化石化）已修（S1b：runner 不回放 `MIRVM_*`），`MIRVM_THREADS`
  不会被化石化进假二进制。

**c) 键/指纹交互（按 a) 的时序天然解决，但要立为纪律）**：`ircache` 键 =
`fnv(MIRVM_BUILD_ID, rustc_args)` 且 header 全量回放比对（ircache.rs:8,79-81）；
pack 亦存 args（pack.rs:583）；cargoless unit 指纹含完整 rustc 参数。三处纪律：
**`-Zthreads` 永不进入任何键/指纹/回放材料**——注入只发生在 `run_compiler` 前的
独立副本上。dep 单元若未来注入（T4 重评后），同样走 `run_dep_compiler` 内部 seam
（args 已被调度器指纹消费完毕），不动 `dep_rustc_args`。

**d) 确定性契约（本项目特有硬约束，上游明说不修）**。

风险机理：analysis 相并行后，const-eval/查询执行序非确定 → `AllocId` 数值、新造
DefId 序随跑漂移。我方串行 drain 的 id/地址分配序不变（键是 `Instance`/符号名，
稳定），但任何**按 `FxHashMap<AllocId,_>`/DefIndex 迭代序影响输出字节**的路径都会
暴露（S4 验收当年恰好抓过一只 HashMap 随机序缺陷，同族风险）。

腐蚀窗口的精确形态（为什么这不是"美观问题"）：
- base image 键 = `fnv(build_id, sysroot stamp)`（baseimage.rs:12），**不含构建产物
  字节**；deps image 键 = `fnv(build_id, base key, extern 内容戳)`（depsimage.rs:107），
  同样不含产物字节。L2 delta/ir 条目通过**键链**引用这两层，且函数体里烤的是 image
  **绝对地址**（固定域样条）。
- 于是"**同键异字节重建**"= 键链全绿但地址错位：image 文件被删（purge/手工）后重建，
  若并行导致函数序/物化序漂移，旧 ir 条目烤的 image 地址指向新 image 的错误函数——
  验证器只验数量前缀（`verify::Prefix`），**捕不住**。串行构建的字节确定性
  （S4 幂等验收）正是今天关闭这个窗口的机制，并行化必须原样保住它。
- 结论：**base/deps 构建会话的字节确定性是键链正确性的前提**，不是洁癖。注意
  deps image 构建发生在 runner 会话内（split lower），所以"只给 runner 开并行"
  也踩这个面——阶段划分因此把"默认开启"押后到字节 gate 收口之后（§5 阶段 2）。

验收口径（现成 gate 复用）：底座构建双跑字节比对（S4 幂等纪律）+ deps image
双跑字节比对 + L2 store/load 双跑 + diff/diff_cargo warm 维度，全部在
`MIRVM_THREADS=8` 下加跑。漂移处置见 T5。

stderr 逐字节差分：并行下诊断**顺序**非确定（上游 5-10% UI 测试因此重排）。
mirvm 三维差分 oracle 主体是 guest stdout/stderr（与编译诊断无关）；告警程序本就
拒缓存。风险收敛为"编译期告警文案参与比对"的少数夹具——开启前 grep 排查一次。

**e) 资源预算**：cargoless worker 池（≈ncpu）× 每单元 `-Zthreads=N` = 超订。
pinned nightly 无 `--jobs`（#159675 在 pin 之后），jobserver 未接。所以本轮只对
**关键路径单会话**（runner 主会话、base build）开，dep 单元恒钉 1；
bump 到含 `--jobs` 的 nightly 后随 T4 触发器重评。

**f) 死锁面**：处理器 = abort（coldstart §4.3）。mirvm 进程即 rustc，abort =
非零退出 + 响亮失败，语义可接受；开启期观察即可（pinned 已含 #143035，理论不触发）。

### 3.2 轴 B：并行预取（V5-lite，Linker 零改动）

**机制**：lower 的 75-80% 耗在逐 instance 只读查询，查询值缓存在 tcx，谁先算都一样。
在 `-Zthreads>1` 会话内，用 **rustc 自己的并行原语**（`rustc_data_structures::sync`
的 `par_*`/`join` 族——任务落在 rustc 线程池，查询 TLS 上下文/循环检测/死锁检测才
成立；施工前按 pinned 源树核对确切原语名）对即将 drain 的 instance 集合旁路预热，
串行 drain 随后命中温缓存。**禁止外接 rayon 池直接调 tcx**（ImplicitCtxt 缺失 =
未定义行为面；这是约束不是取舍，见 T7）。

**预取载荷**（与 drain 实耗对齐）：`tcx.instance_mir(inst.def)`（rmeta 解码大头）+
`tcx.symbol_name(inst)`；可选二期加 body `local_decls` 类型的 `layout_of`。
**过滤谓词必须与 lower 一致**：foreign item 无 MIR（`instance_mir` 会 panic，
func/mod.rs:1720-1726 注释在案）、intrinsic 等不可入队形态跳过——直接复用
worklist 入队侧的既有判断，不另写一份。

**hook 点**：
- 种子波：`collect::collect(tcx)` 输出（lower/mod.rs:421）入队后、drain 前整批预取
  （种子 ≈ 闭包大头，coldstart §2.3）；
- 迭代波（二期，按 T6 触发器）：drain 每 K 项快照 `linker.queue` 预取下一波，
  覆盖链接仿真新发现的 std 私有函数等增量。

**确定性**：预取只填值缓存，不碰 Linker；id/地址分配序 = 串行 drain 序，不变。
唯一残余 = 与轴 A(d) 同源的 AllocId 数值漂移（预取改变 const-eval 首算序），
同一套双跑字节 gate 覆盖，无新增风险类。

**收益上界**：lower 查询份额 ×(1−1/N)，受 interning 分片锁竞争折损（上游 >16 线程
明显）；以 eco 冷 delta lower 与 image 构建会话为基准实测，不预填数字（T12 数据
裁决纪律同款）。**改动面**：lower/mod.rs 一处预取函数 + 种子波调用点 + 可选探针，
不触 linker/func 任何文件。

### 3.3 轴 C：V5 完全体（全并行 lower）

**为什么难**：`func::lower_instance(tcx, env, inst, &mut Linker)` 发射全程内联调
可变 Linker，且拿到的是**具体值烤进 body**——`func_id`（发现序分配）、
`fn_entry_addr`/`ensure_alloc`/`foreign_slot`（冻结区物化序地址）、
`reserve_asm_stub`（序号进符号名 `mirvm_asm_{id}`）、`tls_id`。字节确定性契约
（§3.1d）要求分配序**不能变成完成序**——naive 加锁直接违约，锁本身也会在
27.5% 的查询/interning 份额之外新增竞争。

**可行设计骨架（波前 + 符号化发射 + 确定性合并）**：
1. 波 = 队列快照；波内 instance 用 rustc 池 par 发射，worker 只产出**符号化 body**
   （callee 记 `Instance`、alloc 记 `AllocId`、地址位留占位）+ 按调用点序的效果流
   （新发现 callee/alloc/asm/tls 请求）；
2. 串行合并：按（波序 × 波内 id 序 × 效果流序）回放效果流做真实分配——id/地址
   分配序与今日串行 drain **逐位同构**；冻结区物化保持串行（allocate-before-fill
   断环纪律不变）；
3. 补丁 pass 把符号化 body 落成具体 id/地址。
   机制先例都在树内：`Rebase` 已对 body 做穷尽 fn/tls/asm id 重映射（rebase.rs，
   split 模式在产），v4 的 `LinkAddr`/`FrozenReloc`/`GotFixup` 已证地址晚绑定可
   穷尽表达。真正的新工程 = 把发射路径的"取值即烤"改成"取符号 + 晚绑定"。

**受影响面**：`lower/func/` 八件（发射主体，改 Linker 交互面）、`lower/linker/`
五件（拆只读上下文 / 效果记录 / 合并器）、`lower/mod.rs`（波前调度）、
`rebase.rs`（泛化到地址）；split（A2 双域）与 base 复用逻辑叠加其上，交互面大。
**周-月级**。

**设计期开放问题**（立项时设计文档必须逐条回答）：
- 波内新发现 callee 的 id 回放序如何做到与串行 BFS 逐位同构（波边界改变发现时机）；
  若做不到逐位同构，字节确定性验收就要改为"并行自身幂等 + 与串行产物语义等价"，
  这是一次**契约变更**，须单独裁定；
- `fn_entry_addr` 在发射期被"取值即烤"的站点普查（数量决定符号化改造的真实面积）；
- 串行合并段（冻结物化 + 回放）的吞吐是否成为新瓶颈（Amdahl 的新分母）;
- split 双域 + base 复用 + 并行波的三方交互矩阵。

**立项前置（明确的不立项条件，见 T10）**：B 落地后以 `MIRVM_PURITY_STATS`/perf
重测——若查询侧并行后 lower 串行残余（发射 6.2% + 符号名 2.6% + 合并）已不是账面
主项，**C 不立项**。另一观察项：上游宏展开/name resolution 并行化落地会再压
frontend 相，进一步降低 C 的相对价值。

## 4. 设计取舍总表

| # | 决策点 | 选项 | 建议与理由 | 代价/记账 |
|---|---|---|---|---|
| T1 | 注入位置 | (a) 参数拼装期进 `rustc_args`；(b) `run_compiler` 前独立副本 | **(b)**。键/指纹/回放材料天然干净（§3.1a 时序），单 helper 四站点，无需动 6 处拼装点 | args 出现"键视图 vs 会话视图"两个版本，helper 注释里写明；ircache header 回放的是键视图（合同如此，非缺陷） |
| T2 | 旋钮形态与默认 | (a) 只在开启时注入；(b) **恒注入，默认 `1`**，`MIRVM_THREADS` 覆盖；(c) CLI flag；(d) 按 crate 体量自动启发 | **(b)**。默认 `1` 在当前 pin 是语义空转（塌缩 None），却对 #162848 后的 nightly 默认 2 线程**前瞻免疫**——防御与功能一个 seam 完成。(c) 后补不冲突；**(d) 拒**：不可预测的会话形态违背确定性纪律 | 恒注入让"会话视图"恒多一个旗标；升级到 `--jobs-frontend` 时（§6-2）只改 helper 一处 |
| T3 | N 的语义 | (a) 透传 rustc（`0`=auto/`1`=off/`2..=256`）；(b) 自定义档位 | **(a)** 最少惊讶；非法值响亮 exit 2（对齐 `parse_stack_size` 风格） | `0`（auto=核数）在 128 核机上直接撞上游 >16 线程竞争区——文档口径建议 2/4/8 |
| T4 | dep 单元 / sysroot 会话 | (a) 一并开；(b) **恒钉 `1`，触发器重评** | **(b)**。三重理由：调度器已按 crate 图子进程并行（超订）；pinned 无 `--jobs`/jobserver 联动；self/cargo 双轨对拍不引入参数漂移面。`run_dep_compiler` 站点不读 `MIRVM_THREADS`（env 会被 cargoless 子进程继承，必须显式隔离） | 长依赖链关键路径上的大 crate（syn 类）暂拿不到收益；触发器 = bump 后 `--jobs` 在位 + 调度器预算联动设计 |
| T5 | 字节漂移处置 | (a) 修迭代序（排序规范化）；(b) 该会话钉 `=1` 记账；(c) image 键加产物内容哈希（关闭同键异字节窗口） | **先 (b) 后 (a)**：image 构建是一次性成本，钉 `=1` 收益损失小、立即安全；漂移源定位后若是廉价排序修复再做 (a)。**(c) 是键链契约变更**（delta 键链随 image 重建失效→自愈），仅当 (a)(b) 都不可行才评审 | (b) 需在 open-issues 记账"并行不覆盖 image 构建会话"；(a) 有隐藏第二只漂移源的风险（修一只≠修完），仍靠双跑 gate 兜底 |
| T6 | B 的波形态 | (a) 种子波一次性；(b) 种子波 + 迭代波 | **先 (a)**。种子 ≈ 闭包大头（coldstart §2.3），实现一处调用点。(b) 的触发器 = 预取命中率账本（drain 时 `instance_mir` 已缓存比例，临时探针量）低于经验阈值 | (a) 对链接仿真新发现的增量（std 私有函数等）无预热；探针属临时仪器，量完回滚（coldstart §1 纪律） |
| T7 | B 的并行原语 | rustc 池 `par_*` vs 自建线程/rayon 直调 tcx | **唯一正确 = rustc 池**（ImplicitCtxt/循环检测/死锁检测都挂在池上）。写成约束不是取舍，违反 = UB 面 | 原语名随上游漂移，施工前按 pinned 源树核对 |
| T8 | 阶段 1 与 split 面的关系 | (a) 开并行时 bypass deps split；(b) **gate 收口前默认恒 off，实测走显式 env** | **(b)**。(a) 会造成"有无 image 的 L2 键链"行为分叉，自愈矩阵语义被旋钮污染 | 实测期间 gate 环境显式 `MIRVM_THREADS=8`；默认翻转是阶段 2 之后的独立裁定 |
| T9 | 形态级默认值 | (a) 全局单默认；(b) 脚本 off / cargo·大 crate on | 先 (a)=全局 off；实测数据在手后若脚本 lower 倒退实证成立，再按 (b) 分形态翻转（翻转本身是新裁定，带数据） | (b) 增加一个行为分叉维度，diff 矩阵翻倍——只有数据证明值得才做 |
| T10 | C 立项 | (a) 直接立项；(b) **触发器化** | **(b)**：触发器 = 阶段 3 后 `MIRVM_PURITY_STATS`/perf 账本证明串行残余仍为主项 **且** ripgrep 类冷 lower 绝对值仍 >100ms 量级。命中后另出设计文档过审（§3.3 开放问题逐条回答），不从本文直接开工 | D7 的 V5 字面项在 (b) 下可能终局为"B 即够，C 关闭"——届时按登记册纪律移账并留证据 |

## 5. 施工路径（分阶段，每步带验收）

各阶段独立可回退：旋钮默认不开并行（T2 的默认 `1` = 现状语义），任何阶段红灯
即停在上一阶段的绿灯状态；缓存腐蚀兜底 = 键链自愈 + `mirvm cache purge`。

### 阶段 1：注入 seam + runner 实测（天级；防御半件建议无条件先行）

1. `cli` 增 `parallel_frontend_arg() -> String` helper：读 `MIRVM_THREADS`
   （未设/空 → `1`；`off` → `1`；`0`/`2..=256` → 透传；其余响亮 exit 2），
   产出 `-Zthreads={n}`。
   → verify: helper 单测矩阵（含非法值）。
2. 四站点注入：`run_driver`/`pack_driver`（读 env）、base build（读 env）、
   `run_dep_compiler`（**不读 env，恒 `1`**）；注入点一律在键材料快照后的
   `run_compiler` 独立副本上（§3.1a）。
   → verify: `MIRVM_TIMING=1` 下 L2 键不变（同一程序 threads on/off 命中同一条目）；
   `MIRVM_A2_DEBUG` 下 deps pre-key 不变。
3. 生效实证：`MIRVM_THREADS=8` 跑 ripgrep/tokei 形态，frontend 相计时显著下降
   （或线程数探针）；`=1` 与未设 mirvm 行为逐字节一致。
   → verify: 三形态（脚本/eco/ripgrep）× `MIRVM_THREADS∈{1,2,4,8}` 的
   `MIRVM_TIMING` 矩阵入档。
4. 倒退检查：脚本形态 `frontend+lower` 在 `=8` 下与 `=1` 对比——lower 段允许的
   倒退预算为 0（超出即记账并维持脚本形态用 `=1` 的建议默认）。
   → verify: 30-demo 抽样（coldstart 附录 A 同源）计时对比入档。
5. 差分安全面：`MIRVM_THREADS=8` 显式跑 `diff.sh`/`diff_cargo.sh` + corpus smoke
   一轮（不改默认 gate 矩阵——预算纪律，见 T8）；预先 grep 排查"编译期告警文案
   参与比对"的夹具。
   → verify: 全绿；发现告警序敏感夹具则单列记录。
6. 文档：AGENTS/使用文档补 `MIRVM_THREADS` 一行；open-issues D7 状态推进。

**退出条件**：步骤 3 的收益矩阵 + 步骤 4 的倒退账本在手。此时默认仍是 `1`，
生产行为零变化；#162848 防御已焊死（步骤 2 的恒注入）。

### 阶段 2：image 构建会话与字节确定性 gate（0.5-1 天）

1. 底座双跑字节 gate：`MIRVM_THREADS=8` 连建两次 base image，产物逐字节比对
   （复用/扩展 S4 幂等验收的既有脚本）。
   → verify: 两次 sha256 相等；与 `=1` 产物**也**相等（更强：并行不改变产物）。
2. deps image 双跑字节 gate：固定 eco 夹具，`=8` 下触发 split 构建两次，img 文件
   逐字节比对；同样与 `=1` 产物比对。
   → verify: 同上三方相等。
3. L2 store/load 双跑 + warm 一致（diff_cargo warm 维度已有，`=8` 环境加跑）。
   → verify: warm 命中且执行逐字节一致。
4. 漂移处置（若红）：按 T5——先把红的会话钉 `=1`（helper 加会话形态参数）记账
   open-issues；随后定位漂移源（优先嫌疑：`FxHashMap<AllocId,_>` 迭代序、
   `exports` 序列化序），若是廉价排序修复则修复后回摘。
   → verify: 处置后 1-3 全绿。

**退出条件**：1-3 全绿（或红项已钉 `=1` 记账）。此后"默认是否开并行、开到几"
才允许作为独立裁定提出（带阶段 1 数据）。

### 阶段 3 = 轴 B：并行预取（3-5 天）

1. 施工前核对：pinned 源树 `rustc_data_structures::sync` 可用 par 原语名与签名
   （coldstart §4 来源纪律）。
2. `lower/mod.rs` 增 `prefetch_wave(tcx, &[Instance])`：rustc 池 par 执行
   `instance_mir` + `symbol_name`；过滤谓词复用 worklist 入队侧判断（foreign/
   intrinsic 无 MIR 者跳过）；`-Zthreads<=1` 会话直接返回（原语退化串行也白付遍历）。
   → verify: `=1` 下调用与不调用产物逐字节一致（空转安全）。
3. 种子波接线：`collect::collect` 入队后、drain 前调用。
   → verify: 阶段 2 全套字节 gate 在 `=8` 下复跑全绿。
4. 收益账本：eco/ripgrep 冷 delta lower + base/deps 构建会话，`MIRVM_TIMING` ×
   `MIRVM_PURITY_STATS` 前后对比；临时探针量预取命中率（量完回滚）。
   → verify: 账本入档（本文档追补或另立数据附录），不预填目标数字。
5. 迭代波（可选二期）：仅当命中率账本触发 T6；实现 = drain 每 K 项快照队列预取。
   → verify: 同 3 的字节 gate + 4 的账本增量。

**退出条件**：账本明确回答"查询侧并行后，lower 串行残余占比与绝对值"——这是
T10（C 立项与否）的判据输入。

### 阶段 4 = 轴 C：V5 完全体（触发器化，默认不施工）

触发器（T10）命中后：另出设计文档过审，逐条回答 §3.3 开放问题（id 回放序同构性、
`fn_entry_addr` 烤址站点普查、合并段吞吐、split/base/并行三方矩阵），验收基线 =
与串行产物**逐位同构**（做不到则先裁契约变更）；未命中则按登记册纪律关闭 C，
D7 移账留证据。

### 观察面（不排期）

- dep 单元/sysroot 注入：T4 触发器（bump 后 `--jobs` + jobserver 预算联动设计）。
- 形态级默认翻转：T9（要阶段 1 的倒退账本与阶段 2 的绿灯）。
- 上游宏展开/name resolution 并行化落地：frontend 相账目重估（利好轴 A，压低轴 C）。

## 6. bump pin 检查单与跟进渠道

**bump 越过 2026-09 的 nightly 前必查**：
1. [#162848](https://github.com/rust-lang/rust/pull/162848) 是否已合并——已合并则
   nightly/dev channel 默认 2 前端线程。**阶段 1 步骤 2 落地后免疫**（恒注入默认 `1`）；
   未落地就 bump = 全部进程内会话被动并行，底座字节 gate / stderr 差分可能无预警红。
2. `--jobs`/`--jobs-frontend`（#159675/#160697）在位后：helper 改产
   `--jobs-frontend={n}`（语义更精确），并触发 T4 重评（jobserver 联动）。
3. DefId 可复现性修复（#162580/#162555 族）在位情况——影响 §3.1d/轴 B 的
   AllocId/DefId 漂移面大小（利好：漂移源变少）。
4. 复核 pinned 源树 `rustc_thread_pool`（#143035 已并树）与 #151509 commit 在位；
   `-Zthreads`/`--jobs` 选项解析语义复核（coldstart §4 来源纪律）。

**跟进渠道**：月报 [goals#121](https://github.com/rust-lang/goals/issues/121)；
Zulip `#t-compiler/wg-parallel-rustc`；总追踪
[rust#113349](https://github.com/rust-lang/rust/issues/113349)；稳定化计划
[compiler-team#1005](https://github.com/rust-lang/compiler-team/issues/1005)。

## 附：本次核对的代码事实索引

- 进程内 `run_compiler` ×4 + `COMPILER_SESSION` 互斥：cli/mod.rs:23-26、
  cli/driver.rs:45-48/739-741、cli/cargo.rs:218-220、baseimage.rs:520-522。
- `run_driver` 完整时序（router → 栈/deps 载入 → L2 lookup → callbacks 快照 →
  run_compiler）：cli/driver.rs:625-741。
- 参数拼装：cli/entry.rs:107/471、cargo_shim.rs:756-775、
  cargoless/schedule/args.rs（`dep_rustc_args`:108 等族）、baseimage.rs:508-515；
  cargoless dep 子进程入口 `run_cless_dep`→`run_dep_compiler`：cli/cargo.rs:197-234。
- 键材料：ircache 键 = 完整 `rustc_args`（ircache.rs:8/53-81）；pack 存 args
  （pack.rs:561-583）；base 键 = fnv(build_id, sysroot stamp)（baseimage.rs:12-13）；
  deps 键 = fnv(build_id, base key, extern 内容戳)（depsimage.rs:96-135）——
  三者均不含产物字节 ⇒ §3.1d 同键异字节腐蚀窗口。
- 串行 drain 与 split 双队列：lower/mod.rs:539-580；`lower_one` 带 `&mut Linker`
  贯穿发射：lower/mod.rs:240-255 → func/。
- Linker 全量状态（ids/queue/frozen/alloc_addrs/fn_entries/asm_sites 序号命名/
  got/foreign_slots/code_arena/base 表/next_fn）：lower/linker/mod.rs:14-163。
- lower 查询面三族：`instance_mir`（func/mod.rs:1726；foreign 无 MIR 注释
  :1720-1726）、`layout_of`（frame.rs:56 起）、`symbol_name`（linker 全域）。
- 晚绑定先例：rebase.rs（fn/tls/asm id 穷尽重映射）、ir 的
  `LinkAddr`/`FrozenReloc`/`GotFixup`（v4 可重复实例化）。
- 诊断钩子/emitter 线程安全：cli/driver.rs:63-164、diagnostics.rs:28/314-349。
- cargoless 并行 = 子进程级：cargoless/driver/build.rs:303-420。
- P1 化石化已修（runner 不回放 `MIRVM_*`）：current-status M6 片4（S1b）。
