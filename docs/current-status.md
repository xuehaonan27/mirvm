# mirvm 当前开发状态

> 状态日期：2026-07-18（事实截至 2026-07-17 HEAD `2f9efef`；本文于 2026-07-18 文档大
> 精简时刷新索引）。本文是当前状态的唯一汇总入口；若与早期计划、README 或交接文档
> 冲突，以当前代码、可复现测试结果和本文为准。文档权威规则见 [README.md](README.md)。
> **未解决债务/开放问题/拒绝边界的唯一登记入口 = [open-issues.md](open-issues.md)**，
> 本文 §4 只保留最有代表性的边界快照。

## 1. 阶段结论

| 阶段 | 状态 | 当前含义 |
|---|---|---|
| M0–M2.5 | **历史完成** | 基于 rustc `InterpCx` 的 bootstrap；tier-0 源码已于 2026-07-09 删除 |
| M4.0–M4.5 | **完成** | 自研 typed bytecode、tcx-free tree-walking interpreter、FFI、unwind、真线程、TLS 与回调 |
| M5.0 | **完成并复审** | x86_64 inline-asm stub 工厂：GAS wrapper → `.so` → `dlopen`/`dlsym` |
| M5.1 | **完成（2026-07-12）** | numbigint、xgetbv、sha2、blake3、ecosystem、diff_cargo 3/3 与六个 release tracer 全绿 |
| 真实项目 TDD | **继续扩面（2026-07-15）** | workspace-local ripgrep/tokei 驱动四项通用语义修复；corpus 升级为三维逐字节差分（mirvm 默认 / native / 逢调即编 JIT）并两批扩编 27 个真实 crate（24 绿入 gate5 corpus 段 57 程序、2 expected-red 锁定、1 手工批；[corpus.md §5](corpus.md)）：撞出并已修两个产品 bug（fn_addrs 地址域分拆 `718dac5`、extern fn-ptr foreign 通道 `cb09b5b` 解锁 ring），另钉死 M5.x intrinsic 内建欠账队列（psad.bw/pclmulqdq/aesni/avx512ifma/avx2-gather） |
| M5.2 | **完成（2026-07-14）** | 非 JIT 语义补全（轨 A 完备）：标量/simd intrinsic 差集、真栈深度、f16/f128、atomic 序、backtrace/signal/fork/atexit/global_asm/naked、128 位残余、嵌套 DST 全清；两个历史 XFAIL 转绿（[m5.2-design.md](history/m5.2-design.md) D8a–D8l，施工日志见 [m5-log.md](history/m5-log.md)） |
| M6 ①② | **完成（2026-07-14）** | 轨 C 分发（[distribution-design.md](designs/distribution-design.md) D9f）：① 相位计时（MIRVM_TIMING 账本）+ ② L2 post-mono engine-IR 缓存（冻结区固定基址整包序列化；热跑加载相 11–15×，std-only 程序 374ms→33ms；告警程序诚实不缓存）；③④⑤ 未立项；施工日志 [m6-log.md](history/m6-log.md) |
| M6 片3 前置调研 | **完成（2026-07-14）** | 冷启动/lower 全解剖与杠杆清单（[coldstart-research.md](history/coldstart-research.md)）：lower 是近常数 std 税（~0.10ms/instance；执行集仅占降低集 7–29%）、lower 相 75% 耗在 rustc 查询/解码机器、依赖构建 codegen 白烧实证、三个缓存盲区（P1 runner 环境化石化 / P2 空 stub .d / P3 diff_cargo 无 warm 维度）；**施工顺序已裁定（decision-history §7.2）：S1 小件包 → S2 依赖剪枝 → S4 底座（设计过审）→ S3 懒降低并入 M5.3 JIT 联合设计** |
| M6 片4（S1 小件包） | **完成（2026-07-14）** | 四个语义单元逐 commit 全绿（m6-log 片4）：S1a sysroot 仪式 stamp 化（warm fib 墙钟 85→55ms；加载相性能门 443→364ms）；S1b runner 不回放 MIRVM_\*（P1 化石化修复）；S1c 假二进制 dep-info 真实化（P2；源码编辑触发重录，runner 回写路线实测证伪后改 wrapper 侧 --emit=dep-info）；S1d diff_cargo 补 L2 warm 复跑维度（P3；runner 缓存路径首次有 gate 覆盖） |
| M6 片5（S2 / D9d 依赖剪枝） | **完成（2026-07-14）** | target 依赖 in-process + `-Zno-codegen`（树内现成空转；metadata-only rlib 由默认 link 路径照常产出）；post-mono const-eval 错误面由显式 mono 收集补齐（探针：native/mirvm 同一条 E0080）。账本诚实修正：wall ≈0（128 核噪声内，调研预估证伪）、CPU −12%、磁盘 −60%、少 22 次 exec（m6-log 片5） |
| M6 片6（S4 std 预降低底座） | **完成（2026-07-15）** | 空 main 底座（0x6800 域）+ delta（0x6900 域）偏移合并（施工偏离 §7.3：取代域位双表，解释器零改动）；v0 symbol_name 复用（fn/static/TLS 去重）；降低指纹会话内验证；L2 分层 base_key。**脚本纯冷 385→104ms（lower 9.6×）**，ecosystem runner lower 1272→988ms（std 份额）；底座构建幂等且字节确定（验收抓获 HashMap 随机序缺陷）；gate5 46→47（新增旁路冒烟）全绿（[s4-base-image-design.md](history/s4-base-image-design.md)，m6-log 片6） |
| **M5.3 JIT 骨架** | **完成（2026-07-15）** | 方法级 Cranelift JIT 三片全落地（[m5.3-design.md](history/m5.3-design.md) Q1-Q4 全批；m5-log M5.3 节）：J1 分层基座（call_guest 单一派发点 + PLT/计数，S4 合并 FuncId 空间）+ 翻译器标量子集（语义 = 与解释器逐位一致；调用点两路分治——热路 PLT 间接/冷路 c2i）+ CFI（spike5 管线产品化）。**fib(32)：解释 engine 922.6→19.3ms（47.8×）= 2.9× native；硬门 ≤80ms 达成（69ms 含加载）**。oracle：逢调即编（阈值=1）diff 30/30 + JIT-off 对齐 + gate5 三新行（硬门/逢调即编全量/off 冒烟），gate 总数 47→50 |
| S3′a（image 栈重构） | **完成（2026-07-15，commit b4ed691）** | 多源查找 + 地址样条（行为等价）：单底座泛化成 image 栈（`[std 底座, dep…]`）；frozen 样条域 + is_valid_home 白名单；ImageStack 并集查找/累积偏移/键链；Linker 收 &ImageStack。无 image/单底座两态字节等价，gate5 50/0/0/0 |
| **S3′b（依赖成像）** | **完成（2026-07-15）= A2 纯化聚合 deps-image** | chain 方案撞"线性链无法表达非线性依赖 DAG"固有难题（4/19 证伪）后，purity 探针实测 eco 账本（tainted 72 inst/1.9ms）裁定 A2（[decision-history.md §7.5](decision-history.md)）；[s3b-a2-design.md](history/s3b-a2-design.md) 过审后三片全落地：A2-1 split lower 机件（双队列/标签 id/双 arena 路由/编译期穷尽 rebase）、A2-2 写盘/装载/L2 键链（**eco 冷 924→热 66ms**，自愈矩阵验证）、A2-3 默认开启 + gate 双态冒烟 + S3′c 同 workspace 跨 bin 共享（gate5 50→51；[m6-log.md](history/m6-log.md) 片7/8/9） |
| M5.4（翻译器全覆盖） | **a/b 完成（2026-07-15），c/d 待施** | a = 帧模型 v2 + 内存操作数；b = 标量全集 + 128 位族 + atomics（[m5-log.md](history/m5-log.md) M5.4a/b 节）——含 analyze_frame 区间模型实锤根因修复（place 通道字节区间内槽漏提升 = 错值级）。oracle：逢调即编 diff 30/30 + diff_cargo 5/5 + gate5 51/0/0/0（fib(32) JIT 74ms ≤80ms 硬门）。c = ABI 泛化（Pair/Indirect/track_caller）+ LSDA 产品化（probe 5/5 已过）；d = SIMD + 收口 |
| 地址模型 P2（GOT 间接） | **完成（2026-07-17）** | §7.5b 手术单定场（真实地址模型保留）→ §7.5c 零 IR 变更 GOT 机制（槽 = 冻结区普通格 + 启动相重填；extern static/fn 值不再烤宿主地址，字节码复用 `Mem{Static(槽)}`/`SubImm` 通道，JIT/interp 零改动）→ §7.5d 拒缓存三判据全退役 + 纯 std 会话 want_split 修正（先存 A2 沉默债：L2 对纯 std 程序永 miss）。外来符号用例冷→热全通（c_process 463→30ms），gate5 117/0/0。JIT 间接调用准入记债（[open-issues.md E1](open-issues.md)） |
| 地址模型 P1（fn 条目可执行化） | **完成（2026-07-17，commit `4202317`）** | §7.6：FFI 可派生条目值 = 可执行 stub 码址（新第三固定地址域族 0x6C00/0x6D00/0x6E00+k + libffi closure 蹦床 + 配方随模块、启动相重建封存 RX）——thunk 盲区结构性根治（旧 debt §6 关闭，对照见 [open-issues.md](open-issues.md)；负对照 flate2 C-libz 结构体内嵌回调往返，三维+L2 热一致）。残余边界 = 签名不可派生条目（Rust ABI/聚合/变参）保持数据槽，无实质盲区；SIGSEGV 诊断化可选后补（open-issues T4）。gate5 117/0/0 |
| corpus 批7（激进 24 三波） | **完成（2026-07-17）** | 23/24 全绿可用（corpus.md §5 批7）；**修出两只产品 bug 当日修复**：native-archive 链接行收 crate 图动态库（`867b3de`，libgit2 红转绿）+ custom `#[global_allocator]` 运行时统一路由 `__rust_*`（§7.7，c_mimalloc 三维绿、跨堆 SIGSEGV 根治）。c_tree_sitter 按值聚合 FFI 记档（[open-issues.md C1](open-issues.md)，待立项转正）。gate5 128→**139**；corpus 实测真实 crate 总账 123 |
| corpus 批8（重型 10+ 三波） | **完成（2026-07-17，总收口 `2f9efef`）** | 波1 重 FFI/C 5/5 绿（ladder：lddqu 补面 + constructor 分治 + `\x01` 前缀剥除 + GOT 键名盲点，§7.8）；波2 VM/大物 4/5 绿（JIT analyze_frame 帧末 ZST 实锤修复 `dc6e30c`；c_wasmtime_wat 双层欠账记档 [open-issues.md C2/C3](open-issues.md)：符号在 rlib + asm noreturn）。gate5 139→**148**；corpus 实测真实 crate 总账 ≈133 |
| M5.5 | **未实现** | vmctx 终裁计量与 gate6 收口（[designs/m5-design.md](designs/m5-design.md) 原案不动）。S3′b 已定（A2 纯化聚合）；S3′c 完整形态按实需立项（open-issues D6） |

产品执行引擎 = M4 解释器 + 方法级 JIT（cranelift 为默认 feature；JIT 默认开启，
`--jit off`/`MIRVM_JIT=off` 回退纯解释；tsan harness 不开 cranelift）。M5.2 把解释器
语义面补全（gate5 从 40 PASS/2 XFAIL 升到 **46 PASS / 0 XFAIL / 0 FAIL**），为 JIT 期
交付语义面干净的基线；M5.4a/b 把 JIT 翻译器推进到标量/内存/128 位/原子全覆盖，
解释器继续作为差分 oracle 与未准入构造（ABI 泛化/cleanup 边/SIMD）的唯一语义源。

## 2. 当前实现路径

```text
.rs / Cargo 项目
  → cli / cargo wrapper + runner
  → rustc_driver::after_analysis（加载相仍可访问 tcx；callback 只 lower）
  → lower_program
      collector 种子 + call-site worklist 求单态化闭包
      冻结 layout / ABI / place / statics / vtable / TLS / FFI 签名
      物化可支持的 asm stubs 与受约束 Static native archives；未知构造降为带诊断的 Trap
  → tcx-free ir::Module（留在 callback）
  → Cargo runner 仅在 lower 后安装窄 TRACK_DIAGNOSTIC filter
  → run_compiler 完整执行 tcx.finish / diagnostics / compiler drop，然后恢复 filter
  → 仅 compiler success 时执行 Module
  → Shared（进程期）+ 每个宿主线程一个 Ctx
  → interp_frame / run_blocks
      guest 调用：宿主递归
      guest → native：dlsym + libffi
      native → guest：libffi closure thunk + TLS attach
      inline asm：调用已物化的 fn(*mut u8) stub
```

代码热点集中在 `src/lower/func.rs` 与 `src/vm/engine/interp.rs`。实现目前实质上是
Linux/ELF/x86_64 优先：依赖 pthread、dlopen、GNU 链接行为和 x86 asm wrapper。

## 3. 已验证边界

- debug/release 构建可通过；执行必须优先使用 release 版本。
- M4 的纯函数、值/内存、unwind、FFI、真线程和 TSan gate 已有端到端覆盖。
- `tests/diff.sh` 对当前 demo 做 native 差分，M5.0 的 asm probe 已进入回归。
- 2026-07-13 当前 Rust tests 为 **35/35 passed**：asm stub、12 个 native archive、
  2 个 FFI 必需/可选库、Width、9 个 volatile（含低对齐、padding、宽值和重叠快照）、
  direct dyn 尾动态对齐、2 个 x86 helper、6 个 Cargo shim/rustflags/wrapper 测试，
  以及 1 个 runner warning-summary 结构化过滤测试；Rust 层覆盖仍小，
  shell gate 仍是主要安全网。
- `tests/gate_truth_regression.sh` 最终 **12/12**，除了双方失败假绿、XFAIL 原因、
  XPASS、corpus 状态传播和 gate0/1/2，还锁住 TSan/性能/CPU 特性 SKIP 不得冒充
  PASS、`diff.sh` 单侧 stderr 不得被忽略，x86 vector 的 pshufb/SHA 子能力必须分别记账。
- `tests/m51_addcarry.sh` 已把 addcarry/subborrow 边界 checksum 与 native 差分，c_numbigint 转绿；
  `tests/m51_xgetbv.sh` 在 CPUID guard 下与 native 输出一致；x86 vectors 与 `simd_insert`
  /extract、SIMD shifts、vzeroupper 也有 feature-gated native differential probes。六个 M5.1
  release tracers 全部通过。
- M5.1 收官时的 full release gate5（含 gate0/1/2/4、TSan、六个 M5.1 tracer 脚本/七个子断言）为
  **40 PASS / 2 XFAIL / 0 SKIP / 0 FAIL**；XFAIL 分别是 signal guest handler 和
  guest backtrace/frame-IP 映射。
- 2026-07-13 本轮产品补丁后的最终 full gate5 为
  **40 PASS / 2 XFAIL / 0 SKIP / 0 FAIL**，`diff.sh` 为 **20/20**、`diff_cargo.sh` 为
  **5/5**；新增的 `cargo_warning_return` 锁住 Cargo runner 不得在 guest 正常返回后泄漏额外
  rustc warning summary，同时保留 guest 自己输出的同文 stderr。同轮 fmt、35/35 Rust tests、
  Clippy `-D warnings` 与 release build 均最终通过；最终复跑的 load/rayon 子项分别为 443 ms / 927 ms。
- `tests/real_projects_regression.sh` 最终 **65/65**，覆盖严格 case schema、repo/rev/lock provenance、
  native-first oracle、exit/stdout/stderr 精确比较、canonical XFAIL/XPASS、断网与写隔离、
  toolchain/host target、干净 env/EOF stdin、私有 runtime cache、source mutation，以及 benchmark
  correctness 准入、warmup/交替采样、每轮 oracle、基础设施状态、完整 summary provenance、
  namespace-private 稳定逻辑路径、项目 `.cargo/config.toml` rustflags 保留，以及会被私有 `/run`
  tmpfs 隐藏的 workspace/suite/toolchain/cache 宿主路径在启动时 fail-fast。新增 identity/evidence
  tracer 还锁住同名不同语义、Case/Check/Bench/Evidence 分层、Git/controller 与 sysroot 内容身份、
  exact check 依赖及发布前复验、同名与 suite-cache 并发锁、controller/grouping 子 shell 的锁 FD
  隔离、两层 hard-crash staging 与临时 symlink 回收、prepare 恢复、后台后代隔离、consumer 身份重推、
  PASS/XFAIL oracle 语义复算、exact check execution provenance、额外 sidecar 拒绝、
  samples/median/p95 复算、pre-rename 只读封存、current symlink 原子发布和 failed-check 撤销 current。
  无外部 workload tool 的当前发布格式仍是 schema 3；显式声明外部工具的 case 使用 schema 4
  记录其身份。consumer 仍按独立 schema-2/3 路径验证既有历史对象，不把新字段静默回写成旧
  schema 当时已具备的事实。
- 产品 Cargo shim 会在接管 Cargo 前检查非空 `RUSTC_WRAPPER` / `RUSTC_WORKSPACE_WRAPPER`，以及
  有效的 `build.rustc-wrapper` / `build.rustc-workspace-wrapper`；发现任一自定义 wrapper 就
  fail-closed。当前明确**不支持 wrapper composition**，不能把拒绝误写成项目 wrapper 已兼容。
- Cargo runner 的 callback 现在只 lower `Module`；lower 后才安装 `TRACK_DIAGNOSTIC` 结构化
  filter。它仅抑制无 lint/code/span/children/suggestions 的 `ForceWarning` `N warnings emitted`，
  并继续委托原 hook；guest 自己写出的同文 stderr 不经该路径。`run_compiler` 完整执行
  tcx/diagnostics/compiler 收尾并恢复 hook，只在 compiler success 后才执行 VM。直接
  `process::exit` 是已被取代的中间方案；纯单文件仍正常收尾。
- 当前工作集是 Git-ignored 的 `artifacts/real-projects/` 下固定 revision/lock 的 ripgrep 与 tokei。
  本轮完成 direct dyn 尾 alignment、u128 `SwitchInt`、`track_caller` Reify shim 和宽 volatile
  修复及最小 native 差分。最终使用 namespace-private `/run` 逻辑根 + RUSTC proxy harness 复跑：
  ripgrep 与 tokei 的原始 JSON 切片、稳定 compact languages 切片均 PASS，warmup 1、samples 3，且
  共同使用 release mirvm sha256
  `7b064b3f8861e39cfe07dd7df67583d73c0ec32bcd6108e12fb8e217c89da16e`。ripgrep native/mirvm
  median 分别为 52.10 ms/3.687 s；原始 tokei JSON 为 129.80 ms/3.796 s；覆盖 `tests/data`
  206 个 tracked fixtures 的 `tokei_languages` 为 148.10 ms/4.663 s，205 行 stdout 两侧
  SHA-256 均为 `0f5058901e0042629c9ff02714134206ca417235ebf6f892aeccec03a34d64c4`。
  这些 case manifest 与结果仍被 Git ignore，仓库尚未提交远程项目 case，
  因此这些结果仍不是持续 gate，也不能外推为“任意 Rust”。完整合同、revision 和证据层级见
  [real-projects.md](real-projects.md)。
- 本轮最终 `cargo fmt --all -- --check`、35/35 Rust tests、Clippy `-D warnings` 与 release build
  均通过。GitHub Actions workflow 已建立，执行 fmt、Clippy、Rust tests、gate harness、
  release build、独立可见的 TSan 与 runtime gates。
- 测试脚本必须以**可观察输出或不变式**为 oracle，不能把“双方都失败”或“仅退出码相同”当成功。

历史日志中的 “gate5 31/31” 只表示旧脚本口径，不再等同于 31 个语义正确断言。特别是：

- `c_signal` 曾在 handler 未执行时仍以退出码 0 被判绿；
- `diff_cargo.sh` 曾可能把 native 与 mirvm 同时失败判为 PASS；
- `corpus.sh` 曾可能在存在失败时仍以状态 0 结束。

这些发现促成了 M5.1 退出标准重写。最终结果已记录在 m5-log；不能把新 oracle 回写成旧日志
当时已经具备的事实。

## 4. 当前缺口与诚实边界

| 类别 | 当前状态 |
|---|---|
| signal / `sigaction`（M5.2 D8d） | **async 信号 guest handler 已支持**：经 AS-trampoline（复用 M4.4 thunk 工厂，`(i32)->void`）真执行，signal() 与 sigaction() 两条注册路，重入（handler 内嵌套 raise）已 native 差分。SIG_DFL/IGN 直通。**sync 故障信号（SEGV/BUS/FPE/ILL/TRAP）guest handler 仍响亮拒绝**（宿主/guest 故障不可分辨）。c_signal 转绿 |
| backtrace（M5.2 D8e） | **guest 影子帧栈已支持**：Ctx 维护每帧合成 IP，`_Unwind_Backtrace/GetIP(Info)/FindEnclosingFunction/GetCFA` 由影子帧诚实回答。合成 IP 不经 dladdr 符号化（诚实 `<unknown>`，不伪造宿主符号）→ backtrace 文本非 well-defined，oracle 是影子帧不变式（捕获/非空/深度反映）。c_backtrace 转绿。其余 `_Unwind_Set/GetGR/Resume/CFA-外` context 家族维持 `Unsupported`。`atexit` 已 builtin 化（引擎 LIFO + libc trampoline）；`dl_iterate_phdr` 仍走 native FFI |
| fork / exec（M5.2 D8f） | **exec 族直通**（进程替换语义正确）；**fork 仅 guest 单线程时放行**（守卫用 `/proc/self/task` 对 guest-main 基线判定，避 Ctx 计数 TOCTOU）——解锁 `Command::pre_exec`。多线程 fork、vfork/clone/setjmp 系维持响亮拒绝 |
| volatile | 独立 volatile IR 使用 alignment=1 的 opaque `MaybeUninit` 字节载体，不把 padding 解释成宿主整数。1/2/4/8/16-byte 保持单个后端 volatile 事件；更宽 memory-repr 值先快照，再按 16/8/4/2/1-byte 块分解，不承诺原子性。该结论于 2026-07-13 推翻旧“其他宽度 Trap/不得拆”选择，演变见 decision-history |
| direct dyn 尾字段 | sized prefix 后的 direct `dyn` 尾不能一律使用 lower 期静态 offset；当前从 vtable 读取运行期 alignment，并考虑 `repr(packed)` 上限后向上取整。slice/str 仍走静态公式，其他嵌套 DST 继续显式拒绝 |
| 128-bit `SwitchInt` | targets 与 discriminator 现都保留完整 128 位；i128/u128 discriminator 由 `SwitchDiscr::Wide` 从 place 读取，不再截成 u64。这不等于所有 128-bit ABI 形态都已标量化 |
| `track_caller` fn pointer | `ReifyFnPointer` 使用 rustc `resolve_for_fn_ptr`；需要 caller location 时由 Reify shim 以普通 fn-pointer ABI 接参并补 Location。ClosureFnPointer 等其他 adjustment 尚不能由此外推 |
| M5.1 收口 | 六个 release native-differential tracer 脚本通过，x86_vectors 内 pshufb/SHA 分别记账；numbigint、xgetbv、sha2、blake3、ecosystem 全部转绿。M5.1 旧前沿 expected-red 已删除，diff_cargo 3/3；signal/backtrace 仍是独立 XFAIL |
| x86 向量 helper | pshufb128/256 与 SHA256 msg1/msg2/rnds2 已通过 tcx-free stdarch target-feature helpers 接入；m51_x86_vectors native 差分与 c_sha2 两个标准 SHA256 输出通过 |
| guest 静态归档 | Linux/ELF 受约束路径已接产品：收集 rustc `Static NativeLib`、内容寻址 `.a→.so`，作为 required library 在任何 dlsym 前以 `RTLD_NOW` 加载；失败保留 `dlerror` 并立即终止。constructor/destructor 已分治（§7.8：`.init_array/.fini_array/ctors/dtors` 段经 DT_INIT 与 native 同构放行；裸 `.init/.fini` 仍拒）；RTLD_DEFAULT 同名碰撞已改归档句柄优先（`fb0b204`，native 链接期绑定语义）。其余拒绝面仍在：非 PIC、thin、跨 archive 依赖/顺序/重名导出、export-symbols——多 archive link plan 未立项（[open-issues.md R6](open-issues.md)），不是通用链接器 |
| JIT | **生产已落地**：方法级 Cranelift JIT = M5.3 骨架 + M5.4a/b（标量/内存/128 位/原子全覆盖），默认开启（`--jit off`/`MIRVM_JIT=off` 回退纯解释）。未实现：JIT LSDA 产品化与 ABI 泛化（M5.4c，open-issues T1）、SIMD（T2）、OSR/deopt/生产 tiering 终裁（E2）、间接调用准入（E1）、JIT 码常驻（E5） |
| 生命周期/嵌入 | `Shared`、thunk、asm handle、部分 TLS 存储按进程期保存；错误路径可退出进程；runner 诊断 filter 依赖进程全局 `TRACK_DIAGNOSTIC`，目前只在单 compiler CLI 模型下安全。daemon/嵌入/并发 compiler 需先加 guard 或替代接口；尚非稳定多 Engine API |
| 分发与产品面 | `.mirvm` mode B、daemon、REPL、checked 模式、正式沙箱均未实现；分发轨方向已批未立项（D9，2026-07-14，[distribution-design.md](designs/distribution-design.md)）：先 L2 engine-IR 缓存，mode B=缓存可移植化（M5.3 后），发行先 miri 式 |
| 平台 | 当前仅应宣称 Linux/ELF/x86_64 开发基线 |
| Cargo wrapper / runner | 非空 `RUSTC_WRAPPER` / `RUSTC_WORKSPACE_WRAPPER` 或有效的 `build.rustc-wrapper` / `build.rustc-workspace-wrapper` 当前都会在 MIR capture 前 fail-closed；尚无 wrapper composition。runner 在 lower 后用窄结构化 filter 去除额外 warning-count summary，但保留完整 driver 收尾，只在 compiler success 后执行 VM |
| 真实项目隔离 | build/check unshare network，所有 guest sandbox unshare PID，但不 unshare IPC；仅适用于受信任、固定 provenance 的 source，不是对抗性安全边界。mirvm build env 与 guest runtime env 尚未彻底分离。`MIRVM_ENCODED_RUSTFLAGS_APPEND` 在 Cargo 完成 fingerprint 计算后才追加，尚未进入 Cargo fingerprint |
| 任意 Rust / 真实项目 | 尚不支持任意 Rust；已有严格 real-project harness、两个项目的十一个 correctness PASS workload，其中三个完成 benchmark，以及通用修复的最小差分回归。case 仍是 Git-ignored workspace evidence，远程固定 case 尚未入库 |

架构目标不等于当前资格。尤其“RAM 运行参考实现”是长期语义契约；在上述已知缺口和有限 corpus
仍存在时，不应把它描述成已经覆盖完整 Rust 语义的成品。

## 5. 当前开发顺序

1. **corpus 扩编继续**：三维逐字节差分铁律不动摇（mirvm 默认 / native / 逢调即编，
   全部逐字节一致才算绿）；新候选见 [corpus.md §7](corpus.md)；撞出的实锤债务登记到
   [open-issues.md](open-issues.md) 再按优先级转正。
2. **债务转正优先级**（以 open-issues.md 为准）：T1/T2（M5.4c/d）蓝图在手；
   C1 按值聚合 FFI、C2/C3（wasmtime 双层）、C4（dep global_asm）按真实 workload 排序。
3. **基建预算纪律**（根 AGENTS.md）：harness 只在当前产品 RED 无法复现/判定正确时
   做最小修改；不为未来加固。
4. **文档纪律**：完成阶段 = 代码 + 可复现 gate + 施工记录 + 本文更新四件套；
   新债入 open-issues.md，推翻入 decision-history.md，history/ 只读不再更新。
5. **远程项目与 GitHub Issues 暂停**至维护者明确恢复（open-issues G1）。

## 6. 完成一个阶段时如何更新

阶段完成必须同时留下四类证据：代码、可复现 gate、对应施工记录（history/ 日志或
decision-history 条目）、本文的状态变化。若结果推翻既有设计，还须在
[decision-history.md](decision-history.md) 记录旧选项为何曾合理、什么新证据触发了
改变，以及未来何时应重新评估；新产生的未解决债务登记到
[open-issues.md](open-issues.md) 对应分区。
