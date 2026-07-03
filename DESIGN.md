# mirvm — 设计文档

> 工作代号 `mirvm`（MIR Virtual Machine），随时可改名。
> 创建于 2026-07-03，记录项目奠基时的架构决策。改动决策请更新本文档。

## 1. 一句话定义

一个**拥有自己执行引擎的 Rust runtime**：用真 rustc 做前端（宏展开、类型检查、trait 求解、诊断），
拿到 MIR 之后由自研引擎解释执行（后续增加可开关的 Cranelift JIT 热点编译），
跳过 codegen 和链接，实现"改完即跑"。

对标定位："LuaJIT for Rust"。**不是** evcxr（编译器外壳）、**不是** Miri（UB 检测器）的替代品。

## 2. 目标与优先级（2026-07-03 决策）

按优先级排序，前三项均为 P0，第四项明确后置：

1. **LLM/Agent 脚本执行**：单文件 Rust 脚本快速启动；沙箱与资源限制（超时/内存/syscall 白名单）；
   结构化 JSON 诊断（rustc 自带，免费）。语法对齐即将 stable 的 cargo script frontmatter
   （RFC 3424，单文件内嵌 manifest）。
2. **项目开发内循环加速**：`mirvm run .` 跑完整 cargo 项目，改一行代码亚秒级重跑
   （省 codegen + 链接；依赖 MIR 一次构建全局缓存）。
3. **REPL / Notebook**：交互式后置（M6），但架构为其铺路——解释器拥有堆栈，
   状态持久化天然成立，跨 cell 借用不再是问题。
4. ~~嵌入式脚本引擎~~：暂不紧要。但从第一天起 engine 做成 library、CLI 只是薄壳，
   保证这条路不被堵死。

### 执行模式（用户明确要求，HotSpot 风格）

先把**解释器**做好做对，JIT 之后再加，且始终可配置：

```
--engine=interp   # 纯解释（类比 java -Xint），默认（JIT 落地前唯一模式）
--engine=mixed    # 解释 + 热点 JIT（类比 -Xmixed），JIT 落地后的默认
--engine=jit      # 尽量全 JIT（类比 -Xcomp），用于对比测试
```

## 3. 为什么是路线 A（rustc 前端 + 自研执行引擎）

评估过的三条路线（详细分析见 2026-07-03 会话记录）：

| 路线 | 结论 |
|---|---|
| **A. rustc_private 前端 + 自研执行层** | ✅ 选定。语言 100% 保真，自研含量集中在执行引擎（最有价值的部分） |
| B. rustc + cg_clif JIT 整合 | 备胎/兜底。出货快但核心不是自己的；Windows JIT 不可用、unwinding 仅 Linux |
| C. 完全不依赖 rustc（ra_ap_* 或裸写） | ❌ 否决。前端是十年工程；诊断质量长期弱于 rustc，恰好毁掉"LLM 写 Rust 更准"的红利 |

决定性理由：

- **四堵墙**决定了绕不开 rustc：① 前端（trait solver/类型推断/宏卫生）是十年工程；
  ② proc-macro 是编译期运行的原生代码，纯解释器不成立；③ 泛型跨 crate 单态化，
  依赖的 MIR 也必须能执行；④ std 地基是 unsafe + intrinsics + syscall，shim 工作量巨大。
- 复用 rustc 前端后，**layout / ABI 与原生代码精确一致**（直接用 rustc 的 layout query），
  为后续"非泛型依赖函数原生直调"和 FFI 留下正确性基础。
- "在 mirvm 里能跑 ≈ 能通过 rustc 编译"的保证不丢，这是 LLM 场景的核心价值。

## 4. 架构

```
┌─────────────────────────────────────────────────────┐
│ 产品层: mirvm CLI / daemon / (后置: REPL, 嵌入 API)   │
├─────────────────────────────────────────────────────┤
│ 运行时服务: syscall shims / FFI(libffi) / 线程调度    │
│            / panic-unwind / 沙箱与资源限制           │
├─────────────────────────────────────────────────────┤
│ 执行引擎 (自研核心，分阶段演进):                       │
│   tier 0: fast Machine on rustc InterpCx  (M1)      │
│   tier 1: 自研紧凑字节码 VM + 内联缓存      (M4)      │
│   tier 2: Cranelift 热点 JIT（可开关）      (M5)      │
├─────────────────────────────────────────────────────┤
│ 前端: rustc (rustc_private) → typeck 后的 MIR        │
│   依赖/std: -Zalways-encode-mir 构建，全局缓存        │
├─────────────────────────────────────────────────────┤
│ 输入: 单文件脚本 (cargo script frontmatter) / cargo 项目│
└─────────────────────────────────────────────────────┘
```

### 关键设计决策

| # | 决策 | 理由 |
|---|---|---|
| D1 | **不做 borrow check** | 不影响合法程序的运行语义；留给 CI 的真 rustc。大幅砍范围 |
| D2 | **惰性单态化** | 解释器带着泛型实参直接执行 MIR，不生成代码。这正是省掉 codegen 的地方 |
| D3 | **起步用 rustc 内置 `InterpCx` + 自写 fast Machine** | rustc_const_eval 的解释器核心是通用的，Miri 只是一个"开满检查的 Machine"。我们写"关掉全部 UB 检查、为吞吐调优"的 Machine，数周可跑真程序。它同时是后续自研 VM 的**正确性基线（差分测试 oracle）** |
| D4 | **unwind 自己实现** | 解释器拥有栈帧，panic/catch_unwind 跨平台无痛（cg_clif JIT 做不到的点） |
| D5 | **std/依赖以 `-Zalways-encode-mir` 构建** | rlib 携带全部函数 MIR（Miri sysroot 同款做法）；一次构建，内容寻址全局缓存 |
| D6 | **proc-macro 原生编译执行** | 别无选择；由 cargo 正常构建 proc-macro crate，rustc 前端照常加载 |
| D7 | **FFI 走 libffi 蹦床** | Miri native-lib 模式已趟路（整数/指针参数可用；C→Rust 回调是已知坑，后置） |
| D8 | **线程先做协作式调度** | Miri 同款；真 OS 线程并发解释后置 |
| D9 | **nightly 锁定 + 定期 bump** | rustc_private API 随 nightly 漂移；Miri/clippy/kani 证明可维护。策略：`rust-toolchain.toml` 锁具体日期版本，每月 bump 一次，rustc 交互层集中在少数模块隔离漂移面 |
| D10 | **engine 是 library，CLI 是薄壳** | 为嵌入场景（现在后置）留路；也方便测试 |
| D11 | **Miri 代码可借鉴移植** | MIT/Apache-2.0 双许可，shim 结构、intrinsic 清单尤其值得参考（保留 attribution） |

### 明确的非目标

- 不做 UB 检测（那是 Miri 的工作；我们假设程序合法，追求快）
- 不自研前端、不自研 trait solver
- 不追生产级峰值性能（目标：解释 tier 可用于脚本/测试反馈，JIT tier ≈ debug build）
- 初期不支持 Windows（unwinding/FFI 都以 Unix 优先；macOS 次之）

## 5. 里程碑

- **M0 — 工具链打通**（今天）：rustc_private 驱动能编译源文件、定位 entry fn、打印其 MIR。
  验收：`mirvm run demo/fib.rs --dump-mir` 输出 main 的 MIR。
- **M1 — 最小解释器**：fast Machine on InterpCx；纯计算程序 → panic/unwind →
  std hello world（第一批 syscall shims：write/exit/alloc 系）。
  验收：fib、Vec/String/HashMap、panic+catch_unwind、println! 全通过；
  与原生编译产物差分测试（stdout/exit code 一致）。
- **M2 — 吃下真实生态**：cargo 集成（依赖用 `-Zalways-encode-mir` 构建 + 全局缓存）、
  proc-macro、文件/env/时间/随机数 shims、libffi FFI、协作式线程。
  验收：跑通用 serde_json + rand + regex 的真实脚本。
- **M3 — 产品面（对齐 P0 场景）**：`mirvm run` 单文件脚本（frontmatter 依赖声明）与
  cargo 项目两种入口；常驻 daemon（前端增量状态留内存）；agent API：
  JSON 诊断、超时、内存上限、syscall 白名单沙箱。
  验收：脚本二次运行（缓存热）端到端 < 300ms；改一行重跑 < 1s（中型项目）。
- **M4 — 性能 tier 1**：MIR → 自研紧凑字节码 VM（替换 InterpCx 热路径）、内联缓存、
  非泛型依赖函数原生直调。验收：对 InterpCx 基线 ≥ 5× 提速；差分测试全绿。
- **M5 — JIT tier 2**：Cranelift 热点编译，`--engine=interp|mixed|jit` 开关。
  验收：计算密集 benchmark ≈ cg_clif debug build 的 2× 以内。
- **M6 — REPL/Notebook**（用户决策：后置）：解释器持久堆上的增量求值；跨 cell 借用成立。
- **M7+ — 嵌入 API**（用户决策：暂不紧要）。

## 6. 风险与对策

| 风险 | 对策 |
|---|---|
| nightly API 漂移导致维护负担 | D9 锁定+定期 bump；rustc 交互隔离在 `src/rustc_glue/`；关注 Miri 同步提交作为迁移指南 |
| std shims 工作量失控（最大风险） | 按需实现（跑目标程序缺什么补什么）；大量借鉴 Miri（D11）；M1 只做 write/exit/alloc 最小集 |
| InterpCx 性能天花板（AllocId 间接寻址等） | 它只是 tier 0 基线；M4 自研 VM 才是性能答案；届时 InterpCx 转为差分测试 oracle |
| FFI 回调（C→Rust）难做对 | 已知坑（Miri 同样未解决）；M2 只承诺单向调用，回调场景明确报错 |
| 异步/tokio 生态（epoll/io_uring shims） | 大工程，放 M3 后评估；先支持阻塞式 std |
| cargo script 语法仍在 FCP | 跟踪 rust-lang/cargo#16569，语法完全对齐官方，不自创 |

## 7. 先行者参考

- [evcxr](https://github.com/evcxr/evcxr) — 编译器外壳式 REPL（我们的反面教材：延迟、状态搬运、不可嵌入）
- [Miri](https://github.com/rust-lang/miri) — InterpCx/Machine 用法、shims、intrinsics 的最佳参考实现
- [rustc_codegen_cranelift](https://github.com/rust-lang/rustc_codegen_cranelift) — M5 JIT 的直接参考；[2025-06 unwinding 进展](https://bjorn3.github.io/2025/06/30/progress-report-june-2025.html)
- [subsecond / ThinLink](https://docs.rs/subsecond) — 热补丁与增量链接思路（路线 B 元素，daemon 阶段可借鉴）
- [cargo script RFC 3424](https://rust-lang.github.io/rfcs/3424-cargo-script.html) · [稳定化 PR](https://github.com/rust-lang/cargo/pull/16569)
- [Rustc Dev Guide: rustc_private / driver](https://rustc-dev-guide.rust-lang.org/rustc-driver/intro.html)
- 解释开销数据点：Miri 系解释 ≈ 25×（[Asterinas 论文](https://arxiv.org/pdf/2506.03876)，关检查后可更低）
