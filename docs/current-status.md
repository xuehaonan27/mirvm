# mirvm 当前开发状态

> 状态日期：2026-07-14。本文是当前状态的唯一汇总入口；若与早期计划、README 或交接文档
> 冲突，以当前代码、可复现测试结果和本文为准。文档权威规则见 [README.md](README.md)。

## 1. 阶段结论

| 阶段 | 状态 | 当前含义 |
|---|---|---|
| M0–M2.5 | **历史完成** | 基于 rustc `InterpCx` 的 bootstrap；tier-0 源码已于 2026-07-09 删除 |
| M4.0–M4.5 | **完成** | 自研 typed bytecode、tcx-free tree-walking interpreter、FFI、unwind、真线程、TLS 与回调 |
| M5.0 | **完成并复审** | x86_64 inline-asm stub 工厂：GAS wrapper → `.so` → `dlopen`/`dlsym` |
| M5.1 | **完成（2026-07-12）** | numbigint、xgetbv、sha2、blake3、ecosystem、diff_cargo 3/3 与六个 release tracer 全绿 |
| 真实项目 TDD | **继续扩面（2026-07-13）** | workspace-local ripgrep/tokei 驱动四项通用语义修复；三个 workload 已完成 correctness-gated benchmark，另有八个 workload 完成 correctness 对拍 |
| M5.2 | **完成（2026-07-14）** | 非 JIT 语义补全（轨 A 完备）：标量/simd intrinsic 差集、真栈深度、f16/f128、atomic 序、backtrace/signal/fork/atexit/global_asm/naked、128 位残余、嵌套 DST 全清；两个历史 XFAIL 转绿（[m5.2-design.md](m5.2-design.md) D8a–D8l，施工日志见 [m5-log.md](m5-log.md)） |
| M6 ①② | **施工中（2026-07-14 立项）** | 轨 C 分发（[distribution-design.md](distribution-design.md) D9f）：① 相位计时 + ② L2 post-mono engine-IR 缓存；③④⑤ 未立项；施工日志 [m6-log.md](m6-log.md) |
| M5.3–M5.5 | **未实现** | 方法级 Cranelift JIT、tiering、JIT unwind/LSDA 与性能收口（原编号 M5.2–M5.4，2026-07-14 顺延） |

目前唯一产品执行引擎是 M4 解释器。Cargo 默认的 `cranelift` feature 只编译冻结的 Spike 5；
生产调用路径还没有方法级 JIT，也没有 `mixed`/`jit` 产品模式。M5.2 把解释器语义面补全
（gate5 从 40 PASS/2 XFAIL 升到 **46 PASS / 0 XFAIL / 0 FAIL**），为 JIT 期交付语义面
干净的基线。

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
| guest 静态归档 | Linux/ELF 受约束路径已接产品：收集 rustc `Static NativeLib`、内容寻址 `.a→.so`，作为 required library 在任何 dlsym 前以 `RTLD_NOW` 加载；失败保留 `dlerror` 并立即终止。blake3 与 native 三行 hash 一致。thin、`.init/.fini`/constructor/destructor（含优先级 section）、非 PIC、跨 archive 依赖/顺序或重名导出、RTLD_DEFAULT 冲突、export-symbols 等会响亮拒绝，不是通用链接器 |
| JIT | 只有 spike；没有生产 tiering、OSR、deopt 或 JIT LSDA |
| 生命周期/嵌入 | `Shared`、thunk、asm handle、部分 TLS 存储按进程期保存；错误路径可退出进程；runner 诊断 filter 依赖进程全局 `TRACK_DIAGNOSTIC`，目前只在单 compiler CLI 模型下安全。daemon/嵌入/并发 compiler 需先加 guard 或替代接口；尚非稳定多 Engine API |
| 分发与产品面 | `.mirvm` mode B、daemon、REPL、checked 模式、正式沙箱均未实现；分发轨方向已批未立项（D9，2026-07-14，[distribution-design.md](distribution-design.md)）：先 L2 engine-IR 缓存，mode B=缓存可移植化（M5.3 后），发行先 miri 式 |
| 平台 | 当前仅应宣称 Linux/ELF/x86_64 开发基线 |
| Cargo wrapper / runner | 非空 `RUSTC_WRAPPER` / `RUSTC_WORKSPACE_WRAPPER` 或有效的 `build.rustc-wrapper` / `build.rustc-workspace-wrapper` 当前都会在 MIR capture 前 fail-closed；尚无 wrapper composition。runner 在 lower 后用窄结构化 filter 去除额外 warning-count summary，但保留完整 driver 收尾，只在 compiler success 后执行 VM |
| 真实项目隔离 | build/check unshare network，所有 guest sandbox unshare PID，但不 unshare IPC；仅适用于受信任、固定 provenance 的 source，不是对抗性安全边界。mirvm build env 与 guest runtime env 尚未彻底分离。`MIRVM_ENCODED_RUSTFLAGS_APPEND` 在 Cargo 完成 fingerprint 计算后才追加，尚未进入 Cargo fingerprint |
| 任意 Rust / 真实项目 | 尚不支持任意 Rust；已有严格 real-project harness、两个项目的十一个 correctness PASS workload，其中三个完成 benchmark，以及通用修复的最小差分回归。case 仍是 Git-ignored workspace evidence，远程固定 case 尚未入库 |

架构目标不等于当前资格。尤其“RAM 运行参考实现”是长期语义契约；在上述已知缺口和有限 corpus
仍存在时，不应把它描述成已经覆盖完整 Rust 语义的成品。

## 5. 当前开发顺序

1. **继续按真实项目扩面**：当前 harness 已能给出可信 correctness 结果，按根 `AGENTS.md` 的
   infrastructure budget discipline 冻结，不再把 suite inventory、schema 扩展或一般性加固作为
   下一步。一次只增加一个 ripgrep/tokei workload；GREEN 立即前进，只有当前产品 RED 无法复现或
   判定时才允许做最小基础设施修改，否则应最小化失败、补公开接口回归并修复产品代码。
2. **已落第一版**：移除 silent stub、让 guest signal handler 和不能翻译的 unwinder
   context 边界明确 Trap、补真 opaque volatile IR；继续为安全转入 native FFI 的边界
   增加 differential probes。
3. **已完成第一版**：Rust tests、rustfmt、Clippy 与 CI workflow 均已建立；最终 fmt、
   35/35 Rust tests、Clippy、release build 与既有 40 PASS / 2 XFAIL 聚合 gate 均通过，后续继续扩充覆盖。
4. 保持本文、根 README、DESIGN、HANDOFF 和施工日志同步；历史模型只标注替代，不删除。
5. **已完成**：M5.1 按真实前沿收窄并实现；signal/backtrace 两个独立
   XFAIL 没有被伪装成 M5.1 绿。
6. 远程项目与 GitHub Issues 当前按维护要求暂停；恢复后再提交固定 manifests、来源策略与持续 gate。
7. 产品语义施工应优先由真实项目的精确失败前沿驱动；进入 M5.3 方法级 JIT 时，保持
   JIT-on/off/native 三方 oracle 计划。

## 6. 完成一个阶段时如何更新

阶段完成必须同时留下四类证据：代码、可复现 gate、对应施工日志、本文的状态变化。若结果推翻
既有设计，还须在 [decision-history.md](decision-history.md) 记录旧选项为何曾合理、什么新证据
触发了改变，以及未来何时应重新评估。
