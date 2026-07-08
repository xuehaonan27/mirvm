# M4 施工日志

> 总计划见 docs/m4-plan.md。每期一节：gate 结果 + 经验教训。

## M4.0 地基 —— **完成**（2026-07-07）

**Gate 全绿**：`tests/m4_gate0.sh` 9/9（fib 递归 / gcd / sum_to 循环 / collatz 分支 /
popcount 位运算 / mix_signed 有符号+窄化转换，全部 == 预期常量）+ **纯度门禁 PASS**
（tsan harness 构建 = `src/vm` 零 rustc_private 的机械检查）。全量回归无损：spike1-5 全 PASS、
tier-0 diff 16/16。

### 建了什么

- `src/vm/engine/`（纯 Rust 执行相）：`ir.rs` 类型化字节码（Place 溶解为帧偏移 + 宽度；
  执行期零符号表查询）、`frame.rs` ByteRegion（字节 arena + 冻结帧布局）、`ctx.rs`
  Shared/Ctx、`interp.rs` 类型化 interp_frame（spike3 同形：Call 宿主递归、raw-ptr ctx、
  字段级瞬态借用；M4.0 标量子集 + 溢出对 + Assert=引擎诊断退出）。
- `src/lower/`（rustc_private 加载相）：`collect.rs`（`collect_and_partition_mono_items`
  ——与 native codegen 同套 instance 集，D1）、`frame.rs`（逐 local layout → 对齐 bump 冻结）、
  `func.rs`（整体单态化 `instantiate_mir_and_normalize_erasing_regions` + 逐块翻译 +
  **Trap-stub 全覆盖**）、`mod.rs`（编排 + exports 表）。
- CLI：`--engine vm` + `--vm-call 'name(args…)'`（M4.0 gate 入口；main 启动链 M4.3 起）。
- `demo/m4/pure.rs`（`#[unsafe(no_mangle)]` = 收集根 + 稳定导出名）+ `tests/m4_gate0.sh`。

### 经验与教训

1. **Trap-stub 全覆盖机制一次成型**：空 main 的 pure.rs 仍收集整个 std 启动链宇宙
   （数千 instance），全部降完不中止——只有被执行的 6 个函数需要 trap-free。这就是 M4
   各期的增量协议：每期消掉自己负责的 Trap 类，gate 随之扩大；Trap 诊断串直接指认欠账的期。
2. **本 nightly 的 MIR 漂移（侦察先行再次值回票价）**，三处与既有认知不同：
   - `Rvalue::Use(Operand, WithRetag)`——Tree Borrows 的 retag 折进了 Use。**fast machine
     直接忽略 retag**（别名检查是检查器语义，P3 不检测）——白拿一个正确决策。
   - `Rvalue::NullaryOp` 消失：UbChecks 变成 **`Operand::RuntimeChecks`**（UbChecks/
     OverflowChecks/ContractChecks），lower 期按 session 旗标折成 bool 立即数。
   - `EarlyBinder::bind(tcx, v)` 多了 tcx 参。
   实操纪律：**写 lower 前先 dump 真 MIR + grep 本工具链源码**，凭训练知识写 rustc API 必翻车。
3. **防静默错值纪律**：非标量返回 → Return 处 Trap；非标量参数 → 整函数 Trap。宁可 Trap
   带诊断，不可静默返回 0——错值比崩溃贵得多（差分才能信）。
4. **整体单态化（clone body + instantiate 一次）** 比逐语句 subst 干净，Body: TypeFoldable
   直接支持。
5. Place 溶解为帧偏移的设计顺利落地：`resolve_place` 沿投影链累加冻结偏移，字节码只见
   (off, width)。debug 能力靠 FuncBody.name + Trap 诊断串，目前够用。

### 本期遗留（按计划归后续期）

聚合/枚举/投影(Index/Deref)、引用/裸指针、堆、statics（指针常量 → M4.1）；Drop glue、
panic/unwind（M4.2）；intrinsic 内建与 fallback-body 收集路径核实（M4.1 顺带）；128 位、浮点
（M4.1）；main 启动链（M4.3）。
