# Spike 1：最小模型 A 骨架 —— 经验与教训

> 状态：**通过**（2026-07-05）。`mirvm spike1` 全 PASS，tier-0 差分 16/16 无回归。
> 目的：验证模型 A 的基本执行形状成不成立，脱离 rustc InterpCx 写第一段真·自研 VM 代码。
> **2026-09-19 归档**：spike 代码已从 `src/vm/spikes/` 删除（裁决见 decision-history §7.65）。
> 本文是结论与证据的留档；文中 `mirvm spikeN` 命令已不可跑。

## 1. 建了什么

`src/vm/`（**纯 Rust，零 `rustc_private`**，与 tier-0 `src/interp/` 并列）：

| 文件 | 内容 |
|---|---|
| `bytecode.rs` | 寄存器式、贴近 MIR 的字节码：`Program/Body/Block/Stmt/Rvalue/Operand/BinOp/Terminator` |
| `frame.rs` | `OperandRegion`——slaved 操作数区 v0（一条 `Vec<u64>`，帧 reserve/restore 切段） |
| `memory.rs` | `GuestMemory`——`libc::mmap` 真地址 + bump alloc + 裸 `load/store`（§2.5 模型） |
| `interp.rs` | `interp_frame`——tree-walking，`Call` 处**宿主递归**（模型 A 核心）；`Vm` 环境 |
| `spike1.rs` | 手写 fib/loop-sum/mem-sum + 进程内差分 harness（对比 native Rust 参考） |

入口：`mirvm spike1`（cli.rs 一个分支）。三个程序共 1294 组输入全部 == native 参考。

## 2. 验证了什么（成立的结论）

- **模型 A 站得住**：`Call` 处 `interp_frame` 递归一层 = guest 帧落在 native 调用栈上。fib(35) 递归深度 35 正确跑通。控制流（SwitchInt/Goto 循环）、递归调用、二元运算都对。
- **贴近 MIR 的字节码是自然的**：`Body=blocks`、`Block=stmts+terminator`、`Operand=Slot|Const`——手写 fib/循环/内存程序时，每个 MIR 构造都一一对应，**几乎机械**。这佐证了 "字节码贴近 MIR" + "MIR→字节码降低是低风险机械活、可后置"。
- **§2.5 在 greenfield 层白赚解决**：`GuestMemory` 用真地址 + 裸 `ptr::read/write`、**无 AllocId、无检查器 overlay**，mem-sum 的 Alloc/Store/Load 全对。**这正是 tier-0 检查器拒绝的模型**（walkdir/process/mmap 三实例）——M4 甩掉 overlay 后它天生就通。骨架直接兑现了这个判断。
- **slaved 局部 + native 栈控制流是正交的**：控制流/调用活动在 native 栈（interp_frame 递归），局部数据在 slaved 区（Vec 切段）——正是 frame-abi-bytecode.md §2.2 说的 v0 组合。

## 3. 教训与坑（经验）

### 3.1 借用结构：把 `&'p Program` 复制到局部是关键

最初的直觉写法会挂：
```rust
let body = &self.prog.funcs[func];   // 借了 self（因为读 self.prog 字段）
... self.region.write(...)           // ✗ 不能再 &mut self——body 还借着 self
```
读 `self.prog` 字段本身要不可变借 `self`，于是 `body` 的整个生命周期都借着 `self`，挡死后面对 `self.region`/`self.mem` 的可变访问。**解法**：先把共享引用**复制**到局部——
```rust
let prog = self.prog;                // &'p Program 是 Copy；借 self 一瞬即释
let body = &prog.funcs[func];        // body 借 'p（程序活得比 Vm 久），不借 self
... self.region.write(...)           // ✓ 自由
```
**教训**：解释器循环里，"只读的字节码"与"可变的执行态（操作数区/内存）"必须在借用上解耦。字节码借更长的 `'p`、执行态借 `&mut self`。这条对 Spike 2（interp_frame 里再插 i2c/c2i 调用）同样成立，得保持这个结构。

### 3.2 u64-word 值表示够 fib，但**真值必须带类型/尺寸**

skeleton 所有槽都是 `u64` word，BinOp 走无符号 wrapping。fib/sum 天然无符号,没问题。但一旦混 `i64`/`bool`/`f64`/指针/聚合，就需要：按 dest 类型选有/无符号、选位宽、选浮点语义——即**槽要带冻结的 layout 元数据**（size/类型），这正是设计里 "冻结元数据进字节码" 的动因。Spike 1 用 word 是刻意压范围；**下一个碰真类型的 spike 必须先把值/槽类型化**。

### 3.3 模型 A 的栈深忠实：解释器层**不完美**，JIT 层才补齐

模型 A 说"栈溢出忠实"。但要诚实：tree-walking 下**每个 guest 帧 = 一个肥大的 `interp_frame` native 帧**（含 match 循环、临时量），远比 guest 真实帧大。于是**解释器层会在比 native 更浅的 guest 递归深度就溢出 native 栈**——溢出*行为*忠实（该溢出时溢出，不像模型 B 能无界堆增长），但*阈值*不忠实。JIT 层（编译帧 ≈ native 帧尺寸）才恢复精确阈值。**教训**：栈深精确性是 JIT 层的属性，冷解释器层近似即可；真跑深递归可能要给 native 栈调大或加 guard（Spike 3 unwind 时一并考虑）。

### 3.4 slaved 区每次访问是一次带边界检查的 Vec 索引

`region.read(base, slot)` = `slots[base + slot]`，一次越界检查 + 间接。对冷解释器可接受，但**这正是 alloca 迁移的动机**（局部直接内联 native 栈、无间接）。Spike 1 坐实了 slaved 是能用的起步、且接口够窄（4 个方法），换 alloca 时 interp 侧改动面小。

### 3.5 进程内差分 harness 很顺手

"骨架跑手写字节码 vs native Rust 参考" 在同进程比对，快、无外部依赖、无需 sysroot。因 tier-0 已 == native，"== native 参考" ⇔ "== tier-0"。**注意边界**：这验证的是"解释器正确执行了手写字节码"，**不是**"MIR→字节码降低正确"（那是后置的、低风险的另一件事）。

## 4. 对 M4 / 后续 spike 的输入

- **模型 A 地基验证通过**——可以继续往上搭。`interp.rs` 的 `Call` 分支就是 Spike 2 插 **i2c（调编译帧）/c2i（编译帧回调）** 的点，也是 Spike 3 混合栈 unwind 要穿过的点。保持 §3.1 的借用结构。
- **字节码形状定了个雏形**：register-based + MIR 对应。真做 mirvmc 降低前，先按 §3.2 把值/槽类型化。
- **内存模型方向确认**：真地址 + 裸访问 + 无 overlay 是对的（§2.5 白赚）。真实现把 `GuestMemory` 的 bump 换成 TLAB + mimalloc 结构（designs/concurrency-arch.md §3.3），接口（alloc/load/store）应能大致保留。
- **明确未做**（本 spike 边界，非遗漏）：类型化值、聚合/枚举/vtable、真分配器/free、alloca、JIT、unwind/Drop、线程、MIR 降低、Stable MIR。各归后续 spike / M4 正式实现。

## 5. 复现

```
cargo build --release
./target/release/mirvm spike1          # 期望：fib/loop_sum/mem_sum 全 PASS，exit 0
MIRVM=$PWD/target/release/mirvm bash tests/diff.sh   # tier-0 回归：16/16
```
