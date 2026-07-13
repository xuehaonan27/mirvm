# mirvm 当前开发状态

> 状态日期：2026-07-13。本文是当前状态的唯一汇总入口；若与早期计划、README 或交接文档
> 冲突，以当前代码、可复现测试结果和本文为准。文档权威规则见 [README.md](README.md)。

## 1. 阶段结论

| 阶段 | 状态 | 当前含义 |
|---|---|---|
| M0–M2.5 | **历史完成** | 基于 rustc `InterpCx` 的 bootstrap；tier-0 源码已于 2026-07-09 删除 |
| M4.0–M4.5 | **完成** | 自研 typed bytecode、tcx-free tree-walking interpreter、FFI、unwind、真线程、TLS 与回调 |
| M5.0 | **完成并复审** | x86_64 inline-asm stub 工厂：GAS wrapper → `.so` → `dlopen`/`dlsym` |
| M5.1 | **完成（2026-07-12）** | numbigint、xgetbv、sha2、blake3、ecosystem、diff_cargo 3/3 与六个 release tracer 全绿 |
| M5.2–M5.4 | **未实现** | 方法级 Cranelift JIT、tiering、JIT unwind/LSDA 与性能收口 |

目前唯一产品执行引擎是 M4 解释器。Cargo 默认的 `cranelift` feature 只编译冻结的 Spike 5；
生产调用路径还没有方法级 JIT，也没有 `mixed`/`jit` 产品模式。

## 2. 当前实现路径

```text
.rs / Cargo 项目
  → cli / cargo wrapper + runner
  → rustc_driver::after_analysis（加载相仍可访问 tcx）
  → lower_program
      collector 种子 + call-site worklist 求单态化闭包
      冻结 layout / ABI / place / statics / vtable / TLS / FFI 签名
      物化可支持的 asm stubs 与受约束 Static native archives；未知构造降为带诊断的 Trap
  → tcx-free ir::Module
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
- 2026-07-13 当前 Rust tests 为 **25 passed**：asm stub、
  12 个 native archive、2 个 FFI 必需/可选库、Width、5 个 volatile（含低对齐和
  padding）、2 个 x86 helper，以及 harness Cargo 命令 locked/普通 CLI 可创建 lockfile 两种模式；Rust 层覆盖仍小，
  shell gate 仍是主要安全网。
- `tests/gate_truth_regression.sh` **10/10**，除了双方失败假绿、XFAIL 原因、
  XPASS、corpus 状态传播和 gate0/1/2，还锁住 TSan/性能/CPU 特性 SKIP 不得冒充
  PASS，x86 vector 的 pshufb/SHA 子能力必须分别记账。
- `tests/m51_addcarry.sh` 已把 addcarry/subborrow 边界 checksum 与 native 差分，c_numbigint 转绿；
  `tests/m51_xgetbv.sh` 在 CPUID guard 下与 native 输出一致；x86 vectors 与 `simd_insert`
  /extract、SIMD shifts、vzeroupper 也有 feature-gated native differential probes。六个 M5.1
  release tracers 全部通过。
- 最终 full release gate5（含 gate0/1/2/4、TSan、六个 M5.1 tracer 脚本/七个子断言）为
  **40 PASS / 2 XFAIL / 0 SKIP / 0 FAIL**；XFAIL 分别是 signal guest handler 和
  guest backtrace/frame-IP 映射。
- 2026-07-13 最终复跑仍为 full gate5 **40 PASS / 2 XFAIL / 0 SKIP / 0 FAIL**，
  `diff.sh` 17/17、`diff_cargo.sh` 3/3；本次性能快照 load 397ms、rayon 820ms。
- `tests/real_projects_regression.sh` **38/38**，覆盖严格 case schema、repo/rev/lock provenance、
  native-first oracle、exit/stdout/stderr 精确比较、canonical XFAIL/XPASS、断网与写隔离、
  toolchain/host target、干净 env/EOF stdin、私有 runtime cache、source mutation，以及 benchmark
  correctness 准入、warmup/交替采样、每轮 oracle、基础设施状态和完整 summary provenance。
- 已有本地临时 clone 的真实项目证据：hexyl correctness PASS 且 3 样本 benchmark 成功；ripgrep
  与 tokei 分别在完整调用点诊断上精确 XFAIL。仓库尚未提交远程项目 case，因此这些结果不能称为
  持续 gate，也不能外推为“任意 Rust”。完整合同与 revision 见
  [real-projects.md](real-projects.md)。
- `cargo fmt --all -- --check` 与 Clippy `-D warnings` 当前均通过。GitHub Actions workflow 已建立，
  执行 fmt、Clippy、Rust tests、gate harness、release build、独立可见的 TSan 与 runtime gates。
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
| signal / `sigaction` | 静默 StubZero 已移除；SIG_DFL/SIG_IGN 可受限直通，guest handler 当前明确 Trap。异步信号安全 trampoline 与真实 handler 语义仍未实现 |
| 其他旧 StubZero 边界 | `atexit`、`dl_iterate_phdr` 改走 native FFI 后仍需逐项 differential probe。`_Unwind_Backtrace` 与 Get/Set context 家族已显式 `Unsupported`：宿主 unwinder 只能返回解释器/libffi 栈，在 guest frame/IP 映射完成前不允许伪造成功 |
| volatile | 已增加独立 volatile IR，以 alignment=1 的 opaque `MaybeUninit<[u8; N]>` 执行等宽 1/2/4/8/16-byte 访问；因此低对齐聚合值和未初始化 padding 不会被解释为宿主整数。其他宽度明确 Trap |
| M5.1 收口 | 六个 release native-differential tracer 脚本通过，x86_vectors 内 pshufb/SHA 分别记账；numbigint、xgetbv、sha2、blake3、ecosystem 全部转绿。M5.1 旧前沿 expected-red 已删除，diff_cargo 3/3；signal/backtrace 仍是独立 XFAIL |
| x86 向量 helper | pshufb128/256 与 SHA256 msg1/msg2/rnds2 已通过 tcx-free stdarch target-feature helpers 接入；m51_x86_vectors native 差分与 c_sha2 两个标准 SHA256 输出通过 |
| guest 静态归档 | Linux/ELF 受约束路径已接产品：收集 rustc `Static NativeLib`、内容寻址 `.a→.so`，作为 required library 在任何 dlsym 前以 `RTLD_NOW` 加载；失败保留 `dlerror` 并立即终止。blake3 与 native 三行 hash 一致。thin、`.init/.fini`/constructor/destructor（含优先级 section）、非 PIC、跨 archive 依赖/顺序或重名导出、RTLD_DEFAULT 冲突、export-symbols 等会响亮拒绝，不是通用链接器 |
| JIT | 只有 spike；没有生产 tiering、OSR、deopt 或 JIT LSDA |
| 生命周期/嵌入 | `Shared`、thunk、asm handle、部分 TLS 存储按进程期保存；错误路径可退出进程；尚非稳定多 Engine API |
| 分发与产品面 | `.mirvm` mode B、daemon、REPL、checked 模式、正式沙箱均未实现 |
| 平台 | 当前仅应宣称 Linux/ELF/x86_64 开发基线 |
| 任意 Rust / 真实项目 | 尚不支持任意 Rust；已有严格 real-project harness 和一个本地 PASS 项目切片，另有两个精确 XFAIL。远程固定 case 尚未入库 |

架构目标不等于当前资格。尤其“RAM 运行参考实现”是长期语义契约；在上述已知缺口和有限 corpus
仍存在时，不应把它描述成已经覆盖完整 Rust 语义的成品。

## 5. 当前开发顺序

1. **已完成第一版**：真实项目 TDD harness、可信 oracle、精确 XFAIL 与 correctness-gated benchmark；
   继续把临时实证转成可维护的固定 case。
2. **已落第一版**：移除 silent stub、让 guest signal handler 和不能翻译的 unwinder
   context 边界明确 Trap、补真 opaque volatile IR；继续为安全转入 native FFI 的边界
   增加 differential probes。
3. **已完成第一版**：Rust tests、rustfmt、Clippy 与 CI workflow 均已建立并在本地质量门通过；
   后续随功能继续扩充覆盖。
4. 保持本文、根 README、DESIGN、HANDOFF 和施工日志同步；历史模型只标注替代，不删除。
5. **已完成**：M5.1 按真实前沿收窄并实现；signal/backtrace 两个独立
   XFAIL 没有被伪装成 M5.1 绿。
6. 远程项目与 GitHub Issues 当前按维护要求暂停；恢复后再提交固定 manifests、来源策略与持续 gate。
7. 产品语义施工应优先由真实项目的精确失败前沿驱动；进入 M5.2 方法级 JIT 时，保持
   JIT-on/off/native 三方 oracle 计划。

## 6. 完成一个阶段时如何更新

阶段完成必须同时留下四类证据：代码、可复现 gate、对应施工日志、本文的状态变化。若结果推翻
既有设计，还须在 [decision-history.md](decision-history.md) 记录旧选项为何曾合理、什么新证据
触发了改变，以及未来何时应重新评估。
