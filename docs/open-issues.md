# mirvm 未解决债务与开放问题登记册

> 覆盖 mirvm 自始（M0 tier-0 时代）至 2026-08-10 当前复核的
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
- **E23 checked 模式**：可闭合，但闭合界 = 其声明契约（raw 解引用范围检查），
  不是完备 UB 防线——该契约必须如实写进验收标准。

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

**T 区当前清空**——T1–T3（M5.4c/d + M5.5）与 T5（syscall 拦截）已全部闭合
（2026-07-21，decision-history §7.18–§7.20）；T4（SIGSEGV 诊断化兜底）非排期项，
2026-07-19 起已并入 R1 崩溃诊断化总案（见 R1 条，不再单列）。闭合证据与重开
条件均在 decision-history 对应节。

## C. corpus 实锤产品欠账（实锤驱动，未立项）

| ID | 事项 | 关键内容与转正要件 | 出处 |
|---|---|---|---|
| C6 | **M5.x intrinsic 按需队列残余** | pclmulqdq.256/.512、vaes、其余 gather 形态、avx512.pmadd 系等：遇真实 workload 按既有四触点法补（已清先例：psad.bw/pclmulqdq/aesni/crc32/permd/gather/vpmadd52/F16C/lddqu/`2b4766b`）。AES 等未触发项保留响亮 Trap | corpus §5，history/m5.1-design.md §1 |
| C8 | **Rust 侧 ctor / `.init_array`（linkme 族）未触发** | C 原生归档侧 constructor 已由 decision-history §7.8 分治放行（DT_INIT）；裸 `.init`/`.fini` 仍拒。Rust 侧 ctor/linkme 从未进 corpus，按需立项，不预支 | history/m4.5-plan.md D6（已删，git 历史），§7.8 |

<!-- C1/C2/C3/C5/C7 已闭合（2026-07-18，decision-history §7.10–§7.13/§7.9），
     按「只收未解决」规则移出；残余边界分别转 R17/R16/E32。 -->

## E. 引擎与架构欠账

### E.1 JIT / 引擎内部

<!-- E1（JIT 间接调用准入）已闭合 2026-07-21、E21（os/arch 双 leaf）已闭合
     2026-07-18/19，按「只收未解决」规则移出——证据与重开条件见
     decision-history §7.19/§7.16 -->

| ID | 事项 | 状态与内容 | 出处 |
|---|---|---|---|
| E2 | **无 OSR/deopt/生产 tiering/后台 JIT 服务线程** | `记账` 长跑单循环 main 永不触发 JIT（无调用边界、无 OSR）记账接受 | designs/frame-abi-bytecode.md §10.4，history/m5-log |
| E3 | **JIT 帧不压影子帧** | `记账` panic 在 JIT 帧内展开时列帧少于解释口径 | history/m5-log.md M5.3 |
| E4 | **CLIF 原子统一 SeqCst** | `记账` Cranelift 0.133 无弱序；JIT 侧全部最强序（合规强化，无分歧） | history/m5-log.md |
| E5 | **JIT 机器码随模块常驻** | `记账` cranelift-jit 不支持逐函数释放，进程生命周期记账 | designs/m5-design.md |
| E6 | **分配快路径/guest TLS 快路径内联未做** | `未立项` m5-design D5 格③空（分配走 mirvm_alloc 助手、TlsRef 走助手）；手卷 TLAB 同题（E14）。**2026-07-21 身份升格 = vmctx 复测双闸之闸①**（vmctx-passing §7；立项时以格③真实负载复测 T vs R，对照基线 = §7.20 计量：corpus alloc 7.82M / tls_ref 0.38M） | designs/m5-design.md §3，decision-history §7.20 |
| E7 | **JIT 优化项池** | `未立项` SwitchInt 用 br_table 替代 icmp+brif 链；取址逃逸精化（Q1 残余优化触发器）；CallIndirect 内联缓存（挂 T3）；LSDA 存储升 JIT data object（挂 T3）；**2026-07-21 增**：SIMD 净映射家族 CLIF 向量内联（Q4 原案——T1-d 以全助手先行锚定正确性，提升纯属性能）；CallIndirect/Call 的 PLT try_call_indirect 快路（T1-c v1 统一 c2i-try_call 的预留项）。**当前实锤触发器（2026-08-10）**：标准 `performance.limits` 在热缓存下跑 `fib(32)` 最快约 **97ms**，超过既定 **80ms** 硬门；`MIRVM_TIMING` 约为 cache-load 52ms、engine 32ms，输出正确且 JIT 仍比关闭时约 1050ms 快。不得通过放宽门槛关闭；复现：`./tests/run.sh suite performance.limits` | designs/m5.4-design.md，tests/suites/performance/limits.sh |
| E8 | **backtrace 真符号化 + 合成 IP 近似** | `绕行` 合成 IP 不经 dladdr（诚实 `<unknown>`，不伪造宿主符号）；真符号化（物化符号 ELF 给 dladdr）可选未立项 | current-status §4，decision-history §6.1 |
| E9 | **`dl_iterate_phdr` 差分探针欠账** | `绕行` 走 native FFI「无观测到缺陷」，探针补课未做 | current-status §4，history/m5.2-design.md |
| E10 | **guest TLS 实例块回收** | `记账` 每线程每 TLS 一小块泄漏（dtor 副作用已正确）；M4.4 起挂账「按需」，至今未立项 | history/m4-log.md:439 |
| E11 | **guest 线程栈大小精确化 + 编译帧 SIGSEGV 优雅化** | `未立项` guest 栈大小语义近似（**栈深度与 native 逐字节一致 = UNSPECIFIED 域，2026-07-19 用户确认不追求**；可闭合的只是诊断与 `--stack-size` 配置语义）；JIT 编译帧撞 guard page = 裸 SIGSEGV（与 native 差一条报错消息）。同根：栈守卫精确化 | history/m4.4-design.md，m5-design #11 |
| E12 | **alloca 迁移承诺（换掉 slaved 操作数区）** | `未立项` frame-abi §2.2「slaved 仅是起步，后续必换 alloca（真内联 native 栈）」；与 checked 模式正交轴纪律在案 | designs/frame-abi-bytecode.md，DESIGN.md C12 |
| E13 | **guest 异常独立 exception class/personality 无裁决** | `未立项` 是否独立于宿主 panic + 自有 personality——spike3 §4.1 是全库唯一登记处，从未裁决（现共用宿主 panic + downcast） | history/spike3-mixed-stack-unwind.md |
| E14 | **hand-rolled TLAB 未立项** | `绕行` v1 = mimalloc crate 后端；chunk/大小类/remote-free 队列细节无；与 E6 关联 | history/spike1-model-a-skeleton.md，designs/concurrency-arch.md §9 |
| E15 | **`--vm-stats` fn-ptr 无出边盲点** | `绕行` 仪器债：fn-ptr 间接调用无出边 → 债务读法永远「至少欠这些」；继续靠增量发现 | src/vm/engine/stats.rs 头注 |
| E16 | **io_uring 直通未实证** | `未立项` tokio-uring 可选路径全库仅 designs/async-stackless.md §5.2 提及，无 corpus 对拍 | designs/async-stackless.md |
| E17 | **L2 缓存两处** | `未立项` ①有告警/错误的会话拒入账、诊断回放未做（告警程序永不享缓存，session 门函数在 src/cli.rs:395、计数器在 :368）；②条目无逐出——**手动 GC 面已由 `mirvm cache purge`（默认清陈代）补上（§7.14）**，自动 LRU/容量上限不立项 | history/m6-log.md 片2/8 |
| E18 | **Cranelift 自有内联（0.133.1 inline.rs）备用杠杆** | `拒绝` 默认不开，记为收口期备用杠杆 | designs/m5-design.md |
| E19 | **rustix 裸 syscall vs os:: 收口的张力** | `记账`（2026-07-21 定稿，**③④ 已由 T5 闭合**）：**「mirvm 拦截一切 syscall」在真实生态成立**——① FFI libc 包装：builtin 注册表即现成挂载点（HostWrite/HostGetenv/HostFork/HostSignal/HostSyscall 在产拦截）；② `libc::syscall(...)` 变参：`Builtin::HostSyscall` 单点内建；③ guest inline-asm 裸 syscall（rustix linux_raw）与 ④ global_asm/naked 内：**已拦截**（T5 `d0470fc`：asm-stub 文本生成点改写 → GOT 两级间接槽 → trampoline 全契约 → dispatch v1 直通 + TRACE；探针三维一致 + rustix 系零回归）；⑤ vendored C 库：常态（C 调 libc 包装）经 native_archive 链接序插桩可闭合，罕见叉（C 内联汇编自写 `syscall` 指令）cc 产物不透明；⑥ JIT 与①③同入口；⑦ 对抗式自修改/`.byte 0x0f,0x05` 书写无真实形态。**唯一如实残余 = ⑤罕见叉与⑦，只有 OS 层 seccomp 能兜**（维持原判）；虚拟化语义（统一 fd 空间/假 FS/计费）属 D10 本体，钩子已备 | corpus §2.3，§5；decision-history §7.18 |
| E20 | **字节码验证 pass 未建** | `未立项` loader 鲁棒性开放问题；内容寻址缓存已由 S3′b A2 兑现，独立验证 pass 无实锤驱动 | designs/frame-abi-bytecode.md §10.6 |
| E37 | **日志系统 v2（[designs/mirvm_high_performance_log.md](designs/mirvm_high_performance_log.md)）** | `未立项` v1 同步路径继续在役；2026-08-07 已把 `mirvm_log!` 同源接入 TSan crate，原纯度门禁编译红消失并实跑零竞争通过。v2（ring + 消费者线程 + feature 闸门）仍归 D16 后台服务线程同设计；文档数字全系量级估算（其 §6 自述），§7 基准随 D16 profile 实测 | decision-history §7.32/§7.33 |

### E.2 架构与边界

| ID | 事项 | 状态与内容 | 出处 |
|---|---|---|---|
| E22 | **多 Engine 嵌入 API / 生命周期收敛** | `未立项` Shared/thunk/asm handle/部分 TLS 进程期泄漏；错误径可退进程；runner `TRACK_DIAGNOSTIC` 进程全局单槽（daemon/嵌入/并发 compiler 前必须带所有权 guard）；dlopen 嵌入 TLS model 退化同题（vmctx §3.2） | current-status §4，decision-history §8 |
| E23 | **checked 模式（L3）未建；L1 guard page 未建** | `未立项` region-check 设计储备在 designs/concurrency-arch.md §6；L1 目前只有固定地址域分池，无 PROT_NONE guard；正式沙箱归 OS 层（P3 VM 层已永久冻结 → H 节） | DESIGN.md C13，decision-history §7.5b |
| E24 | **Cargo wrapper composition fail-closed** | `拒绝` 非空 `RUSTC_WRAPPER`/`RUSTC_WORKSPACE_WRAPPER` 或有效 build.rustc-wrapper 一律拒绝，不做 wrapper 链组合；重开 = 可重开的实现选择 | current-status §4，src/cargo_shim.rs:63 |
| E25 | **`MIRVM_ENCODED_RUSTFLAGS_APPEND` 未进 Cargo fingerprint；build/env 未分离** | `未立项` 改值可能复用旧 fake binary；mirvm build env 与 guest runtime env 尚未彻底分离 | real-projects.md §6，current-status §4 |
| E26 | **平台仅 Linux/ELF/x86_64** | `记账` pthread/dlopen/GNU 链接/x86 asm wrapper 依赖面；unwind「跨平台无痛」未逐平台验证；macOS 次之后议 | current-status §4，DESIGN.md §11 |
| E28 | **`__rust_alloc_error_handler`（kind=Global）路由未动** | `未立项` 仍走 ③ Trap（src/lower/builtins.rs）；首个 workload 触达时再立项（`__rust_*` 族已由 §7.7 统一路由，本条是例外残余） | decision-history §7.7 |
| E29 | **手写 shim → 通用直通通道** | `未立项` neat 终态（polish，不急）；现行为手写 shim + dlsym 兜底 | DESIGN.md §7.2 |
| E30 | **C7 regex 42s 基线 JIT 后未复测** | `未立项` 旧性能靶子；JIT 之后无重测记录，「评审须给预估收益」要求未兑现 | DESIGN.md C7 |
| E31 | **A2 mtime 粒度传递依赖漏检残余风险** | `记账` 键安全论证承认 mtime 粒度残余；挂档（distribution §6 既有条目同案） | history/s3b-a2-design.md §9.4 |
| E32 | **inline-asm setjmp/longjmp 的捕获帧内存复用 hazard（C3 定稿边界）** | `记账` asm-stub 模型下 setjmp 捕获点在 stub 包装帧；解释帧在捕获与恢复之间复用该宿主栈内存的合成协议可撞死（v2 spike 实锤，落点 `Channel::send` 内部）。真实 workload（wasmtime 全 trap 面）不发生该形态、三维确定性绿。消除 = JIT 真帧身份（compiled guest fn = native 帧语义）；不宣称全形态闭合。**进展 2026-07-21（T1）**：JIT 帧已是真 native 帧（含真 unwinder 穿透/着陆，双 CIE + 全覆 LSDA）——setjmp/longjmp 所在函数一旦发布即脱出 hazard 面；interp 帧路径维持原记账 | [parked/c3-resume-spike.md](parked/c3-resume-spike.md)，decision-history §7.19 |
| E33 | **unsafe trust-boundary 优先审计**（audit M-03，2026-07-22 登记） | `未立项` ~475 个 unsafe block、显式 SAFETY 注释仅 5 处——真实地址模型/FFI/ELF/asm-stub/unwind 决定大量 unsafe 不可避免；正确策略非机械补注释，而是优先审计 FFI、全局 Shared、ELF 解析、固定地址映射、thunk、unwind 五个信任边界，为每个实际不变量补最小证明或测试；ASan/fuzz 类保证无实锤前不立项 | history/development-status-audit-2026-07-22.md §9 |
| E34 | **JIT 翻译器大 match 治理**（audit M-04 关联，2026-07-22 登记） | `记账` jit/translate.rs 2,939 行单文件三 match 是维护热点；分族拆文件的收益/扰动比未评，结构重构战役（E21）模式可复用，下次大改前评估 | src/vm/engine/jit/translate.rs |
| E35 | **编译 worker FIFO 排空竞态**（2026-07-22 登记） | `记账` 短命程序退出时编译队列可能未排空——已投递函数永不发布（语义零影响：解释兜底正确；仅 JIT_DEBUG 口径观察与短程序性能非确定）。处置方向 = 退出前 drain 或记账接受，无实锤驱动前不动 | src/vm/engine/interp/mod.rs:414 |
| E36 | **cargo 项目模式 guest cwd=项目目录，与 cargo run 语义分叉**（2026-07-23 测试管线整顿实锤） | **self 路径已闭合（D15 P2，decision-history §7.29）**：cargoless driver 全程不 chdir——guest cwd=调用者 cwd，与 cargo run（含 --manifest-path）语义一致。**长期保留的 compat 路径仍有欠账**：phase_cargo 以 current_dir=项目目录驱动 cargo，runner 继承之 → guest cwd=项目目录（`mirvm run <项目目录>` 时 guest cwd=项目目录；而 `cargo run` 从不 chdir——argv 探针实锤两语义分叉；corpus/projects 对拍以 {ROOT} 绝对化夹具路径绕行，harness 层合法）。不能再等删除 compat 自然消失，后续应在 compat runner 协议内传回原调用目录并补对拍 | `tests/suites/corpus/cases.manifest` {ROOT} 注，decision-history §7.26/§7.29/§7.37 |

> E27（weak 符号真地址化缺定向验收）已于 2026-07-18 关闭并实修「weak extern
> static 恒 0 判空 cell」缺陷——现走 GOT 启动相真解析（命中=真址/缺席=0；引擎
> 接管符号强制缺席），探针 `demo/weak_extern.rs` 三维绿（decision-history §7.9）。

## D. 分发与产品面

明确缺失能力的合并施工顺序与逐阶段验收口径见
[产品能力补全计划](designs/product-capabilities-plan.md)。本表仍是各项债务状态的唯一真源。

| ID | 事项 | 内容 | 出处 |
|---|---|---|---|
| D2 | **发行形态与命名（D9f⑤）** | 先 miri 式后 JDK 式自包含 tarball（成熟后）；kit 命名候选 MDK/mirvm toolkit（MRsDK 已否决） | designs/distribution-design.md |
| D3 | **零拷贝装载（rkyv 类）→ D16 主杠杆** | postcard 解码封顶（eco ~60ms / ripgrep ~450ms）；mmap+逐函数惰性解码是下一数量级唯一杠杆；与 mode B 同题。**2026-07-29 升格（§7.32）**：包布局 mmap 直读 + 逐函数惰性解码 + 预测序预取（上次运行真实触发顺序落 cache，预测错只慢不错）+ demand 插队队首（等待上界 = 一个在跑函数编译完成；无抢占断点机制——Cranelift 无中断续跑接口）——「entry 先跑、后台并发、按需插队」在模式 B 成立（无 tcx）；模式 A 懒降低维持 J2 否决不复活 | history/coldstart-research.md V6，m6-log 片8，decision-history §7.32 |
| D4 | **对外格式冻结重估** | M5.3 收官触发器（2026-07-15）已响，被有意再推迟到 mode B 立项；**2026-07-29 排序裁定（§7.32）：排在 D3 零拷贝布局评审之后**，否则冻结后必为布局改版 | decision-history §7，§7.32 |
| D5 | **L3 JIT 机器码缓存** | 禁令条件「M5.3–M5.5 定型前禁做」已随 M5.5 收官（2026-07-21）消失；**2026-07-29 判为 dev 循环最大单根杠杆**（热函数每进程重烧 = 纯白烧），既有 MC 机器码节 + 进程内 ELF 装载器已证 JIT 产物可序列化再装载；归 D16 候选 | designs/distribution-design.md，history/m5.3-design.md，decision-history §7.32 |
| D6 | **S3′c 完整形态（跨项目共享）** | 路径无关内容哈希键（~50ms/次）+ tainted 层/多层 image 合并；按实需立项（已兑现的只是同 workspace 跨 bin 冒烟） | current-status §5.8，history/s3b-a2-design.md §3.4 |
| D7 | **frontend 相成本无杠杆认领；V5 `-Zthreads` 并行 lower 未立项** | eco ~143ms / ripgrep ~730-800ms 在账无人认领；V5（tcx DynSync+worklist rayon 化）在 V3 不建后无人重启 | history/coldstart-research.md，m6-log 片8/10 |
| D8 | **8 个 correctness case 未 benchmark** | ripgrep_gzip/parallel_nomatch/mmap_binary/parallel_match/multiline_replace、tokei_sort_code/streaming_json/rust_files | real-projects.md §5 |
| D9 | **registry 依赖 crate 底座化 / 自适应底座 / 底座 AOT 机器码入 base** | S4 三未立项方向；第三条与「机器码不入缓存」世界观冲突，立项前先核 | history/s4-base-image-design.md §0/§4 |
| D10 | **M3 产品面** | daemon、agent API、资源治理、正式沙箱、虚拟化钩子（假 FS/路径重定向/计费——区分 guest 调 open 与解释器自读缓存） | DESIGN.md §9，§7 沙箱节 |
| D11 | **REPL/Notebook + 嵌入 API（M7+）** | 原愿景 M6 编号已被轨 C 冷启动占用；REPL = 持久堆天然成立，未立项 | DESIGN.md §3/§9 |
| D12 | **`-Cincremental` 脚本路径** | 非当前杠杆；触发式重启（「大用户 crate 编辑-重跑」形态）；`finalize_session_directory` 坑在案 | history/coldstart-research.md §4 |
| D13 | **地址模型 P5 增量（扩域/回收）** | 维持现固定基址样条工程；增量能力记 M7+ | decision-history §7.5b |
| D14 | **原生内容寻址依赖存储（统一依赖 cache 终态；用户 2026-07-18 裁定方向）** | 去重单位 = 完整编译键（crate 版本 × features × 依赖闭包 × cfg/flags × toolchain）：多脚本/多项目共引 X@V 时其构建产物机器级唯一。**近期片已落地（§7.15：共享 cargo target dir，fingerprint 即编译键内容寻址；实测 ethers 二跑 0.66s、两树并集 525M）**；**终态 = mirvm 原生 store（`~/.mirvm/store/<编译键哈希>/`，自管 build plan + extern 注入），与 `.mirvm` 本地解析同设计；P5 依赖来源和 env/GC 开工时合并评审**。并发模型已裁定：发布一次后续只读命中、无大锁常驻；清理粒度粗可接受 | 2026-07-18 缓存讨论，decision-history §7.14/§7.15 |
| D15 | **砍掉默认路径对 Cargo 的强依赖（自有依赖解析 + 编译调度；用户 2026-07-22 纳入日程）** | **P1-P4、resolver 2/3 常见 workspace、rust-version-aware 选择与 Git 依赖已完成**（2026-07-27 至 08-10，decision-history §7.28-§7.38）：除既有 manifest/lock/registry/resolve、自有编译调度、build.rs、rustflags、增量、并行、sysroot、默认 self、workspace/resolver 3 外，现支持 Git 默认分支/branch/tag/rev、仓库内 package/path/feature、精确 commit lock、离线复跑、篡改拒绝和同名同版本双 commit 图；Git 来源进入编译指纹，self lock 被固定 Cargo 接受。**长期边界**：Cargo compat 不删除，作为用户显式回退、行为裁判和持续对拍路径；cargoless 保持默认。**P5 仍开放**：alt registry/config、source replacement/patch、resolver 1、复杂成员 glob/嵌套 workspace/workspace lints；均须响亮拒绝，不能静默退化。其他边界：HOST_RUSTFLAGS、完整 Cargo config 合并、`mirvm pack` 翻 self 尚未做 | decision-history §7.22/§7.27-§7.38，[d15-cargoless-design.md](designs/d15-cargoless-design.md)，[mirvm test 合同](designs/mirvm-test-cargoless-contract.md) |
| D16 | **冷启动战役（用户 2026-07-29 立项）** | profile 先行：MIRVM_TIMING 相位账本现成 + dev 循环基准场景（corpus/projects 改一行重跑计时）+ 日志设计 §7 三组基准（关闭 ≤2ns / 路径 A ≤1.5µs / 入队 ≤200ns）顺带实测。候选杠杆按裁定序：①**D5/L3 JIT 码持久化（dev 循环最大单根杠杆，禁令条件已灭）**；②D3 零拷贝 + 按需三件套（mmap 直读/逐函数惰性解码/预测序预取 + demand 插队）；③后台服务线程（cache write-behind + log 落盘合一，单线程多优先级队列）；④D12 -Cincremental（触发器制）。线程原则：guest 优先、编译（JIT/解码/预取同池）吃剩余核；优先级三级 demand > 预测序 > 闲时回填；不拍固定配比（调参产物非设计产物）。模式 A 懒降低维持否决不复活；guest 永不卡住等编译（解释器零等待地板）为架构事实 | decision-history §7.32 |
| D17 | **mirvm test（用户 2026-07-29 立项）** | **单包主链与 resolver 2/3 常见 workspace 已完成（2026-08-10）**：除既有 lib/bin/integration/example/custom harness、Dev 依赖和 test profile 外，现支持默认/当前成员、`--workspace`/`--all`、`-p`/`--package`、`--exclude`、默认/指定/依赖 feature、继承、统一 lock、多包 fail-fast 与 rust-version-aware 选择；单包合同 20/20、workspace 合同 27/27，self 腿由 PATH 哨兵 + execve 审计证明零 Cargo。**仍缺**：bench、根 proc-macro；doctest 继续明确不做，需 rustdoc 专项。resolver 1 和复杂依赖来源归 D15 | [mirvm test 合同](designs/mirvm-test-cargoless-contract.md)，decision-history §7.32/§7.34-§7.36 |
| D18 | **env/GC 管理面（用户 2026-07-29 裁定）** | uv 式环境 = 全局 store（D14 已有）+ 环境（lock 物化的引用集，`~/.mirvm/envs/` 登记为根）；**GC = 登记根 + 标记-清扫**（否决裸引用计数：落盘计数崩溃半截即永久不一致；sweep 崩溃安全、无计数一致性）；purge 环境 = 摘根 + 从根集合可达性扫描回收不可达。缺包行为：默认自动拉取（日志明示），--offline/--locked 响亮报错 + 提示 fetch 指令。与 D14 终态原生 store 合并评审 | decision-history §7.32 |

## R. 响亮拒绝边界（现行定型；重开需实锤驱动）

| ID | 边界 | 重开条件 | 出处 |
|---|---|---|---|
| R1 | **同步故障信号（SEGV/BUS/FPE/ILL/TRAP）guest handler 拒绝**（2026-07-19 重构定稿） | **拒绝的是「guest handler 代码的执行」**（三硬因：宿主/guest 故障不可分辨；解释器深度不可重入、信号帧内跑解释态代码原理性非 async-signal-safe；handler 返回 = 重执故障指令 = 无限再故障）。**崩溃期退出语义已忠实**：guest 真故障 = 与 native 同信号死亡（真实地址模型直落），stack overflow = 与 native 同 SIGABRT（宿主 std handler 代打，实证：`thread 'mirvm-guest' has overflowed its stack`）；guest std 的 `stack_overflow::init` 条件安装（仅 SIG_DFL 才装）读回宿主 std 已装 handler → 静默跳过，从未触发拒绝（sigread 三维实证）。**可闭合面 = 崩溃诊断化**（故障落点归属判定：guest 冻结域/代码域/帧区 → guest 化崩溃行 → 同信号终止；T4 泛化 + `MIRVM_SEGV_DUMP` 产品化） | current-status §4（M5.2 D8l），2026-07-19 用户裁定 |
| R2 | **vfork/clone/clone3/setjmp/longjmp 系、pthread_exit、pthread_atfork、多线程 fork 拒绝** | fork-alone 单线程已放行（D8f `/proc/self/task` 守卫）；其余 = 帧模型级工程/非局部控制流穿解释帧 | current-status §4，src/lower/mod.rs:48-62 |
| R3 | **unwinder context/state 家族 11 个 Unsupported**（`_Unwind_Set/GetGR/SetIP/Resume/ForcedUnwind/LSDA…`） | **进展 2026-07-21（T1）**：`_Unwind_Resume` 已由 JIT Resume 臂直调（cg_clif 同构：try_call pad 的 TryCallExn(0) → 续传宿主 unwinder）——该符号从 Unsupported 面移除；其余 10 个（Set/GetGR/SetIP/ForcedUnwind/LSDA 读侧…）维持原判。guest frame/IP/LSDA 翻译层 + 差分探针（Backtrace/GetIP/FindEnclosingFunction/GetCFA 已由影子帧兑现） | decision-history §7.19，src/vm/engine/jit/translate.rs |
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
| R18 | **C-unwind 边界残余（F-09 → 属性保全接受）** | ① callback 形：`C-unwind` 签名**接受并保全 unwind 属性**（`ForeignSig.unwind`，2026-07-22 c_mlua_lua 实锤反转——冻结拒绝会把真实 workload 打红；接受是「读过的」非「没看见」）。**残余边界 = callback 内 panic 仍 abort 于 nounwind trampoline**（libffi 闭包代码无 unwind info，宿主 unwinder 原理性不可穿；真 propagation 需 per-sig CFI stub 新机制，按实锤再立）。longjmp 形不经 unwinder、机器层不受 ABI 属性影响，可用（mlua lua_Alloc 实锤）；② 出向形：foreign 直调的 `C-unwind` 未建模（libffi 边界天然不可传播异常，语义差记档） | src/lower/ffi_sig.rs，src/lower/linker/calls.rs，src/vm/engine/thunks.rs |
| R19 | **`#![no_main]` / `#[start]` 入口形态拒绝**（2026-07-22 登记） | 入口类型非 `EntryFnType::Main` 一律响亮拒绝（exit 1 + 诊断，src/cli.rs:489）；嵌入式/bootloader 式入口形态无 corpus 实锤，重开需真实 workload | src/cli.rs |

## G. 维护态与基建

| ID | 事项 | 内容 | 出处 |
|---|---|---|---|
| G1 | **GitHub/远程全面暂停** | issues/PRD/PR/`gh` 一切操作冻结至维护者恢复；恢复后待办：corpus manifest 入库、来源/rev/lock/许可重审、远程持续 gate、triage 恢复 | AGENTS.md |
| G2 | **corpus/真实项目证据为 Git-ignored，非持续 gate** | harness 暂缓集：suite inventory、对象 GC/保留策略、跨主机 cache、NFS/对象存储耐久、远程 gate；CheckID 有界闭包（Cargo fingerprint/完整 sysroot Merkle 成差异源时扩展） | real-projects.md §6，decision-history §5 |
| G3 | **jieba_cut / opencc 全绿但未接线自动 gate** | jieba 单跑 77-89s 贴 timeout 留 corpus.sh；opencc 需 /tmp/opencc-local 前缀，留 corpus.sh 手工批有 gating | corpus §5 |
| G5 | **A2 后评估三触发器 + purity 账本复核未做** | ①clap-derive 类 tainted 巨集 → 项目本地 tainted image；②跨项目共享无实需 → 简化回单项目键；③ripgrep/tokei purity 账本复核 | decision-history §7.5 |
| G6 | **zxcvbn 上游 exact-tie 非确定（登记防误判）** | scoring.rs 对 u64::MAX 饱和并列取 HashMap 迭代序，native 自对拍都不稳；已离饱和区，非 mirvm 债 | corpus §5 批5 |
| G7 | **真实 workload 可判定性提升**（audit V-01/V-02，2026-07-22 登记） | `未立项` 现状（已如实）：corpus 129 项的三维逐字节差分只在 driver 创建时执行，持续门 = 默认 mirvm 单跑 exit-code/oracle 级；frontmatter 依赖仅 `MIRVM_CARGO_LOCKED` 置位才 `--locked`（CI 未置位 = clean runner 可重解析）。release acceptance 前需：选小而固定的代表 crate 集、钉依赖 lock、持续三维（解释/JIT compiled-entry/native）可复现；不把 129 项扩成昂贵通用门 | history/development-status-audit-2026-07-22.md §7，docs/corpus.md:118 |

> G4（stale 绕行回摘两颗钉：exr half 钉、flate2 gz/zlib 原生 API）已于 2026-07-18
> 回摘关闭并三维验收，移出本表（证据见 decision-history §7.9）。

## F. 条件触发型重开项（触发器速查）

| 触发条件 | 重开什么 | 出处 |
|---|---|---|
| 栈式协程/continuation、独立 VM 栈收益出现 | Frame model B 重评 | decision-history §2，designs/frame-stack-models.md §5.5 |
| E6（分配/guest TLS 快路径内联）立项进场，**或**多 Engine 嵌入立项（双闸，先到先裁；用户 2026-07-21 裁定） | vmctx R 缓存层复测（T3 已闭合，T 骨架生产定稿） | designs/vmctx-passing.md §7，decision-history §7.20 |
| pinned rustc 改 summary 结构或出正式诊断协议 | runner 诊断 hook 改 guard/正式接口（→E22） | decision-history §5 |
| pinned toolchain 升级 | D9e emit 剪枝重测 | decision-history §7 |
| 「单次运行、超大 delta、不可预降」负载形态实测（REPL/宏展开型） | 懒降低候选 A 重启（前置：L2 statics/const 分域 + 会话生命周期重设计，两件单独立项） | history/m5.3-design.md §3.1 |
| fixed stdarch helper 数量显著膨胀，或某指令无 stdarch/CLIF 表达 | D7b 通用 asm-stub 向量 ABI 重开 | history/m5.1-design.md |
| 跨项目 S3′c 共享成为硬需求 | 方案 B（chain + 构建屏障）备选复活（WIP 存 [parked/s3b-chain-wip.patch](parked/s3b-chain-wip.patch)） | history/s3b-design-fork.md §6 |
| 「大用户 crate 编辑-重跑」场景 | `-Cincremental` 脚本路径（→D12） | history/coldstart-research.md |
| 首个 kind=Global alloc error handler workload | `__rust_alloc_error_handler` 路由（→E28） | decision-history §7.7 |
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

## 旧编号对照（m4-debt-map.md 已删，本文承接）

| 旧条目（docs/m4-debt-map.md，git 历史可查） | 现落点 | 状态 |
|---|---|---|
| §0–§5 M4 Trap 普查（2026-07-07 快照） | 精髓在 [history/m4.1-design.md](history/m4.1-design.md) F1–F7 与 [history/m4-log.md](history/m4-log.md)；普查方法与 §2 三发现已被施工兑现 | 历史 |
| §6 thunk 盲区（结构体内嵌 fn-ptr） | 已根治（P1 条目可执行化 `4202317`，decision-history §7.6）；残余阶梯 → T4 | **已关闭** |
| §7 dep crate global_asm | C4 已于 §7.23 闭合；残余 → R16 | 已关闭主项 |
| §8 JIT 间接调用准入 | → E1 | 开放 |
| §9 FFI 按值聚合封送 | → C1 | 开放 |
| §10 ① 符号在 rlib ② asm noreturn | → C2 / C3 | 开放 |

## 附录：考古来源与方法

2026-07-17 全数过录以下文档的「未解决/未立项/绕行/拒绝/待施」标记并逐条核对现状：
`DESIGN.md`、根 `README.md`、`docs/` 全部（current-status、decision-history、corpus、
real-projects、m4/m5 各设计与日志、spike1–5、coldstart-research、s3b×2、s4、distribution、
m4-debt-map、AGENT-HANDOFF）与 `docs/designs/` 六篇。已过录条目均在上表；「可能已修」疑点
已现场核实（见 G4/E27/C7 等条）。corpus 逐 crate 绕行记录另见 `corpus/c_*.rs` 头注。
后续新债务直接登记本文对应分区；关闭条目时连同行移出并在 decision-history 留证据。
