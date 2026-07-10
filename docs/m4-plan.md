# M4 实施计划 —— 自研生而并发字节码 VM（模型 A）

> 获批：2026-07-07。前置：5 个 spike 全过（骨架 / i2c-c2i / 混合栈 unwind / 并发 TSan / 真 Cranelift），
> corpus 五批五票据收口。**M4 是大体量、低未知度的工程**——高风险赌注已在 spike 期验证完。
> 过程纪律：M4.0 与 M4.4 两个关键期开工前过审，其余期开工简报即行；每期完成出 gate 报告，
> 经验记 docs/m4-log.md。
>
> **进度（2026-07-10）**：M4.0 ✅ M4.1 ✅ M4.2 ✅ M4.3 ✅ **M4.4 ✅**（真线程收官：
> gate4 11/11，threads 5/5 差分、挂死双场景、rayon 0.9s、TSan；各期 gate 与经验见
> m4-log）；tier-0 已移除（81772e4，oracle = native 直跑）。剩 M4.5 收口。

## 0. 目标与退出判据

把执行从 tier-0（rustc `InterpCx` + 协作调度，弃子）整体迁移到自研引擎：寄存器式字节码 +
冻结元数据、模型 A 帧、真 1:1 线程无 GIL、真地址无 AllocId overlay、`os::` 收口（P7）。

**M4 结束 = tier-0 能跑的一切在新引擎上可观测一致，tier-0 退役为差分 oracle。**

## 1. 架构与模块（一条硬纪律）

```
src/lower/   加载相（rustc_private 域）：驱动/tcx、mono 收集、MIR→字节码降低、
             布局/vtable/drop-glue/statics 冻结 —— tcx 关在这里，永不出境
src/vm/      执行相（纯 Rust，零 rustc_private）：
  bytecode   类型化字节码 + 冻结元数据（完全自包含，无 rustc 类型）
  engine     interp_frame / call_guest / Shared+Ctx / unwind(CleanupGuard) / 内建 intrinsics
  heap       真地址分配器（mimalloc 后端 + 薄包装）/ statics 区
  os         P7 注册表：直通(dlsym+libffi) / VM 内建 / 合成(libm)；denylist；thunk 工厂
```

**执行相纯度的机械门禁**：tsan harness（`#[path]` 只含 `src/vm`，无 rustc_private 依赖）——
`vm/` 若漏进 rustc 类型，TSan 构建当场编译失败。spike4 的通道即 M4 的架构护栏。

## 2. 关键设计决策（获批）

- **D1 降低策略 = eager，复用 rustc 单态化收集器**：`collect_and_partition_mono_items`
  给出与 native codegen 同一套可达 instance 集（正确性白拿）。加载相单线程降完 → 发布只读
  Shared → 执行相永久 tcx-free（C8）。逃逸兜底 = 惰性加载服务（显式同步格，insert-once），
  预期不触发。startup 代价后置优化（并行降低 / .mirvm 缓存属后续）。
  **修正（2026-07-07，债务普查发现，docs/m4-debt-map.md §2-A）**：collector 是 codegen/
  链接视角——跨 crate **非泛型**函数不重复收集（链接时用定义 crate 的机器码），而 mirvm
  是解释视角、无"链接 libstd.so"可言 → **collector 集合作种子 + 调用点 worklist 闭包扩集**
  （与 D5 fallback-body 补收同一机制）。扩集仍在加载相，执行相 tcx-free 不变。
- **D2 帧局部 = 字节区 + 冻结帧布局**：每函数冻结 frame layout（每 local 的
  offset/size/align），操作数区 = 字节 arena，帧 = 切段。聚合/枚举天然落位（spike1
  "真值必须带类型/尺寸"教训的落地）。slaved v0，alloca 后置（接口窄，C13 解耦纪律）。
- **D3 分配器 v1 = mimalloc crate 后端 + 薄真地址包装**；hand-rolled TLAB 后置。
  `__rust_alloc` 在 **lower 时**改写为引擎分配调用；`libc::malloc` 直通不动。
- **D4 fn-ptr 值 = 每 instance 一个真地址条目**（条目表）：内部经地址→instance 反查快路径；
  逃逸给 native 时该地址物化为 libffi thunk。比较/转型语义正确。
- **D5 intrinsics 政策**：有 MIR fallback body 的当普通函数降低（吃掉大头）；
  `must_be_overridden` 的做引擎内建（copy/原子/volatile/数学/discriminant 系，清单来自
  tier-0 + spike 实战）。
- **D6 vmctx：M4 无 P/R 问题**。纯解释器 = 宿主代码，ctx 是 Rust 参数；内部约定只在有编译码
  时存在 → **P/R 真负载终裁检查点挂 M5 JIT 接入**（勿丢）。M4 需要的只是**边界 TLS + attach**
  （thunk 被任意线程调——signal/pthread；vmctx-passing §1 结论），随 M4.4 thunk 工厂落地。

## 3. 分期施工（差分为 gate，demo 套件驱动）

| 期 | 内容 | Gate（== native，经新引擎） |
|---|---|---|
| **M4.0 地基**（过审） | 类型化字节码；lower 骨架（mono 收集→逐 instance 降低：算术/控制流/调用）；引擎骨架（类型化 interp_frame）；CLI `--engine=vm` | **fib 端到端**；执行相零 tcx/零 InterpCx |
| **M4.1 值与内存** | 全 Rvalue/聚合/枚举/投影；真地址堆（D3）；statics 冻结（`eval_static_initializer` → bytes + **重定位修正**成真地址）；常量池 | strings/hashmap/ptr_int；mmap 裸访问探针（**§2.5 三案例天生通过的实证**） |
| **M4.2 unwind** | panic 运行时 lower 成引擎原语；CleanupGuard；Drop→drop glue；catch_unwind | catch/panic_exit |
| **M4.3 os:: + FFI** | 注册表三处置 + denylist；args/env/time/fs/math/getrandom；libffi 直通 | args_env/time_fs/ffi_libc/**ffi_zlib**；corpus walkdir/tempfile/process |
| **M4.4 真线程（收官之战，过审）** | 1:1 直通（pthread/futex/join **真调用**）；thunk 工厂 + TLS 边界 attach；guest TLS per-thread；原子全序映射宿主；signal（§2.3） | threads_* 真线程；**c_blocking_io 与 net_echo_threaded（tier-0 挂死的两个）通过**；TSan 指向新引擎干净；rayon 28s→秒级 |
| **M4.5 收口 + 切默认** | cargo/frontmatter 接通；async；corpus 全绿（asm 阻塞 4 项记预期红，M5 归宿）；双引擎差分 | 全量差分绿；性能不慢于 tier-0（硬门）+ 记录加速比；`--engine` 默认切 vm |

## 4. 总验收

① **语义**：tier-0 通过的全部 demo/diff/cargo/corpus（非 asm）在新引擎可观测一致；
② **并发**：真线程、TSan 干净、两个 tier-0 挂死场景通过；
③ **架构**：执行相零 tcx（机械门禁）、零 AllocId、os:: 收口、模型 A；
④ **性能**：硬门 = 不慢于 tier-0；软目标 = 显著快（甩掉 per-op tcx 查询 + AllocId 间接）。

## 5. 风险与缓解

- **statics 重定位修正**（指针互指/vtable 图）= 最复杂单块 → M4.1 重点，复用 rustc alloc
  图遍历，一次冻结。
- **intrinsic/shim 面广** → D5 fallback body 吃大头 + corpus/diff 驱动 + tier-0 清单在手。
- **std 内部耦合**（lang_start/TLS/panic 内部）→ tier-0 已趟一遍、锁 nightly-2026-07-02。
- **体量**（数周级）→ 每期独立 gate 可回退，tier-0 全程保留作 oracle。

## 6. 不做

JIT/asm-JIT（M5，帧 ABI 已备好）、.mirvm 序列化与 mirvmc（模式 B，M4 后另开）、checked 模式
（C13 后置）、alloca、fork-alone（§2.6 处置后置，denylist 暂留）、REPL（M6）、嵌入 API（M7）、
弱内存序特殊化（映射宿主原子即在 RAM non-det 包络内）。

## 7. 挂起检查点（勿丢）

- **vmctx 内部约定 P vs R 真负载终裁** → M5 JIT 接入时（spike5 初判 R 领先 ~8%，但微基准
  偏袒 R——寄存器压力面未测）。
- **landing pad/LSDA**（JIT 帧内跑 drop glue）→ M5（spike5 已验 CFI 传播半边）。
- fork/clone 的 os::+atfork 处置（corpus §2.6）→ M4 后。
- **预降低 std 发行工件**（用户 2026-07-07 提，好主意并入 mode B）：mirvm 随发行自带
  ".mirvm 化 sysroot" = **非泛型 std 预降成字节码 + 泛型 std 带多态 MIR**（序列化，加载时
  按用户实例化单态降低——泛型不可能全预降）。收益：worklist 闭包大头是 std → 启动只降用户
  crate；消费端零 rust-src/零 rustc。落位：mode B（.mirvm）工程的第一个客户，M4.5 之后/
  M5 前后；M4 全程用 mode A（sysroot MIR + worklist）即可，不阻塞。
