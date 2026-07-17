# M4 全期 Trap 债务地图（--vm-stats 全 demo 普查）

> 调研产物（2026-07-07，M4.1 设计的扩展篇；**数字是 M4.1 开工前快照**——当期债务读法
> 用 `--vm-stats` 现跑；三个架构级发现（§2）仍是现行设计的依据）。仪器 `--vm-stats` 经两轮盲点修复后对
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

## 6. thunk 盲区：结构体内嵌 guest fn-ptr（corpus 批3 实锤，2026-07-16 记）

**已根治（2026-07-17，P1 commit `4202317`）**：fn-ptr 值自 P1 起对 FFI 可派生
（extern "C"/System·非变参·全标量类）的 guest fn 直接是**可执行码址**——
任何姿势流给 native（结构体内嵌、全局表、返回值带出）回调都落到 libffi
closure 蹦床进解释器，不再跳数据域。负对照实锤：flate2 C-libz 后端
zalloc/zfree 结构体内嵌回调现完整往返（三维+L2 热一致）。机制与分域见
decision-history §7.6。

**残余边界（知情面，非本项债）**：签名不可派生（Rust ABI / 聚合按值 / 变参）
的条目保持数据槽 + 逃逸位按需 thunk 原机制——此类 fn-ptr 被 native 调用
本来即 UB，盲区无实质剩余；若被 native 调仍跳崖（SIGSEGV 诊断化 =
MIRVM_SEGV_DUMP 既有旋钮 + 下记②阶梯可后补）。

<details><summary>原始记录（2026-07-16）</summary>

**现象**：guest 把含 fn 指针的**结构体**传给 native 库，native 回调该指针时，宿主
直接跳进 guest 数据地址执行——静默 SIGSEGV，无任何诊断。实锤：flate2 的 C-libz
后端把 Rust allocator（zalloc/zfree，extern "C" fn 指针）嵌进 `z_stream` 结构体，
libz 在 `deflateInit2_` 内回调；LD_PRELOAD 注入 SIGSEGV handler 实测
`si_addr==rip==0x6a000002e0c0`（delta image 冻结域，rw 非可执行），栈顶返回
地址落在 libz.so `deflate` 内。

**根因**：mirvm 的 thunk 机制（`sig.thunk_args`，native_sig 的 fn-ptr 形参现做
trampoline）只覆盖**显式 fn-ptr 实参**。结构体内嵌的回调从不以实参形态出现，
lower/运行期都看不见这次指针外流——真实地址模型的代价之一：寄给 native 的指针
就是宿主裸地址，被调用即跳崖。

**为什么难**：通用解不可行（无法静态知道 native 结构体哪些字段会被当回调调）。
可行阶梯：①**已知协议的显式签名表**（z_stream 的 zalloc/zfree、qsort 的 cmp、
pthread_create 的 start——按 C 协议语义在 freeze_c_fnptr_sig/结构体冻结时识别
fn-ptr 字段并物化 trampoline；覆盖 99% 真实场景，余者仍崩）；②**SIGSEGV 诊断
兜底**（MIRVM_SEGV_DUMP 已有，生产侧若将「rip 落在 guest 冻结域（非可执行）」
识别为跳崖并给出「疑似结构体内嵌回调」提示，把静默崩变成可读诊断——成本极低，
值得先做）；③长期：FFI 结构体白名单制（同 ① 但入库管理）。
关联：`x86 intrinsic 七族` 已解锁 flate2 纯 Rust 后端（zlib-rs 路线同病不触及）、
C-libz 路线仍挂此债（corpus/c_gix_pure.rs 文件头留了三路线排查记录）。

</details>

## 7. dep crate 的 global_asm 物化（corpus 批6 faer 实锤，2026-07-16 记）

**现象**：依赖 crate 内的 `global_asm!`（faer 的 pulp V3 LD_ST 汇编表
`libpulp_v0_21_*_{ld,st}_b32s_<mask>`）在 mirvm 下无机器码物化面——按值取址时
`符号未命中（归档兜底表 / dlsym 全域均无）`（corpus/c_faer_lu.rs 头注记录全诊断；
driver 以 default-features=false 标量内核绕行，三维已绿）。

**根因（S2 的账）**：mono 收集器的 RootCollector 走 `hir::ItemId` = **本地 crate
专属**（rustc_monomorphize/collector.rs:1550 处 `DefKind::GlobalAsm` 入队也仅在
本地项循环内）——native 语义下依赖 crate 的 global_asm 靠**该 crate 自己的
codegen 产物**（rlib 内的 object）经最终链接进场；mirvm 的 S2 依赖构建走
`-Zno-codegen`（metadata-only rlib，省 codegen 白烧），object 天然不存在，
本 crate 的 global_asm 物化机制（M5.2 D8h，mono 收集种子里只有本地项）接不到。

**可行路径（未立项，按真实 workload 优先级排）**：
①把 global_asm 收集面从本地项扩到 **used_crates 的依赖项**——每 crate 经
`collect_and_partition_mono_items` 拿 mono 图里的 GlobalAsm（名字/模板/操作数），
随本地 global_asm 同通道 cc 汇编 + dlopen 物化（preload 点不变），装载序 =
crate 图序（与链接序同构）；符号解析走既有 ③ 全域 + ② 句柄新序，pulp 的
LD_ST 表即解。随 S2 键链入 L2（物化产物按 asm_sites 同契约 warm 重物化）。
②**判名放行**（更省）：编译期对 used_crates 探测 `DefKind::GlobalAsm` 存在性，
仅对命中 crate 关掉 `-Zno-codegen`（让它的 rlib 带 object 走 archive 通道）——
改装面在 cargo_shim 构建指令，不改 lower；成本 = 只对 psm/pulp 一族付 codegen。
真 workload 触发前不接（faer 已有官方标量后门，不算阻塞）。

## 8. JIT 间接调用准入（CallIndirect/CallForeign/CallBuiltin 未进编译道，2026-07-17 记）

**现象**：`jit_compile.rs::admit` 的终止子白名单只放 Goto/Return/Unreachable/
SwitchInt/Call（`Call` 还要求 unwind=Continue + 返回值 Ignore/Scalar），
**CallIndirect、CallForeign、CallBuiltin 一律拒收**——含间接调用/FFI/内建
调用的函数体整体退回解释道。因此 fn-ptr 调用点（vtable 派发、回调、qsort
比较子等）永远是解释速度；MIRVM_JIT_THRESHOLD=1 三维对拍不受影响（语义
同源），但热点间接调用将来必上性能账单。

**与 P1 的关系**：P1（fn 条目可执行化，decision-history §7.5b）让 fn-ptr
**值**变成真码地址，宿主/解释器拿到都能跳；但 JIT 编译体**内部**没有间接
派发道，编译体遇 CallIndirect 仍然整体不被编译。P1 不解决本项，本项也不
阻塞 P1——两者正交，P1 先把"跳崖"治掉，本项是后续的速度项。

**可行路径（未立项，性能工作重启时排）**：①编译体的 CallIndirect 编译为
"反查 fn_addrs 命中 → 经 PLT 槽快路直调编译体 / 未命中 → c2i 回解释"（与
直接调用同套蹦床基建，反查可用缓存行内联 last-1）；②CallForeign 编译道
（libffi 调用点 CLIF 化或预物化 stub）；③CallBuiltin 逐内建评估（HostWrite
等直通族优先）。准入扩面时先扩 admit + 三维验收，alpha 铁律不变。
