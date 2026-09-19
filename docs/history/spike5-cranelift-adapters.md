# Spike 5：真 Cranelift 接入 —— 经验与教训

> 状态：**通过（含 stretch）**（2026-07-07）。`mirvm spike5` 全 PASS：P/R 两约定 × 4 配置矩阵、
> 微基准、**unwind 穿真 JIT 帧（eh_frame 自注册后传播成功）**。spike1-4 + diff 16/16 + TSan 无回归。
> 目的：兑现挂起的 M4 检查点——spike2"Cranelift 能否发此约定"、vmctx-passing"内部约定等真数据"、
> spike3"真 Cranelift CFI 留复核"，一次收三个。
> **2026-09-19 归档**：spike 代码已从 `src/vm/spikes/` 删除（裁决见 decision-history §7.65）。
> 本文是结论与证据的留档；文中 `mirvm spikeN` 命令已不可跑。

## 1. 建了什么

- 依赖：cranelift 0.133.1 家族 + gimli 0.33（对齐 cranelift-codegen 内部版本），**feature
  `cranelift`（default）门控**——tsan/ harness 不开 feature → spike5 被 cfg 排除，TSan 通道零感知。
- `src/vm/spike5.rs`：spike3 协议的解释器（自含）+ **CLIF 手搓互递归 fib**（FunctionBuilder）
  两约定各两对（经 shim / 模块内直接调用）+ `mirvm_call_guest` c2i shim（`JITBuilder::symbol`
  注册 imported symbol）+ JITModule 装配 + 矩阵/基准 harness + 子进程 unwind probe +
  **eh_frame 自注册**（`create_unwind_info` → gimli `FrameTable` → `__register_frame`）。

## 2. 结果

### 2.1 矩阵（P/R 各 4 配置，fib 0..=20 + 24 全对）

i2c（interp 拿 code ptr 直调 JIT 码）✓、c2i（JIT 码经 imported shim 调回解释器）✓、
**cc→cc 模块内直接调用**（FuncRef，非经 dispatch）✓——spike2 那条"cc→cc 经 dispatch 是骨架
简化"正式杀掉。**Cranelift 能发我们的调用约定**（SysV (ctx,n)->r，transmute code ptr 即
`CompiledFn`）——spike2 的替身假设坐实。

### 2.2 vmctx 内部约定 P vs R：第一份真数据（回填 designs/vmctx-passing.md §5.2）

| fib(30)，min of 5 | 耗时 | 相对 |
|---|---|---|
| interp（骨架 tree-walk） | ~125 ms | 47× |
| **JIT-P（显式 ctx 参，直接调用）** | 5.90 ms | 2.24× |
| **JIT-R（pinned r15，直接调用）** | 5.47 ms | 2.08× |
| native（rustc -O） | 2.63 ms | 1× |

- **两约定都可行**（Cranelift `enable_pinned_reg` + `get/set_pinned_reg` 开箱即用）；
- **R 比 P 快 ~8%**（此负载）。vcode 坐实机制：P 每帧把 ctx 挪进 callee-saved（`movq %rdi,%r13`）
  再在**每个调用点**重装（`movq %r13,%rdi`）——threading 税可见；R 内部直调**零 ctx 搬运**，
  需要 ctx 时一条 `movq %r15,%rdi`。
- **R 的边界入口**（f_boundary）按 vmctx-passing §5 的设计实现并验证：`saved=get_pinned_reg →
  set_pinned_reg(ctx) → call fast(n) → set_pinned_reg(saved) → ret`——save/restore 让 r15 对
  宿主调用方保持 callee-saved 语义（宿主代码自由用 r15），且混合矩阵里 shim→interp→再入 JIT
  的重入路径全对（同线程同 ctx，重设幂等）。
- JIT 距 native 2.1-2.2×：合理（fib 全是调用开销，我们未开内联/尾调，Cranelift 代码质量
  本就目标"≈ 比 LLVM 慢 ~14%"；此处非代表性负载，勿过度解读）。
- **判定仍留 M4 真负载**（用户定），但初判数据：R 无一处输于 P，且逃逸/多入口故事更顺
  （vmctx-passing §5 的混合形态 = TLS 边界 + R 内部,本 spike 验证了 R 内部半边的可行与收益）。

### 2.3 unwind 穿真 JIT 帧（spike3 残余风险的正面收割）

- **裸跑：SIGABRT，如预测**——cranelift-jit **不注册系统 eh_frame**（其异常路线是
  `wasmtime-unwinder` feature = Wasmtime 自有两阶段 unwinder + try_call 异常表，**与宿主
  Rust unwinder 不互操作**——我们明确**不采**，因为解释帧是宿主 Rust 帧、guest unwind 必须
  与宿主 personality 同轨，见 frame-abi §7 候选 A）。
- **eh_frame 自注册后：传播成功**——`probe result=777 drops=[102, 100]`，guest panic 从
  interp 底帧起、**穿过真 Cranelift 帧**（CFI-only，无 personality）、回到 interp 顶帧的
  catch，payload 与 drop 顺序全对。
- 注册管线（cg_clif JIT 模式同款，~40 行）：`compiled_code().create_unwind_info(isa)` →
  `UnwindInfo::SystemV::to_fde(Address::Constant(终址))` → gimli `FrameTable`/`EhFrame` →
  按 libgcc 语义**逐 FDE** `__register_frame`（CIE 判别：len 后 4 字节为 0；整段注册是
  libunwind 语义，别混）。FDE 地址在 `finalize_definitions` 之后才可知——先收集 UnwindInfo、
  finalize 后统一建表。
- **spike3 残余就此收窄**：CFI 传播（unwind 走过 JIT 帧）已用真 Cranelift 验证；仍留 M4 的
  只剩 **landing pad/LSDA**（JIT 帧内跑 drop glue + catch）——路线明确：cg_clif 的
  personality + 异常表知识，或 Cranelift try_call。

## 3. 教训

1. **侦察先行值回票价**：写码前 grep 了 0.133.1 的真实源码（API 签名/feature 门/gimli 版本），
   ~700 行一次编译通过、一次全 PASS。Cranelift API 与训练知识有漂移（如 unwind 走
   wasmtime-unwinder 而非 eh_frame），凭记忆写必翻车。
2. **gimli 版本必须与 cranelift-codegen 对齐**（0.33）——`to_fde` 返回的是**它那个** gimli
   的类型,版本不齐类型不通。
3. **`__register_frame` 的 libgcc/libunwind 语义分裂**是唯一的坑点：libgcc 按单 FDE、
   libunwind 按整段。按 FDE 逐条注册 + CIE 判别字段（非位置猜测）最稳。
4. **feature 门控保住了 TSan 通道**：重依赖隔离在 `cranelift` feature 后，tsan crate 零感知
   ——"引擎核心依赖纪律"的又一次兑现。
5. JITModule 保活纪律：code ptr 的生命周期 = module 的生命周期，用完再 drop。

## 4. 对 M4 的输入

- **JITBackend 的 Cranelift impl 有了骨架级报告**：ISA flags（unwind_info/
  preserve_frame_pointers/enable_pinned_reg）、imported symbol c2i、FuncRef 直调 cc→cc、
  finalize→ptr→transmute i2c、unwind info 收集与注册——全部路径趟通。
- **vmctx 内部约定**：R（pinned r15）初判占优且多入口结构已实证；M4 真负载终裁（检查点保留，
  但数据基础已从"零"变"一"）。
- **unwind**：M4 的 JIT unwind = 本 spike 的 eh_frame 注册管线 + landing pad/LSDA（唯一剩余，
  cg_clif 先例）。wasmtime-unwinder 路线正式记为**不采**（与宿主 unwinder 不互操作）。

## 5. 复现

```
cargo build --release                 # feature cranelift 默认开
./target/release/mirvm spike5         # 矩阵 + 基准 + 双 probe，exit 0
# vcode 对照：/tmp/spike5-vcode-{p,r}.txt
```
