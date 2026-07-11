# M5 设计：JIT —— asm 三面孔清零 + 方法级 Cranelift 加速（待审）

> 状态：**待审阅**（2026-07-11，过审期，开工前请用户过目）。
> 前置：M4 全期关账（gate5 31/31，总验收四条对勾）；spike5 真 Cranelift 全过
> （i2c/c2i/cc→cc 直调、P/R 两约定、eh_frame 自注册后 unwind 穿真 JIT 帧）。
> 数据来源：本日实测（三面孔 asm 的逐字形状、宿主 CPU 特性、模块规模）+ 逐文件
> grep 本 nightly 的 cg_clif 与 cranelift-codegen 0.133.1 真源码（绝不凭训练知识）。

## 0. 一句话

M5 = **两条独立的轨**在一个共享机件（asm-stub 工厂）上交汇：
**轨 A（语义完备性）**——asm 三面孔 + 静态归档装载 + SIMD 补面，把 corpus/diff_cargo
剩余全红 5 用例清零，"全绿 − asm"的脚注从此删除；
**轨 B（性能）**——方法级 Cranelift JIT（惰性 tiering），把 fib 类热代码从解释器
~120-150× 拉到个位数×。两轨可独立验收，轨 A 不依赖方法级 JIT。

## 1. 调研数据（2026-07-11 实测 + 源码核对）

### 1.1 三面孔 asm 的真实形状（逐字捕获）

| 面孔 | 用例 | 实测 asm（TRAP 诊断逐字） |
|---|---|---|
| cpuid | blake3/sha2/ecosystem | `asm!("mov {0:r}, rbx\n cpuid\n xchg {0:r}, rbx", ...)`（std_detect/cpufeatures 的 rbx 保存模式） |
| 裸 syscall | tempfile | `asm!("syscall", inlateout("ax"), in("di"), in("si"), lateout("cx") _, lateout("r11") _, options(PRESERVES_FLAGS\|NOSTACK))`（rustix syscall2；syscall0..7 一族同形） |
| 算术 div | numbigint | `asm!("div {0:r}", in(reg), inout("dx"), inout("ax"), options(PURE\|NOMEM\|NOSTACK))`（div_wide） |

**三张面孔全是小型纯寄存器块**：显式寄存器 + reg 类操作数 + clobber，无内存操作数、
无 sym、无 label——完全落在"wrapper 包装"可覆盖的形状内（§D1）。

### 1.2 cg_clif 先例核对（本 nightly 树内源码，关键事实修正）

写码前逐文件读了 `rustc_codegen_cranelift`（rustc-src 树内）。四个发现：

1. **inline asm 的机制事实修正**：corpus §2.2 决策注记写过"Cranelift 有 inline-asm
   降低"——**此说不确**。Cranelift 本身没有任何 asm 文本能力；cg_clif 的做法是
   （`src/inline_asm.rs`，~900 行）：自己给操作数做**寄存器分配**（显式寄存器优先、
   reg 类从 `rustc_target::asm::allocatable_registers` 池分配）、生成一个**GAS 文本
   wrapper 函数**（原型 `fn(*mut u8)`——单指针指向槽缓冲；prologue 存 clobber 寄存器
   →从缓冲装输入寄存器→模板本体→回存输出→恢复 clobber）、拼进 global_asm 文本、
   **经外部汇编器**（`as` 或 rustc+LLVM 后端）汇编成对象。**决策不变**（"asm 块本身
   就是机器码，直接汇编执行"），但机制 = wrapper+外部汇编器，不是 Cranelift。
2. **cg_clif 的 JIT 模式根本不支持 inline asm**（`driver/jit.rs:169` 直接 fatal）。
   mirvm 要走的比先例远一步：AOT 侧的 wrapper 生成器可整套同构借用，**运行期装载
   要自己补**——出路是汇编成 `.so` 后 dlopen（§D1），管线与既有 `native_libs` 同族。
3. **JIT 帧 LSDA/landing pad 有完整树内先例**（`unwinding` cargo feature，默认关）：
   带 cleanup 的调用发 `try_call` + `ExceptionTableData`（`abi/mod.rs:865-940`）；
   `UnwindResume` → 直接 libcall **`_Unwind_Resume`**（= 宿主 unwinder 同轨，正是
   spike3 路线）；每函数从 `compiled_code().buffer.call_sites()` 取
   `FinalizedMachExceptionHandler::Tag(cleanup_tag, landing_pad)` 生成
   **GccExceptTable**（cg_clif 自带 ~270 行写出器）挂到 FDE.lsda，CIE.personality =
   `rust_eh_personality`；且有 **`register_jit`**（JIT 模式带 LSDA 注册，
   `debuginfo/unwind.rs`）。M5 的 unwind 工作 = 把 spike5 的 CFI-only 注册管线扩展
   一层 LSDA+personality，同构源完整在树。
   ——一处**不照抄**：cg_clif 的 register_jit 在 Linux 上整段 `__register_frame`
   （libunwind 语义假设）；spike5 实测本机是 libgcc 逐 FDE 语义。**沿用 spike5 的
   逐 FDE + CIE 判别注册，不抄 cg_clif 这行。**
4. **异类 SIMD 指令 cg_clif 也用 asm wrapper 兜底**：`llvm.x86.sha256rnds2`、
   `llvm.x86.aesni.aesenc` 等 Cranelift 无对应 IR 的指令，cg_clif 直接构造
   `InlineAsmTemplatePiece::String("sha256rnds2 xmm1, xmm2")` 走同一 wrapper 机制
   （`intrinsics/llvm_x86.rs:944,1164`）。**asm-stub 工厂天然是 SIMD 异类指令的
   兜底通道**——一套机制两类客户（§D7）。

版本核对：cg_clif 钉 cranelift **0.132**，mirvm 用 **0.133.1**——已 grep 0.133.1
真源码确认 `try_call`/`ExceptionTableData`/`FinalizedMachExceptionHandler`/
`call_sites()` 全部在位（`ir/exception_table.rs`、`machinst/buffer.rs`），API 无碍。
另：0.133.1 带 `inline.rs`（Cranelift 自有内联器）——性能收口期的备用杠杆。

### 1.3 blake3 的第二层：静态原生归档（新债类，被调研逼出）

blake3 crate（1.8.5，corpus 默认 features）**不走 Rust intrinsics**：build.rs 把
`c/blake3_{sse2,sse41,avx2}_x86-64_unix.S` 编成**静态归档**，Rust 侧是
`extern "C"`（src/ffi_avx2.rs 等）。即：cpuid 修好后，blake3 下一步是调
`blake3_hash_many_avx2` 这类**guest build-script 产物静态库符号**——dlsym 不到
（.a 不进进程）。这是一类通用债（ring/zstd-sys 等 cc 编译静态库同族），处置见 §D2。
sha2（0.10）与 ecosystem（regex/memchr）则是纯 Rust intrinsics 路径（§D7 的客户）。

### 1.4 宿主与规模

- **宿主 CPU**：AMD EPYC 7773X（Zen 3）——**有 avx2/sha_ni/vaes/pclmulqdq，无
  AVX-512**。"虚拟 CPU = 真宿主 CPU"下，五个红用例在本机的真实内核选择：blake3→
  AVX2（.S 归档）、sha2→SHA-NI（intrinsics）、memchr/regex→SSE2/AVX2（intrinsics）。
  差分对拍与 native 同机同特性，天然一致。`cranelift_native::builder()` 自动继承
  宿主特性，与该决策同轨。
- **模块规模**（fib.rs 实测）：worklist 闭包共 **3027 个 instance**，入口可达 361；
  加载相 413ms。→ **eager 全量 JIT 不可行**（3027 × ~0.3-1ms ≈ 1-3s，破加载 ≤1s
  硬门），tiering 必须惰性（§D4）。
- **性能锚点**（M4.5 记录）：fib(32) 解释 0.94s vs native -O 8ms ≈ **~120×**
  （m4-log ★ 节记 ~150×，两次测量口径差；取 0.94s 为基线数字）。spike5 微基准：
  真 Cranelift fib(30) 直调距 native 2.1-2.2×。

## 2. 总体结构：双轨 + 一个共享机件

```
 轨 A 语义完备性（corpus 清零）              轨 B 性能（方法级 JIT）
 ├─ M5.0 asm-stub 工厂 ◄──共享──┐          ├─ M5.2 JIT 骨架 + tiering
 ├─ M5.1 静态归档装载            │          ├─ M5.3 翻译器全覆盖 + LSDA
 │       + SIMD 补面 ───────────┘          └─ （M5.4 收口时合流计时）
 └─ gate：corpus 全绿、diff_cargo 3/3       └─ gate：fib ≤10×、全量回归 JIT-on 无损
```

- **轨 A 不依赖方法级 JIT**：asm-stub 是加载相物化的真机器码，解释器按槽缓冲 ABI
  直接调（corpus §2.2 当年的预判："即便解释器，也可以对单个 InlineAsm 汇编+调用，
  一个自足小 JIT"——正是本设计）。好处：corpus 清零不被 JIT 工程量挡路，且 JIT
  关掉时语义面完整（JIT 纯加速器，语义零耦合）。
- **轨 B 的正确性护栏是新 oracle**：同一引擎两个 tier——**JIT-on vs JIT-off 差分**
  跑全量 demo/corpus。JIT 误编译显形为两 tier 分歧，比对拍 native 更早、定位更准。

## 3. 关键决策（请审）

### D1 asm-stub 工厂：加载相物化 + 解释器可直调（cg_clif wrapper 同构）

- **lower 侧**（rustc_private 域，可用 `rustc_target::asm`）：遇 `TerminatorKind::
  InlineAsm` 不再降 Trap，而是：cg_clif 同构的寄存器分配（显式寄存器优先、类操作数
  从 allocatable_registers 池取、跳冲突）→ 渲染 GAS wrapper 文本（`fn(*mut u8)` 槽
  缓冲 ABI：clobber 保存→装输入→模板→回存输出→恢复）→ 记录每操作数的缓冲槽偏移。
  IR 新增 `Terminator::InlineAsm { stub: StubId, ins: Vec<(off, Operand)>,
  outs: Vec<(off, 落点)>, buf_size, target }`。
- **物化**（加载相末尾，一次批量）：全部 wrapper 拼一个 .s → `cc -shared` 汇编成
  .so → dlopen → 逐 wrapper dlsym 回填真地址。**按内容哈希缓存**
  `~/.cache/mirvm/asm-stubs/<hash>.so`（与 sysroot 缓存同族）——热路径零 cc 调用，
  冷路径一次 ~50-100ms，在加载 ≤1s 硬门内。
- **引擎侧**（纯 Rust 不变）：执行 = 栈上开 buf_size 缓冲、按 ins 写入、call 真地址、
  按 outs 读出。JIT 帧里同一 stub 直接 native call（cg_clif call_inline_asm 同构）。
- **v1 支持面**：in/out/inout/lateout × 显式寄存器/reg/xmm 类 + const + options
  （PURE/NOMEM/PRESERVES_FLAGS/NOSTACK 等只是优化提示，wrapper 一律保守执行）。
  **sym/label（asm goto）/非 x86 = Trap-stub 保留诊断**（三面孔用不到；label 连
  cg_clif AOT 也 fatal）。may_unwind 不支持（cg_clif 同款 FIXME）。
- 依赖记账：加载相新增外部工具 `cc`（仅冷缓存时；与"corpus 本就要 cc 链 native
  oracle"同一环境假设）。
- 【备选：进程内汇编器 crate——x86 GNU 语法完整文本解析器无成熟纯 Rust 实现，
  自写 = 重新发明汇编器，不取】

### D2 静态原生归档装载：.a → .so 一次转换 + dlopen

blake3 逼出的通用债：guest 依赖树里 build-script 产物 `-l static=xxx`（cc 编译的
.S/.c 静态归档）。处置：lower 收集链接集时**补上 `tcx.native_libraries(cnum)`**
（目前只收 CLI `-l`，src/lower/mod.rs:624），对 Static 类在搜索路径找 `lib<name>.a`
→ `cc -shared -Wl,--whole-archive` 包成 .so（内容哈希缓存，同 D1）→ 进既有
`native_libs` dlopen 清单 → CallForeign 的 dlsym 既有路径直接命中。真实地址模型下
零编组直调（与 ffi_zlib 无差别）。
【备选：自写 .a 解析+重定位装载器 = 重新发明 ld，不取；对 corpus 换 blake3 的
prefer_intrinsics feature = 回避真实默认形态、且丢掉这类通用债的解决，不取】

### D3 JIT 输入 = 引擎字节码（不是 MIR）

翻译器吃 `ir::FuncBody`（布局/偏移/调用目标/判别式编码全已冻结、宽度自描述），
不回头吃 MIR。理由：① **架构纪律**——JIT 在执行相（运行期编译），tcx 永不出境；
② **mode B 白拿**——.mirvm 分发件（无 tcx 环境）同一翻译器直接可用；③ 字节码
"贴近 MIR"的既定设计（frame-abi §7.5）本就为此——cg_clif 的 MIR→CLIF 是**知识
来源**（ABI/语义对照），不是代码来源。
翻译要点：**两类局部**——从未取址的标量槽提升为 Cranelift SSA 变量（def_var/
use_var，mem2reg 在翻译时白拿，fib 类代码质量回到 spike5 档位）；其余落
Cranelift 栈槽（真地址取址照常成立，帧局部在编译帧自管——frame-abi §2.1 既定）。

### D4 tiering：惰性 + 调用计数 + 后台编译线程；JIT↔interp 差分是新 oracle

- **单一派发点**：新增 `call_guest(ctx, FuncId, &[u64]) -> (u64,u64)` 收拢现有六处
  interp_frame 调用（Call 终止子/CallIndirect/thunk 蹦床/run_main/run_export/
  call_fn_addr=CatchUnwind 回调）——查 per-func 原子 code-ptr 表：有码走 i2c，无码计数+解释，
  过阈值（v1 定 1000，可调）入队后台编译线程，完成 Release 发布、调用方 Acquire 读。
  thunk/CallIndirect 零改动自动受益。
- **编译服务线程**：单线程持 JITModule（批量 finalize；代码内存随模块常驻——
  cranelift-jit 不支持逐函数释放，进程生命周期记账）。发布协议只有一个原子指针
  交换，简单到不需要 TSan 佐证（TSan 通道本就不编 cranelift feature，既有纪律）。
- **无 OSR、无去优化**：调用边界整方法编译（frame-abi §3.2 既定）；我们从冻结
  字节码直译、**零投机假设 ⇒ 零 invalidation 需求**（比 HotSpot 简单一个量级的
  结构性红利，值得点名）。长跑单循环 main 不触发编译（无调用边界）——记账接受，
  fib 锚点是递归调用形态，不受影响。
- **CLI**：`--jit off|on`（默认 on，阈值可调）。gate 全量跑两遍（on/off 差分）。

### D5 调用约定与 vmctx 终裁：分层——骨架 T（M5 落地）+ R 为兼容缓存层（触发器明确）

> 2026-07-11 审阅修订：初稿的 T 论证部分依赖**当前实现状态**（分配走 Builtin 出线
> 调用、TlsRef 走助手），用户指出这不是理论判断。本节按"哪些前提是架构承诺、哪些
> 只是实现现状"重推，结论从"T vs R 二选一"修正为**分层**：T 是骨架，R 是 ABI 兼容
> 的缓存升级层——需要 hardwire 进 M5 的部分恰好全是两案共享的。

**每编译函数两个入口**（与终裁无关——T/R 两案在此完全一致）：
- **packed 入口**（i2c 用，统一形状）：`extern "C-unwind" fn(args: *const u64,
  ret: *mut [u64;2])`——prologue 按冻结 ParamAbi 拆槽装寄存器。解释器→编译码
  一跳直达，任意元数无需 per-shape transmute。
- **fast 入口**（cc→cc 用，native 约定）：**纯 guest 签名**——Scalar→i64 寄存器参、
  Pair→双参/双返、Indirect→指针参 + sret 指针，track_caller 尾参照常。注意
  spike5 里 R 变体的 fast 签名 `(n)->r` 与 T 完全相同（R 的 ctx 在 r15 不在签名）；
  只有 P（显式首参）签名不同——P 被签名错位（vmctx-passing §2 的 thunk 遍地问题）
  + spike5 vcode 实证的 threading 税双重淘汰，本设计不再保留。编译码间调用**经
  函数表间接**（PLT 式：表槽初值 = 该函数的 c2i 小蹦床，编译完成后原子换成 fast
  入口——调用点恒定"load 表槽 + call reg"，无分支、无 patch）。红利（两案同享）：
  逃逸指针热身后可直接给 fast 入口地址（vmctx-passing §5.1 的 thunk 消失路径）。

**理论重推：编译码触碰每线程执行态的点，按"扛不扛得住实现演化"三分**：

| 类 | 内容 | 判据性质 |
|---|---|---|
| ① 结构性不存在 | GC 写屏障、safepoint 轮询、搬迁式 TLAB bump、栈增长检查、线性内存基址 | **架构承诺，不随实现变**：无 GC（DESIGN 托管非搬迁）、native 栈（模型 A）、真实地址（C2）。HotSpot r15 / Go g / Wasmtime vmctx 的存在理由**逐条**落在此格——先例的"为什么"映射到 mirvm 全为空 |
| ② 语义上就是助手形状 | FFI（dlsym+libffi）、c2i（热身期）、catch_unwind（宿主 catch）、panic 簿记 | 助手自身重量级，ctx 获取摊销为零头——**与实现无关恒成立** |
| ③ 内联后可能变热 | 分配快路径内联（D3 的"hand-rolled TLAB 后置"项）、guest TLS 快路径内联 | **唯一随实现演化的格**（审阅问题的实体）。M5 范围内此格为空：分配走 Builtin、TlsRef 走助手，M5 编译码零站点 |

对格 ③ 的理论上界（即便内联落地）：T 的成本 = 每个**使用 ctx 的激活**一次
initial-exec TLS load（入口提升、寄存器携带全函数体；tpoff 常数 JIT 时已知——
vmctx-passing §3.2 既有机制。attach 时同步写一个 `#[thread_local]` POD 镜像即可：
无析构故 TSD dtor 相位仍可读，与 M4.4 相位教训兼容）。R 省掉这一条 load 的租金 =
**全程征用 r15**（spike5 自己标注"寄存器压力面未测"的风险项）+ 每边界入口
save/set/restore + per-arch 选寄存器。生产对照：mimalloc/tcmalloc 的快路径本身
就是"TLS load + 免锁链表"形态跑 ~10ns 级，CoreCLR x64 不保留线程寄存器（线程
静态量/分配上下文走内联 TLS 序列）——**"TLS load 的每线程快路径"是工业标准形态，
不是性能妥协**；而 Rust 负载的分配密度又远低于 Java/Go（值类型为主）。

**结构性事实（把"判断翻了"的代价钉死）**：T 与 R 不是岔路，是分层——fast 签名
相同、边界 TLS attach 相同（M4.4 已落地且是被逼定的，见 vmctx-passing §1）、
助手协议相同；全部差异收敛为 (a) `enable_pinned_reg` ISA 旗标 (b) 翻译器里
"取 ctx"的降低方式（TLS load vs `get_pinned_reg`）(c) 边界入口是否包
save/set/restore（spike5 已验代码形状）。**T→R 是 ABI 兼容的单开关升级，不是
重设计**。翻译器把"取 ctx"收拢为单缝（`get_ctx()` 一处），开关就位。
诚实条款：分层 ≠ 同进程逐函数混用（T 码体内 r15 是普通 callee-saved 临时、R 码
假设 r15≡ctx，跨制度调用需包装）——翻转粒度 = 整个 JIT 代码缓存按新制度重编，
而这免费：代码缓存是进程内易失物、mode B 分发字节码非机器码，无跨进程兼容面。

**终裁形式**：M5 落 T 骨架（M5 编译码格 ③ 为空，真负载测不出 T/R 差——这本身
就是"不该预付寄存器租金"的数据）；挂起检查点**不结死，改写为带触发器的活检查点**
（写回 vmctx-passing §7）：格 ③ 进场（分配内联 / guest TLS 内联开工）时，以**该
负载**复测 T vs R 再裁缓存层——彼时才存在能区分两案的 workload。M5.4 照做
fib/rayon 计量 + 助手调用频度统计，作为触发器复测时的对照基线。
【备选：R-first——若预期分配内联很快进场、愿意先付寄存器租金与边界机件，可直接
落 R（spike5 全套已验）；因骨架共享，两案工程差异很小。请裁】

### D6 JIT 帧 unwind：eh_frame（已验）+ LSDA/landing pad（cg_clif 同构）；准入过渡

- **CFI 半边**：spike5 管线原样产品化（create_unwind_info → gimli FrameTable →
  逐 FDE `__register_frame`，CIE 判别字段）。
- **LSDA 半边**（新）：带 cleanup 边的调用发 `try_call`+异常表（cg_clif
  abi/mod.rs 同构）；cleanup 块内 Drop 照常降调用、`Resume` → libcall
  `_Unwind_Resume`；每函数 GccExceptTable（cg_clif 写出器同构移植）挂 FDE.lsda，
  CIE.personality 指进程自带 `rust_eh_personality`（mirvm 链 std，符号链接期可取）。
  **guest 异常与宿主 panic 同 personality 同轨**——spike3"宿主 unwinder 就是我们的
  unwinder"在 JIT 帧的延伸，catch_unwind 仍走宿主 Builtin（编译帧调它=普通调用，
  无需 LSDA-catch，只用 cleanup 标签）。
- **准入过渡**：M5.2 骨架期先只收**unwind-transparent 函数**（无 cleanup 边——
  纯穿透已被 spike5 验证），fib 锚点即达；M5.3 先跑**LSDA probe**（扩展 spike5
  probe：JIT 帧内带 Drop 义务的 cleanup 在 guest panic 时执行、顺序与 native 一致）
  再铺全量。probe 不过（预判会过，先例完整）则准入限制转为 v1 记账、LSDA 挂 M5.x
  ——热点数值内核多为无 cleanup 函数，轨 B 的性能 gate 不被绑架。

### D7 SIMD 补面：常见 → CLIF 向量（cg_clif llvm_x86 同构）；异类 → asm-stub 兜底

cpuid 返真后，sha2/ecosystem 的内核是纯 Rust intrinsics（§1.3），两条通道处置：
- `simd_*` 泛型与常见 `llvm.x86.*`（loadu/cmpeq/movemask/set1/pshufb/palignr 一族，
  memchr/teddy/sha2 消息调度所需）：解释器扩既有 SIMD 最小集（SimdBin/Splat/
  Bitmask 加 32 字节 lane 档），JIT 翻译成 CLIF 向量 op（cg_clif llvm_x86.rs
  79 个映射为同构源）。
- **异类指令**（sha256rnds2/sha256msg1/2、aesenc 等）：**直接走 D1 asm-stub**——
  cg_clif 在树先例就是这么兜的（§1.2-4），零新机制。lower 对这类 foreign 调用
  按名合成一个单指令 asm 站点即可。
- 面的边界由数据划：以五个红用例真实触发为准逐个补，未触发的不预支
  （M4.5 "128 位 niche 可达性被真数据检验"的教训反着用：先留门，实测触发再补）。

## 4. 分期施工（每期独立 gate，轨 A 先行）

| 期 | 内容 | Gate |
|---|---|---|
| **M5.0 asm-stub 工厂**（轨 A） | D1 全套：lower 寄存器分配+wrapper 渲染、批量 cc+dlopen+哈希缓存、IR 终止子、解释器槽缓冲执行 | **tempfile + numbigint 绿**（syscall/div 两面孔）；cpuid 站点跑通（blake3/sha2 推进到下一层实测）；全量回归无损 |
| **M5.1 归档装载 + SIMD 补面**（轨 A 收口) | D2 .a→.so + native_libraries 收集；D7 数据驱动补面（sha2 SHA-NI stubs、memchr/teddy 所需 lane 档） | **corpus 25 项全绿（零 asm 例外）+ diff_cargo 3/3（ecosystem 绿）**——M4 脚注删除 |
| **M5.2 JIT 骨架**（轨 B） | D4 派发/计数/编译线程；D3 翻译器标量子集（int/float/place/call/switch/SSA 提升）；D5 两入口 + PLT 表；CFI 注册 | **fib(32) ≤ 10× native**（硬门，锚点 0.94s→≤80ms）；diff 16/16 JIT-on/off 双跑全绿；加载 ≤1s 不破 |
| **M5.3 翻译器全覆盖 + LSDA** | 先 LSDA probe（D6）再铺：try_call/GccExceptTable/personality；IR 全构造翻译（128 位/原子/SIMD/foreign 助手/track_caller）；准入放开 | gate2 unwind 九用例 JIT-on 通过；**全量（demo/corpus/diff_cargo）JIT-on == JIT-off == native** |
| **M5.4 终裁 + 收口** | D5 计量基线（fib/rayon + 助手频度）+ 检查点改写为带触发器活检查点（回写 vmctx-passing §7）；rayon/corpus JIT-on 计时记账；`tests/m5_gate6.sh`；m4-log 式 M5 条目 + handoff/memory 收笔 | gate6 全绿（下方退出判据）；vmctx 检查点处置有数据有触发器 |

## 5. 风险与缓解

- **wrapper 寄存器分配的正确性**（D1 最尖风险——错一个 clobber = 静默错值）：
  cg_clif allocate_registers 逐行同构 + 每面孔最小重现单测（cpuid/syscall/div 的
  已知输入输出）+ 差分把关；诊断先行（stub 生成期打印分配表，防静默错值纪律）。
- **LSDA 是最大未知**（0.133.1 的 try_call 比 cg_clif 钉的 0.132 新，JIT 模式
  personality 路径无人踩过）：probe 先行 + 准入限制兜底（D6），轨 B gate 不被绑架。
- **SIMD 面撑大**（sha2/memchr 实测触发面超预估）：异类全可退 asm-stub（单指令
  wrapper 机械生成），面大 = 站点多而非机制多；每期 gate 卡真实用例不卡完备性。
- **编译线程与发布的并发正确性**：协议收窄到单原子指针交换 + 只读代码；TSan 通道
  维持 interp-only（feature 门既有纪律），JIT 侧靠 on/off 差分与协议极简保正确。
- **加载硬门回归**（cc 冷调用 + 收集面扩大）：全部产物内容哈希缓存；gate6 保留
  加载 ≤1s 上限项。

## 6. 不做（本期）

OSR / 去优化 / 内联（0.133.1 自带 inline.rs，记为收口期备用杠杆，默认不开）/
asm sym·label 操作数 / may_unwind asm / 非 x86_64 后端 / mode B（.mirvm 分发，
M5 后另开，D3 已为它留好形状）/ signal 真装载·weak fn 真地址·guest TLS 回收·
.init_array（按需清单不动）/ 编译码栈溢出优雅化（编译帧撞 guard page = 裸 SIGSEGV，
与 native 差一条报错消息——记账，随"线程栈精确化"按需项走）。

## 7. 退出判据（gate6 = tests/m5_gate6.sh）

① corpus 25 项**全绿零例外** + diff_cargo **3/3**（asm 预期红清单从 gate 删除）；
② fib(32) JIT-on **≤ 10× native**（记录实测倍数入 m4-log M5 条目）;
③ 全量差分三重一致：JIT-on == JIT-off == native（diff 16/16 + gate0-5 + corpus）；
④ 性能上限无回归：加载 ≤1s、rayon ≤5s（JIT-on 计时另记账）；
⑤ TSan 零警告（interp 通道）+ spikes 回归 + 纯度门禁；
⑥ vmctx 挂起检查点处置落笔：T 骨架数据基线 + R 缓存层触发器（分配/guest-TLS
   内联进场时以该负载复测），写入 vmctx-passing.md §7；
⑦ m4-log M5 条目（gate 结果+教训+遗留归期）+ handoff/memory 收笔。
