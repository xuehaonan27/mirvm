# Spike 3：混合栈 unwind —— 经验与教训

> 状态：**通过**（2026-07-07）。`mirvm spike3` 4 用例全 PASS（与 native 参考逐位一致），
> spike1/spike2/tier-0 diff 16/16 无回归。
> 目的：验证 **frame-abi-bytecode.md §7 候选 A**（复用平台 unwinder + personality）在
> 混合栈（解释帧 + 编译帧交替的一条 native 栈）上现实可行——这是模型 A 的头号硬骨头，
> 倒逼帧 ABI 封版。

## 1. 建了什么

- **共享 `bytecode.rs` 扩展**（unwind 形状是耐久设计产物，进共享草图）：
  `UnwindAction { Continue, Cleanup(bb) }`；`Call` 增加 unwind 边；新终止子
  `Drop { slot, target, unwind }`（spike 语义 = 记 drop 日志验证顺序；真身 = drop glue 调用）、
  `Panic { payload, unwind }`（≈ 调 panic 运行时的 diverging call）、`Resume`（cleanup 链尾）、
  `CatchCall`（≈ catch_unwind intrinsic 简化形）。spike1/2 补 catch-all 臂，无回归。
- **`src/vm/spike3.rs`**（自含）：CleanupGuard 协议的 interp_frame、cleanup 链解释器、
  `extern "C-unwind"` 编译帧替身（drop guard = landing pad）、4 用例 + 结构镜像的 native
  参考实现（drop 顺序不靠手推，靠对拍）+ 跨 FFI abort 的子进程 harness。

## 2. 验证了什么（4 用例，全部 == native 参考）

| 用例 | 栈形态 | 结果 |
|---|---|---|
| 1 纯解释链 | 6 解释帧，底部 Panic(777)，顶帧 catch | ✅ drops=[105..100] 内层先 |
| 2 **混合交替（headline）** | interp/compiled 交替 + **cleanup 内 Call 编译 helper** | ✅ drops=[105,104,103,102,**9002**,101,100] |
| 3 catch 在编译帧 | 编译帧内 catch_unwind（模拟 JIT catch landing pad） | ✅ result=1278, drops=[103,102,101,100] |
| 4 跨 FFI abort | 链中插 plain `extern "C"` 帧，深处 panic | ✅ 子进程 SIGABRT |

**结论：候选 A 坐实**——平台 unwinder 逐帧走齐解释帧与编译帧、按 guest 顺序（内层先）跑全
部 Drop、catch 语义正确、穿 C 帧 = abort。**候选 B（自研栈行走）退役为纸面兜底。**
用例 2 的 9002 证明 **landing pad 内可再入混合执行**（cleanup 链 Call 编译 helper——C++
析构调函数的日常，drop glue 的真实形状）。

## 3. 帧 ABI（unwind 维度）封版雏形 —— 本 spike 的核心产出

| | 解释帧 | 编译帧（M4 真身） |
|---|---|---|
| unwind 时跑本帧 Drop | `CleanupGuard::drop` → 解释 cleanup 块链 | landing pad → drop glue（Cranelift 发） |
| "当前 unwind 边"记在哪 | guard 里的**动态** `unwind_edge` cell（每个 Call/Panic 前更新） | **静态** LSDA：call-site → landing pad 表 |
| 恢复帧局部存储 | guard 里 `region.restore(base)`（§2.2 预告的"unwind 恢复区 SP"） | 无需（局部在 native 帧，unwinder 自动退） |
| catch 点 | `catch_unwind` 包 `call_guest` | 不 resume 的 landing pad |
| 排序保证 | **单条 native 栈 ⇒ 平台 unwinder 天然逐帧、内层先，VM 侧零协调** | 同左 |

最后一行是模型 A 的回报兑现：tier-0（模型 B，自己 pop Vec<Frame>）要自己维护的顺序，
模型 A 白拿。

## 4. 教训与发现

### 4.1 guest 异常 = 宿主 Rust panic，是候选 A 的**具象**而非近似

Rust panic 本身就是"`_Unwind_RaiseException` + Rust personality + landing pad"——用它承载
guest panic 就是在跑候选 A 的机制本体。三个实操点：
- **raise 用 `resume_unwind`**（不是 `panic_any`）：不触发 panic hook → 无"thread panicked"
  噪声输出，也不用去改 hook。
- **catch 点必须 downcast 区分**：`GuestPanic` → 按 guest 语义处理；其他（宿主 panic = VM
  bug）→ 原样 `resume_unwind` 续传，**绝不吞**。这条纪律 M4 保留。
- 开放问题（M4）：guest 异常要不要独立 exception class + 自有 personality（与宿主 panic
  彻底隔离、支持跨语言语义）。spike 证明"共用宿主机制 + downcast 区分"是够用的第一版。

### 4.2 JIT 调用约定必须 unwind-capable（`"C-unwind"` 这一课）

编译帧替身写成 plain `extern "C"` 时 panic 穿过直接 abort（Rust 1.81+ 的 nounwind shim）。
**JIT 生成的 guest 函数的调用约定必须允许 unwind 穿过**——对 Cranelift 即"发 unwind info +
不标 nounwind"（cg_clif 同款，"Rust ABI"天然如此）。而 plain `extern "C"` 的 abort 语义
恰好免费提供了"跨 FFI panic = abort"的正确行为 + 测试机制（用例 4 就是它）。

### 4.3 CleanupGuard 的双路径纪律

- **正常返回**：读 ret → `region.restore` → `mem::forget(guard)` → return。
- **unwind**：guard::drop → 跑 cleanup 链（若有边）→ `region.restore` → unwind 继续。

`forget` 在 Return 臂内 move guard 然后立刻 return——borrowck 流敏感，编译通过。guard 的
`unwind_edge` 用 `Cell`（各臂只做瞬时 set/get，不与循环借用打架）。**每个可 unwind 终止子
（Call/Panic）执行前设置边、Call 正常返回后清 None**——统一 raise 协议：Panic 终止子自己
的 live Drop 也由自己的 guard 按边跑，帧内帧间同一机制，无特判。

### 4.4 双 panic / 宿主与 guest 的边界

cleanup 链里若再 panic = 双 panic → abort（与 native 一致，spike 不测但语义明确）。
`CatchCall` 里宿主 panic 不吞（4.1）。架构声明（M4）：**guest 的 panic 运行时在 lower 时
映射成引擎原语**（`rust_begin_unwind`/`__rust_start_panic` → 引擎 raise；tier-0 今天就这么
shim），不解释 std panic_unwind 的内部。

### 4.5 残余风险（诚实边界）

替身验证不了"**真 Cranelift** 能否为我们的帧发 LSDA/landing pad + 与 Rust personality 协作"
——cg_clif 已对全 Rust 趟通（风险低），留 M4 真 Cranelift 管线复核。**与 vmctx 内部约定
（显式参 vs pinned reg，docs/vmctx-passing.md §5.2）是同一个 M4 检查点，一并做。**

## 5. 对 M4 / 后续 spike 的输入

- **模型 A 的三大地基全部验证完**：骨架（Spike 1）+ 廉价互操作（Spike 2）+ 混合栈 unwind
  （Spike 3，头号）。帧 ABI 的 unwind 维度有了封版雏形（§3 表）。
- 字节码的 unwind 形状定型进共享草图：`UnwindAction`/cleanup 块/`Resume`/catch 结构
  ≈ MIR 机械对应（佐证降低仍是机械活）。
- **剩 Spike 4（并发）**：N 真宿主线程各跑 interp_frame、共享只读字节码、per-thread
  region/ctx，过 TSan（= corpus §2.1 的 M4 答案；concurrency-arch.md 的引擎 Sync 主张）。
- guard/edge 协议、异常对象 downcast 纪律、C-unwind 要求——全部进 M4 帧 ABI 定稿。

## 6. 复现

```
cargo build --release
./target/release/mirvm spike3          # 期望：4 用例全 PASS，exit 0
./target/release/mirvm spike1 && ./target/release/mirvm spike2   # 回归
MIRVM=$PWD/target/release/mirvm bash tests/diff.sh               # tier-0 回归：16/16
```
