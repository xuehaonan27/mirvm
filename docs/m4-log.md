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

## M4.2 unwind —— **完成**（2026-07-09）

**Gate 全绿**：`tests/m4_gate2.sh`——unwind 五函数九用例（catch+Drop-in-unwind /
跨多帧传播（内层先）/ 越界 Assert→真 panic_bounds_check→catch / panic 消息 payload
跨 unwind 存活+downcast / 捕获后 resume_unwind 重抛）经 `--vm-call` 全部 == 同源
native 直跑值 + `--vm-stats` 复测 **M4.1/M4.2 份内债务清零**（resume 770 处全部消化，
可达集仅剩 foreign 6 处=M4.3）。全量回归无损：gate0/gate1、纯度门禁、diff 16/16、
spike1-5、TSan 零警告。panic hook 打印（消息+精确 location "file:line:col"）与
未捕获 panic 退出码 101 均与 native 一致。

### 建了什么

- **spike3 协议平移**（第 1 步）：`GuestPanic{exception}` 载 guest 侧
  `_Unwind_Exception` 指针 + `resume_unwind`（无 hook 噪声）；`FrameGuard` = 解释帧
  landing pad（**动态 LSDA**：`unwind_edge` Cell 在每个可 unwind 终止子前设置）——
  unwind 穿帧时 Drop 跑 cleanup 链（`run_blocks` 复用主循环、`Resume` 终止子=返回让
  宿主 unwind 续传，**零 payload 栈**）+ region 恢复统一（正常/unwind 同路）；
  `UnwindAction::Terminate`=catch+abort；顶层 catch=退出码 101；**递归深度守卫**
  （frame-abi §9 guest 栈溢出近似，MAX_DEPTH=8000——也是调试无限递归的仪器）。
- **两个原语**（第 2 步）：`_Unwind_RaiseException` → `Builtin::UnwindRaise`
  （**panic_unwind 照常解释**：Exception 结构 Box 在 guest 堆闭环、引擎只运载指针——
  debt-map §2-B "拦 std 声明的 extern 边界"的兑现）；`catch_unwind` intrinsic →
  `Builtin::CatchUnwind`（宿主 catch + `call_fn_addr` 派发 try/catch fn + downcast
  区分 GuestPanic/宿主 panic——**宿主 panic 绝不吞**）。
- **track_caller ABI**（第 3 步）：`requires_caller_location` → 帧尾 &Location 槽
  （sret 同款附加槽）；调用点转发（本 fn track）或 `span_as_caller_location` 合成
  （物化走既有常量机器）；**Virtual 调用也传**（vtable 侧是 VTable shim 接收）；
  **fallback intrinsic 调用点即换 `new_raw`** 使 caller/callee ABI 一致（cg_ssa
  `IntrinsicResult::Fallback` 同构）；`caller_location` intrinsic=读槽/合成。
- **Assert 展开真 panic**（第 4 步）：Assert 终止子从 IR 删除，lower 展开为 cg_ssa
  同构（SwitchInt + 合成 panic 块：Call panic lang item，BoundsCheck=[index,len,loc]、
  其余=`msg.panic_function()`+[loc]）。
- **内建补全**：abort intrinsic→HostAbort；assert_inhabited 系=
  `check_validity_requirement` lower 期判定；saturating_add/sub=IntSat；
  simd_reduce_all/any；**simd_shuffle=const 索引 lower 期读出展开为逐 lane 拷（零新
  IR）**；Cmp128（TypeId 判等）；dyn→dyn 同 principal 位拷。
- **os:: 最小直通**（panic 链逼出，M4.3 换正式注册表）：getenv/write/strlen/abort/
  **syscall（可变参按实参数分派）**；weak extern static（gettid）=判空 cell 写 0
  （guest 走 syscall fallback）；TLS `ThreadLocalRef` 单线程物化（**M4.4 义务：真线程
  时换 per-thread——代码已醒目标注**）。

### 经验与教训

1. **spike3 的"单条 native 栈零协调"兑现得比预想还干净**：Resume=返回、payload 只在
   raise/catch 两点接触、cleanup 里再 panic 穿出 Drop=宿主 double-panic abort 天然
   正确——unwind 的全部复杂度都被"宿主 unwinder 就是我们的 unwinder"吸收了。
2. **两个自递归陷阱**（都被深度守卫抓获）：① dyn place 的 Drop 调 `resolve_drop_glue`
   会解析回 `drop_glue::<dyn T>` 自身——**dyn drop 必须虚派发 vtable 槽 0**（cg_ssa
   同构；且槽可为 null=无 Drop 类型 → CallIndirect 加 null_ok）；② unsized place 的
   Drop glue 参数是**胖指针**（缺 meta 半 = ABI 越界）——resolve_place 的 meta 跟踪
   补上。
3. **track_caller 是 ABI 幻影参**，三处一致性缺一不可：普通 Call（requires 判定）、
   Virtual（也 requires——vtable 里 VTable shim 接收）、**fallback intrinsic（调用点
   必须先换 new_raw 再判**，否则 caller 按 Intrinsic kind=false 不传、callee 按 Item
   =true 期望收——`unchecked_funnel_shl` 用 ABI 越界教会我们 cg_ssa 的
   `IntrinsicResult::Fallback(instance)` 为什么要返回换过的 instance）。
4. **prologue 的实参计数校验值回票价**：ABI 不匹配从"神秘越界 panic"变成带函数名的
   一行诊断（防静默原则的又一处落地）。
5. **本 nightly 新漂移**（续前清单）：`Linkage::ExternalWeak`（extern block 内 item
   的 linkage 在 **import_linkage** 字段）；`ValidityRequirement::from_intrinsic`+
   `check_validity_requirement((req, PseudoCanonicalInput))`；`AssertKind::
   panic_function()`；`tcx.span_as_caller_location(span)` 一步给 Location 常量。
6. **spread_arg 在引擎自有调用约定下是 no-op**：caller MIR 传一个 tuple operand、
   callee 的 spread local 就是一个 tuple local，两侧按 ValKind 对称展平——native
   FnAbi 才需要按字段展开。自定义约定的又一红利。

### 本期遗留（归期明确）

- intrinsics::abort = SIGABRT（native=SIGILL trap）——信号级差异，差分若比信号再对齐。
- dyn trait 上溯（principal 变换的 vtable 槽读）、InvalidEnumConstruction assert
  （u128 实参）、128 位算术（比较已通）→ 按需。
- foreign 残余（clock_gettime/free/`llvm.x86.sse2.pause`/`__errno_location` 等）+
  weak cell 的 dlsym 真地址 + os:: 正式注册表（直通/内建/合成三处置）→ M4.3。
- TLS per-thread 化 + atomic fence 弱序复查 → M4.4。

## M4.3 os:: + FFI + main 启动链 —— **完成**（2026-07-09）

**Gate 全绿**：`tests/diff_vm.sh`——**全量差分 11/11 非线程用例**（fib/strings/args_env/
time_fs/ptr_int/hashmap/catch/panic_exit/async_hand/async_suspend/**ffi_libc**）经新引擎
**完整 main 启动链**跑出与 native 逐字节一致的 stdout + 退出码。threads_* 5 个 = M4.4
预期红；ecosystem/ffi_zlib 是 frontmatter/cargo 形态（引擎 cargo 接线 M4.5，diff.sh 同样
SKIP）。全量回归无损：gate0/gate1/gate2、tier-0 diff 16/16、spike1-5、纯度门禁。

### 建了什么

- **os:: 直通通用道（P7 处置①）**：`CallForeign{sym, ForeignSig}` 终止子 + engine
  `ffi.rs`（dlsym 缓存含缺席缓存 + `-l` dlopen 清单随 Module + libffi 直调，变参
  `Cif::new_variadic` 按调用点实参冻结尾参）。真实地址零编组：参数就是 u64 位。
  denylist（fork/exec/setjmp/pthread 生命周期类→Trap 带归期）+ stub 表（sigaction/
  sigaltstack/atexit/dl_iterate_phdr/_Unwind_Backtrace→假成功 0——回调装载挂 M4.4
  thunk）。tier-0 native.rs 的无 provenance 简化版。
- **extern static = 真符号**：非 weak（environ 等数据符号）= lower 期 dlsym 真地址；
  weak fn 符号维持判空 cell 0（真地址给出去会被 guest 当 fn ptr 调——条目反查失败；
  M4.4 thunk 后升级）。
- **weak 符号的链接器语义补全**：导出表记 is_weak；weak 且非 Rust 内部前缀
  （__rust/__rdl/rust_）且宿主有强符号 → 让位直通（compiler-builtins 的 weak
  sqrt/memcmp vs libm/libc——native 链接器行为的忠实仿真）。**Rust 内部符号绝不直通**
  （librustc_driver 也导出它们，直通=打穿引擎堆/panic 模型）。
- **main 启动链**（cg_ssa create_entry_fn 同构）：`EntryPlan{lang_start, main 条目地址,
  argc, argv(冻结区 C 串表), sigpipe}`；lang_start **照常解释**（sys::init/args 存放/
  hook/Termination 全走 guest 代码）；`--engine vm` 缺省即跑 main，退出码 = lang_start
  返回值。argv 布进冻结区（tier-0 setup_process_memory 同构）。
- **128 位整数补全**（f64 Display 的 ryu/grisu 逼出）：Bin128（宿主 u128 直算，读两半
  组→算→写两半；WithOverflow 旗标 @+16 布局核验）、128 位 IntToInt 双向（zext/sext
  拆两半、trunc 取低半）、128 位整数常量物化。ClosureFnPointer（resolve_closure
  FnOnce，cg 同构）。标量→同尺寸小聚合 transmute。
- **lower 的 panic 韧性**：语句/终止子级 catch_unwind——rustc API 的 panic 面（布局
  角例）兜成 Trap 占位带 panic 消息，绝不中止整个降低（Trap-stub 协议的完备化）。

### 经验与教训

1. **"asm 阻塞"是误诊，真凶是链接仿真的 weak 语义缺口**：ffi_libc 的 sqrt 被解析到
   compiler-builtins 的 **weak** sqrt（inline asm 实现）而非 libm 强符号。native 链接器
   的 weak 让位规则也是"链接器本来会做什么"的一部分——仿真补全后 asm 根本不在路径上。
2. **cg_ssa 的 Aggregate 只对 Adt 做 variant downcast**：Coroutine/Closure/Tuple 的
   operands（upvars）落**顶层** fields——coroutine 的 variant fields 是暂停点 saved
   locals，不是 upvars！错做 downcast = 空 fields 越界 panic。async 两 demo 因此解锁
   （coroutine Aggregate + 初始 variant 判别式）。
3. **u128 在 fmt 深处**：f64 Display（ryu/grisu）满地 u128 乘法/移位/溢出检查——
   "浮点格式化"实为 128 位整数算术的压力测试。宿主 u128 直算 20 行搞定。
4. **exit 直通天然正确**：guest std::process::exit → libc exit 直调 = 进程真退出，
   mimalloc/FrozenArena 由 OS 回收，无善后。
5. **变参 libffi 的正确姿势**：调用点实参类型已知 → 每调用点冻结完整签名 +
   `Cif::new_variadic`（x86_64 AL 寄存器语义 libffi 负责）——不需要 tier-0 的
   "拒绝变参"限制。
6. **本 nightly 漂移**（续）：`FnSig::c_variadic()` 是方法非字段。

### 本期遗留（归期明确）

- threads_* 全量差分 + weak fn 符号真地址 + 回调 thunk（signal/TLS dtor/
  dl_iterate_phdr 真实现）→ M4.4。
- 引擎 cargo/frontmatter 接线（runner 用 vm 引擎；ffi_zlib/ecosystem gate）+
  corpus 全绿盘点 + `.init_array` ctors（tier-0 有先例）→ M4.5。
- async demo 里两处小 Trap 残留不在执行路径（Repeat 非标量元素、dyn Error 上溯）→ 按需。

## M4.4 真线程 —— **完成**（2026-07-10）

**Gate 全绿**：`tests/m4_gate4.sh` 11/11——threads_* **5/5 差分 == native**（spawn/
channel/sync/time/panic，×3 复跑稳定）+ **tier-0 时代挂死双场景通过**（c_blocking_io
0.8s、c_net_echo_threaded 0.7s——真阻塞 syscall 只挡自己）+ **c_rayon 0.9s**（tier-0
28s → 秒级目标达成；work-stealing 池/par_iter/par_sort 全通）+ threads demo 可达
trap-free + **TSan 多线程真身零警告**（8 线程共享 Shared/各自 Ctx/thunk 工厂并发，
`engine/tsan_mt.rs`）。全量回归无损：diff.sh **16/16**（基线从 11 升 16）、gate0/1/2、
纯度门禁、spikes 5/5、diff_cargo（ffi_zlib）。

### 建了什么（对照设计 docs/m4.4-design.md）

- **step 0（Shared 'static + 边界 TLS attach）**：cli Box::leak 提升；run_main/
  run_export/thunk 一条 attach 路（commit 1772162）。
- **D1 thunk 工厂**（`engine/thunks.rs`，本期唯一新机制）：CallForeign 的 fn-ptr 实参位
  （裸 fn ptr + `Option<fn>`——niche 下 None=0 直传）lower 期冻结内层 ForeignSig；执行期
  条目地址（fn_addrs 反查命中）→ `get_or_create` 物化 libffi Closure 真码（缓存键 =
  (条目地址, 签名)，Mutex；Closure/ThunkData leak 进程级）；NULL 与已是 native 真码
  原样直传。trampoline = 边界 TLS attach → 按签名搬参 → interp_frame → 返回值写回。
- **D2 denylist**：移除 pthread_create/join/detach（join/detach 纯直通，tid 真
  pthread_t——into_pthread_t 教训终局）；保留 pthread_exit/atfork/fork/exec/setjmp。
- **FFI 反方向之二（设计外，实测逼出）**：`CallIndirect.native_sig`——extern "C" 系
  fn-ptr 调用点冻结签名；条目反查未命中 = guest 持**运行期 dlsym 所得真码**（std
  `min_stack_size` 的 `__pthread_get_minstack`）→ `ffi::call_addr` 按签名直调。
- **D3 guest TLS per-thread**：`ThreadLocalRef` → 稠密 TlsId + 模板物化冻结区（复用
  ensure_alloc，重定位白拿）；`Rvalue::TlsRef` 执行期 Ctx.tls 惰性物化（heap 分配 +
  模板拷贝）。**真 TLS dtor（设计"收尾可选块"提前进主线，threads_panic 需要）**：
  Ctx 从宿主 thread_local 改**自管 pthread key + 迟退 3 轮**（dtor 里 setspecific 挂回，
  glibc 上限 4 轮）——guest 的 run_dtors（pthread_key_create dtor 实参经同一 thunk 工厂）
  跑在 TSD 相位时 Ctx 必然还活着。宿主 thread_local 不可行：C++ TLS 析构相位**先于**
  TSD 相位，dtor thunk 内 attach 必撞已销毁宿主 TLS。
- **D4 fence 补真**：atomic_fence → 宿主 fence(SeqCst)、singlethreadfence →
  compiler_fence(SeqCst)（M4.2 nop 复查义务兑现）。
- **rust-call ABI 真协议（本期最大意外收获）**：物理约定 = tuple **按字段展平**
  （cg_ssa/Miri 同构）。调用点 untuple 尾参（含常量 tuple：ZST 跳过 / 整体标量映射唯一
  非 ZST 字段 / pair 按偏移认领字段 / Indirect 常量落冻结区照常投影）+ shim body 的
  spread_arg tuple local 按字段展开为多参数。**此前单参闭包靠 (A,) 与 A 布局巧合蒙混过
  M4.1-4.3 全部 gate**；双参闭包（spawn 链的 map_try_fold/LocalKey::set 闭包）一来就炸。
- **by-value dyn 派发**：`Box<dyn FnOnce>::call_once` 内 `F::call_once(move (*self))`
  ——receiver 是 unsized dyn place：data = place 真地址、callee 经 place meta 查槽
  （cg_ssa `Ref(PlaceValue{llextra:Some})` 臂同构；槽内是 `ShimKind::VTable` shim，
  收 `*mut Self` 瘦指针再 move 出，shim 侧 MIR 自动正确）。
- **真线程逼出的 M4.1 遗留清偿**：Subslice 投影（数组折常量；slice 用新
  `Operand::SubImm` 表达 len−k）、Repeat 聚合元素（写 dst[0] + `Stmt::RepeatBytes`
  铺满）、volatile_load/store（解释器不消除内存操作 = 普通存取）、结构体尾字段 unsize
  （`struct_lockstep_tails_for_codegen`，rayon 的 PolymorphicIter）。
- **D6 gate 载体**：`tests/m4_gate4.sh`（差分×5 + 双场景 + rayon 20s 硬门 + vm-stats +
  TSan）；`engine/tsan_mt.rs`（手构 Module：解释态原子自增 + thunk 并发同键同码 +
  跨线程调 thunk 再入）。设计里"runner 加 MIRVM_ENGINE 透传"已无必要——tier-0 移除后
  runner 本就是 vm 引擎。

### 经验与教训

1. **设计的"步 1 gate = threads_spawn 绿"过于乐观**：子线程一起跑就要 set_current
   （TLS），threads_spawn 实际需要步 1+2 齐活；threads_panic 需要真 TLS dtor（设计 D3
   "五用例不依赖 dtor"判断有误——threads_panic 就是专测线程内 TLS Drop 的）。教训：
   **设计期用 --vm-stats 看债务表之外，还要读 gate 用例源码本身**。
2. **rust-call 的"spread_arg 在自有调用约定下是 no-op"假设是错的**（M4.1 注释）：
   闭包**本体**的 MIR 参数天然已拆开（env, a, b），shim body 才是 (self, tuple)+
   spread_arg——两侧形状不同，"对称展平"不成立。单参闭包的布局巧合掩盖了三期。
   横切面 ABI 的老教训再+1：**一次做全，别赌巧合**。
3. **TSan 线程态在 TSD dtor 相位前已析构**：任何插桩代码（哪怕空函数的
   __tsan_func_entry）跑在 pthread key dtor 里 = SEGV 读已亡 trace 状态。处置：
   `cfg(sanitize = "thread")` 下 Ctx 不注册 dtor（每线程泄漏，仅测试配置）。**推论：
   TSan 通道跑"guest 注册 TSD dtor"的场景永远不可行**——挑 TSan 用例时避开。
4. **guest 调 native fn ptr 是双向 FFI 的另一半**：dlsym 直通给了 guest 真码地址，
   guest 迟早会调它。thunk（guest→native 逃逸）+ native_sig（native 真码回调）配对
   才闭环。std 里 dlsym! 宏一处就逼出（__pthread_get_minstack，GLIBC_PRIVATE）。
5. **迟退 N 轮是控制 TSD dtor 相对顺序的标准技巧**：键序不可控，但 glibc 多轮扫描
   （PTHREAD_DESTRUCTOR_ITERATIONS=4）+ dtor 里重新 setspecific = 把自己排到别人后面。
6. **本 nightly 漂移**（续）：`InstanceKind` 重构为 `Item/Intrinsic/Virtual/Shim(ShimKind)`，
   VtableShim → `ShimKind::VTable`（unsizeable self 的 vtable 槽 shim，收 `*mut Self`）；
   `struct_lockstep_tails_for_codegen(src, dst, env)` 取 unsize 尾对。
7. **诊断先行省时间**：prologue 实参槽数先验（带 fn 名/期望/实收）+ 间接调用未命中带
   调用者名——两个诊断强化把三个 ABI bug 的定位从"盲猜"变"读一行"。

### v1 记账（成本已核，非缺陷）

- **guest TLS 实例块泄漏**（每线程每 TLS 一小块）：dtor **副作用**经 run_dtors thunk
  正确执行（threads_panic 验证），但块本身不随线程回收——主动 free 有 UAF 风险
  （guest dtor 轮次可晚于我们的收尾轮，错值比泄漏贵）。长驻多线程服务的累积成本挂
  M4.5 盘点。
- **TSan 配置下 Ctx 泄漏**（见教训 3，仅测试配置）。
- **guest 线程栈大小语义近似**：解释帧消耗在宿主线程栈（~1KB/帧），guest 指定
  stacksize 直通生效但"能递归多深"与 native 不同（2MB 默认栈 ≈ 2000 解释帧 <
  MAX_DEPTH=8000——深递归小栈线程可能先撞真栈）。精确化挂 M5（编译帧更浅）。
- **signal = 裁定 A**（推荐项，stub 维持假成功）：threads_*/双场景/rayon 均不依赖；
  真装载挪 M4.5 前（thunk 工厂已就绪，增量 = 从 stub 表移到直通 + handler thunk）。
  用户如裁 B 随时可改。

### 本期遗留（归期明确）

- 引擎 cargo/frontmatter 接线完善（diff_cargo 的 script/project 预期红恢复）+ corpus
  全绿盘点 + 性能硬门 + `.init_array` ctors + async 收口 + signal 真装载（若需）→ M4.5。
- weak fn 符号真地址化（thunk 就绪后可升级；现维持判空 cell）→ 按需。
- dyn 上溯 vtable 变换 / unsized→dyn（async demo 不可达残留）→ 按需。
