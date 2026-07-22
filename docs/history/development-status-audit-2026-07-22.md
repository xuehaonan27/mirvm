# mirvm 开发状况审查快照（2026-07-22）

> **文档性质**：这是针对本地 HEAD `cda7421` 的一次性审查证据快照，完成后冻结，
> 不拥有当前状态、开放债务或架构决策的权威。实现事实仍以当前代码与可复现测试为最高
> 依据；跨阶段状态、未解决事项和决策变更分别由
> [`current-status.md`](../current-status.md)、[`open-issues.md`](../open-issues.md) 和
> [`decision-history.md`](../decision-history.md) 承接。报告中的“建议”不等于已经立项，
> “发现”也不等于已经修复。
>
> **Canonical 迁移状态：pending。** 本文已经完整归档审查发现，但 F-01/实跑矩阵尚未同步
> 到 `current-status.md`，F-02～F-09 尚未正式登记到 `open-issues.md`，“M5 全收”被新证据
> 推翻的结论也尚未追加到 `decision-history.md`。完成该迁移前，三个权威入口仍是不完整的；
> 本历史快照不能替代它们。
>
> **审查时间与环境**：2026-07-22，Asia/Shanghai；工作区
> `/home/xuehaonan/mirvm`。维护者已暂停所有远端仓库、GitHub、Issue、PRD、PR 与 `gh`
> 操作，本轮未访问或改变远端状态。除新增本报告及索引入口外，没有修改产品代码、测试
> harness 或既有权威文档。

## 1. 总体结论

mirvm 当前是一个技术含量高、核心执行链真实可运行的**高级研究原型 / pre-alpha**，不是
纸面项目，但也还不是可发布或可用于生产执行不可信 Rust 程序的 runtime。

账面里程碑已经覆盖 M4 typed-bytecode 解释器、M5.0–M5.5 方法级 Cranelift JIT，以及 M6
冷启动和缓存链；更准确的工程判断是：**M5 功能施工基本完成，但正确性验收与稳定化尚未
完成**。当前存在以下发布阻断条件：

1. 仓库声明的 CI 在格式、Clippy 和 gate 自回归三个独立步骤上确定为 RED。
2. 默认开启的 JIT 存在可由源码路径确定的宽浮点静默错值。
3. threshold=1 差分没有证明目标函数真正发布和执行机器码，因而会漏掉上述错值。
4. FFI 聚合布局和固定聚合参数参与变参调用时存在静默 ABI 错调。
5. corpus 的持续门禁强度、依赖锁定和 README 的“持续三维逐字节绿”口径不一致。
6. 文档权威链落后实现且内部冲突，已无法直接用作可靠排期入口。

与此同时，lower/engine 分层、解释器 oracle、JIT/FFI/线程/unwind/cache 的闭环，以及本轮
真实通过的 76 个单测、45 项双态差分和局部 TSan，证明项目已有可观的实现深度。审查结论
不是“推倒重来”，而是应立即从功能扩张切到最小稳定化。

| 维度 | 审查结论 |
|---|---|
| 架构与实现深度 | 高；核心技术路线已经落地 |
| 当前窄平台功能广度 | 高；Linux/ELF/x86_64 单进程 CLI 模型下覆盖面广 |
| 核心正确性保证 | 不足；存在 JIT 和 FFI 静默错值路径 |
| 持续集成 | RED；不能认定当前 HEAD 可合并/可发布 |
| 回归证据强度 | 中；单测和 demo 有价值，真实 crate 的持续三维证据不足 |
| 可复现性 | 不足；部分 frontmatter 依赖未锁、真实项目 artifact 不在仓库 |
| 可移植性与分发 | 低；绑定 pinned rustc/LLVM，仅 Linux/x86_64 |
| 生产成熟度 | pre-alpha；无 release、无长期 soak、非稳定多 Engine library |

## 2. 审查范围、方法与证据等级

### 2.1 已检查范围

- 仓库治理与文档权威：`AGENTS.md`、`docs/agents/*`、`docs/README.md`、
  `current-status.md`、`open-issues.md`、`decision-history.md`、`DESIGN.md`、RAM spec 和相关
  M5/FFI/vmctx/并发设计。
- 核心源码：rustc lower、typed IR、解释器派发、Cranelift JIT 编译与翻译、JIT helpers、
  libffi FFI、GOT/符号合流、thunk、线程与全局 Engine 状态。
- 测试与 CI：GitHub Actions 配置、格式/Clippy/单测/release 构建、demo 差分、threshold=1
  差分、TSan 局部门、gate-truth 自回归，以及 corpus/gate5/gate6 脚本的判定逻辑。
- 产品化边界：平台硬门、动态依赖、缓存体积、发行元数据、全局状态、unsafe 审计面和真实
  项目证据可复现性。

### 2.2 证据等级

报告严格区分四类证据：

| 等级 | 含义 | 本报告示例 |
|---|---|---|
| A：本轮实跑 | 在本地 HEAD 上实际执行并取得退出码/输出 | 单测、diff、TSan、fmt、Clippy、gate-truth |
| B：当前源码确定 | 当前代码控制流、签名或操作码可直接推出 | JIT f16 Div、f128 powi、FFI 布局丢失 |
| C：当前静态盘点 | 对脚本、文件、测试项或文档进行计数/交叉核对 | 76 tests、45 demo、129/147 corpus 清单 |
| D：历史记录 | 仅由旧日志或 README 声称，本轮未复现 | 167 PASS、真实项目 ripgrep/tokei、完整 gate6 |

历史日志的绿色不被否认，但不能写成本轮已复现结果。完整 gate5、129/147 corpus 与
`m5_gate6.sh` 本轮没有执行：它们会进行重负载并清理/写入 `$HOME/.mirvm`，frontmatter
项目还可能重新解析依赖；当前又有远端操作暂停。CI 已在更早步骤确定失败，因此没有必要
为得出“当前 CI RED”而继续运行这些重型门。

## 3. 当前事实基线

### 3.1 仓库与开发节奏

- HEAD：`cda74219a329da21243762721588394bbc96befc`（短 hash `cda7421`），提交时间
  2026-07-21 16:25:05 +08:00。
- 证据采集开始时，本地 `main` 相对**当时的本地 tracking ref** `origin/main` ahead 1。
  报告落盘期间，该 tracking ref 于 2026-07-22 17:00:38 +08:00 以 reflog 记录
  `update by push` 前进到同一 `cda7421`；最终检查时两者一致。该外部状态变化不是本轮审查
  命令造成的；本轮没有执行 fetch/push/`gh`，也不据本地 tracking ref 推断远端实时状态。
- 审查开始时无 tracked 修改；已有未跟踪 `node_modules/`、`package.json`、
  `package-lock.json`，不属于本轮审查产物。
- 自 2026-07-03 起共有 207 个 commit，Git shortlog 仅一位作者；仓库无 tag。
- M5 全收口发生在审查前一天，尚无长期 soak、稳定分支或 release 证据。
- 最新修改 `src/` 的提交为 `05021d2`（M5.5 stats instrumentation）；之后的本地提交主要是
  gate 和文档收口。这说明功能面刚刚冻结，尚未经历独立稳定化周期。
- [`Cargo.toml`](../../Cargo.toml) 版本为 `0.0.1`，`publish = false`。

### 3.2 实际阶段

按当前代码而非陈旧状态段判断：

- M4.0–M4.5：tcx-free typed bytecode、tree-walking interpreter、真实地址模型、FFI、
  unwind、1:1 OS 线程、TLS 和 callback 已落地。
- M5.0–M5.2：asm stub、x86 intrinsic 扩面及非 JIT 语义补全已落地。
- M5.3–M5.5：方法级 Cranelift JIT、i2c/c2i、ABI 泛化、LSDA/unwind、准入覆盖和统计基础
  已有实现；但本报告发现的错码与覆盖盲区使“M5.4 翻译器全覆盖已验收”不能成立。
- M6 冷启动/cache：sysroot、IR、base/deps image 等链条已落地；这不等于 mode B、自包含
  pack 或正式分发已经完成。
- 当前真实工作重心应是后 M5 稳定化和 corpus 驱动的功能扩面。

### 3.3 规模与现有资产

- `src/`：约 33,728 行 Rust，84 个 tracked Rust 源文件。
- 单测：静态 `#[test]` 计数 76。
- `demo/`：47 个 `.rs`，`diff.sh` 排除两个 frontmatter 用例后持续差分 45 项。
- `diff_cargo.sh`：5 个顶层项目形态。
- `tests/corpus.sh` 默认清单：129 个真实 crate driver，源文件均存在。
- `m4_gate5.sh` corpus 段：147 项；满环境历史汇总口径为 167 个顶层 PASS 项。
- `m5_gate6.sh` 静态上有 4 个顶层汇总项；本轮没有把历史 4/4 当作实际复现。
- 所有 `tests/*.sh` 通过 `bash -n` 静态语法检查。
- 当前 `src/vm/engine/jit/translate.rs` 为 2,939 行的大型翻译 match，是明显的维护热点。

### 3.4 平台、分发与本机缓存

- 当前只支持 Linux/ELF/x86_64；非 x86_64 和非 Linux 在
  [`src/arch/mod.rs:25`](../../src/arch/mod.rs#L25) 与
  [`src/os/mod.rs:27`](../../src/os/mod.rs#L27) 显式 `compile_error!`。
- release 二进制动态依赖 pinned `nightly-2026-07-02` 中的 `librustc_driver` 和
  `libLLVM.so.22.1-rust-1.98.0-nightly`，不是可脱离工具链独立搬运的制品。
- `mirvm cache status` 报本机 `$HOME/.mirvm` 合计约 17.5 GiB，其中共享 `target` 约
  17.2 GiB、26,999 项。缓存换取了冷启动收益，但磁盘预算和清理策略尚不是产品级体验。

## 4. 已确认的工程优点

1. **分层方向正确**：rustc-private/tcx 主要留在 lower，相对独立的 tcx-free engine 使解释器、
   JIT 和缓存拥有清楚边界。
2. **语义 oracle 已成形**：typed IR 和解释器为 JIT 差分提供了真实参考，不依赖把 rustc
   `InterpCx` 永久留在产品执行相。
3. **技术闭环完整**：JIT、FFI、native→guest thunk、真线程、TLS、unwind、signal、ELF/GOT
   与冷启动缓存不是计划占位，而是已有运行路径。
4. **部分并发纪律可靠**：JIT 槽发布使用 Release/Acquire；解释帧 guard 恢复 region、depth、
   shadow 与 cleanup；固定地址缓存恢复失败倾向 fail-closed。
5. **测试不是零**：76 个单测、45 项解释/JIT 双态差分、局部 TSan 四场景均在本轮实际通过。
6. **决策意识较强**：decision history 保存了被拒方案、重开条件和性能负账；主要问题是最新
   施工结束后同步纪律失守，而不是完全没有工程方法。

这些优点足以支持继续投资，但不能抵消下面的静默正确性问题。

## 5. 发布阻断级发现

### F-01：仓库声明的 CI 当前确定为 RED

**严重度：阻断。证据：A（本轮实跑）。承接位置：CI/测试修复；状态结果同步
`current-status.md`。**

CI 在 [`ci.yml:32`](../../.github/workflows/ci.yml#L32) 依次运行 fmt、Clippy、单测、
gate-truth、release build、TSan 和 runtime gate。本轮独立实跑的 CI 步骤中有三个确定性失败：

- `cargo fmt --all -- --check` 退出 1，diff 涉及 48 个 `src/` 文件。
- `cargo clippy --locked --all-targets --all-features -- -D warnings` 退出 101；lib-test 路径
  32 条诊断，普通 lib 为其中 18 条子集，包含 unused import/dead code、doc comment 空行、
  `items_after_test_module` 等。
- `bash tests/gate_truth_regression.sh` 为 7 pass / 5 fail。

gate-truth 的直接原因是 gate5 新增调用
[`a2_deps_image.sh`](../../tests/m4_gate5.sh#L223)，但 fake bash 没有处理该嵌套 gate，落入
[`unexpected nested gate`](../../tests/fixtures/gate_truth/bash#L44)。这是 harness 自回归失配，
不是产品语义失败，但它仍使 CI 明确失败，也说明 gate 接线变更没有同步自身测试。

单测和 release build 通过不能把整体状态写成“CI 绿”。

### F-02：JIT `f16` 除法被翻译成余数

**严重度：阻断。证据：B（当前源码确定）。承接位置：`open-issues.md` 的 JIT correctness
债务；修复后进入产品代码与真 JIT 回归。**

- 翻译器把 `FloatOp::Div` 编码为 4：
  [`translate.rs:1586`](../../src/vm/engine/jit/translate.rs#L1586)。
- `mirvm_f16_bin` 只显式处理 0/1/2，其他操作码全部执行 `%`：
  [`helpers.rs:682`](../../src/vm/engine/jit/helpers.rs#L682)。
- 解释器的正确除法分支位于
  [`interp/rvalue.rs:75`](../../src/vm/engine/interp/rvalue.rs#L75)。

因此相关函数一旦真正进入编译版本，`a / b` 会变成 `a % b`。默认 JIT 开启，表现是函数
可能先由解释器给出正确值，达到热度后转为错误值，属于最危险的时序相关静默错码。

### F-03：JIT `f128::powi` 把整数指数当作 `f128` 位型

**严重度：阻断。证据：B。承接位置：同 F-02。**

- 对标量指数，翻译器把原始值放进 `(blo=v, bhi=0)`：
  [`translate.rs:1005`](../../src/vm/engine/jit/translate.rs#L1005)。
- helper 随后先执行 `f128_of(blo, bhi)`，再以 `b as i32` 作为 `powi` 指数：
  [`helpers.rs:568`](../../src/vm/engine/jit/helpers.rs#L568)。
- 解释器正确地直接读取标量指数：
  [`interp/stmt.rs:663`](../../src/vm/engine/interp/stmt.rs#L663)。

例如整数 3 的低位被解释为极小 `f128`，再转成 `i32` 近似为 0，结果退化为
`a.powi(0) == 1`。

### F-04：JIT 宽整数/浮点转换存在参数和调用 ABI 错配

**严重度：阻断。证据：B。承接位置：同 F-02。**

存在两个方向的问题：

1. 浮点转 `i128/u128`：调用端传 `[bits, kind, signed]`，见
   [`translate.rs:985`](../../src/vm/engine/jit/translate.rs#L985)；helper 签名却是
   `(kind, v, signed, out)`，见
   [`helpers.rs:653`](../../src/vm/engine/jit/helpers.rs#L653)。参数顺序反转可使常见输入归零
   或产生其他错误。
2. `i128/u128` 转 `f32/f64`：
   [`Wide128ToFloat`](../../src/vm/engine/jit/translate.rs#L926) 通过返回 I64 的统一
   [`call_helper1`](../../src/vm/engine/jit/translate.rs#L385) 声明调用 compiler-builtins，具体
   调用位于 [`translate.rs:947`](../../src/vm/engine/jit/translate.rs#L947) 和
   [`translate.rs:958`](../../src/vm/engine/jit/translate.rs#L958)。两个 I64 入参在当前 x86_64
   SysV 下可能碰巧与 i128 的寄存器拆分兼容；**确定错误的是返回值 ABI 分类**：真实函数以
   F32/F64、经 XMM0 返回，调用端却按 I64/RAX 读取。若符号解析或函数定义失败，路径又会
   静默留在解释器，掩盖覆盖缺口。

### F-05：threshold=1 差分不能证明机器码发布或执行

**严重度：阻断验证。证据：A+B。承接位置：最小 gate 修复；不得扩建通用 harness。**

当前调用顺序是：先读发布槽；槽为空时计数并投递编译；**当前这次调用仍执行解释器**：
[`call_guest`](../../src/vm/engine/interp/mod.rs#L414)。所以 threshold=1 的含义是“第一次调用
请求编译”，不是“第一次调用就走 JIT”。只调用一次的函数不会执行它的编译版本。

同时，gate 不区分以下不同状态，最终都表现为继续解释：

- JIT worker 创建结果被忽略：[`compiler.rs:44`](../../src/vm/engine/jit/compiler.rs#L44)。
- `admit == false` 是设计上的“不准入”，不是编译失败；当前 gate 不记录具体不准入集合。
- 对已经判定 admissible 的函数，`define_fast`、`define_packed`、`finalize_definitions` 失败仍
  直接返回且不会使 gate RED：[`compiler.rs:344`](../../src/vm/engine/jit/compiler.rs#L344)。
- gate5 只检查差分汇总，不断言 admissible 函数的 slot 发布、编译错误或 compiled-entry 执行：
  [`m4_gate5.sh:206`](../../tests/m4_gate5.sh#L206)。

现有 [`float_wide_probe.rs`](../../demo/float_wide_probe.rs) 恰好包含 f16 除法、f128 powi 和宽
转换，但 `f16_lane`、`f128_lane` 都只调用一次。本轮 threshold=1 差分仍为 45/45，正好证明
了盲区，而不是证明上述路径正确。

仓库另有 `jit_call_probe`、`jit_builtin_probe`、`jit_unwind_probe`、`jit_abi_probe` 等带
`MIRVM_JIT_DEBUG` 发布实证的专项探针；它们能证明**所选子集**曾发布机器码，不能推出全部
admissible 函数、尤其 float-wide 路径都发布并执行。这里的结论是“覆盖证明不充分”，不是
“仓库从未执行过 JIT 机器码”。

即使函数调用超过一次，后台编译和短循环之间仍有竞态：程序可能在发布完成前把所有调用都
解释完。因此“多调用一次”也不是 compiled-entry 覆盖证明，测试需要显式发布屏障或同步模式。

生产运行保留“未准入或 JIT 编译失败时回退解释器”是合理策略；验证模式必须提供最小的同步
编译/发布屏障、对 admissible 函数的 fail-on-compile-error 和 compiled-entry 覆盖断言。该
harness 变更有明确的当前 blocker，符合基础设施预算纪律。

### F-06：FFI 聚合真实布局被冻结后又被完全丢弃

**严重度：阻断。证据：B。承接位置：`open-issues.md` FFI ABI correctness。**

- lower 保存聚合 `size`、`align` 和字段 `off`：
  [`ffi_sig.rs:86`](../../src/lower/ffi_sig.rs#L86)。
- 执行端只把字段类型依次交给 `FfiType::structure`，没有消费任何 offset、size 或 align：
  [`ffi.rs:180`](../../src/vm/engine/ffi.rs#L180)。

例如 `#[repr(C, packed)] struct S { a: u8, b: u32 }` 的 Rust/C 布局是 size 5、align 1、
`b@1`，libffi 自然结构会按 size 8、align 4、`b@4` 处理。显式对齐的确定性嵌套反例是：

```rust
#[repr(C, align(8))]
struct A(u8);

#[repr(C)]
struct Outer {
    tag: u8,
    value: A,
}
```

rustc 布局中 `value@8`、size 16；当前递归 libffi 构造丢掉内层 `A` 的 align/size，会得到
`value@1`、size 2。单独按值传 `repr(C, align(8)) struct A(u8)` 在 x86_64 SysV 下可能碰巧
仍落同一个 INTEGER 寄存器档，因此不把该单层形态作为确定的可观察错值例。这些布局目前
都不会在 freeze 阶段响亮拒绝，现有聚合探针又只覆盖自然布局，R17 没有登记该边界。

最小正确修复是：在 freeze 时验证布局能被当前 libffi 类型表达；不能表达的 packed/显式对齐
形态先响亮拒绝。只有当前 workload 确实要求时再实现完整 padding/布局表达。

### F-07：变参 FFI 的固定聚合参数会导致真实尾参类型被删除

**严重度：阻断。证据：B。承接位置：同 F-06。**

- 固定签名允许包含聚合类型：
  [`call.rs:175`](../../src/lower/func/call.rs#L175)；设计契约也明确“fixed 聚合 OK”：
  [`c1-ffi-agg-design.md:19`](../designs/c1-ffi-agg-design.md#L19)。
- `tail_kinds` 只在非聚合标量分支追加；固定聚合参数被跳过：
  [`call.rs:201`](../../src/lower/func/call.rs#L201)。
- 最后却按固定参数总数从 `tail_kinds` 切除前缀：
  [`call.rs:339`](../../src/lower/func/call.rs#L339)。
- 执行端使用 `args.zip(sig.args)`，长度不一致时静默截断：
  [`ffi.rs:251`](../../src/vm/engine/ffi.rs#L251)。

合法的 `extern "C" fn f(S, ...); f(s, 42)` 会把 `42` 的类型也切掉。设计明确 fixed 聚合受
支持，因此这不是已知拒绝边界。修复时还应加入 `args.len() == sig.args.len()` 的响亮不变量，
防止未来签名漂移再次静默截断。

## 6. 其他高置信正确性发现

### F-08：GOT 同名 weak/strong 合并采用“首次出现决定强弱”

**严重度：高，触发面较窄。证据：B。承接位置：`open-issues.md` linker/GOT。**

- 同名符号命中后直接返回旧索引，不合并 `weak`：
  [`got.rs:17`](../../src/lower/linker/got.rs#L17)。
- `foreign_slot` 缓存也绕过后续更新：
  [`got.rs:47`](../../src/lower/linker/got.rs#L47)。
- image 合流仍按名称去重：
  [`ir.rs:1527`](../../src/vm/engine/ir.rs#L1527)。
- 启动解析按最终记录的 `weak` 决定缺失符号是否只写 NULL：
  [`ffi.rs:143`](../../src/vm/engine/ffi.rs#L143)。

若 weak 引用先出现、同名 strong 引用后出现，缺失符号会被视为可选并写 0；native 强引用
应当链接或装载失败。合并规则应为：任一引用是 strong，合并项即 strong。

### F-09：`C-unwind` fn pointer/callback 被压成普通 `C` thunk

**严重度：高，触发面较窄。证据：B。承接位置：`open-issues.md` FFI/unwind。**

- fn-pointer 签名冻结接纳 `C` 和 `C-unwind`，但
  [`ForeignSig`](../../src/vm/engine/ir.rs#L1225) 不保存 unwind 属性：
  [`ffi_sig.rs:17`](../../src/lower/ffi_sig.rs#L17)。
- 反向 trampoline 固定声明为 `extern "C"`：
  [`thunks.rs:103`](../../src/vm/engine/thunks.rs#L103)、
  [`thunks.rs:183`](../../src/vm/engine/thunks.rs#L183)。

合法的 `extern "C-unwind"` guest callback 若向 native 逃逸 unwind，会在 thunk 边界 abort。
这项证据直接确定的是 fn pointer/callback 路径。直接 foreign 冻结路径同样没有保存 unwind
属性：[`calls.rs:130`](../../src/lower/linker/calls.rs#L130)，说明出向调用的 unwind 语义也未
建模，但仅凭当前静态证据不把它断言成与 callback 相同位置的必然 abort。最低成本的诚实行为
是在 freeze 期暂时拒绝 `C-unwind`；完整支持需要保留 ABI 属性并生成可 unwind trampoline。

## 7. 回归证据与可复现性发现

### V-01：README 对 129 个 corpus driver 的持续证据强度表述过高

**严重度：高。证据：A+C。承接位置：README、`current-status.md`、`corpus.md`。**

[`README.md:24`](../../README.md#L24) 写“129 个真实 crate driver，mirvm/native/逢调即编
三维逐字节绿”。但当前持续门的实际行为是：

- [`corpus.md:118`](../corpus.md#L118) 自己说明三维逐字节差分只在 driver 创建时执行，gate
  内是 exit-code/oracle 级。
- [`tests/corpus.sh:43`](../../tests/corpus.sh#L43) 对 129 项只运行一次默认 mirvm，并按退出码
  汇总。
- [`m4_gate5.sh:67`](../../tests/m4_gate5.sh#L67) 的 147 项 corpus 段只跑默认 mirvm；聚合层
  仅对 numbigint、backtrace、signal、blake3、sha2、volatile 六项固定核 stdout，其余 141 项
  只要求 exit=0（driver 自身断言另计）。
- native 与 threshold=1 持续逐字节差分只覆盖 45 个 demo，不覆盖 129 个真实 crate。

driver 内部断言仍有真实价值，但它不能等价替代对 native/解释/JIT 三态 stdout、stderr 和
exit 的持续比较。正确口径应是：“按历史台账记载，129 项曾在创建时完成开发验收；当前
gate 持续运行默认 mirvm 和有限 oracle”。

### V-02：frontmatter/corpus 的依赖解析未持续锁定

**严重度：高。证据：B+C。承接位置：可复现 gate 配置。**

Cargo shim 只有设置 `MIRVM_CARGO_LOCKED` 才添加 `--locked`：
[`cargo_shim.rs:172`](../../src/cargo_shim.rs#L172)。CI runtime gate 没有设置该变量，脚本项目
生成的 lock 又位于 `$HOME/.mirvm/scripts` 而非仓库。clean runner 因此可能重新解析依赖，
根 crate 的 `cargo --locked` 不能约束这些 frontmatter 项目。

这会引入依赖漂移、网络可用性和 45 分钟 CI 上限风险。最小方案不是给 129 项建设新 schema，
而是选择当前 release acceptance 所需的代表集，钉住 lock/依赖并保持可复现。
当前 CI 也没有为这些生成项目设置独立依赖 cache，clean-run 的耗时和外部状态敏感性会进一步
放大。

### V-03：M5.5 收口门没有进入 CI，且默认使用 debug 二进制

**严重度：高。证据：C。承接位置：CI/gate 接线。**

- CI 最终只运行 `m4_gate5.sh`：[`ci.yml:50`](../../.github/workflows/ci.yml#L50)。
- [`m5_gate6.sh:10`](../../tests/m5_gate6.sh#L10) 默认 `target/debug/mirvm`，随后把它传给包含
  性能门的完整 gate5，覆盖 gate5 自身的 release 默认值。

这既违反性能/执行使用 release 的项目纪律，也可能消费陈旧 debug binary。历史“m5_gate6
4/4”不是持续 CI 性质。

### V-04：JIT stats 不是发布覆盖证明

**严重度：中高。证据：B+C。承接位置：最小 gate 修复。**

`MIRVM_JIT_STATS=1` 统计 helper 调用频度，可用于评估 vmctx/E6 候选，但当前 gate6 只检查
dump 行和某个桶非零：[`m5_gate6.sh:34`](../../tests/m5_gate6.sh#L34)。它不报告每个 admissible
函数是否编译、发布或进入 compiled entry，因此不能弥补 F-05。

### V-05：真实项目历史证据本工作区不可复现

**严重度：中。证据：A+C。承接位置：`real-projects.md`/发行验收。**

- `artifacts/real-projects` 在当前工作区不存在，且被 `.gitignore` 忽略。
- `real_projects_regression.sh` 不在 CI。
- 本机试跑因缺少文档要求的 `bwrap` 仅得到 8/65；这是环境阻塞，不应记为产品 57 项失败。
- 历史 ripgrep/tokei PASS、benchmark 和 provenance 本轮无法复验。

如果产品化，至少应选择一个固定真实项目建立 clean-machine acceptance；不要把不可携带的
workspace artifact 当作 release 证据。

### V-06：`diff_cargo.sh` 本轮因沙箱写权限阻塞，不能形成产品判定

**严重度：环境/证据限制。证据：A。承接位置：本轮验证记录，无产品债务结论。**

本轮实际尝试：

```bash
MIRVM="$PWD/target/release/mirvm" bash tests/diff_cargo.sh
```

命令约 6.5s 后退出 1，表面汇总为 0/5；五项都因当前沙箱不能写
`$HOME/.mirvm/target/{native,mirvm}/debug/.cargo-build-lock` 而失败。因此这不是五项产品 RED，
也不能据此判断 harness 逻辑；它只说明本轮环境无法有效执行该工作负载。

### V-07：完整 gate 数字不得当作本轮实测

**严重度：记录纪律。证据：A+D。承接位置：所有状态文档。**

README 中的 gate5 `167 PASS`、gate6 `4/4`、corpus 全量等是历史记录。本轮由于重负载、
`$HOME/.mirvm` 写清理行为、可能重新解析依赖及远端暂停，没有复跑 full gate5/gate6/corpus。
它们可以保留为带日期的历史票据，但不得与本轮实际通过矩阵混写。

## 8. 文档权威链与用户界面发现

### D-01：`current-status.md` 落后 HEAD 且内部自相矛盾

**严重度：高。证据：C。承接位置：`current-status.md`。**

- 文件头仍称事实截至 `2f9efef`：[`current-status.md:3`](../current-status.md#L3)，比当前 HEAD
  落后 56 commits。
- 同文先称 M5.5 完成，又在后文称未实现：
  [`current-status.md:28`](../current-status.md#L28)、
  [`current-status.md:44`](../current-status.md#L44)。
- JIT 边界表仍称 T3/vmctx 未实现：
  [`current-status.md:164`](../current-status.md#L164)，开发顺序却称 T3 已闭合。
- 仍混有 35 tests、20 diff、40 PASS/2 XFAIL 等旧账，并继续把 signal/backtrace 写成 XFAIL。
- “代码热点”仍指向重构后不存在的 `src/lower/func.rs` 和
  `src/vm/engine/interp.rs`。

这使文档权威顺序中的第二层不能可靠履行职责。

### D-02：`open-issues.md` 违反“已关闭项移出”的自身规则

**严重度：高。证据：C。承接位置：`open-issues.md`。**

文件声明只收未解决事项：[`open-issues.md:3`](../open-issues.md#L3)，但至少仍挂着 11 个
已经标记关闭的条目：T1、T2、T3、T5、C1、C2、C3、C5、C7、E1、E21。代表位置见
[`open-issues.md:58`](../open-issues.md#L58)。

T4 也存在分类歧义：若它仍是已批准待施项，当前开发顺序不应直接跳到 E6/C4；若只是条件
触发的可选诊断增强，应从 T 区迁到相应 R/E 条目。

### D-03：decision history 的“当前摘要”仍称生产 JIT 未实现

**严重度：高。证据：C。承接位置：append-only `decision-history.md` 新收束条目。**

- vmctx 章头仍称生产 JIT 尚未实现。
- 当前选择表仍称唯一产品引擎是解释器。
- 最后的“尚未兑现”清单仍称方法级 JIT 不是现状：
  [`decision-history.md:1087`](../decision-history.md#L1087)，即使相邻 §7.19/§7.20 已记录 M5
  全收。
- constructor、RTLD_DEFAULT 重名等部分拒绝项也已被后续实现推翻，却仍留在当前摘要。

由于该文档要求 append-only，正确修复不是删除历史论证，而是追加一段明确收束旧摘要的当前
结论与重开条件。

### D-04：README、CLI 与自身最新状态冲突

**严重度：中高。证据：C。承接位置：README/CLI。**

- README 首段称方法级 Cranelift JIT 是“长期目标”：
  [`README.md:3`](../../README.md#L3)，同页后文却称 M5.0–M5.5 全收、JIT 默认开启。
- `mirvm --help` 仍称 JIT 是 M5.3a 计数基座、“尚无编译”：
  [`cli.rs:41`](../../src/cli.rs#L41)。
- help 仍指向已经迁入 `docs/history/` 的旧 `docs/spike*.md` 路径：
  [`cli.rs:55`](../../src/cli.rs#L55)。
- README 的基础回归只列到 `m4_gate5.sh`，没有列账面 M5 收口门。
- CI 的 runtime gate 同样只运行 `m4_gate5.sh`，没有运行 `m5_gate6.sh`；详细证据见 V-03。

### D-05：多份活设计头部和根 DESIGN 没有跟随实现更新

**严重度：中。证据：C。承接位置：相应活设计或 decision-history 收束。**

- [`vmctx-passing.md`](../designs/vmctx-passing.md) 头部仍称生产 JIT 未实现，而 §7 已写 T
  骨架生产定稿。
- [`frame-abi-bytecode.md`](../designs/frame-abi-bytecode.md) 头部仍称方法级 JIT、compiled
  frame、i2c/c2i 未实现。
- [`concurrency-arch.md`](../designs/concurrency-arch.md) 头部仍称独立 `os::` 层未实现，E21
  实际已经关闭。
- 根 [`DESIGN.md`](../../DESIGN.md) 仍写缓存根 `~/.cache/mirvm`，实现使用 `$HOME/.mirvm`。
- DESIGN 把自管 arena/TLAB 写成当前 heap 形态，实际实现为 mimalloc v1，hand-rolled TLAB
  后置。
- REPL 一处仍写“后置 M6”，里程碑已改为 M7+。

### D-06：旧纯文本路径和行锚失效，但 Markdown 链接目标总体未断

**严重度：中。证据：C。承接位置：各文档勘误。**

全仓 Markdown 链接目标扫描没有发现普遍断链。问题主要是不可点击的纯文本路径和失效
`file:line` 锚：

- `open-issues.md` 多处把 `frame-abi-bytecode.md` 写成不存在的
  `history/frame-abi-bytecode.md`，实际位于 `designs/`。
- M5.4 文档仍写 `src/vm/engine/jit_compile.rs::lsda_probe`，实际已迁到
  `src/vm/engine/jit/lsda_probe.rs`。
- E28 的 `src/lower/mod.rs:1299` 已失效，当前线索位于 `src/lower/builtins.rs`。
- 多份活文档仍写旧 `docs/spike*.md`，实际位于 `docs/history/`。

应把“Markdown 断链”和“旧纯文本锚”分开统计，避免错误宣称链接系统整体损坏。

### D-07：文档索引本身有遗漏和分类口径不清

**严重度：低到中。证据：C。承接位置：`docs/README.md`。**

- `docs/designs/c1-ffi-agg-design.md` 与 `c2-rlib-symbols-design.md` 存在，但没有出现在 designs
  表。
- `docs/parked/c3-resume-spike.md` 存在，但 parked 段只列出 `s3b-chain-wip.patch`。
- designs/history 的分界不应简单理解为“未完成/已完成”：M5、M5.4、C1、C2 已完成但仍承载
  现行规范。更准确的划分是：`designs/` 保存仍具规范性或重开时必须遵守的契约，
  `history/` 保存非规范性的施工证据、被替代方案和当时记录。

### D-08：最近一次“陈旧标记复扫”验收方法自身漏检

**严重度：中。证据：C。承接位置：文档维护纪律。**

当前 HEAD 的提交说明声称“M5 全收对齐”和“陈旧标记复扫零残留”，但同一 HEAD 仍保留上述
“生产 JIT 尚未实现”“M5.5 未实现”和旧测试账。这说明问题不仅是若干漏改行，还包括文档
同步验收只核了有限关键词/位置，没有沿权威链做冲突检查。

最小改进是阶段关闭时核对 README、CLI help、current-status 的阶段表/边界表/开发顺序、
open-issues 关闭项以及 decision-history 当前摘要；不需要为此新建通用 schema。

## 9. 产品成熟度与工程保证风险

### M-01：当前是单模块 CLI 进程模型，不是稳定多 Engine library

**严重度：中高，产品化阻断。证据：B。承接位置：现有 E22/嵌入触发器。**

- `Shared` 由 [`Box::leak`](../../src/cli.rs#L612) 提升为进程期 `&'static`。
- JIT compiler 使用全局 `AtomicPtr<Shared>`：
  [`compiler.rs:33`](../../src/vm/engine/jit/compiler.rs#L33)。
- entry thunk、atexit、线程 Ctx 等路径也依赖单 Engine 假设。
- 多个引擎失败路径直接 `std::process::exit(70)`，不构成可组合的库错误 API。

这对当前 CLI 研究原型可以接受，但不能宣称嵌入、多实例或同进程隔离。

### M-02：发行物绑定编译器工具链

**严重度：中高，分发阻断。证据：A+B。**

release 二进制的 `ldd` 结果直接指向 pinned nightly 中的 `librustc_driver` 和 LLVM。当前没有
mode B 包、自包含 pack、正式 installer、daemon 或稳定库 ABI；clean-machine 用户不能只复制
一个二进制获得可靠运行环境。

### M-03：unsafe 规模大而系统化审计保证不足

**严重度：中等 assurance 风险，不是单独的已证实 bug。证据：C。**

- `src/` 中约 475 个 `unsafe {}` block。
- 显式 `SAFETY:` 注释仅 5 处。
- 没有 `unsafe_code`/undocumented-unsafe 类门禁。
- 没有持续 ASan/UBSan、fuzz、loom 等保证；当前只有局部 TSan 场景。

真实地址模型、FFI、ELF、asm stub 和宿主 intrinsic 决定了大量 unsafe 不可避免。正确策略不是
为了数字机械补注释，而是优先审计 FFI、全局 Shared、ELF 解析、固定地址映射、thunk 和
unwind 等 trust boundary，并为每个实际不变量补最小证明或测试。

### M-04：维护集中度和近期变更密度高

**严重度：中等治理风险。证据：C。**

约三周内 207 个单作者提交，M5 全收发生在审查前一天；同时 JIT 翻译器是 2,939 行大 match，
文档和 gate 同步已出现遗漏。当前缺少第二审阅者、长期 soak、tag/release 和 clean runner 证据，
因此“短期快速扩面绿”不能等价为维护稳定性。

### M-05：尚不存在的产品能力

以下仍是产品边界，不应出现在当前能力宣称中：

- 任意 Rust/任意平台支持；当前仅 Linux/ELF/x86_64 和有限 corpus。
- mode B、自包含 pack、稳定发行、daemon、REPL。
- 稳定嵌入 API、多 Engine 同进程。
- checked 模式、正式沙箱或多租户不可信代码隔离。
- 可脱离 pinned rustc/LLVM 的独立制品。

## 10. 本轮实际验证矩阵

以下结果均来自本地 HEAD；耗时是 warm 本机观测，仅用于说明本轮执行范围，不是性能基线。

| 命令/验证 | 结果 | 约耗时 | 证据解释 |
|---|---:|---:|---|
| `cargo test --locked --all-features` | 76/76 PASS，4 warnings | 10.7s | CI 同构 debug 单测 |
| `cargo test --release --locked` | 76/76 PASS，4 warnings | 8.8s | release 单测 |
| `cargo build --release --locked` | PASS | 0.1s | warm build |
| `MIRVM="$PWD/target/release/mirvm" bash tests/diff.sh` | 45/45 PASS | 27.8s | native/default mirvm demo 差分 |
| `MIRVM_JIT_THRESHOLD=1 MIRVM="$PWD/target/release/mirvm" bash tests/diff.sh` | 45/45 PASS | 29.4s | 不等于全部执行机器码，见 F-05 |
| `tests/spike4_tsan.sh` | 四类场景 PASS，无 TSan warning | 0.4s | warm、局部并发场景 |
| `cargo fmt --all -- --check` | FAIL，48 文件 | 1.6s | CI 阻断 |
| CI exact Clippy `-D warnings` | FAIL，32 条 lib-test 诊断 | 5.4s | CI 阻断 |
| `tests/gate_truth_regression.sh` | 7 PASS / 5 FAIL | 0.8s | harness 接线回归 |
| 所有 `tests/*.sh` 的 `bash -n` | PASS | — | 仅脚本语法，不是语义 gate |
| `MIRVM="$PWD/target/release/mirvm" bash tests/diff_cargo.sh` | 退出 1，表面 0/5 | 6.5s | 沙箱不能写 cache lock，环境阻塞，不作产品判定 |
| `bash tests/real_projects_regression.sh` | 8 PASS / 57 FAIL | 70.7s | 缺 `bwrap` 引发级联基础设施失败，不作产品/harness 判定 |

未执行：full `m4_gate5.sh`、`m5_gate6.sh`、`tests/corpus.sh` 的 129 项、gate5 corpus 段的
147 项，以及完整真实项目 workload。历史数字仍可作为带日期票据保留，不能冒充本轮实测。

## 11. 建议的最小稳定化顺序

### P0：立即冻结新功能扩面

1. 修复 F-02～F-04 的 JIT 错码。
2. 把现有浮点探针改成确定经历“解释 → 发布 → compiled entry”的热调用回归，逐位比较
   f16 Div、f128 powi、四个宽转换方向。
3. 增加只服务当前 blocker 的 strict/synchronous JIT 测试模式：记录不准入集合；对已判定
   admissible 的函数，编译错误即失败；等待发布并断言代表性函数进入 compiled entry。完成
   后冻结该 harness。
4. 修复 FFI 聚合布局和变参固定聚合问题；不能表达的布局先响亮拒绝；调用前断言参数与签名
   等长。
5. 修复 weak/strong 合并；对 `C-unwind` 选择“保存并支持”或“冻结期响亮拒绝”，不能继续
   静默当作 `C`。

### P0：恢复仓库基本健康

1. 运行 rustfmt，消除 Clippy `-D warnings` 诊断。
2. 给 gate-truth fake runner 补当前 `a2_deps_image.sh` 嵌套形态，复跑同一 12 项工作负载。
3. 使用明确的 release `MIRVM` 运行现有 m5 gate；确认后把 M5 收口检查接入 CI。
4. 不借修 CI 增加 schema、provenance、通用 inventory 或未来鲁棒性层。

### P1：恢复文档真值

1. 把 `current-status.md` 精简到当前事实：刷新 HEAD，删除 M5.5/T3 未实现旧句和旧测试账，
   更新路径与本轮 CI RED。
2. 从 `open-issues.md` 移出 11 个关闭条目；明确 T4 是待施、条件触发还是应降类。
3. 将 F-02～F-09 登入 canonical open-issues，并给出修复验收和重开条件。
4. 在 append-only decision history 追加收束条目，不删除旧推理。
5. 同步 README、CLI help、活设计头部、缓存根、heap/REPL 口径和旧路径锚。
6. 将 corpus 声明改为“创建时三维验收 + 当前默认 mirvm/oracle 持续门”，直到真正持续复现。

### P1：提高当前真实 workload 的可判定性

1. 选择小而固定的代表性真实 crate 集，锁依赖，保留可观察输出或明确不变量。
2. 对代表集分别证明解释器和 JIT compiled-entry，而不是把 129 项全部扩建成昂贵通用门。
3. 若当前 workload 已能可信判绿，立即冻结 harness 并返回 `src/`。

### P2：下一产品战役

- 优先 C4/corpus 功能轴：faer 默认形态已有 dep crate `global_asm` 的真实 blocker，当前依赖
  `default-features=false` 绕行。
- 不建议直接进入 E6：目前只有 helper 调用频度，没有证明它是当前 wall-time 主因；先用固定
  workload 证明收益，再触发 vmctx 复测。
- mode B、嵌入 API、checked/沙箱继续后置。若目标明确转为产品化，第一步应是定义
  clean-machine 安装和一个固定真实项目的 release acceptance，而不是继续扩功能面。

## 12. “M5 全收”重新成立的最低验收条件

至少满足以下条件后，才建议恢复“M5 翻译器全覆盖/全收”措辞：

1. 配置化 CI 的 fmt、Clippy、单测、gate-truth、release、TSan、runtime gate 全部通过。
2. F-02～F-04 已修复，并由明确执行过 compiled entry 的回归锁定。
3. strict 测试模式能记录不准入集合，并对代表性 admissible 函数报告编译、发布和执行；
   admissible 函数的任何编译失败都使 gate RED。
4. F-06/F-07 已修复或把当前不能表达的 ABI 形态响亮拒绝，新增 packed/align/变参组合负正例。
5. weak/strong 与 `C-unwind` 行为已定型并有测试。
6. `current-status.md`、`open-issues.md`、README、CLI 与 decision-history 当前摘要没有互相冲突。
7. 至少一个依赖锁定的真实 workload 在 clean 环境可复现解释/JIT/native 的可信判定。

## 13. 候选 canonical 迁移清单

本表只记录本次审查发现后续可能由哪里承接，不在本历史快照内维护开放状态，也不能在
写入 canonical 文档前据此排期。

| 发现 | 后续权威承接位置 | 验收后如何关闭 |
|---|---|---|
| F-01 CI RED | `current-status.md` + CI/harness | CI 同构命令全绿 |
| F-02～F-05 JIT correctness/coverage | `open-issues.md` + JIT 设计/测试 | 真 compiled-entry 逐位回归全绿 |
| F-06/F-07 FFI ABI | `open-issues.md` + C1/FFI 设计 | packed/align 边界诚实、变参组合矩阵全绿 |
| F-08 GOT weak/strong | `open-issues.md` | weak-first/strong-later 缺符号负例正确失败 |
| F-09 C-unwind fn pointer/callback | `open-issues.md` | 支持可 unwind thunk 或冻结期精确拒绝 |
| V-01～V-07 证据纪律 | `current-status.md`、README、corpus/gate 文档 | 声明与持续门行为一致 |
| D-01～D-08 文档失真 | 各 canonical 文档 + decision-history 新收束 | 权威链交叉核对无冲突 |
| M-01～M-05 产品边界 | `current-status.md`/既有 E、D、R 条目 | 仅在对应产品能力实际落地后更新 |

本报告归档后只允许勘误。后续修复结果必须写入上述 canonical 文档，不能在本报告里滚动打勾，
否则会制造第二份状态上下文。
