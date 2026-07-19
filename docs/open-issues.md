# mirvm 未解决债务与开放问题登记册

> 覆盖 mirvm 自始（M0 tier-0 时代）至 2026-07-17（corpus 批8 收官，HEAD `2f9efef`）的
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

- **R1 同步故障信号 handler**：真实地址模型下宿主/guest 故障不可分辨；唯一闭合
  路（虚拟地址/线性内存/全 CPU 仿真）已裁决否决（decision-history §7.5b）。
- **E19 rustix 裸 syscall 拦截**：无符号可拦，VM 层原理不可闭合；唯一闭合 =
  OS 层 seccomp（H 节已定型）。
- **E11 中「栈深度与 native 逐字节一致」**：解释器栈膨胀属 as-if 允许域（栈深度
  UNSPECIFIED）；可闭合的只是诊断与 `--stack-size` 配置语义。
- **R8 的 asm `goto`/label**：转出 stub 需把宿主函数整体 native 编译，超出
  Cranelift 栈（同 cg_clif fatal 边界）；唯一理论出路（单函数 AOT 逃逸舱）
  未证实，不预支。

## T. 已立项待施（蓝图在手）

| ID | 事项 | 关键内容 | 出处 |
|---|---|---|---|
| T1 | **M5.4c：JIT ABI 泛化 + LSDA 产品化** | Pair/Indirect/track_caller 调约泛化；CallIndirect/Builtin/Foreign/TlsRef/InlineAsm 助手；try_call/Resume/Terminate；双 CIE + 全覆 LSDA（cleanup 边 JIT 帧内着陆）+ 准入放开。锚点 = gate2 unwind 九用例 JIT-on。LSDA probe 5/5 已过。兼收割 spike3/spike5 挂起检查点与 frame-abi §10.3 聚合调约残余 | [designs/m5.4-design.md](designs/m5.4-design.md)，current-status §1 |
| T2 | **M5.4d：JIT SIMD + 收口** | CLIF 向量族 + x86 helpers 助手 + 全量三重差分扩展 + 账本/文档收口 | designs/m5.4-design.md |
| T3 | **M5.5：vmctx 终裁计量 + gate6 收口** | T 骨架已落生产；R 缓存层复测触发器 = 分配/guest TLS 内联进编译码（见 E6）。挂载点：CallIndirect 内联缓存、LSDA 存储改 JIT data object、检查点回写 vmctx-passing、tests/m5_gate6.sh | [designs/m5-design.md](designs/m5-design.md) §3 D5/§7，m5.4-design |
| T4 | **P1 残余：SIGSEGV 诊断化兜底（可选后补）** | rip 落在 guest 冻结域（非可执行）时识别为「疑似结构体内嵌回调」提示，把静默跳崖变可读诊断；`MIRVM_SEGV_DUMP` 既有旋钮之上的产品侧提示 | decision-history §7.6（debt §6 阶梯②），2026-07-17 |

## C. corpus 实锤产品欠账（实锤驱动，未立项）

| ID | 事项 | 关键内容与转正要件 | 出处 |
|---|---|---|---|
| C1 | **FFI 按值聚合封送**（旧 debt §9；**已闭合 2026-07-18**） | 关闭：FfiAgg 冻结布局 + 出/入向全聚封送（[designs/c1-ffi-agg-design.md](designs/c1-ffi-agg-design.md)；合成矩阵探针 `demo/ffi_agg_probe.rs` 三维绿、**c_tree_sitter 原样三维转绿**、gate5 全量复绿，decision-history §7.10）。残余边界转 R17 | 本表 R17，designs/c1-ffi-agg-design.md |
| C2 | **native-archive 闭包缺口：符号在 rlib**（旧 debt §10①；**已闭合 2026-07-18**） | 关闭：失败救援链落地（elfsym 静态枚举 `SHN_UNDEF` ∩ exported_defs → `fn_entry_addr` 预算 → `.hidden` P1 跳板重链；[designs/c2-rlib-symbols-design.md](designs/c2-rlib-symbols-design.md)）。验收：MRE 转绿、c_bzip2_csys（bzip2 vendored C 后端）三维绿、**c_wasmtime_wat 换面**（层①消除，锁层② asm noreturn/70 = C3 入口）、gate5 全量复绿。数据符号维持如实拒绝另立 | decision-history §7.11，designs/c2-rlib-symbols-design.md |
| C3 | **inline asm `noreturn`**（旧 debt §10②；**已闭合 2026-07-18**） | 关闭：两面孔物化（outs 恒空 + Unreachable 落点兜底）；ud2 终止形三维绿；**resume/longjmp 转移形 = c_wasmtime_wat 全 trap 面三维确定性绿**（冷缓存+三次重复+三维逐字节——VM-in-VM 旗舰转绿入 gate）。如实边界转 E32 | decision-history §7.12，[parked/c3-resume-spike.md](parked/c3-resume-spike.md) |
| C4 | **dep crate global_asm 物化**（旧 debt §7） | faer pulp V3 LD_ST 汇編表：S2 `-Zno-codegen` 致 rlib 无 object，本 crate global_asm 通道（收集种子只含本地项）接不到。路径①收集面扩到 used_crates（同通道 cc+dlopen，装载序=crate 图序）；②命中 crate 关 `-Zno-codegen`（判名放行，只付一族 codegen）。当前 default-features=false 标量内核绕行 | corpus §5 批6（fb327cc 记档） |
| C5 | **dyn 上溯 vtable 变换**（M4.2 欠账；**已闭合 2026-07-18**） | 关闭：`dyn_unsize_tails` 统一递归判据（builtin_deref 直达 / 解引用落点 lockstep 再判 / Pat 壳 / Adt 唯一非 ZST 字段）+ PC::Unsize 臂 chase（目标 vtable = `*(源 vtable + supertrait_vtable_slot×8)`，cg_ssa `unsized_info` 同构）。验收：`demo/dyn_upcast_probe.rs`（Arc 包装链 + & 直达两形）入 diff.sh、**c_datafusion_sql 三维 95 行逐字节**、c_typst_pdf fnv 锚点无回归。常量胖指针上溯未接（如实报错，未遇真实 workload）。pgp_packet armor 体层绕行保留无害 | decision-history §7.13 |
| C6 | **M5.x intrinsic 按需队列残余** | pclmulqdq.256/.512、vaes、其余 gather 形态、avx512.pmadd 系等：遇真实 workload 按既有四触点法补（已清先例：psad.bw/pclmulqdq/aesni/crc32/permd/gather/vpmadd52/F16C/lddqu/`2b4766b`）。AES 等未触发项保留响亮 Trap | corpus §5，history/m5.1-design.md §1 |
| C7 | **naked/global_asm `sym` 指向解释执行 guest fn**（**主面已闭合 2026-07-18**） | 关闭：签名 FFI 可派生的 guest fn 经 P1 条目 stub 预算 + trampoline 导出符号，机器码 call 直落条目 stub 回解释器（探针 `demo/global_asm_guest_fn.rs` 三维绿，decision-history §7.9）。残余边界转 R16 | 本表 R16，history/m5-log.md |
| C8 | **Rust 侧 ctor / `.init_array`（linkme 族）未触发** | C 原生归档侧 constructor 已由 decision-history §7.8 分治放行（DT_INIT）；裸 `.init`/`.fini` 仍拒。Rust 侧 ctor/linkme 从未进 corpus，按需立项，不预支 | history/m4.5-plan.md D6（已删，git 历史），§7.8 |

## E. 引擎与架构欠账

### E.1 JIT / 引擎内部

| ID | 事项 | 状态与内容 | 出处 |
|---|---|---|---|
| E1 | **JIT 间接调用准入**（旧 debt §8） | `未立项` `admit()` 白名单不收 CallIndirect/CallForeign/CallBuiltin → vtable 派发/回调/qsort 比较子恒解释速度。路径：①编译体 CallIndirect = fn_addrs 反查命中 PLT 快路/未命中 c2i；②CallForeign CLIF 化或预物化 stub；③HostWrite 直通族优先。性能工作重启时排，alpha 铁律不变 | 2026-07-17，src/lower/jit_compile.rs |
| E2 | **无 OSR/deopt/生产 tiering/后台 JIT 服务线程** | `记账` 长跑单循环 main 永不触发 JIT（无调用边界、无 OSR）记账接受 | history/frame-abi-bytecode.md §10.4，m5-log |
| E3 | **JIT 帧不压影子帧** | `记账` panic 在 JIT 帧内展开时列帧少于解释口径 | history/m5-log.md M5.3 |
| E4 | **CLIF 原子统一 SeqCst** | `记账` Cranelift 0.133 无弱序；JIT 侧全部最强序（合规强化，无分歧） | history/m5-log.md |
| E5 | **JIT 机器码随模块常驻** | `记账` cranelift-jit 不支持逐函数释放，进程生命周期记账 | designs/m5-design.md |
| E6 | **分配快路径/guest TLS 快路径内联未做** | `未立项` m5-design D5 格③空（分配走 Builtin、TlsRef 走助手）；手卷 TLAB 同题；是 T3 R 复测的触发前提 | designs/m5-design.md §3 |
| E7 | **JIT 优化项池** | `未立项` SwitchInt 用 br_table 替代 icmp+brif 链；取址逃逸精化（Q1 残余优化触发器）；CallIndirect 内联缓存（挂 T3）；LSDA 存储升 JIT data object（挂 T3） | designs/m5.4-design.md |
| E8 | **backtrace 真符号化 + 合成 IP 近似** | `绕行` 合成 IP 不经 dladdr（诚实 `<unknown>`，不伪造宿主符号）；真符号化（物化符号 ELF 给 dladdr）可选未立项 | current-status §4，decision-history §6.1 |
| E9 | **`dl_iterate_phdr` 差分探针欠账** | `绕行` 走 native FFI「无观测到缺陷」，探针补课未做 | current-status §4，history/m5.2-design.md |
| E10 | **guest TLS 实例块回收** | `记账` 每线程每 TLS 一小块泄漏（dtor 副作用已正确）；M4.4 起挂账「按需」，至今未立项 | history/m4-log.md:439 |
| E11 | **guest 线程栈大小精确化 + 编译帧 SIGSEGV 优雅化** | `未立项` guest 栈大小语义近似；JIT 编译帧撞 guard page = 裸 SIGSEGV（与 native 差一条报错消息）。同根：栈守卫精确化 | history/m4.4-design.md，m5-design #11 |
| E12 | **alloca 迁移承诺（换掉 slaved 操作数区）** | `未立项` frame-abi §2.2「slaved 仅是起步，后续必换 alloca（真内联 native 栈）」；与 checked 模式正交轴纪律在案 | history/frame-abi-bytecode.md，DESIGN.md C12 |
| E13 | **guest 异常独立 exception class/personality 无裁决** | `未立项` 是否独立于宿主 panic + 自有 personality——spike3 §4.1 是全库唯一登记处，从未裁决（现共用宿主 panic + downcast） | history/spike3-mixed-stack-unwind.md |
| E14 | **hand-rolled TLAB 未立项** | `绕行` v1 = mimalloc crate 后端；chunk/大小类/remote-free 队列细节无；与 E6/T3 关联 | history/spike1，designs/concurrency-arch.md §9 |
| E15 | **`--vm-stats` fn-ptr 无出边盲点** | `绕行` 仪器债：fn-ptr 间接调用无出边 → 债务读法永远「至少欠这些」；继续靠增量发现 | src/vm/engine/stats.rs 头注 |
| E16 | **io_uring 直通未实证** | `未立项` tokio-uring 可选路径全库仅 designs/async-stackless.md §5.2 提及，无 corpus 对拍 | designs/async-stackless.md |
| E17 | **L2 缓存两处** | `未立项` ①有告警/错误的会话拒入账、诊断回放未做（告警程序永不享缓存，session 门在 src/cli.rs:534）；②条目无逐出——**手动 GC 面已由 `mirvm cache purge`（默认清陈代）补上（§7.14）**，自动 LRU/容量上限不立项 | history/m6-log.md 片2/8 |
| E18 | **Cranelift 自有内联（0.133.1 inline.rs）备用杠杆** | `拒绝` 默认不开，记为收口期备用杠杆 | designs/m5-design.md |
| E19 | **rustix 裸 syscall vs os:: 收口的张力** | `记账` linux_raw 无符号可拦：mirvm 层虚拟化 OS 资源会被绕过，只有 seccomp 能兜（运行本身已通：M5.1 tempfile 全绿） | corpus §2.3，§5 |
| E20 | **字节码验证 pass 未建** | `未立项` loader 鲁棒性开放问题；内容寻址缓存已由 S3′b A2 兑现，独立验证 pass 无实锤驱动 | history/frame-abi-bytecode.md §10.6 |

### E.2 架构与边界

| ID | 事项 | 状态与内容 | 出处 |
|---|---|---|---|
| E21 | **`src/os/` P7 物理层**（**主面已闭合 2026-07-18**） | 关闭：`src/os/` 建成（mod 契约 + linux/{mem,thread,signal,dll,process}，leaf 零 engine/rustc 依赖、原语不裁决）；engine/lower/cli 全部 libc 触点归并，**非 os 域 `libc::` grep 机械清零**（spikes 冻结原型不在门禁内）；固定基址数值提升 `vm/engine/addrlayout.rs` 三方共享。DESIGN.md P7 段已终态化。残余 = 巨文件治理（func/interp/jit/lower-mod）与 `arch/` 层（x86.rs 一族），属同一战役后续分片 | DESIGN.md P7，decision-history §7.16（战役落档随总收口） |
| E22 | **多 Engine 嵌入 API / 生命周期收敛** | `未立项` Shared/thunk/asm handle/部分 TLS 进程期泄漏；错误径可退进程；runner `TRACK_DIAGNOSTIC` 进程全局单槽（daemon/嵌入/并发 compiler 前必须带所有权 guard）；dlopen 嵌入 TLS model 退化同题（vmctx §3.2） | current-status §4，decision-history §8 |
| E23 | **checked 模式（L3）未建；L1 guard page 未建** | `未立项` region-check 设计储备在 designs/concurrency-arch.md §6；L1 目前只有固定地址域分池，无 PROT_NONE guard；正式沙箱归 OS 层（P3 VM 层已永久冻结 → H 节） | DESIGN.md C13，decision-history §7.5b |
| E24 | **Cargo wrapper composition fail-closed** | `拒绝` 非空 `RUSTC_WRAPPER`/`RUSTC_WORKSPACE_WRAPPER` 或有效 build.rustc-wrapper 一律拒绝，不做 wrapper 链组合；重开 = 可重开的实现选择 | current-status §4，src/cargo_shim.rs:63 |
| E25 | **`MIRVM_ENCODED_RUSTFLAGS_APPEND` 未进 Cargo fingerprint；build/env 未分离** | `未立项` 改值可能复用旧 fake binary；mirvm build env 与 guest runtime env 尚未彻底分离 | real-projects.md §6，current-status §4 |
| E26 | **平台仅 Linux/ELF/x86_64** | `记账` pthread/dlopen/GNU 链接/x86 asm wrapper 依赖面；unwind「跨平台无痛」未逐平台验证；macOS 次之后议 | current-status §4，DESIGN.md §11 |
| E28 | **`__rust_alloc_error_handler`（kind=Global）路由未动** | `未立项` 仍走 ③ Trap（src/lower/mod.rs:1299）；首个 workload 触达时再立项（`__rust_*` 族已由 §7.7 统一路由，本条是例外残余） | decision-history §7.7 |
| E29 | **手写 shim → 通用直通通道** | `未立项` neat 终态（polish，不急）；现行为手写 shim + dlsym 兜底 | DESIGN.md §7.2 |
| E30 | **C7 regex 42s 基线 JIT 后未复测** | `未立项` 旧性能靶子；JIT 之后无重测记录，「评审须给预估收益」要求未兑现 | DESIGN.md C7 |
| E31 | **A2 mtime 粒度传递依赖漏检残余风险** | `记账` 键安全论证承认 mtime 粒度残余；挂档（distribution §6 既有条目同案） | history/s3b-a2-design.md §9.4 |
| E32 | **inline-asm setjmp/longjmp 的捕获帧内存复用 hazard（C3 定稿边界）** | `记账` asm-stub 模型下 setjmp 捕获点在 stub 包装帧；解释帧在捕获与恢复之间复用该宿主栈内存的合成协议可撞死（v2 spike 实锤，落点 `Channel::send` 内部）。真实 workload（wasmtime 全 trap 面）不发生该形态、三维确定性绿。消除 = JIT 真帧身份（compiled guest fn = native 帧语义）；不宣称全形态闭合 | [parked/c3-resume-spike.md](parked/c3-resume-spike.md) |

> E27（weak 符号真地址化缺定向验收）已于 2026-07-18 关闭并实修「weak extern
> static 恒 0 判空 cell」缺陷——现走 GOT 启动相真解析（命中=真址/缺席=0；引擎
> 接管符号强制缺席），探针 `demo/weak_extern.rs` 三维绿（decision-history §7.9）。

## D. 分发与产品面（方向已批：D9，2026-07-14；施工未立项）

| ID | 事项 | 内容 | 出处 |
|---|---|---|---|
| D1 | **mode B `.mirvm` 包 + `mirvm pack`（D9f④）** | 缓存可移植化为路线；内含 fat artifact 多 target、字节码版本化（C12）、S3′ 产物可携带化 | [designs/distribution-design.md](designs/distribution-design.md) |
| D2 | **发行形态与命名（D9f⑤）** | 先 miri 式后 JDK 式自包含 tarball（成熟后）；kit 命名候选 MDK/mirvm toolkit（MRsDK 已否决） | designs/distribution-design.md |
| D3 | **零拷贝装载（rkyv 类）** | postcard 解码封顶（eco ~60ms / ripgrep ~450ms）；mmap+逐函数惰性解码是下一数量级唯一杠杆；与 mode B 同题 | history/coldstart-research.md V6，m6-log 片8 |
| D4 | **对外格式冻结重估** | M5.3 收官触发器（2026-07-15）已响，被有意再推迟到 mode B 立项 | decision-history §7 |
| D5 | **L3 JIT 机器码缓存 D9c 禁令期** | M5.3–M5.5 定型（CFI/PLT/重定位）前禁做；v1 进程内易失 | designs/distribution-design.md，history/m5.3-design.md |
| D6 | **S3′c 完整形态（跨项目共享）** | 路径无关内容哈希键（~50ms/次）+ tainted 层/多层 image 合并；按实需立项（已兑现的只是同 workspace 跨 bin 冒烟） | current-status §5.8，history/s3b-a2-design.md §3.4 |
| D7 | **frontend 相成本无杠杆认领；V5 `-Zthreads` 并行 lower 未立项** | eco ~143ms / ripgrep ~730-800ms 在账无人认领；V5（tcx DynSync+worklist rayon 化）在 V3 不建后无人重启 | history/coldstart-research.md，m6-log 片8/10 |
| D8 | **8 个 correctness case 未 benchmark** | ripgrep_gzip/parallel_nomatch/mmap_binary/parallel_match/multiline_replace、tokei_sort_code/streaming_json/rust_files | real-projects.md §5 |
| D9 | **registry 依赖 crate 底座化 / 自适应底座 / 底座 AOT 机器码入 base** | S4 三未立项方向；第三条与「机器码不入缓存」世界观冲突，立项前先核 | history/s4-base-image-design.md §0/§4 |
| D10 | **M3 产品面** | daemon、agent API、资源治理、正式沙箱、虚拟化钩子（假 FS/路径重定向/计费——区分 guest 调 open 与解释器自读缓存） | DESIGN.md §9，§7 沙箱节 |
| D11 | **REPL/Notebook + 嵌入 API（M7+）** | 原愿景 M6 编号已被轨 C 冷启动占用；REPL = 持久堆天然成立，未立项 | DESIGN.md §3/§9 |
| D12 | **`-Cincremental` 脚本路径** | 非当前杠杆；触发式重启（「大用户 crate 编辑-重跑」形态）；`finalize_session_directory` 坑在案 | history/coldstart-research.md §4 |
| D13 | **地址模型 P5 增量（扩域/回收）** | 维持现固定基址样条工程；增量能力记 M7+ | decision-history §7.5b |
| D14 | **原生内容寻址依赖存储（统一依赖 cache 终态；用户 2026-07-18 裁定方向）** | 去重单位 = 完整编译键（crate 版本 × features × 依赖闭包 × cfg/flags × toolchain）：多脚本/多项目共引 X@V 时其构建产物机器级唯一。**近期片已落地（§7.15：共享 cargo target dir，fingerprint 即原生内容寻址；实测 ethers 二跑 0.66s、两树并集 525M）**；**终态 = mirvm 原生 store（`~/.mirvm/store/<编译键哈希>/`，自管 build plan + extern 注入），与 .mirvmar 本地解析/mode B 同设计——用户裁定原生为更好方向，立项时与 D1 合并评审**。并发模型已裁定：发布一次后续只读命中、无大锁常驻；清理粒度粗可接受 | 2026-07-18 缓存讨论，decision-history §7.14/§7.15 |

## R. 响亮拒绝边界（现行定型；重开需实锤驱动）

| ID | 边界 | 重开条件 | 出处 |
|---|---|---|---|
| R1 | **同步故障信号（SEGV/BUS/FPE/ILL/TRAP）guest handler 拒绝** | 宿主/guest 故障可分辨的机制出现（伪造恢复=静默错值） | current-status §4（M5.2 D8l） |
| R2 | **vfork/clone/clone3/setjmp/longjmp 系、pthread_exit、pthread_atfork、多线程 fork 拒绝** | fork-alone 单线程已放行（D8f `/proc/self/task` 守卫）；其余 = 帧模型级工程/非局部控制流穿解释帧 | current-status §4，src/lower/mod.rs:48-62 |
| R3 | **unwinder context/state 家族 11 个 Unsupported**（`_Unwind_Set/GetGR/SetIP/Resume/ForcedUnwind/LSDA…`） | guest frame/IP/LSDA 翻译层 + 差分探针（Backtrace/GetIP/FindEnclosingFunction/GetCFA 已由影子帧兑现） | src/lower/mod.rs:1355 |
| R4 | **一般嵌套 DST / 其他 metadata 形态拒绝** | 冻结一份通用 DST layout expression 的评估（slice/str 静态公式与 direct dyn 尾 vtable 运行期对齐已支持） | current-status §4，decision-history §5 |
| R5 | **冷面 128 位形态 Trap**（`Transmute pair→聚合`、tag 宽>8B `InvalidEnumConstruction`） | 未被真实 workload 撞出；撞到再补 | history/m5-log.md 片10 |
| R6 | **static archive 拒绝面残余** | 非 PIC/thin/跨 archive 依赖与顺序/重名导出/RTLD_DEFAULT 碰撞/export-symbols/非 Linux-ELF/裸 `.init`/`.fini`；`.init_array` 族已放行（§7.8），RTLD_DEFAULT 同名碰撞已改归档优先（fb0b204）。多 archive link plan 与 modifier 等价语义未立项——放宽前必须先立 | src/native_archive.rs，history/m5.1-design.md D2 |
| R7 | **弱内存序特殊化不做** | 映射宿主原子即在 RAM non-det 包络内；真实 workload 再评估 | DESIGN.md，history/m4-plan（git 历史） |
| R8 | **asm 拒绝面** | `att_syntax`/`sym`/`label`(asm goto)/`may_unwind`/非 x86_64；asm-stub xmm/向量值操作数未扩槽（被 stdarch helper 路线绕行，无 workload 触发）。`noreturn` 已升级实锤债 → C3 | history/m5-log.md，src/lower/func.rs:2500 |
| R9 | **`type_id`/`type_name`/`offset_of`/`field_offset` Trap** | 真实程序撞到再补 | history/m5-log.md |
| R10 | **`va_arg`/`carryless_mul`/`autodiff`/`rustc_peek`/SVE 5 个** | 前两个无真实用例保留 Trap；后三个无生态意义（实验/调试/ARM） | history/m5.2-design.md D8l |
| R11 | **`intrinsics::abort` SIGABRT vs native SIGILL 差异** | 授权差异绕行；差分若开始比信号再对齐 | history/m4-log.md |
| R12 | **TSan 通道跑 guest TSD dtor 场景不可行** | TSan 线程态先于 TSD 相位析构（持久边界）；TSan 配置下 Ctx 永久泄漏，测试用例避开 | history/m4-log.md |
| R13 | **虚拟地址模型（含线性内存折中）否决，不作方向** | FFI 轴有原理性障碍；真实地址模型保留（P1/P2 已修，P3 冻结、P4 判非问题、P5→D13） | decision-history §7.5b |
| R14 | **tokei 并行 JSON reports 次序不稳定** | 上游行为非 mirvm 债；用稳定 compact aggregate 绕；JSON 作 oracle 前须先解决确定排序 | real-projects.md §6 |
| R15 | **`ClosureFnPointer` 等 track_caller 外 adjustment 未支持** | `ReifyFnPointer` 只走 rustc `resolve_for_fn_ptr`；不能由此外推 | current-status §4 |
| R16 | **global_asm `sym` 拒绝面残余（C7 闭合后）** | ①`sym` fn 指向签名不可派生（聚合/Rust ABI/变参）的 guest fn——机器码调此类同形本即 UB，响亮拒绝；②`sym` static 指向 guest static 未接（mangled 静态名审计仍会命中），按 workload 再立 | src/lower/global_asm.rs，decision-history §7.9 |
| R17 | **FFI 按值封送残余边界（C1 闭合后）** | union 按值（SysV union 分类另规则）、SIMD 向量按值、变参尾参位聚合、align>8 聚合、multi-variant enum 按值——五形态 freeze 响亮 Err（文案可鉴红分类）；`{i128}`/f128/long-double/_Complex 既有标量边界不动。各形态同 helper 可扩，按真实 workload 触发再立 | src/lower/mod.rs，designs/c1-ffi-agg-design.md §0 |

## G. 维护态与基建

| ID | 事项 | 内容 | 出处 |
|---|---|---|---|
| G1 | **GitHub/远程全面暂停** | issues/PRD/PR/`gh` 一切操作冻结至维护者恢复；恢复后待办：corpus manifest 入库、来源/rev/lock/许可重审、远程持续 gate、triage 恢复 | AGENTS.md |
| G2 | **corpus/真实项目证据为 Git-ignored，非持续 gate** | harness 暂缓集：suite inventory、对象 GC/保留策略、跨主机 cache、NFS/对象存储耐久、远程 gate；CheckID 有界闭包（Cargo fingerprint/完整 sysroot Merkle 成差异源时扩展） | real-projects.md §6，decision-history §5 |
| G3 | **jieba_cut / opencc 全绿但未接线自动 gate** | jieba 单跑 77-89s 贴 timeout 留 corpus.sh；opencc 需 /tmp/opencc-local 前缀，留 corpus.sh 手工批有 gating | corpus §5 |
| G5 | **A2 后评估三触发器 + purity 账本复核未做** | ①clap-derive 类 tainted 巨集 → 项目本地 tainted image；②跨项目共享无实需 → 简化回单项目键；③ripgrep/tokei purity 账本复核 | decision-history §7.5 |
| G6 | **zxcvbn 上游 exact-tie 非确定（登记防误判）** | scoring.rs 对 u64::MAX 饱和并列取 HashMap 迭代序，native 自对拍都不稳；已离饱和区，非 mirvm 债 | corpus §5 批5 |

> G4（stale 绕行回摘两颗钉：exr half 钉、flate2 gz/zlib 原生 API）已于 2026-07-18
> 回摘关闭并三维验收，移出本表（证据见 decision-history §7.9）。

## F. 条件触发型重开项（触发器速查）

| 触发条件 | 重开什么 | 出处 |
|---|---|---|
| 栈式协程/continuation、独立 VM 栈收益出现 | Frame model B 重评 | decision-history §2，designs/frame-stack-models.md §5.5 |
| 分配/guest TLS 内联进编译码，或 ctx 获取成热点 | vmctx R 缓存层复测（→T3） | designs/m5-design.md D5，designs/vmctx-passing.md §7 |
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
| GIL-over-真线程 tier-0 / wasmtime-unwinder 路线 | 死题（引擎 Sync 当日直证）/ 不采（与宿主 unwinder 不互操作） | DESIGN.md §5，history/spike5 |
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
| §7 dep crate global_asm | → C4 | 开放 |
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
