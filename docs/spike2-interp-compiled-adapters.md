# Spike 2：interp↔compiled 适配（i2c/c2i）+ 混合栈 —— 经验与教训

> 状态：**通过**（2026-07-05）。`mirvm spike2` 4 配置全 PASS，Spike 1 + tier-0 差分 16/16 无回归。
> 目的：验证模型 A 的**核心赌注**——解释帧与编译帧同在一条 native 栈上、靠薄适配器廉价互操作
> （这是当初选 A 而非 B 的理由）。

## 1. 建了什么

`src/vm/spike2.rs`（复用 Spike 1 的 bytecode/frame/memory；Spike 1 的 interp.rs 保持不动作为首版）：

- `Ctx`（= **vmctx**）：拥有 `Program` + `kinds`（每函数 Interp/Compiled）+ 操作数区 + 内存。
- `call_guest(ctx, func, args)`：**统一 dispatch**，既是 i2c（路由到编译帧）也是 c2i（编译帧调它、路由回解释器）。
- `interp_frame(ctx, ...)`：Spike 1 逻辑的 **dispatch 化演进**——`Call` 处调 `call_guest`（可再入到编译帧）。
- `compiled_fib_a/b`：**手写 `extern "C" fn(*mut Ctx, u64) -> u64`**，经 `call_guest` 调对方。
- harness：两个互递归 fib（fib_a 调 FIB_B、fib_b 调 FIB_A），按 `(fib_a kind, fib_b kind)` 跑 **2×2 配置**。

## 2. 验证了什么：四种转移，2×2 矩阵一举覆盖

| 配置 | 覆盖 | 结果 |
|---|---|---|
| interp / interp | interp→interp | ✅ |
| compiled / compiled | cc→cc | ✅ |
| interp / compiled | **i2c + c2i 交替（混合栈）** | ✅ |
| compiled / interp | 镜像 | ✅ |

全部 fib(0..=30) == native 参考。**interp/compiled 与 compiled/interp 配置的 native 栈真正交替
`interp_frame` 帧与 `compiled_fib` 帧**——正确跑通即证混合栈成立。**模型 A 的核心赌注成立。**

## 3. 头号发现：再入迫使 ctx 为**裸指针 vmctx**，而非 Rust `&mut`

c2i 会**再入**解释器：`interp_frame → (i2c) compiled_fib → (c2i) call_guest → interp_frame`，第二个
interp_frame 又要 `&mut` 操作数区。**Rust 的 `&mut` 无法表达这种再入式共享可变**——不能把 `&mut Ctx`
跨编译帧边界持有（持有期内再入会造出第二个 `&mut`，别名冲突）。

所以 ctx 全程以 `*mut Ctx` 传递，对字段做**瞬态借用、绝不跨 `call_guest` 持有**。安全性来自两点：
1. 操作数区是**纪律化的栈**：caller 的槽在下、callee reserve 的槽在上，切片天然不相交；
2. 单线程顺序执行：任一时刻只有栈顶帧在动，`&mut` 时序也不重叠。

**这就是 vmctx**：真 M4-JIT 里 Cranelift 把一个 vmctx 指针作特殊首参传给编译码（Wasmtime 同款），
编译码经它够到解释器态回调。骨架的"显式 `*mut Ctx` 首参"与之同构。三条备选里——(a) 显式 ctx 首参
（本 spike）/ (b) thread-local / (c) 保留寄存器 r15（HotSpot）——(a) 最干净且直接对应 Cranelift vmctx。

> **后记（2026-07-07）**：三选的完整分析与图示见 **docs/vmctx-passing.md**，结论比上面更锐利——
> **边界**（FFI 逃逸/回调/信号）被 plain-C 逃逸 + 每线程 ctx + 任意线程信号三条约束**逼定为
> TLS + 惰性 attach**，(a) 只剩内部约定的候选资格（vs pinned reg，M4 定）。本 spike 用 (a)
> 验证适配器模型仍然成立（spike 内无逃逸场景）。

## 4. 实现中踩到的坑：裸指针 ctx 的访问纪律

初版 `unsafe { (*ctx).region.write(...) }` **编译失败**：edition 2024 的 `dangerous_implicit_autorefs`
——方法调用语法在裸指针 deref 上**隐式** autoref（`&mut (*ctx).region`）被禁。两点教训：

1. **必须显式借用**：写成显式 `&mut (*ctx).region` 再调方法。封进 `reg_read/reg_write/mem_load/...`
   几个 helper，主体反而更干净。
2. **必须字段级，不能整体**：`&mut (*ctx).region`（只借 .region 字段）✓；`&mut *ctx`（借整个 Ctx）✗
   ——后者会与循环里长活的 `&(*ctx).prog`（借 .prog 字段供 body/block 用）在 Stacked/Tree Borrows 下
   冲突（整体 &mut 声称独占全 Ctx，作废了 &prog）。字段级借用彼此不重叠才 sound。

**这是裸指针 ctx 的通用纪律**，M4 真做 vmctx 访问时同样适用：不可变部分（prog/kinds）与可变执行态
（region/mem）要**字段级分开借用**，且可变借用瞬态不外泄。

## 5. 其他教训

- **适配器对标量确实薄**：i2c = 把单 u64 参直接进寄存器（`f(ctx, args[0])`）、c2i = `call_guest` 路由，
  **无 VM 帧编组**。这正是模型 A "廉价互操作" 的实证。真 Rust ABI 有聚合/多参时要按 fn_abi 编组
  （寄存器 + 栈槽铺参、返回位置），但仍是**槽/寄存器搬运**，不是构建 VM 帧——量级不同。
- **cc→cc 经 dispatch 是骨架简化**：本 spike 里编译帧调编译帧也走 `call_guest`（一次动态路由）。
  M4-JIT 会在编译期把 cc→cc 发成**直接 native call**（调用点链接/回填），省掉 dispatch 跳。这是 JIT
  优化，与适配器模型正交，不影响本 spike 结论。
- **手写编译帧足以验证适配器模型**：rustc 编译的 extern C 函数就是遵循约定、在 native 栈、经 ctx 回调的
  真 native 码，与 Cranelift-JIT 码在适配器层等价。"Cranelift 能否发此约定 + vmctx 参"是另一问题（后置）。

## 6. 对 M4 / 后续 spike 的输入

- **模型 A 核心赌注成立**（混合栈廉价互操作）——**给 Spike 3 开绿灯**：穿这条 interp+compiled 混合栈
  做 unwind + guest Drop + catch_unwind（**头号硬骨头**，倒逼帧 ABI 封版）。
- **vmctx 定形**：执行态经裸指针 ctx（= Cranelift vmctx）到达；访问要字段级、瞬态。这条 ABI 结构继承到 M4。
- **未做**（本 spike 边界）：Cranelift 真 JIT、跨边界 unwind、聚合/多参 Rust ABI 编组、cc→cc 直接调用
  链接、线程。各归后续 spike / M4。

## 7. 复现

```
cargo build --release
./target/release/mirvm spike2          # 期望：4 配置全 PASS，exit 0
./target/release/mirvm spike1          # 回归：全 PASS
MIRVM=$PWD/target/release/mirvm bash tests/diff.sh   # tier-0 回归：16/16
```
