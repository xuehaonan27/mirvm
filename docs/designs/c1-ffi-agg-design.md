# C1：FFI 按值聚合封送（出向 + 入向）施工设计

> 2026-07-18 立项（open-issues C1 转正）。实锤供养：c_tree_sitter 全 parse 路径汇到按值
> TSInput（内嵌 read 回调）+ `ts_node_*` 按值传/返 TSNode(32B)/TSPoint(8B)。
> 本文是闭合契约与分片设计；完成度由 [../current-status.md](../current-status.md) 维护。

## 0. 闭合契约（验收成立即关闭）

extern "C"/"system" 非变参签名的**按值聚合参数**与**按值聚合返回**，在
**出向**（`CallForeign` + `CallIndirect` native_sig 道）与**入向**（M4.4 thunk
工厂 + P1 条目 stub 蹦床）两个方向 × 全 ABI 形态（单字段/多字段/嵌套/数组、
≤16B 寄存器档、>16B sret 档）上，与 native **逐字节**一致。

**原理可闭合的依据**：libffi `ffi_type_struct` 全聚合能力（eightbyte 拆分/sret/
寄存器对内建，语义不自证）+ rustc layout 精确字段展开 + 引擎既有聚合约定
（`ParamAbi::Indirect` 传址 + prologue memcpy / `RetAbi::Indirect` sret 通道，
guest↔guest 双向早已现役）。

**原理边界（如实拒绝，按 workload 再立）**：
- union 按值（SysV 分类需额外 union 规则；TS 一族无此形态）——freeze 响亮 Err。
- SIMD 向量按值（BackendRepr::SimdVector）——另一轴，另立。
- 变参尾参位聚合（fixed 聚合 OK，尾参聚合 Err）。
- 聚合内嵌 fn-ptr 成员按**原生字节**直传（P1 条目可执行化后可调；不可派生
  条目同 P1 文档边界：被 native 调本即 UB）——TSInput.read 走 P1 条目，天然解。
- i128/f128/长双/_Complex 既有标量边界不动。

## 1. 数据模型

```rust
FfiKind::Agg(FfiAgg)                       // 新增变体（serde；IR 结构变更 = 全缓存失效）
FfiAgg  { size: u32, align: u32,
          fields: Vec<FfiField> }        // 声明序、含位间隔 padding 隐含于 offset
FfiField{ off: u32, leaf: FfiLeaf }
FfiLeaf { Scalar(FfiKind) | Agg(FfiAgg) }  // 递归嵌套（数组 = 元素重复字段）
```

- lower 的展平器：rustc `layout_of` → BackendRepr 三分：`Scalar`→叶（今路径）；
  `ScalarPair`→2 字段 Agg；`Memory` 等→逐字段递归（Adt single-variant/tuple/array；
  union/SimdVector/unsized/ZST-of-sig 响亮 Err）。
- `ffi_kind_of` 放开产出；`freeze_c_fnptr_sig` 放行（附带的正面效应：带聚合签名
  的 guest fn 自此**进入 P1 可执行条目候选**——TS 的 read 回调即此家族）。

## 2. 全局约定（一条，消灭方向间错配）

**FFI 边界两侧的聚合一律按真地址（字节）交接**：

- 出向参数：ir 实参 = `Operand::AddrOf(place)`（func.rs:2851 现成形态）→
  eval 得聚合字节地址 → libffi avalue 直接指向该内存（`Arg::new(&[u8])` 对
  `?Sized` 取数据地址，已实证）。
- 出向返回：`RetDest::Indirect(dst)`（调用点对按值聚合返回**强制**——
  Pair/单字段同大档统一此路；libffi 结果缓冲按 16 字节对齐桶分配，调用后
  memcpy `agg.size` 字节到 dst）。
- 入向参数：libffi closure `avalue[i]` 恒为聚合字节地址（<16B 与 sret 档同形）；
  marshal 按 callee `ParamAbi` 映射：Indirect → 传址（prologue memcpy）；
  Scalar/Pair → 按 `FfiAgg` 声明序字段读值（callee 槽序 = 布局序，rustc layout
  同源恒等）。
- 入向返回：RetAbi::Indirect → thunk 把 `result` 指针当隐藏首实参槽（现成
  sret 约定）；Pair/小档 → 返回 (lo,hi) 后按 `FfiAgg` 字段偏移**重打包**为
  结构体字节（屏蔽宽度小李端语义，与出参同构）。

JIT 无涉（CallForeign/CallIndirect 未准入，照退回解释道；间接调用项 E1 独立）。

## 3. 分片

- **片 A（出向 + 数据模型）**：ir `FfiKind/Agg` + 展平器与 freeze 放开 +
  `ffi::call/call_addr` 按 kind 分派参数缓冲 + 聚合返回缓冲与 dst memcpy +
  `CallForeign`/native_sig 返回放行 Indirect + 调用点 RetDest 强制。
  无行为回退的检查 = 既有全标量用例路径逐字节不变。
- **片 B（入向）**：thunks `marshal_args` 聚合语义 + 新 `interp::call_guest_ffi`
  （按 callee ParamAbi 展开 av）+ trampoline/entry_trampoline 返回两档
  （sret 直传 / FfiAgg 重打包）。
- **片 C（验收）**：合成矩阵探针（1/2/3+ eightbytes × 单字段/多字段/嵌套 ×
  参数/返回 × 两方向，cc 现编小 .so 经 Command+dlopen，双维同构）+
  **c_tree_sitter 原样三维转绿**（B 维 15 行 oracle 已固定）+ 全量 gate。

## 4. 失败学与诊断

- freeze 遇 union/SIMD/unsized：响亮 Err 沿用「非标量（按值聚合形态 X）」
  文案 + 指明边界分类（如实红锁定）。
- 入向 marshal 遇到 callee ParamAbi 与 FfiAgg 字段数不付（理论不可能，因
  同源 layout）= engine_abort 带符号名，不静默降级。
- libffi 结构类型构建失败（align/size 契约）= Err 上行（珍重了 dlerror 的先例）。

## 5. 与登记册的关系

本设计落地即关闭 open-issues C1；c_tree_sitter 的 expected-red pattern
（`非标量（按值聚合）`/70）随之 XPASS 转绿。关联不阻塞：C2（符号在 rlib）
正交；E8 backtrace 符号化无关；R16 的 SymFn 旁路与本片同享 P1 条目预算。
