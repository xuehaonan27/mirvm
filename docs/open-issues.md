# mirvm 未解决债务与开放问题登记册

> 覆盖 mirvm 自始（M0 tier-0 时代）至 2026-08-19 当前复核的
> **全部**记录在案且至今未解决的债务、开放问题、响亮拒绝边界与暂缓项。
> 本文是未解决事项的**唯一登记入口**；已根治/已完成项一律不收录（查
> [current-status.md](current-status.md) 与 [decision-history.md](decision-history.md)）。
> 新产生的债务登记在本文；推翻或关闭条目时在 decision-history 追加证据。

## 使用说明

- **ID 规则**：`T`=已立项待施（有批准蓝图）；`C`=corpus 实锤产品欠账（待立项）；
  `E`=引擎/架构欠账；`D`=分发/产品面（方向已批未立项）；`R`=响亮拒绝边界（现行定型）；
  `G`=维护态/基建。F 节 = 条件触发型重开项（触发器速查）；H 节 = 定型否决与已关闭（防重提）。
- **状态词**：`待施`（蓝图已批，只差施工）；`未立项`（有诊断与修法路径，未批开工）；
  `绕行`（有合法 workaround 在役）；`记账`（知情接受的限制）；`拒绝`（定型边界，重开需实锤）。
- 每条带出处。corpus 候选投放清单（zune-jpeg/typst 等）不是债务，见
  [corpus.md §7](corpus.md)。

## 原理闭合性总判（2026-07-18，用户裁定）

开工纪律：**每片先写明本片的闭合契约——闭合到哪一条可观察边界？原理上能否完全
闭合（允许任意工程量）？** 原理上能闭合就做彻底，不做「大多数没问题」；原理上
不能闭合必须事先明说，不许用绕行冒充闭合。

**原理可完全闭合（排到即做彻底）**：

- T1–T4、C1–C2、C3 终止形（`ud2`/int3 类）、C4–C8、E1–E18、E20–E31、D 全部、R3（T1 落地后有真 unwinder
  载体）、R4、R5、R6（工程量 = 完整 link plan/自写装载器，原理无堵点）、R9、
  R10、R11、R15、G 全部。
- **C3 的 resume 面（穿解释帧 longjmp）**：语义保真度未证——唯一允许把它改判
  「可闭合」或「不可开」的证据是开工前的验证 spike，不许猜。
- **R2 的多线程 fork / vfork / pthread_atfork**：可闭合到 **native-parity**
  （JVM 式 atfork 锁重置纪律；「尽力/可能坏」是 native 本身的口径，不是掺水）。
- **E23 checked 模式**：原理上可闭合，但现有 IR 已丢失指针来源，不能用“地址已映射”
  冒充范围合法。闭合前置 = IR 保留来源、运行时自动维护所有权范围；闭合界仍只是其
  声明的 raw 解引用检查，不是完备 UB 防线，也不是沙箱。

**原理不可闭合（如实标注，不掺水，勿重提）**：

- **R1 同步故障信号 handler 的【guest 代码执行】**：三硬因——宿主/guest 故障不可
  分辨；解释器深度不可重入（信号帧内跑解释态代码 = 任意断点重入引擎，非
  async-signal-safe 是原理性）；handler 返回 = 重执故障指令 = 无限再故障。
  **崩溃期退出语义已忠实**（真故障 = 同信号死亡，stack overflow = 同 SIGABRT，
  宿主 std handler 代打）；**可闭合面 = 崩溃诊断化**（T4 泛化：故障落点归属
  判定 + guest 化崩溃行 + 以同信号终止，2026-07-19 用户裁定重构方向）。
- **E19 rustix 裸 syscall 拦截**：硬编码 inline-asm syscall 无符号边界可拦，
  VM 层原理不可闭合；唯一闭合 = OS 层 seccomp（H 节已定型）。FFI 与
  `libc::syscall` 变参两形态均有 chokepoint，虚拟化可闭合（三形态分析见 E19 条）。
- **E11 中「栈深度与 native 逐字节一致」**：UNSPECIFIED 域（2026-07-19 用户确认
  不追求）；可闭合的只是诊断与 `--stack-size` 配置语义。
- **R8 的 asm `goto`/label**：转出 stub 需把宿主函数整体 native 编译，超出
  Cranelift 栈（cg_clif 对一切 inline asm 均 fatal）；唯一理论出路（单函数
  AOT 逃逸舱）未证实，不预支。自研 JIT 设计注脚与 Cranelift issue 素材见 R8 条。

## T. 已立项待施（蓝图在手）

T1–T3（M5.4c/d + M5.5）、T5（syscall 拦截）、T6/L1（页和线程生命周期）、
T10/P1（JIT 地址登记）与 T13/D0（诊断通道分层）已经闭合；
T4 已并入 R1。现行施工队列沿用日志设计里的 L（日志采集主线）和 P（外部 perf profile
线）编号：

| ID | 事项 | 闭合边界与依赖 | 出处 |
|---|---|---|---|
| T7 / L2 | **fork 子代采集代际** | **已闭合（2026-09-18）**：子代在普通边界按配方自建 generation、文件、页池、writer、producer 与 errno pointer；回归 `runtime.telemetry`（父 gen0 / 子 gen1 各自 pid 与记录） | decision-history §7.59/§7.60 |
| T8 / L3 | **HostSyscall 直接热路** | **部分闭合（2026-09-18）**：trace 域独立 ISA/module、独立发布槽、边界蹦床（含展开 landing pad）、`get_pinned_reg` 直调的 syscall 站点、fork 子代钉寄存器修复均已落地并有回归（§7.61-§7.63）。**剩余三项**：① 健康 pair 的逐记录冷 sequence 更新尚未移除——`next_sequence` 同时是 producer-end 账本的 `attempted`，改成页内推导会动 v0 文件/sequence 合同，须单独裁决；② 设计要求代码域在**最外层 activation 入口**选择，当前是 Engine 构造时冻结，补齐需要同一 Engine 同时物化 plain/trace 两套代码与解释器两条循环；③ 页内内联写（§5.9 的 64B Enter + 24B Exit）尚未做。plain 反汇编零采集成本 | 同上 §5.2.3/§5.3/§11 |
| T9 / L4 | **1B stateless raw syscall site** | 依赖 L3；双物化 inline-asm raw site，并以 RFLAGS、GPR、red zone、栈、完整向量态和 raw 返回语义对拍闭合。完成后才能宣称首个内部 syscall 纵切完成 | 同上 §5.9/§11 |
| T11 / P2 | **Linux perf capture** | P1 已完成，可与 L2–L4 并行；交付 profile 命令和薄脚本，首版 user-space/IP-only/inherit，权限、lost samples、缺映射均响亮失败或标 incomplete；重跑 fib 与 D16 真实 workload；fork registry/map 重置与 L2 闭合 | 同上 §9/§11 |
| T12 / 数据裁决 | **自适应页池与 writer 参数** | 依赖 L1–L4/P2；同一内存预算下实测 4/16/64 KiB、24/32B Exit、return gap、drop、guest cycles、RSS 与 writer CPU，再实现 4→64 KiB 自动伸缩并裁定批量/checksum；不得先填数字 | 同上 §10/§12.2 |

## C. corpus 实锤产品欠账（实锤驱动，未立项）

| ID | 事项 | 关键内容与转正要件 | 出处 |
|---|---|---|---|
| C6 | **M5.x intrinsic 按需队列残余** | pclmulqdq.256/.512、vaes、其余 gather 形态、avx512.pmadd 系等：遇真实 workload 按既有四触点法补（已清先例：psad.bw/pclmulqdq/aesni/crc32/permd/gather/vpmadd52/F16C/lddqu/`2b4766b`）。AES 等未触发项保留响亮 Trap | corpus §5，history/m5.1-design.md §1 |
| C8 | **Rust 侧 ctor / `.init_array`（linkme 族）未触发** | C 原生归档侧 constructor 已由 decision-history §7.8 分治放行（DT_INIT）；裸 `.init`/`.fini` 仍拒。Rust 侧 ctor/linkme 从未进 corpus，按需立项，不预支 | history/m4.5-plan.md D6（已删，git 历史），§7.8 |

<!-- C1/C2/C3/C5/C7 已闭合（2026-07-18，decision-history §7.10–§7.13/§7.9），
     按「只收未解决」规则移出；残余边界分别转 R17/R16/E32。 -->

## E. 引擎与架构欠账

### E.1 JIT / 引擎内部

<!-- E1（JIT 间接调用准入）已闭合 2026-07-21、E9（dl_iterate_phdr 差分探针）
     已闭合 2026-08-12、E12（alloca 必迁承诺）已撤销 2026-08-12、E13（guest
     异常分类与运输）已重裁并闭合 2026-08-12、E21（os/arch 双 leaf）已闭合 2026-07-18/19，
     按「只收未解决」规则移出——证据与重开条件见 decision-history
     §7.19/§7.48/§7.49/§7.50-§7.52/§7.16 -->

| ID | 事项 | 状态与内容 | 出处 |
|---|---|---|---|
| E14 | **hand-rolled TLAB 未立项** | `绕行` v1 = mimalloc crate 后端；chunk/大小类/remote-free 队列细节无；与 E6 关联 | history/spike1-model-a-skeleton.md，designs/concurrency-arch.md §9 |
| E15 | **`--vm-stats` fn-ptr 无出边盲点** | `绕行` 仪器债：fn-ptr 间接调用无出边 → 债务读法永远「至少欠这些」；继续靠增量发现 | src/vm/engine/stats.rs 头注 |
| E16 | **io_uring 直通未实证** | `未立项` tokio-uring 可选路径全库仅 designs/async-stackless.md §5.2 提及，无 corpus 对拍 | designs/async-stackless.md |
| E17 | **L2 缓存两处** | `未立项` ①有告警/错误的会话拒入账、诊断回放未做（告警程序永不享缓存，session 门函数在 src/cli.rs:395、计数器在 :368）；②条目无逐出——**手动 GC 面已由 `mirvm cache purge`（默认清陈代）补上（§7.14）**，自动 LRU/容量上限不立项 | history/m6-log.md 片2/8 |
| E19 | **rustix 裸 syscall vs os:: 收口的张力** | `记账`（2026-07-21 定稿，**③④ 已由 T5 闭合**）：**「mirvm 拦截一切 syscall」在真实生态成立**——① FFI libc 包装：builtin 注册表即现成挂载点（HostWrite/HostGetenv/HostFork/HostSignal/HostSyscall 在产拦截）；② `libc::syscall(...)` 变参：`Builtin::HostSyscall` 单点内建；③ guest inline-asm 裸 syscall（rustix linux_raw）与 ④ global_asm/naked 内：**已拦截**（T5 `d0470fc`：asm-stub 文本生成点改写 → GOT 两级间接槽 → trampoline 全契约 → dispatch v1 直通 + TRACE；探针三维一致 + rustix 系零回归）；⑤ vendored C 库：常态（C 调 libc 包装）经 native_archive 链接序插桩可闭合，罕见叉（C 内联汇编自写 `syscall` 指令）cc 产物不透明；⑥ JIT 与①③同入口；⑦ 对抗式自修改/`.byte 0x0f,0x05` 书写无真实形态。**唯一如实残余 = ⑤罕见叉与⑦，只有 OS 层 seccomp 能兜**（维持原判）；虚拟化语义（统一 fd 空间/假 FS/计费）属 D10 本体，钩子已备 | corpus §2.3，§5；decision-history §7.18 |

### E.2 架构与边界

| ID | 事项 | 状态与内容 | 出处 |
|---|---|---|---|
| E22 | **嵌入公开信任面与进程期函数地址** | `记账 + 未立项` **已完成的机制**：Package v4 可重复实例化；每 Engine P1/native/MC 隔离且旧地址不复用；Running/Closing/Finalizing/Closed、执行租约、pthread start/线程私有数据（TSD）析构器的 `DeferredHold`、暂停异常持有、逐实例 ctor/fini、`close`/`wait_closed` 与长寿命线程 `CtxSlot` 清理已接通；传统异步 signal 也已有进程定向 owner inbox（待处理信号箱）、`SI_TKILL` 目标 pthread cell（线程槽）、在途 frame 原子计数、非 LIFO disposition 恢复和 close/线程退出 drain，不再是 E22 的生命周期欠口。固定桩、registration（一次 handler 安装的登记对象）和线程槽因原生代码可能保留其地址而存活到进程结束，关闭后的陈旧桩只会明确以状态 70 失败，不会误投到新 owner。ctor 受控异常转成实例化 `Result` 失败，fini 是不可展开的拆除边界，任何 MIRVM/foreign/宿主 Rust 异常逃出都固定诊断后 `abort`。**剩余一：不是全 safe typed API。** `Package::load` 是 safe 的 owned snapshot 校验，但 `Package::instantiate` 必须是 `unsafe`：验证器不能证明包内 native 库、宿主符号和 FFI 签名相符。手工 Module 与 raw 两机器字 export 同样 `unsafe`，`Shared` 不公开。要关闭这部分，必须为具体导出签名生成/验证 typed binding，不能把责任藏回用户配置。**剩余二：裸地址没有通用撤销协议。** 任意第三方库可无限期保存 callback；引擎不知道所有副本何时消失，所以已发布普通/P1 closure、JIT 码/展开表、committed MC/native 映像保留到进程结束。close 后 closure 只持小型 owner 墓碑，不持有 Module；C-unwind 稳定报告 `EngineClosed`，普通 C 按 ABI 终止，新 Engine 不复用旧 P1 地址。若真实产品要求进程内资源严格有界，只能为被撞到的具体 native 注册 API 建立完成/撤销合同，或用子进程隔离一次性回收；不能声称通用 FFI 能找回所有裸指针。进程级故障隔离另归 D10/E23，不由生命周期计数冒充 | decision-history §7.42/§7.51-§7.55，designs/modeb-mirvmar-design.md §6 |
| E23 | **checked 模式（L3）未建；L1 已完成** | `未立项` 操作数区双端 `PROT_NONE` guard 和 JIT 入帧前栈检查已经完成。L3 仍缺指针来源：现有 IR 的 Deref 不区分 raw/reference，也不知道 frame、frozen、allocator、FFI 或 mmap 所有者；只查 mapped 会误放 VM 元数据，要求用户登记则是绕行。闭合需由 lowering/IR 保留来源并由运行时自动维护所有权范围；正式沙箱仍归 P3 OS worker | decision-history §7.42，DESIGN.md C13 |
| E26 | **平台仅 Linux/ELF/x86_64** | `记账` pthread/dlopen/GNU 链接/x86 asm wrapper 依赖面；unwind「跨平台无痛」未逐平台验证；macOS 次之后议 | current-status §4，DESIGN.md §11 |
| E32 | **inline-asm setjmp/longjmp 的捕获帧内存复用 hazard（C3 定稿边界）** | `记账` asm-stub 模型下 setjmp 捕获点在 stub 包装帧；解释帧在捕获与恢复之间复用该宿主栈内存的合成协议可撞死（v2 spike 实锤，落点 `Channel::send` 内部）。真实 workload（wasmtime 全 trap 面）不发生该形态、三维确定性绿。消除 = JIT 真帧身份（compiled guest fn = native 帧语义）；不宣称全形态闭合。**进展 2026-07-21（T1）**：JIT 帧已是真 native 帧（含真 unwinder 穿透/着陆，双 CIE + 全覆 LSDA）——setjmp/longjmp 所在函数一旦发布即脱出 hazard 面；interp 帧路径维持原记账 | [parked/c3-resume-spike.md](parked/c3-resume-spike.md)，decision-history §7.19 |
| E33 | **unsafe trust-boundary 优先审计**（audit M-03，2026-07-22 登记） | `未立项` ~475 个 unsafe block、显式 SAFETY 注释仅 5 处——真实地址模型/FFI/ELF/asm-stub/unwind 决定大量 unsafe 不可避免；正确策略非机械补注释，而是优先审计 FFI、全局 Shared、ELF 解析、固定地址映射、thunk、unwind 五个信任边界，为每个实际不变量补最小证明或测试；ASan/fuzz 类保证无实锤前不立项 | history/development-status-audit-2026-07-22.md §9 |
| E34 | **JIT 翻译器大 match 治理**（audit M-04 关联，2026-07-22 登记） | `记账` jit/translate.rs 2,939 行单文件三 match 是维护热点；分族拆文件的收益/扰动比未评，结构重构战役（E21）模式可复用，下次大改前评估 | src/vm/engine/jit/translate.rs |

> E27（weak 符号真地址化缺定向验收）已于 2026-07-18 关闭并实修「weak extern
> static 恒 0 判空 cell」缺陷——现走 GOT 启动相真解析（命中=真址/缺席=0；引擎
> 接管符号强制缺席），探针 `demo/weak_extern.rs` 三维绿（decision-history §7.9）。

## D. 分发与产品面

明确缺失能力的合并施工顺序与逐阶段验收口径见
[产品能力补全计划](designs/product-capabilities-plan.md)。本表仍是各项债务状态的唯一真源。

| ID | 事项 | 内容 | 出处 |
|---|---|---|---|
| D2 | **发行形态与命名（D9f⑤）** | 先 miri 式后 JDK 式自包含 tarball（成熟后）；kit 命名候选 MDK/mirvm toolkit（MRsDK 已否决） | designs/distribution-design.md |
| D3 | **逐函数惰性装载；档案直接验证与整包复制余项** | **核心已完成（2026-08-11，§7.45）**：MODULE 只存模块元数据，FUNCS 用固定索引指向独立函数体并保存逐体哈希；运行时只让实际访问或预取命中的 `FuncBody` 常驻，真实访问顺序驱动下次后台预取，需求始终优先。v3 曾直接 mmap 源 inode；v4 为了让 safe `Package::load` 在源文件改写/删除后仍保持同一已验证对象，改为一次复制 owned snapshot，保留惰性对象驻留但不再宣称整包零复制。**余项**：postcard 是顺序编码，E20 仍要求 load 时逐函数临时解码；未来偏移式只读归档必须让验证器和执行器遍历同一份有边界检查的不可变字节，并同时说明 snapshot 所有权，不能重引入可变 inode 的校验后替换窗口。下一格式编号不预留；完成前不启动 D4，性能基线另期恢复 | history/coldstart-research.md V6，decision-history §7.32/§7.45/§7.53 |
| D4 | **对外格式冻结重估** | M5.3 收官触发器已响。当前不稳定格式 v4 已补逻辑链接地址和可重复实例化，但首次 load 仍为 E20 做临时逐函数解码，并且 safe owned snapshot 会复制整包。先闭合 D3 的档案直接验证/所有权合同，再评审兼容窗口、能力位、迁移工具和损坏/签名策略；不要因版本号已经到 4 就误称格式冻结 | decision-history §7，§7.32/§7.45/§7.53 |
| D5 | **L3 JIT 机器码缓存** | 禁令条件「M5.3–M5.5 定型前禁做」已随 M5.5 收官（2026-07-21）消失；**2026-07-29 判为 dev 循环最大单根杠杆**（热函数每进程重烧 = 纯白烧），既有 MC 机器码节 + 进程内 ELF 装载器已证 JIT 产物可序列化再装载；归 D16 候选 | designs/distribution-design.md，history/m5.3-design.md，decision-history §7.32 |
| D6 | **S3′c 完整形态（跨项目共享）** | 路径无关内容哈希键（~50ms/次）+ tainted 层/多层 image 合并；按实需立项（已兑现的只是同 workspace 跨 bin 冒烟） | current-status §5.8，history/s3b-a2-design.md §3.4 |
| D7 | **frontend 相成本无杠杆认领；V5 `-Zthreads` 并行 lower 未立项** | eco ~143ms / ripgrep ~730-800ms 在账无人认领；V5（tcx DynSync+worklist rayon 化）在 V3 不建后无人重启 | history/coldstart-research.md，m6-log 片8/10 |
| D8 | **8 个 correctness case 未 benchmark** | ripgrep_gzip/parallel_nomatch/mmap_binary/parallel_match/multiline_replace、tokei_sort_code/streaming_json/rust_files | real-projects.md §5 |
| D9 | **registry 依赖 crate 底座化 / 自适应底座 / 底座 AOT 机器码入 base** | S4 三未立项方向；第三条与「机器码不入缓存」世界观冲突，立项前先核 | history/s4-base-image-design.md §0/§4 |
| D10 | **M3 产品面** | daemon、agent API、资源治理、正式沙箱、虚拟化钩子（假 FS/路径重定向/计费——区分 guest 调 open 与解释器自读缓存） | DESIGN.md §9，§7 沙箱节 |
| D11 | **REPL/Notebook + safe typed 嵌入绑定（M7+）** | 原愿景 M6 编号已被轨 C 冷启动占用；Package/Engine 真实关闭协议已完成，不能继续把嵌入本身写成未来需求。剩余公开信任面精确归 E22：为具体 export 建 typed binding，而不是把 raw ABI 伪装成 safe。REPL = 持久堆天然成立，仍未立项 | DESIGN.md §3/§9，E22 |
| D12 | **`-Cincremental` 脚本路径** | 非当前杠杆；触发式重启（「大用户 crate 编辑-重跑」形态）；`finalize_session_directory` 坑在案 | history/coldstart-research.md §4 |
| D13 | **地址模型 P5 增量（扩域/回收）** | 维持现固定基址样条工程；增量能力记 M7+ | decision-history §7.5b |
| D14 | **原生内容寻址依赖存储（统一依赖 cache 终态；用户 2026-07-18 裁定方向）** | 去重单位 = 完整编译键（crate 版本 × features × 依赖闭包 × cfg/flags × toolchain）：多脚本/多项目共引 X@V 时其构建产物机器级唯一。**近期片已落地（§7.15：共享 cargo target dir，fingerprint 即编译键内容寻址；实测 ethers 二跑 0.66s、两树并集 525M）**；**终态 = mirvm 原生 store（`~/.mirvm/store/<编译键哈希>/`，自管 build plan + extern 注入），与 `.mirvm` 本地解析同设计；P5 依赖来源和 env/GC 开工时合并评审**。并发模型已裁定：发布一次后续只读命中、无大锁常驻；清理粒度粗可接受 | 2026-07-18 缓存讨论，decision-history §7.14/§7.15 |
| D15 | **砍掉默认路径对 Cargo 的强依赖（自有依赖解析 + 编译调度；用户 2026-07-22 纳入日程）** | **P1-P5 与 P2 总 corpus 验收均已完成**（2026-07-27 至 08-10，decision-history §7.28-§7.43）：除自有解析/编译调度、workspace/resolver 2/3、Git 依赖外，现已支持依赖来源所需的 Cargo config 层叠、替代 sparse/Git registry 与 Cargo credential provider、registry/local-registry/directory source replacement、registry 目标上的 path/Git/registry `[patch]`、旧式 `[replace]`，且 `mirvm pack` 缺省复用 cargoless。来源合同 30/30；self locks 与固定 Cargo 逐字节相同并被其离线锁定检查接受；full corpus 最终 **138 pass / 1 skip / 0 fail**。D17 又补齐 resolver 1、复杂成员 glob/package ID 和 workspace lints。**长期边界**：Cargo compat 不删除，作为用户显式回退、行为裁判和持续对拍路径；cargoless 保持默认。**仍开放**：Git source replacement、以 Git URL 为目标的 patch、`paths`/HOST_RUSTFLAGS 等完整 Cargo config 余面；嵌套 workspace 与同名成员按 Cargo 自身规则拒绝，不是 mirvm 私设边界 | decision-history §7.22/§7.27-§7.44，[d15-cargoless-design.md](designs/d15-cargoless-design.md)，[mirvm test 合同](designs/mirvm-test-cargoless-contract.md) |
| D16 | **统一性能战役（用户 2026-07-29 立项，2026-08-18 进行中）** | 日志采集 1A/L1、P1 地址登记与 D0 诊断分层已落地；现行主线为 L2 fork → L3 直接热路 → L4 raw site，P2 profile 可并行，之后是数据裁决。不得用固定双页/通用 helper 的当前数字冻结性能合同。P2 后用真实 workload 和 `MIRVM_TIMING` 裁定 D5/L3 JIT 码持久化、D3 档案直接验证/装载、后台服务线程与 D12 `-Cincremental`，并复测分配/guest TLS、JIT 各类候选和旧 C7 regex 靶子。已知 RED 仍是热缓存 `fib(32)` 约 97ms > 80ms，不得放宽门槛关账。完整进程 syscall/duration 仍须独立 kernel raw-syscall 流；旧 `MIRVM_SYSCALL_TRACE` 只作债务基线 | designs/m5-design.md §3，designs/m5.4-design.md，designs/mirvm_high_performance_log.md §11–§12，decision-history §7.20/§7.32/§7.56/§7.58 |
| D17 | **mirvm test（用户 2026-07-29 立项）** | **Cargo test/bench 合同完成（2026-08-11，§7.44），D19 doctest 后续并入同一命令**：bench、根 proc-macro、resolver 1、复杂成员 glob、workspace lints 和完整 package spec 均已闭合。单包/test/bench/doctest 合同 **34/34**、workspace 合同 **31/31**；self 腿由 PATH 哨兵 + execve 审计证明零 Cargo，固定 Cargo/rustdoc `-vv` 是行为裁判 | [mirvm test 合同](designs/mirvm-test-cargoless-contract.md)，decision-history §7.32/§7.34-§7.46 |
| D19 | **doctest / rustdoc 前端** | **`mirvm test` 范围已完成（2026-08-11，§7.46）**：默认与 `--doc` 选择、参数冲突、根库和 Dev 依赖、build.rs cfg/env、源行号、`no_run`、`ignore`、`compile_fail`/错误码、`should_panic(expected)`、过滤参数、状态文字和退出码均由固定 Cargo/rustdoc 三轨对拍。rustdoc 继续提取和裁判代码块，MIRVM 的 test builder 只把临时 crate 产为带 MIR 的库与 VM 启动器；self execve 审计零 Cargo。独立 HTML 文档生成不是 `mirvm test` 的执行能力，本项不据此宣称存在 `mirvm doc` | [mirvm test 合同](designs/mirvm-test-cargoless-contract.md)，decision-history §7.46 |
| D18 | **env/GC 管理面（用户 2026-07-29 裁定）** | uv 式环境 = 全局 store（D14 已有）+ 环境（lock 物化的引用集，`~/.mirvm/envs/` 登记为根）；**GC = 登记根 + 标记-清扫**（否决裸引用计数：落盘计数崩溃半截即永久不一致；sweep 崩溃安全、无计数一致性）；purge 环境 = 摘根 + 从根集合可达性扫描回收不可达。缺包行为：默认自动拉取（日志明示），--offline/--locked 响亮报错 + 提示 fetch 指令。与 D14 终态原生 store 合并评审 | decision-history §7.32 |

## R. 响亮拒绝边界（现行定型；重开需实锤驱动）

| ID | 边界 | 重开条件 | 出处 |
|---|---|---|---|
| R1 | **同步故障信号（SEGV/BUS/FPE/ILL/TRAP）guest handler 拒绝**（2026-07-19 重构定稿） | **拒绝的是「guest handler 代码的执行」**（三硬因：宿主/guest 故障不可分辨；解释器深度不可重入、信号帧内跑解释态代码原理性非 async-signal-safe；handler 返回 = 重执故障指令 = 无限再故障）。**崩溃期退出语义已忠实**：guest 真故障 = 与 native 同信号死亡（真实地址模型直落），stack overflow = 与 native 同 SIGABRT（宿主 std handler 代打，实证：`thread 'mirvm-guest' has overflowed its stack`）；guest std 的 `stack_overflow::init` 条件安装（仅 SIG_DFL 才装）读回宿主 std 已装 handler → 静默跳过，从未触发拒绝（sigread 三维实证）。**可闭合面 = 崩溃诊断化**（故障落点归属判定：guest 冻结域/代码域/帧区 → guest 化崩溃行 → 同信号终止；T4 泛化 + `MIRVM_SEGV_DUMP` 产品化） | current-status §4（M5.2 D8l），2026-07-19 用户裁定 |
| R2 | **vfork/clone/clone3/setjmp/longjmp 系、pthread_exit、pthread_atfork、多线程 fork 拒绝** | fork-alone 单线程已放行（D8f `/proc/self/task` 守卫）；其余 = 帧模型级工程/非局部控制流穿解释帧 | current-status §4，src/lower/mod.rs:48-62 |
| R3 | **unwinder context/state 家族 11 个 guest 可见符号 Unsupported**（`_Unwind_Set/GetGR/SetIP/Resume/ForcedUnwind/LSDA…`） | **进展 2026-07-21（T1）**：JIT 的内部 MIR `Resume` 已由 try_call pad 的 `TryCallExn(0)` 续传宿主 unwinder；这不等于允许 guest 直调 `_Unwind_Resume`。后者与其余 10 个 context/state 符号仍显式拒绝，因为它们看到的是宿主解释器帧而非 guest 帧。guest frame/IP/LSDA 翻译层 + 差分探针（Backtrace/GetIP/FindEnclosingFunction/GetCFA 已由影子帧兑现） | decision-history §7.19，src/lower/builtins.rs，src/vm/engine/jit/translate.rs |
| R4 | **一般嵌套 DST / 其他 metadata 形态拒绝** | 冻结一份通用 DST layout expression 的评估（slice/str 静态公式与 direct dyn 尾 vtable 运行期对齐已支持） | current-status §4，decision-history §5 |
| R5 | **冷面 128 位形态 Trap**（`Transmute pair→聚合`、tag 宽>8B `InvalidEnumConstruction`） | 未被真实 workload 撞出；撞到再补 | history/m5-log.md 片10 |
| R6 | **static archive 拒绝面残余** | 非 PIC/thin/跨 archive 依赖与顺序/重名导出/RTLD_DEFAULT 碰撞/export-symbols/非 Linux-ELF/裸 `.init`/`.fini`；`.init_array` 族已放行（§7.8），RTLD_DEFAULT 同名碰撞已改归档优先（fb0b204）。多 archive link plan 与 modifier 等价语义未立项——放宽前必须先立 | src/native_archive.rs，history/m5.1-design.md D2 |
| R7 | **弱内存序特殊化不做** | 映射宿主原子即在 RAM non-det 包络内；真实 workload 再评估 | DESIGN.md，history/m4-plan（git 历史） |
| R8 | **asm 拒绝面** | `att_syntax`/`sym`/`label`(asm goto)/`may_unwind`/非 x86_64；asm-stub xmm/向量值操作数未扩槽（被 stdarch helper 路线绕行，无 workload 触发）。`noreturn` 已升级实锤债 → C3。**asm goto/label 证据级记档（2026-07-19 用户裁定）**：①原理 = goto 的控制流转出 asm 块到函数内任意 label，不是 call 边界；asm-stub 形态 = 独立 native 子程序（call-return/noreturn 两面孔），无可表示形状；要接 = 宿主整个函数 native 编译 + 跨边界控制流（guest BB 图与 native 码交织）= 单函数 AOT 逃逸舱，未证实。②**Cranelift 对一切 inline asm 均不支持**（cg_clif 对 inline asm 整体 fatal）——提 issue 的前置是 inline asm 支持先存在，goto 是其上形态问题（issue 素材留此）。③自研 JIT 注脚：换自研 JIT 时 asm goto 可降格为普通 BB 间分支 + 内联 native 序列 | history/m5-log.md，src/lower/func/asm.rs |
| R9 | **`type_id`/`type_name`/`offset_of`/`field_offset` Trap** | 真实程序撞到再补 | history/m5-log.md |
| R10 | **`va_arg`/`carryless_mul`/`autodiff`/`rustc_peek`/SVE 5 个** | 前两个无真实用例保留 Trap；后三个无生态意义（实验/调试/ARM） | history/m5.2-design.md D8l |
| R11 | **`intrinsics::abort` SIGABRT vs native SIGILL 差异** | 授权差异绕行；差分若开始比信号再对齐 | history/m4-log.md |
| R12 | **TSan 通道跑 guest TSD dtor 场景不可行** | TSan 线程态先于 TSD 相位析构（持久边界）；TSan 配置下 Ctx 永久泄漏，测试用例避开 | history/m4-log.md |
| R13 | **虚拟地址模型（含线性内存折中）否决，不作方向** | FFI 轴有原理性障碍；真实地址模型保留（P1/P2 已修，P3 冻结、P4 判非问题、P5→D13） | decision-history §7.5b |
| R14 | **tokei 并行 JSON reports 次序不稳定** | 上游行为非 mirvm 债；用稳定 compact aggregate 绕；JSON 作 oracle 前须先解决确定排序 | real-projects.md §6 |
| R15 | **`ClosureFnPointer` 等 track_caller 外 adjustment 未支持** | `ReifyFnPointer` 只走 rustc `resolve_for_fn_ptr`；不能由此外推 | current-status §4 |
| R16 | **global_asm `sym` 拒绝面残余（C7 闭合后）** | ①`sym` fn 指向签名不可派生（聚合/Rust ABI/变参）的 guest fn——机器码调此类同形本即 UB，响亮拒绝；②`sym` static 指向 guest static 未接（mangled 静态名审计仍会命中），按 workload 再立；③ **dep crate** 的 global_asm/naked 中 `sym` 指向 dep 自身 guest fn——条目预算须在 bin 链接上下文做，当前仍响亮拒绝；pulp 等真实形态零操作数，未遇阻塞 | src/lower/global_asm.rs，decision-history §7.9/§7.23 |
| R17 | **FFI 按值封送残余边界（C1 闭合后）** | union 按值（SysV union 分类另规则）、SIMD 向量按值、变参尾参位聚合、align>8 聚合、multi-variant enum 按值——五形态 freeze 响亮 Err（文案可鉴红分类）；`{i128}`/f128/long-double/_Complex 既有标量边界不动。**2026-07-22 增第六形态**：packed/align(N) 非自然布局聚合（audit F-06——libffi 类型系统只能表达自然布局，冻结校验 `validate_agg_natural` 已把此类从静默错调改为 freeze 响亮拒绝；完整 padding 表达按真实 workload 触发再立）。各形态同 helper 可扩 | src/lower/ffi_sig.rs，designs/c1-ffi-agg-design.md §0 |
| R19 | **`#![no_main]` / `#[start]` 入口形态拒绝**（2026-07-22 登记） | 入口类型非 `EntryFnType::Main` 一律响亮拒绝（exit 1 + 诊断，src/cli.rs:489）；嵌入式/bootloader 式入口形态无 corpus 实锤，重开需真实 workload | src/cli.rs |
| R20 | **libffi foreign/callback 仅支持 C/System ABI** | C/System 的 plain/unwind 两形均已闭合；其他 ABI 不再静默压成 plain C，而是在 lowering 阶段明确拒绝。重开必须为目标 ABI 建立真实 adapter 和 native 对拍，不能凭当前平台机器形状看似相同直接放行 | designs/c-unwind-contract.md，src/lower/linker/calls.rs |
| R21 | **异步 signal 支持面边界**（2026-08-13 定稿） | 当前闭合的是 Linux/ELF/x86_64 上传统、无 guest 高级 flag 的进程定向和线程定向 handler。进程定向事件进入 callback owner 的 inbox（注册 Engine 的待处理信号箱）；`pthread_kill`/真实 libc `raise` 的 `SI_TKILL` 进入目标 pthread 按本次 handler 安装建立的稳定 cell（线程槽），只能由该线程在安全点或退出时执行。未阻塞的 `HostRaise`（MIRVM 承接的 `raise`）返回前完成；阻塞时事件留在内核，`sigwaitinfo` 仍能观察真实 `SI_TKILL`。close 等待已接收的目标线程事件，不能换线程代跑；若当前线程自己仍有事件，`wait_closed` 返回 `ActiveOnCurrentThread`。pthread 退出按 glibc 全局末轮的原始 key 号顺序交替收口 TSD 与 signal，最后才物理阻塞可捕获信号、复查并关闭 inbox。固定桩、registration（一次 handler 安装的登记对象）和线程槽进程期保留；owner 关闭后回装旧桩时，裸内核投递 `_exit(70)`，`HostRaise` 报 `EngineFault(70)`。**仍拒绝**：同步故障 guest handler；realtime（需要逐事件排队并保留 `siginfo`）；`SA_SIGINFO`（需要三参数 guest ABI）、`SA_ONSTACK`（需要替代栈生命周期）、`SA_NODEFER`/`SA_RESETHAND`（会改变 mask/注册状态机）。进程定向外部信号只承诺在 owner Engine 下一普通安全点派送，不承诺原生 handler 级即时延迟；不能退回信号帧直接跑 guest | current-status §4，decision-history §7.54-§7.55 |

## G. 维护态与基建

| ID | 事项 | 内容 | 出处 |
|---|---|---|---|
| G1 | **GitHub/远程全面暂停** | issues/PRD/PR/`gh` 一切操作冻结至维护者恢复；恢复后待办：corpus manifest 入库、来源/rev/lock/许可重审、远程持续 gate、triage 恢复 | AGENTS.md |
| G2 | **corpus/真实项目证据为 Git-ignored，非持续 gate** | harness 暂缓集：suite inventory、对象 GC/保留策略、跨主机 cache、NFS/对象存储耐久、远程 gate；CheckID 有界闭包（Cargo fingerprint/完整 sysroot Merkle 成差异源时扩展） | real-projects.md §6，decision-history §5 |
| G3 | **jieba_cut / opencc 全绿但未接线自动 gate** | jieba 单跑 77-89s 贴 timeout 留 corpus.sh；opencc 需 /tmp/opencc-local 前缀，留 corpus.sh 手工批有 gating | corpus §5 |
| G5 | **A2 后评估三触发器 + purity 账本复核未做** | ①clap-derive 类 tainted 巨集 → 项目本地 tainted image；②跨项目共享无实需 → 简化回单项目键；③ripgrep/tokei purity 账本复核 | decision-history §7.5 |
| G6 | **zxcvbn 上游 exact-tie 非确定（登记防误判）** | scoring.rs 对 u64::MAX 饱和并列取 HashMap 迭代序，native 自对拍都不稳；已离饱和区，非 mirvm 债 | corpus §5 批5 |
| G7 | **真实 workload 可判定性提升**（audit V-01/V-02，2026-07-22 登记） | `未立项` 现状（已如实）：corpus 129 项的三维逐字节差分只在 driver 创建时执行，持续门 = 默认 mirvm 单跑 exit-code/oracle 级；frontmatter 依赖仅 `MIRVM_CARGO_LOCKED` 置位才 `--locked`（CI 未置位 = clean runner 可重解析）。release acceptance 前需：选小而固定的代表 crate 集、钉依赖 lock、持续三维（解释/JIT compiled-entry/native）可复现；不把 129 项扩成昂贵通用门 | history/development-status-audit-2026-07-22.md §7，docs/corpus.md:118 |
| G8 | **清扫遗留：用户可见文本里仍带里程碑标签**（2026-09-19 登记） | （a）`src/cli.rs` 的 `USAGE` 仍写 `mode B slice 2`、`M5.3-M5.5`、`D15 … P4 default flip`、`M4 precursor spikes`、`docs/history/spike*.md`——无测试依赖，属输出措辞改动，未在只做翻译的那一轮里动；（b）`src/native_archive.rs` 两条错误串仍含 `M5.1`（`+/-export-symbols` 修饰符与两条强定义）；（c）`src/lower/linker/{mod,entries}.rs` 的 panic 串仍含 `A2`、`M4.4` 标签；`src/cargoless/driver.rs` 一条错误串仍含 `(P5 boundary)` | decision-history §7.64 |
| G9 | **需要编译器的精简候选**（2026-09-19 登记，未删） | （a）`src/cargoless/{lockfile,manifest}.rs` 的模块级 `#![allow(dead_code)]` 可能掩盖真死码（哪些模型字段没人用只有编译器知道）；（b）`src/cargoless/resolver_config.rs` 整模块 `#![cfg(test)]`、无消费者、与 `config.rs` 的 resolver 策略/include/环检测逻辑重复——候选整体删除；（c）`src/vm/engine/jit/helpers.rs` 的 `(lo,hi)` out-store 重复约 10 处、`trap_if` 的 `_msg` 参数从不读、`STAT` 初始化可用 const-block 数组形式；（d）`src/vm/engine/jit/translate.rs` 的 `addr_of_local` 单用包装；（e）`src/baseimage.rs` 的 `ImageStack::from_images`/`push` 重复并集合并、`src/elfsym.rs` 两趟近乎相同的 ELF64 遍历；（f）`src/telemetry/capture/session.rs` 的 `#[allow(dead_code)]` 已过期（`pending_rebuild_recipe` 有活调用者）、`src/vm/engine/ctx/thread_ctx.rs` 有重复的 `#[cfg(test)] #[cfg(test)]`。每项都因"只看不改无法证明"被留下 | decision-history §7.64 |

> G4（stale 绕行回摘两颗钉：exr half 钉、flate2 gz/zlib 原生 API）已于 2026-07-18
> 回摘关闭并三维验收，移出本表（证据见 decision-history §7.9）。

## F. 条件触发型重开项（触发器速查）

| 触发条件 | 重开什么 | 出处 |
|---|---|---|
| 栈式协程/continuation、独立 VM 栈收益出现 | Frame model B 重评 | decision-history §2，designs/frame-stack-models.md §5.5 |
| D16 选择分配/guest TLS 快路径内联，**或**多 Engine 嵌入立项（双闸，先到先裁；用户 2026-07-21 裁定） | vmctx R 缓存层复测（T3 已闭合，T 骨架生产定稿） | designs/vmctx-passing.md §7，decision-history §7.20 |
| 真实 workload 同时证明 guest activation 长期不返回宿主，且必须在其运行中动态开启/停止 timeline | 重开 OSR/可重建代码域迁移；不得用默认 JIT 每块轮询绕过 | designs/mirvm_high_performance_log.md §5.2.3，decision-history §7.56 |
| 已经处于 trace 域的长期 activation 提出明确的最大事件可见延迟，且页满/自然返回发布不能满足 | 重开 active-page 发布策略，以真实负载比较每 K 条 watermark 与 deadline；不得先加每事件低延迟开关 | designs/mirvm_high_performance_log.md §5.3.1，decision-history §7.56 |
| v0 第一次需要由第二版 producer/consumer 读取，或准备承诺稳定格式 | 进入日志 schema 兼容、扩展名、字典与迁移工具设计 | designs/mirvm_high_performance_log.md §12.3 |
| L4 完成后出现必须观察 libc 内部、opaque archive 或全进程 syscall duration 的真实诊断 | 施工独立 kernel raw-syscall stream 与无歧义关联，不扩写内部流冒充 | designs/mirvm_high_performance_log.md §5.9/§12.3 |
| P2 只能显示解释器宿主热点，不能归因 guest 逻辑位置 | 先做安全点 logical sampling；只有偏差实测不可接受才进入 signal sampling | designs/mirvm_high_performance_log.md §9.3/§12.3 |
| 首个长时 capture 提出磁盘上限或掉电恢复要求 | 施工 rotation、byte cap、final fsync 或持久黑匣子中对应部分 | designs/mirvm_high_performance_log.md §12.3 |
| 动态 capture 会话或 trace code/producer descriptor 可随进程寿命无界增长 | 施工完整 epoch 回收；此前只允许有界 tombstone | designs/mirvm_high_performance_log.md §12.3 |
| T12 证明稳定扫描、`pwritev` 或调度是主瓶颈 | 分别挑战 ready-page MPSC、staging/io_uring/mmap/compression 或调度参数 | designs/mirvm_high_performance_log.md §12.2–§12.3 |
| 真实解释器负载证明局部存储是端到端瓶颈，且 native 栈方案计入清零、栈探测、unwind 与 checked 成本后仍显著更快 | alloca 帧局部存储候选重开（原 E12） | decision-history §7.49 |
| pinned rustc 改 summary 结构或出正式诊断协议 | runner 诊断 hook 改 guard/正式接口（→E22） | decision-history §5 |
| pinned toolchain 升级 | D9e emit 剪枝重测 | decision-history §7 |
| 「单次运行、超大 delta、不可预降」负载形态实测（REPL/宏展开型） | 懒降低候选 A 重启（前置：L2 statics/const 分域 + 会话生命周期重设计，两件单独立项） | history/m5.3-design.md §3.1 |
| fixed stdarch helper 数量显著膨胀，或某指令无 stdarch/CLIF 表达 | D7b 通用 asm-stub 向量 ABI 重开 | history/m5.1-design.md |
| 跨项目 S3′c 共享成为硬需求 | 方案 B（chain + 构建屏障）备选复活（WIP 存 [parked/s3b-chain-wip.patch](parked/s3b-chain-wip.patch)） | history/s3b-design-fork.md §6 |
| 「大用户 crate 编辑-重跑」场景 | `-Cincremental` 脚本路径（→D12） | history/coldstart-research.md |
| 真实 MMIO/设备寄存器 workload | 宽 volatile 分块「不承诺原子性」重开 | decision-history §5 |
| suite inventory/集合身份出现真实需求 | harness 暂缓集（→G2） | decision-history §5 |

## H. 定型否决与已关闭（防重提速览）

| 事项 | 裁定 | 出处 |
|---|---|---|
| 地址模型 P3（VM 层内存隔离） | 永久冻结：题设不可兼得，VM 不掺和 | decision-history §7.5b |
| 地址模型 P4（指针非确定） | 判非问题；字节级回放 = 线性内存+JIT 影栈+FFI 纳管+单线程四联工程，按需再立 | decision-history §7.5b |
| 地址模型 P5（固定基址样条） | 维持现工程；增量 → D13 | decision-history §7.5b |
| 虚拟地址模型 / 线性内存折中 | 响亮否决（FFI 轴原理性障碍） | decision-history §7.5b |
| 调用时懒降低 v1 / 流式降低 | 不建（J2 三重反对：D3 契约/L2 洁净快照/收益塌缩）+ 否决 | history/m5.3-design.md |
| per-crate chain 成像 / relocation 路线 C/D | 4/19 命中率证伪 / 硬伤否决 | history/s3b-design-fork.md |
| L2 MPK/PKU、L4 进程沙箱 | 砍（2026-07-05 用户定）；checked 只查 raw 解引用为诚实界 | designs/concurrency-arch.md，DESIGN.md C13 |
| m5-design 备选枪毙 | 进程内汇编器 crate / 自写 `.a` 装载器（重发明 ld）/ blake3 prefer_intrinsics 回避 / P 调用约定 / 抄 cg_clif 整段 `__register_frame` | designs/m5-design.md |
| GIL-over-真线程 tier-0 / wasmtime-unwinder 路线 | 死题（引擎 Sync 当日直证）/ 不采（与宿主 unwinder 不互操作） | DESIGN.md §5，history/spike5-cranelift-adapters.md |
| tokei JSON 次序语义 | 用 compact aggregate 绕，不修（→R14） | real-projects.md |
| harness 隔离非对抗性 | 不 unshare IPC、信任固定 provenance：声明边界，非对抗安全边界 | real-projects.md §6 |
| mimalloc ctor CFLAGS 绕行 | 确定性保留（`MI_PRIM_HAS_PROCESS_ATTACH`；constructor 解码后可选不必要改） | decision-history §7.8 |
| sysroot stamp 同 toolchain 改 rust-src | 不在防护面（逃生门 = 删 stamp/sysroot） | history/m6-log.md |
| `abort` 信号差异（R11）/ TSan TSD 绕行（R12） | 授权差异 / 测试规避 | history/m4-log.md |
| JIT 原子序、Cranelift 内联与退出排队 | JIT 原子统一 SeqCst 是合规强化；Cranelift 自有内联保持关闭；进程退出不等待纯优化编译队列，解释结果不受影响 | decision-history §7.47 |
| 手写 host shim 全部改写 | 通用符号走 dlsym+libffi；需要 guest 语义或热路径的专用 shim 保留，不以代码形式统一为目标 | decision-history §7.47 |
| OSR/deopt/生产 tiering 与 JIT 逐函数释放（原 E2/E5） | 当前接受：无调用边界的长跑单循环不会中途 JIT；Engine close 会停止 worker、释放 Shared，但 Cranelift 已发布机器码和 `.eh_frame` 保留到进程结束，因为可能仍有休眠 native 栈/展开器引用。前者只在真实长跑负载要求 OSR 时重开；后者若要求严格进程内有界，须先证明所有外来入口和栈引用都已撤销，或采用进程隔离 | decision-history §7.48/§7.53，designs/frame-abi-bytecode.md §10.4 |
| 解释帧必须从 slaved ByteRegion 迁到 alloca（原 E12） | 撤销“必须迁移”：解释器正式使用带 guard page 的每线程 ByteRegion；JIT 已使用 Cranelift native 帧与 SSA。alloca 只是真实解释器性能瓶颈出现后的候选，不是架构终态 | decision-history §7.49，designs/frame-abi-bytecode.md §2.2 |
| guest 异常类 / personality（原 E13） | 已采用 MIRVM 独立异常类和原始异常分类器：外壳区分 guest panic/`EngineFault` 并携所属 Engine，内层 guest panic 对象仍由 guest std 捕获或释放。**没有采用独立 personality**；解释/JIT cleanup 继续复用已有 Rust personality 与 LSDA。每个解释器 raw catch/JIT landing pad 按当前异常指针决定是否 cleanup，TLS token 栈只做 owner/LIFO 记账；lower 精确标出的 `MainPanicBoundary` 与每次 `run_main` 状态栈另行解决真实 main panic 和正常 101 混淆。这样解决多 Engine 串线、同线程嵌套故障与结构化出口，不声称能让 guest `catch_unwind` 捕获 C++ exception | decision-history §7.50-§7.52，designs/c-unwind-contract.md §7 |
| C-unwind 边界残余（原 R18） | 已闭合：direct foreign、native fn pointer、callback/P1 与解释/JIT cleanup 均按源 C/System ABI 分治；普通 C 保持终止，C-unwind 保持异常身份并传播，其他 ABI 响亮拒绝。C++ typed exception 可原样穿出整个 Engine；到达 guest `catch_unwind` 则按固定 rustc 终止。旧“libffi closure 无 unwind info”前提已被捆绑 libffi 真机实测推翻 | decision-history §7.50-§7.52，designs/c-unwind-contract.md |

## 旧编号对照（m4-debt-map.md 已删，本文承接）

| 旧条目（docs/m4-debt-map.md，git 历史可查） | 现落点 | 状态 |
|---|---|---|
| §0–§5 M4 Trap 普查（2026-07-07 快照） | 精髓在 [history/m4.1-design.md](history/m4.1-design.md) F1–F7 与 [history/m4-log.md](history/m4-log.md)；普查方法与 §2 三发现已被施工兑现 | 历史 |
| §6 thunk 盲区（结构体内嵌 fn-ptr） | 已根治（P1 条目可执行化 `4202317`，decision-history §7.6）；残余阶梯 → T4 | **已关闭** |
| §7 dep crate global_asm | C4 已于 §7.23 闭合；残余 → R16 | 已关闭主项 |
| §8 JIT 间接调用准入 | → E1 | 已关闭（decision-history §7.19） |
| §9 FFI 按值聚合封送 | → C1 | 已关闭（decision-history §7.10） |
| §10 ① 符号在 rlib ② asm noreturn | → C2 / C3 | 已关闭（decision-history §7.11/§7.12） |

## 附录：考古来源与方法

2026-07-17 全数过录以下文档的「未解决/未立项/绕行/拒绝/待施」标记并逐条核对现状：
`DESIGN.md`、根 `README.md`、`docs/` 全部（current-status、decision-history、corpus、
real-projects、m4/m5 各设计与日志、spike1–5、coldstart-research、s3b×2、s4、distribution、
m4-debt-map、AGENT-HANDOFF）与 `docs/designs/` 六篇。已过录条目均在上表；「可能已修」疑点
已现场核实（见 G4/E27/C7 等条）。corpus 逐 crate 绕行记录另见 `corpus/c_*.rs` 头注。
后续新债务直接登记本文对应分区；关闭条目时连同行移出并在 decision-history 留证据。
