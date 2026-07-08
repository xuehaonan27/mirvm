# M4 全期 Trap 债务地图（--vm-stats 全 demo 普查）

> 调研产物（2026-07-07，M4.1 设计的扩展篇）。仪器 `--vm-stats` 经两轮盲点修复后对
> **16 个单文件 demo + digest/pure** 普查所得。每条判断可复现：
> `mirvm run --engine vm --vm-stats demo/<x>.rs`。

## 0. 仪器与盲点修复（两轮）

初版可达分析低估严重（hashmap @entry 仅 1 fn 可达）。两轮修复：
1. **语句失败 → `Stmt::Trap` 占位 + 终止子照常降低**（Call 边保住）；非标量参函数 →
   入口 Trap 语句 + 体照常降低（原 trap_body 把整个下游藏掉）。
2. **Call 终止子自身失败**（实参/返回落点非标量）→ 前置 Trap 语句 + **保留 Call 边**；
   **Drop 带 glue → 解析 `Instance::resolve_drop_glue` 并发 Call 边**（M4.2 债务下游可见）。

修复后 @entry 可达 19-299 fn（真实调用图）。**残余盲点**：fn-ptr 间接调用无出边；
layout 失败的整函数 trap_body 无出边；"调用目标未收集"（见 §2-A）无出边——**债务读法
永远是"至少欠这些"**。

## 1. 普查总表

| demo | instance | 含 Trap | @entry 可达 fn / Trap 类 | 备注 |
|---|---|---|---|---|
| pure（M4.0 gate） | ~84 | ~92% | gate 函数全 ✅ | M4.0 已收 |
| fib | 84 | 92.9% | 60 / 114 | main 含 println |
| digest（M4.1 gate） | 191 | 90.6% | per-export 见 §4 | |
| strings | 220 | 91.8% | — | |
| hashmap | 357 | 91.0% | 299 / 524 | 最深 |
| catch / panic_exit | 32 / 37 | ~94% | 19 / 53 | M4.2 素材 |
| ptr_int / time_fs / args_env | 69-92 | ~90-92% | 63-… / 130-… | |
| threads_*（5 个） | 249-472 | ~90-92% | 145 / 283（spawn） | M4.4 素材 |
| async_hand / async_suspend | 32 / 38 | ~88% | 5 / 18 | 意外地浅 |

全 demo 聚合 TOP（按 Trap 块计）：**Deref 2752**、非标量返回 1466、resume 955（全在
cleanup 块，M4.2）、&str pair 595、Layout 实参 326+142、**未收集 panic_nounwind 255 +
panic_fmt 140**、Offset 269、**atomic_cxchg 240**、Downcast 220、Option\<usize\> 153、
btree 系 124（std 内部拉入）、discriminant 105+。分期余额：**M4.1 系 8148 + M4.1+ 4290
（压倒性主导）**、未标注 2015（大头即 §2-A）、M4.2 706、foreign 仅个位数（§2-C）。

## 2. 三个架构级发现（改变计划的）

### A. mono 收集器的集合 ≠ 解释闭包 —— **D1 需修正**（本次普查最重要发现）

`std::io::_print`、`core::panicking::panic_nounwind`、`std::rt::panic_fmt` 全部显示
**"调用目标未收集"**（255+140+ 处）。机制：collector 是 **codegen/链接视角**——跨 crate
的**非泛型**函数在其定义 crate 已编译成机器码，本 crate 不重复实例化，链接时直接链。
而 mirvm 是**解释视角**：没有"链接到 libstd.so 的机器码"这回事，一切 MIR 都要自己降低
执行（tier-0 靠 `-Zalways-encode-mir` + 按需 instance_mir，天然闭包）。

→ **D1 修正：collector 集合作种子 + 调用点 worklist 闭包扩集**——lower 遇到不在表里的
callee（非 foreign、非 intrinsic-内建、非 panic-原语）→ 加入待降低队列、分配 FuncId、
继续。与 D5 的 fallback-body 补收是**同一机制**（worklist 本来就要建）。执行相 tcx-free
不受影响（扩集仍发生在加载相）。M4.1 施工顺序第 2 步之前先落这个。

### B. panic 前端**照常解释**，引擎只在 extern 原语层接管（修订：初版"lower 特判 panic 入口"被否，过 ad-hoc）

worklist 扩集后 panic 路径会拉进 fmt 家族——**这不是要避免的问题**：native 二进制同样带着
这套代码、panic 路径真的跑它们，解释它们是忠实性。优雅解（Miri 同款、tier-0 实际形态、
用户 2026-07-07 定）：`panic_fmt`/panic hook/payload 装箱/fmt 家族**全部照常经 worklist
降低解释**；引擎接管点**下移到 std 自己声明的 runtime 边界**（本来就是 extern 的符号）——
`__rust_start_panic`（unwind 原语，M4.2 = spike3 的 raise）、`__rust_alloc` 系（分配原语，
M4.1）、syscalls（os::，M4.3）。

于是"拦截"不是 ad-hoc 的 lang-item 清单，而是 **foreign 解析的三路处置**（P7 注册表本来的
形状）：**① 引擎原语**（alloc/unwind 级 extern）；**② guest 导出符号 = 链接仿真**——
`panic_impl` 这类 weak lang item，native 靠链接器把 core 的 extern 声明连到 std 的
`rust_begin_unwind`，我们在加载相做同一件事（查 `tcx.lang_items()`/导出符号表；tier-0 的
`for_each_linked_def` 已趟过）；**③ os:: 直通**（M4.3）。未落地前未知 foreign 暂 Trap。
特判的不是"panic 是什么"，而是"链接器本来会做什么"——加载相份内事。

### C. foreign/os:: 种子名单目前被遮蔽 —— M4.3 复查项

普查只见 `__rust_no_alloc_shim_is_unstable`（2 处）——因为 read/write/clock_gettime 的
调用点在 std 非泛型 fn 体内（sys::unix），那些函数"未收集"（§2-A），其体内的 foreign
调用 lower 根本没看到。**worklist 扩集落地后 foreign 名单才会真实浮现**——M4.3 开工调研
= 扩集后重跑本普查，即得 os:: 注册表种子清单。

## 3. 分期债务画像（普查视角）

- **M4.1（压倒性主导）**：Deref/Offset（place 求值）、非标量返回/参数/place（ScalarPair
  大宗：Layout/Option\<usize\>/&str）、Downcast/discriminant（枚举）、Aggregate、常量池、
  Transmute。→ m4.1-design.md 的施工顺序覆盖；**加一步 worklist 扩集（§2-A）**。
- **M4.2**：resume 955（cleanup 块，正常路径零成本）、Drop glue 执行（边已可见）、
  panic 系原语（§2-B）、Assert→真 panic。catch/panic_exit demo 是现成 gate。
- **M4.3**：os:: 种子被遮蔽（§2-C），扩集后复查；`_print` 链、time/fs/env。
- **M4.4**：**atomic_cxchg 240 处**（Once/Arc 引用计数——比预想更早成为高频路径；
  M4.1 就按 spike4 义务直接内建成宿主原子，一步到位不留单线程假实现）、threads 系
  demo 的 pthread/TLS（ThreadLocalRef rvalue 已在债务表）。
- **SIMD 风险项维持**：`__m128i` 实参 20 处**确认在 map_digest 可达路径**（hashbrown
  SSE2 group）——m4.1-design §4 的 A/B 决策仍需拍板。
- async 两 demo 意外地浅（5 fn/18 类）——无栈状态机=普通 MIR 的又一佐证，M4.5 收口无特殊风险。

## 4. digest per-gate（盲点修复后的准确数据，替代 m4.1-design §1 初版）

| gate fn | 可达 fn / Trap 类 | 主债务 |
|---|---|---|
| vec_digest | 28 / 75 | Deref 14、非标量返回 8、&str 8、panic_nounwind 未收集 8 |
| string_digest | 36 / 103 | Deref、Layout 实参 13、&str 11 |
| map_digest | 94 / 168 | Deref 106、非标量返回 35、**rotate_left 24（fallback 补收首客）**、`__m128i` 实参 20 |
| box/enum/slice/rawptr/float/static | 6-28 fn | 与初版判断一致（pair/枚举/place 求值/Transmute/指针常量） |

## 5. 对既有文档的修正

- **m4-plan.md D1**：加"种子 + worklist 闭包扩集"修正（§2-A）。
- **m4.1-design.md**：§1 per-gate 以本文 §4 为准；§4 SIMD 风险维持；施工顺序在
  "place 求值"前插入"worklist 扩集"。
- 仪器（`--vm-stats` + 两轮盲点修复）为永久资产，每期开工调研标配。
