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

## M4.1 值与内存 —— **完成**（2026-07-08）

**Gate 全绿**：`tests/m4_gate1.sh`——digest 九函数（vec/string/map/box/enum/slice/static/
rawptr/float）经 `--vm-call` 全部 == 同源 native（rustc -O）直跑值（含**真 HashMap**：
hashbrown SSE2 group 探测走引擎 SIMD 最小集，"B 目标 A 排序"决策兑现、未触发降级条件）+
`--vm-stats` 复测九函数可达集 **M4.1 份内债务清零**（残余全部归期豁免：resume/caller_location
=M4.2、foreign syscall/errno/sse2.pause=M4.3、ThreadLocalRef=M4.4，均在 panic/OS/TLS 分支，
执行路径实测 trap-free）。全量回归无损：gate0 9/9、纯度门禁、diff 16/16、spike1-5、TSan 零警告。

### 建了什么（按施工顺序）

- **第 0 步 worklist 闭包扩集 + foreign 三路**（D1 修正落地）：`Linker`（lower/mod.rs）
  ——ids/queue worklist（collector 集作种子、调用点扩集）+ ①引擎原语表（allocator-shim
  mangled 符号清单→`CallBuiltin`）②链接仿真（`for_each_linked_def` 同构导出符号表，
  strong 覆盖 weak；panic_impl/__rust_start_panic 自动解到 std/panic_unwind 实现）③未知
  foreign Trap 带 os:: 归期。intrinsic fallback body 同机制补收（`Instance::new_raw`，
  collector 源码同款构造）。`--vm-stats` 加 foreign 全清单段（os:: 种子，M4.3 输入）。
- **第 1 步 mmap 定容 ByteRegion**（F6）：帧地址终身稳定，base 语义改真地址；到界=栈溢出近似。
- **第 2 步 place 求值 + ABI v2**：`PlaceExpr{Local|Static, [Deref|Offset|IndexScaled]}`
  快慢双路（纯帧内静态槽零开销保留）；`Copy`/`RepeatScalar` memcpy 通道；`Ref`/`PtrOffset`；
  **调用约定 v2 一次定型**（Zst/Scalar/Pair 2 槽/Indirect+sret 槽四路，interp_frame 返回
  (lo,hi)）——原计划第 3 步的 pair 通道被 rawptr gate 的 `as_mut_ptr(&mut [u64])` 逼提前。
  投影链编译：Field/Downcast 折偏移（variant 状态）、Deref 追踪胖指针 meta、ConstantIndex
  from_end 数组折叠；位拷 cast 家族 + Unsize 数组→切片；PtrMetadata；offset intrinsic 就地展开。
- **第 3 步 枚举/浮点/Aggregate**：TagInfo 冻结（Direct=Cast 符扩、Niche=NicheDiscr——
  rustc 不变量"niche 下 discr==variant index"使零映射表）；SetDiscriminant lower 期溶解为
  常量写；Aggregate cg_ssa 同构（variant 布局逐字段+set_discr；RawPtr=(data,meta) 合成）；
  FloatBin/Cmp/Neg/Cast/FloatToInt(饱和)/IntToFloat 位进位出；IntCmp3（Ordering）；
  Transmute=字节搬运（同宽标量快路径）。
- **第 4 步 常量池 + statics 重定位**（F5，"最复杂单块"实际很顺）：`FrozenArena`（mmap RW，
  挂 Module 随执行相共享）；`Linker::ensure_alloc` 按需递归物化 GlobalAlloc 图——**先分后填
  破指针环**；重定位=遍 `provenance().ptrs()` 写目标真地址+addend（addend 即 ptr 位置原存
  字节）；Function=D4 fn 条目（16 对齐真地址+addr→FuncId 反查表）；VTable=
  `tcx.vtable_allocation` 现成分配同机器。常量四形态（Scalar::Ptr/Slice/Indirect/ZeroSized）
  全落。dyn unsize=vtable 物化；ReifyFnPointer=条目地址。
- **第 5 步 堆 + Drop + intrinsic 表**：heap.rs=libmimalloc-sys 薄包装（D3）；Drop 正常路径=
  普通 Call（glue 实参 AddrOf(place)）；intrinsic 就地展开表——原子系全套映射**真宿主原子指令**
  （AtomicUN::from_ptr，SeqCst；spike4 义务，不留单线程假实现）、位系 ctpop/ctlz/cttz/bswap/
  bitreverse、memcpy 系（+语句形态）、ptr_offset_from、size_of_val（sized=常量、slice/str=
  meta 折算）、compare_bytes、exact_div、black_box/transmute/assume。
- **第 5.5 步 SIMD 最小集**：SimdBin（逐 lane 比较/位/算术，几何冻结自 SimdVector layout）+
  SimdSplat + SimdBitmask（movemask）——实测尾巴仅 3 个 intrinsic（simd_lt/splat/bitmask 一层
  层浮现），远低于降级阈值 10。**顺带解锁间接调用**：`CallIndirect`（fn-ptr 与 dyn 虚派发同一
  机制）——receiver 胖指针拆 (data,vtable)、callee=*(vtable+idx×8)（本 nightly VirtualIndex
  不加头偏移，idx 即绝对槽号）、经 fn_addrs 反查派发；hashbrown resize_inner 的 dyn Allocator
  回调借此通过。

### 经验与教训

1. **"每步一个 gate 函数变绿"的增量节奏完全兑现**：每步收尾时剩余 digest 恰好全部 Trap 在
   下一步的分期债务上（诊断串归期分毫不差）——Trap-stub 协议 + 分期诊断标签是施工的导航仪。
2. **ABI 是依赖图的咽喉**：设计文档把 pair 通道排第 3 步，实测 rawptr gate 第 2 步就需要
   （as_mut_ptr 的胖指针参数）。教训：**调用约定这类横切面要一次定型**（四路 ABI 直接覆盖
   1466 处非标量返回债务），比按 gate 逐步扩更省返工。
3. **rustc 的不变量白拿正确性**：niche 编码下 discr==variant index 是 layout sanity check
   保证的不变量（rustc_abi 源码注释），NicheDiscr 因此不需要 discr 映射表。**读源码注释比
   猜语义快**。
4. **本 nightly 新漂移**（续 M4.0 清单）：`Rvalue::Reborrow(Ty, Mutability, Place)`（用户
   ADT reborrow=位拷）；`UnOp::PtrMetadata` 取代 Len；`ConstValue::Slice{alloc_id, meta}`
   （直接 AllocId 非 ConstAllocation）；`std::range::RangeInclusive` 字段是 start/**last**；
   `VirtualIndex::from_index` 不加 3（idx 已含 vtable 头）；atomic intrinsic 不带 order 后缀
   （order 是泛型参）；`SpecialAllocatorMethod{Alloc,AllocZeroed,Dealloc,Realloc}` +
   `mangle_internal_symbol` 拿 __rust_alloc 真符号。
5. **两遍法的"先分后填"用按需递归表达更简**：ensure_alloc 先插 map 再填字节，指针环天然安全，
   不需要显式全图两遍。vtable→fn 条目→worklist 的递归链一次打通。
6. **foreign extern static 的坑**：`gettid` 等 weak 符号判空模式走 GlobalAlloc::Static 但
   is_foreign_item——eval_static_initializer 会 panic，须先挡（真符号地址=os:: M4.3）。
   同类：vtable_allocation 对 unsized 源类型 panic（dyn→dyn upcast 须先挡）。
7. **性能顺带观察**：lower 端到端（386→数百 instance 扩集后）仍 ~0.13s（digest.rs，热缓存），
   worklist 扩集未成为负担；正式性能核算挂 M4.5（硬门=不慢于 tier-0）。

### 本期遗留（归期明确）

- u128 算术（2 处，panic 格式化路径；memcpy 通道已通）→ M4.1+/顺带。
- dyn→dyn upcast、Box<dyn> receiver 聚合形态、ClosureFnPointer、Subslice/from_end-on-slice
  投影、`caller_location` → M4.2（panic 链路）或按需。
- atomic fence=nop 的弱序复查、TLS → M4.4。
- foreign 种子清单（syscall/__errno_location/llvm.x86.sse2.pause/write/clock_gettime/abort/
  _Unwind_RaiseException）→ M4.3 os:: 注册表输入（--vm-stats foreign 段直接给）。
