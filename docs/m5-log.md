# M5 施工日志（JIT —— asm 清零 + 方法级 Cranelift 加速）

> 承 `docs/m4-log.md`（M4 关账）。设计见 `docs/m5-design.md`（D1-D7 已批准，
> D5=T 骨架+触发器）。每期 gate 结果 + 教训 + 遗留归期，格式同 m4-log。
> **状态边界**：M5.0、M5.1 已完成；方法级 JIT 未进入产品路径。2026-07-12 对测试
> oracle 的后续审计见 [current-status.md](current-status.md)，旧 gate 计数按当时脚本口径保留。

## M5.0 asm-stub 工厂（轨 A 起步）—— **完成**（2026-07-11）

**做了什么**（cg_clif `rustc_codegen_cranelift/src/inline_asm.rs` 逐段同构）：
inline asm 站点不再降 Trap，而是**自做寄存器分配 + 渲染 GAS wrapper**（`fn(*mut u8)`
槽缓冲 ABI：rbx=缓冲基址，clobber 保存 → 从槽装输入寄存器 → asm 模板本体 → 回存
输出寄存器 → 恢复 clobber → ret），**批量交外部汇编器 cc** 汇编成 .so → dlopen →
dlsym 得真地址；解释器执行 = 栈开缓冲、按 ins 装槽、call wrapper、按 outs 取槽。

- `src/lower/asm.rs`（新）：`generate`（allocate_registers 两相 + allocate_stack_slots
  + generate_asm_wrapper，intel 语法/ELF/x86_64 分支）+ `materialize`（拼 .s → cc
  -shared -fPIC -nostdlib → dlopen RTLD_NOW|LOCAL → dlsym `mirvm_asm_{i}`；FNV-1a
  内容哈希缓存 `~/.cache/mirvm/asm-stubs/<hash>.{s,so}`，热缓存零 cc）。
- `ir.rs`：`Terminator::InlineAsm { stub, buf_size, ins:Vec<(off,Operand)>,
  outs:Vec<(off,ScalarPlace)>, target }` + `Module.asm_stub_addrs: Vec<u64>` +
  `AsmStubId`。
- `func.rs`：`LowerCx.def_id`（asm_target_features 查询）；`lower_inline_asm`——MIR
  操作数 → wrapper 约束 + 配对已降低值/落点与 wrapper 槽偏移（同源一致）。拒
  naked/may_unwind/noreturn/att_syntax/sym/label/const/带 cleanup/非 x86_64 →
  Trap-stub（诊断留痕）。
- `interp.rs`：InlineAsm 终止子执行（栈 `#[repr(align(16))]` 256B 缓冲上限守卫）。

**Gate 结果**：asm-stub 工厂三面孔全验证，corpus **15→16 pass**（tempfile 转绿），
零回归。

| corpus 用例 | asm 面孔 | M5.0 前 | M5.0 后 |
|---|---|---|---|
| **tempfile** | 裸 syscall（rustix） | asm 红 | **全绿** ✓（真 read/write/fstat，文件 IO 对拍一致） |
| numbigint | div（128/64 宽除法） | asm 红（死于 div_wide） | 推进过 div（50!/modpow 正确）→ 撞 `llvm.x86.addcarry.64`（M5.1） |
| blake3 | cpuid（特性检测） | asm 红（死于 cpuid） | 推进过 cpuid → 撞 `llvm.x86.xgetbv`（M5.1） |
| sha2 | cpuid | asm 红 | 推进过 cpuid → 撞 `llvm.x86.ssse3.pshuf.b.128`（M5.1 SIMD） |
| ecosystem（diff_cargo） | cpuid（regex/std_detect） | asm 红 | 推进过 cpuid（serde 正确）→ 撞 `llvm.x86.xgetbv`（M5.1） |

**回归无损**：diff 16/16、gate0/1/2 九用例、spike1-5、TSan 零竞争、纯度门禁、
diff_cargo ffi_zlib+project 绿；加载相 fib 420ms（≈ M4.5 的 413ms，asm 物化无感）。

### 经验与教训

1. **asm 三面孔的 wrapper 生成一次成型，实证 cg_clif 同构可靠**：div wrapper 实测
   `mov rcx,[rbx+0x10]`（divisor→分配得 rcx）/`div rcx`/存商余——机器码逐条正确。
   侦察先行（先 dump 三面孔真 MIR + grep rustc_target::asm 全 API）值回票价：一次
   编译通过、三面孔一次跑对。**Cranelift 本身无 asm 能力**（corpus §2.2 的"Cranelift
   有 inline-asm 降低"不确，已修正为 cg_clif=寄存器分配+wrapper+外部汇编器）。
2. **"修完撞下一层"再次应验**（M4.5 同律）：设计 M5.0 gate 写"numbigint 绿"，实测
   div 只是 numbigint 的**第一层**asm 债——修完 div 立刻撞 `llvm.x86.addcarry.64`
   （scalar 进位加 intrinsic，非 asm）。**这不是 M5.0 缺陷，是设计对 numbigint 依赖
   面的低估**：div 是 asm-stub 的活儿（已清），addcarry 是 intrinsic 的活儿（D7）。
   四个 asm-红用例修完 asm 面后，**下一层全部收敛到 `llvm.x86.*` intrinsic**
   （addcarry/xgetbv/pshufb）——正是 M5.1 的 D7"SIMD/intrinsic 补面"范围。机制不同
   （asm-stub 终止子 vs intrinsic 内建/SIMD lane），故不并入 M5.0，M5.1 统一清。
3. **rbx 基址不变量是 wrapper 成立的支点**（照抄勿创新）：rustc 保留 rbx（LLVM 基址
   寄存器）永不分配给 reg 类操作数；cpuid 惯用法 `mov {0:r},rbx;cpuid;xchg {0:r},rbx`
   自己保 rbx 跨 cpuid（cpuid 写 ebx）——正因此 wrapper 用 rbx 存缓冲基址而安全。
4. **`late` 字段的死代码 lint 陷阱**：InlineAsmOperand 的 late 只在 Out 相区分有用
   （inout 恒相 0，cg_clif 命名 `_late` 回避）；字面模式匹配 `late: true` 不算 lint
   的"读"——两相分配循环里 bind `late` 变量比较才消警（顺带更清晰）。
5. **eager 物化的载入面影响可控**：asm 站点现在**总是**物化（非惰性 Trap），任何拉入
   std asm 的程序会在加载相触发 cc——但内容哈希缓存 + 三面孔外 std asm 多被 Trap-stub
   拒（sym/label/复杂形态）→ fib 载入无感（420ms）。**防静默错值**：att_syntax 模板
   与 intel wrapper 冲突 → 拒而非误汇编。

### 遗留（归期明确）

- **`llvm.x86.*` intrinsic 补面 = M5.1（D7）**：scalar（addcarry.64/xgetbv）+ SIMD
  lane（ssse3.pshuf.b.128 等）→ numbigint/blake3/sha2/ecosystem 全绿。xgetbv 是
  cpuid 姊妹（特性检测），"虚拟 CPU=真宿主 CPU"下宜执行真指令。
- **静态原生归档装载 = M5.1（D2）**：blake3 修完 xgetbv 后下一层是 `.S` 归档符号
  （build.rs 产物，dlsym 不到）——.a→.so 转换 + native_libraries 收集。
- asm 支持面（sym/label/const/may_unwind/非 x86_64）按需再补（三面孔未触及）。

### 复审（2026-07-12，应用户要求二次审查）

**语义面结论：实现正确**——机器码级逐条核对（div wrapper 汇编逐指令对、掩宽链
mem_write/ByteRegion.write 按宽截断闭环、rbx 基址不变量靠 rustc 保留 rbx + cpuid
惯用法自保、clobber 槽按 C-ABI 覆盖集过滤正确、分配 panic → catch_lower → Trap
响亮、stats/纯度/TSan 无涉）+ 新增 `demo/asm_probe.rs` 差分探针（三面孔+类分配+
inout/clobber 与 native 同机逐字节一致，含 cpuid 厂商串——"虚拟 CPU=真宿主 CPU"
使 cpuid 差分有效）。

**三项发现与修复**：
1. **F1（流程，最重）**：M5.0 提交时"全量回归"漏跑 gate4/gate5——gate4 补跑 11/11
   绿；gate5 实测 **28/31 红**：其 asm 预期红判据（grep 'asm!'）滞后于 M5.0 推进的
   前沿（blake3/sha2/numbigint 的诊断已变 `llvm.x86.*`），且 tempfile 转绿未上锁
   （若工厂回归，gate5 会把红当"预期红"吞掉——安全网破洞）。修：判据滚动到新前沿
   （INTRINSIC_RED 三项按 `llvm\.x86` 匹配、诊断变形即 FAIL；tempfile 出清单必须
   PASS）+ asm_probe 进 diff.sh（基线 16→17，工厂永久在差分网里）。修后 gate5
   **31/31**。**教训：预期红清单是滚动记账——每推进一层前沿，gate 判据必须同一
   commit 同步；且"全量回归"必须真的全量（gate0-5 一个不少），列表式汇报防自欺。**
2. **F2（并发竞态）**：materialize 的 cc 直写终名 .so——并发 mirvm 进程同键物化时
   可 dlopen 半成品。修：临时名 + rename 原子发布。
3. **F3（忠实性措辞）**：allocate_registers 注释自称"同构"，实为**保守变体**
   （cg_clif 按 (in,out) 位分别判冲突允许 in↔lateout 共享寄存器；本实现全冲突不
   共享——恒为合法子集，极端密集时提前分配不出 = 响亮 Trap 非静默错值）。修注释
   如实标注分歧。另记：xmm 值操作数现被标量路径拒（Trap 响亮）——M5.1 sha stub
   需扩 16 字节槽通道。

## M5.1 llvm.x86 + 静态归档（轨 A 收口）—— **完成**（2026-07-12）

### 开工前：可信 oracle 与语义基线

- `diff_cargo.sh` 现在先要求 native 达到明示退出码；ecosystem 以
  `llvm.x86.xgetbv` 精确 XFAIL，双方都失败与 XPASS 都会使 gate 失败。
- `corpus.sh` 有失败即返回非零；gate5 分开统计 PASS/XFAIL，并真实调用 gate0、gate1、
  gate2、gate4。`tests/gate_truth_regression.sh` 的 6 个门禁自测通过。
- `c_signal` 增加 `assert!(handler_ran)`；引擎删除 signal/sigaction 的静默成功，guest handler
  当前明确 Trap，SIG_DFL/SIG_IGN 才受限直通。
- volatile 不再降为普通访问：新增独立 IR 和宿主 volatile load/store，覆盖 1/2/4/8/16 字节
  与 unaligned；`c_volatile` 加入 gate，Rust 单元测试覆盖标量、unaligned 与 16-byte。
- 该时点 `cargo test --locked` 为 8 passed。CI workflow 已建立；rustfmt、Clippy 当时尚未
  清零，最终状态见下方总验收。

### 切片 1：addcarry/subborrow —— 完成

- `llvm.x86.addcarry.64` 与 `llvm.x86.subborrow.64` 进入显式 engine builtin 表；未登记的
  `llvm.*` 仍响亮 Trap。
- 解释器使用两次 `overflowing_add` / `overflowing_sub` 保留进位/借位，返回严格走冻结的
  `RetDest::Pair(flag, value)`，不把 pair 静默压成单标量。
- `tests/m51_addcarry.sh` 通过 stdarch 公共 API 将边界输入的 add/sub checksum 与 native
  逐行差分；`c_numbigint` 已转绿。

切片 1 结束时 xgetbv、pshufb/SHA-NI、blake3 静态归档和 ecosystem 后续前沿仍待；随后
xgetbv 已由切片 2 完成。技术路线与退出标准继续以 `m5.1-design.md` 的复核版滚动更新。

### 切片 2：xgetbv —— 完成

- `llvm.x86.xgetbv` 进入 scalar builtin；解释器执行真实宿主 `xgetbv`，以 ecx 输入并合并
  edx:eax 返回。guest 与 host 共享 CPU 特性模型，调用仍由 guest 正常 CPUID/OSXSAVE 分派保护。
- `tests/m51_xgetbv.sh` 先检查宿主 CPUID；可用时逐字节比较 native/mirvm 的 XCR0 输出，
  不可用时比较双方 skip 行为。在可写缓存环境实测 PASS。
- 这次选择固定 builtin 而非加载相 asm-stub：形状固定、纯标量、无模板分配需求；未知
  llvm.x86 仍 Trap，不形成通用按名模拟。

切片 2 后的滚动复测把当时前沿钉为：blake3=静态 archive 符号、ecosystem=`simd_insert`、
sha2=pshufb（随后由切片 3 转绿）。xgetbv 的旧诊断不再被 gate 接受。该时点 release gate5 为
**31 PASS / 4 XFAIL / 0 FAIL**
（第四个 XFAIL 是独立的 signal guest handler）。

### 切片 3：x86 向量 stdarch helpers —— 完成

- 新增 tcx-free `engine::x86`：pshufb128/256 与 SHA256 msg1/msg2/rnds2 均使用
  `#[target_feature]` 宿主 stdarch intrinsic，known-vector 单元测试通过。
- 这替代 M5.1 初稿的“扩通用 asm-stub 向量 ABI”解释器路线：客户面很窄，固定 helper 更小，
  且 stdarch 处理 SHA rnds2 隐式寄存器；未来方法级 JIT 的 CLIF/asm 选择不受影响。
- lower/interpreter 地址式宽值通道已接入；`tests/m51_x86_vectors.sh` 与 native 差分 PASS，
  `c_sha2` 两个标准 SHA256 输出正确并转绿。本切片已从 helper 地基成为产品能力。

### 切片 4：Static native archive（D2）—— 完成（受约束 Linux/ELF）

- 新增 `native_archive.rs`；加载相收集 local + used crates 的 `tcx.native_libraries` Static
  条目，按 cfg/filename/verbatim 和 native search path 找真实 `.a`，转换 `.so` 后加入
  第一版 `Module.native_libs`。这一“可选候选”分类后来被收官语义复审推翻，
  当前的 required 装载见下方复审记录。
- 链接 recipe = `cc -shared -z defs --whole-archive A --no-whole-archive`，每个 archive
  独立转换。内容缓存键包含 recipe、target、cc 身份和 archive bytes，tmp+rename 原子发布。
- 有意拒绝：非 Linux/ELF、thin、ctor/dtor、非 PIC relocation、未闭合/跨 archive 依赖/顺序、
  跨 archive 重名动态导出、与 RTLD_DEFAULT 既有同名符号、export-symbols；这是垂直切片，
  不是假装通用 linker。
- 该切片时点 `cargo test --locked native_archive::tests` 9/9，全 crate 16/16。blake3 两个 build.rs archive 成功转换，
  `c_blake3` 与 native 三行 hash 逐字一致并转绿。

切片 4 完成时只剩 ecosystem 滚动前沿（当时为 `simd_insert`）与最终 gate/日志收口；signal
guest handler 是独立能力缺口，不应混入 M5.1 全绿宣称。

### 切片 5–6：ecosystem SIMD 收口 —— 完成

- lower 按编译期常量索引、lane 类型/宽度和向量界限展开 `simd_insert/extract`；动态/越界/
  类型不匹配继续响亮 Trap，不接受隐式截断。
- 补 `simd_shl/shr`；signed 右移保持算术语义，非法 shift count 明确终止。`CpuPause` 泛化为
  无 RAM 状态效果的 `CpuHintNop`，覆盖 `pause` 与 `vzeroupper`。
- `m51_simd_insert`、`m51_simd_shift`、`m51_vzeroupper` 三个 feature-gated tracer 均与 native
  一致；加上 addcarry、xgetbv、x86_vectors，共六个 M5.1 release tracer 全部通过。
- ecosystem debug/release 完整通过；blake3、sha2、numbigint release 客户链也全部通过。

实现范围完成后，最后收口任务是删掉 diff_cargo/gate5 中已过时的 expected-red、重跑最终
聚合 gate，并把 signal guest handler 保留为独立明确 XFAIL；下方总验收已完成这些事项。

### 收官后语义复审：再次推翻“看起来已经绿”的路径

- 旧 volatile 实现只按尺寸把 1/2/4/8/16-byte 值强转成宿主整数/对齐 8 聚合值。
  复审用 `[u8; 16]` 的 alignment=1 反例稳定触发 Rust 对齐检查 abort，含 padding
  聚合值还有读未初始化字节为整数的 UB。最终改为独立 `Stmt::VolatileLoad/Store`
  + alignment=1 `MaybeUninit<[u8; N]>` opaque 位型搬运，并加低对齐与 padding 回归。
- `_Unwind_Backtrace` 经通用 libffi 虽能调用，只能看到解释器/libffi 的宿主栈，
  不是 guest frame/IP。因此 backtrace 与其余 `_Unwind_Get*/Set*` context 家族改为显式
  `Unsupported`，`c_backtrace` 以原因锁定的 XFAIL 保证不再静默伪造。
- archive `.so` 从“可选 dlopen 候选”分离为 `required_native_libs`，在任何 dlsym 前
  `RTLD_NOW` 加载；失败保留 `dlerror` 并立即终止。lifecycle 检查补齐
  `.init/.fini`、优先级 `.init_array.*` 等 section，定向 archive 测试增至 12，另有
  2 个 FFI 加载测试。
- gate5 现在独立统计 SKIP；TSan 在 CI 有独立可见 step，gate0 只编译纯度 harness。
  x86_vectors 的 pshufb/SHA 子特性分别 PASS/SKIP，不再因宿主缺 SHA-NI 冒充整体 PASS。

### M5.1 总验收

- 四目标客户链：numbigint、sha2、blake3、ecosystem 全绿；diff_cargo 3/3，diff 17/17。
- 六个 release native tracer 脚本全通过，x86_vectors 内 pshufb/SHA 两个子断言分别 PASS；
  cargo test 23/23；gate-truth 10/10。
- rustfmt、Clippy `-D warnings`、release build 全 PASS；CI workflow 已建立。
- full release gate5（含 gate0/1/2/4、TSan）= **40 PASS / 2 XFAIL / 0 SKIP / 0 FAIL**；
  XFAIL 是不属于 M5.1 的 signal guest handler 与 guest backtrace/frame-IP 映射。
- 性能无回归：load 471ms、rayon 732ms。

**M5.1 完成。下一阶段 = M5.2 非 JIT 语义补全（2026-07-14 编号重排：JIT 顺延 M5.3–M5.5）。**

## M5.2 非 JIT 语义补全（轨 A 完备期）—— 施工中（2026-07-14 批准）

设计与决策：[m5.2-design.md](m5.2-design.md)（D8a–D8l 全批）。缺口来源 = 拒绝面全量
清点 + rustc intrinsic 权威差集 + 14 个 native 差分探针（"主动圈定"取代"已知用例绿"口径）。

### 片 1：D8i 标量 intrinsic 差集清零 —— 完成（2026-07-14）

- **fabs 泛型名漂移修复**（本片最尖：`f64::abs()` 曾一调即 Trap，corpus 恰好无人调）：
  math_un/bin 表重构为"后缀名定宽（sqrtf64）+ 裸泛型名按类型参数定宽（fabs<T>）"，
  **全表泛型兜底**——后缀剥离对 nightly 漂移脆弱，将来任一名字去后缀化走同一条道。
  f16/f128 在 `resolve_float_width` 处保持响亮 Err（D8c 接入点）。
- **atomic fetch_max/min 四变体**：`RmwOp` 增 `Max/Min/UMax/UMin`（有符号性由 intrinsic
  名冻结进变体），执行器有符号路经同址 `AtomicI*`，位型回写零扩展。
- **fma/fmuladd**：新 `Rvalue::MathFma`（宿主 `mul_add` 单次舍入；fmuladd 允许融合/不融合，
  融合在允许集合内）；f16/f128 变体自然落到未处理 Err（D8c 消除）。
- **fast/algebraic 浮点 10 个**：按精确 IEEE 语义走既有 `FloatBin`（fast-math 是自由授权，
  精确结果恒在允许集合内）。
- **volatile 批量访存**：`volatile_copy_memory`（重叠=memmove）/`volatile_copy_
  nonoverlapping_memory`/`volatile_set_memory` 走 MemCopy/MemSet 通道（解释器逐条执行
  从不省略=volatile 忠实实现；注意实参序 (dst,src) 与 copy 的 (src,dst) 相反）。
  `nontemporal_store` 走 volatile store 通道（NT 是性能 hint，值语义=普通 store）。
- **杂项**：`ptr_mask`（位与，真实地址下即语义）、`vtable_size/align`（vtable 槽 8/16
  直读）、`breakpoint`（真 int3——native/mirvm 双侧实测 exit 133 SIGTRAP 同构）。
- **nullary 保险臂**：size_of/align_of/variant_count（rustc eval_nullary 同构：Pat 剥壳、
  Adt=变体数、其余具体类型=0）/needs_drop 在 lower 期 tcx 折常量。**type_id/type_name/
  offset_of/field_offset 不做**：TypeId 本 nightly 是 vtable 身份结构、type_name 需字符串
  物化，正常被 GVN 折叠（anyhow corpus 实证），残留维持响亮 Trap；重开条件=真实程序在
  非默认 mir-opt 下撞到。
- **验收**：新差分探针 `demo/intrinsic_probe.rs` native/mirvm 逐字节一致（fma 融合证明值
  8.67e-19 非零=双侧真融合；fetch_max/min 含符号边界 -128/127/255/u64::MAX；重叠 volatile；
  ptr_mask 对齐掩码）；diff.sh 基线 20→21；full gate5 = **40 PASS / 2 XFAIL / 0 FAIL**；
  cargo test 35/35、fmt、Clippy `-D warnings` 全过。

### 片 2：D8a 栈深度保真 —— 完成（2026-07-14）

- **真栈字节守卫替代固定帧数**：删除 `MAX_DEPTH=8_000` 硬编码（对 native 栈界严重失真
  ——native 8MiB 主栈容 ~10 万浅帧）。Ctx 创建时经 `pthread_getattr_np` 冻结本线程
  `stack_floor`（栈低端 + 边距，边距=clamp(size/8, 256K, 4M)）；interp_frame 以本地
  变量地址近似 SP，低于下界即诊断退出（native 语义 SIGSEGV→"has overflowed its
  stack"，此为诊断替身；ram-spec §7 溢出深度 unspecified）。任意线程（主执行/guest
  线程/外来 native 线程 thunk 再入）自适应真实栈界。
- **主执行迁专用大栈线程**：`on_guest_stack`（默认 1 GiB 虚拟保留，按需提交）承载
  run_main/run_export；spawn 失败响亮退出不静默降级。`--stack-size`/`MIRVM_STACK_SIZE`
  旋钮（k/m/g 后缀，JVM -Xss 同位；flag 落 env 使 cargo 形态经 runner 同径生效）。
- **guest 线程栈放大**：`pthread_create` 显式 stacksize（std::thread 恒显式）临时放大
  32×（≥64 MiB 虚拟），调用后还原 attr。**glibc 陷阱**（实测踩中）：未 setstack 的
  attr 经 `pthread_attr_getstack` 返回 `NULL - stacksize`（近 u64 顶假地址）而非
  NULL——以"x86_64 用户地址 ≤47 位"判定未设；真自供栈（地址界内）不动。
- **操作数区同步扩容**：ByteRegion 64 MiB → 1 GiB 虚拟保留 + `MAP_NORESERVE`（RSS 仍
  按触碰页），不再先于栈守卫成为深递归隐形上限。
- **验收**：`demo/recursion_deep.rs`（主线程 50k/带大局部 20k/显式 2MiB 线程 8k/默认
  线程 20k）native/mirvm 逐字节一致；`--stack-size 4m` 到界优雅诊断（深度 ~8k 帧）
  而非 SIGSEGV，非法尺寸清晰报错；diff.sh 21→22；full gate5 = **40 PASS / 2 XFAIL /
  0 FAIL**，加载 397ms、rayon 850ms 无回退；fmt/clippy/35 tests 全过。

### 片 3：D8b simd 家族补全 —— 完成（2026-07-14）

- **★ 顺带根治一颗潜伏静默错值雷**：旧 `SimdBin` 对全部 lane 按整数位运算——float
  lane 的 `simd_add` 是整数加浮点位、比较是位比较（+0.0==−0.0 判不等、NaN 判自反），
  **无任何 Trap**。当时仅因 corpus 全为整数 lane（hashbrown/memchr/sha2）未爆雷；任何
  std::simd 浮点程序都会拿到错数。本片引入 `LaneKind{Int{signed},Float}` 贯穿全部
  simd 语句，使"忘带元素类别"在类型层不可表示；lower 期校验浮点族/位族的 lane 类别，
  执行器只留防御断言。
- **覆盖 20 → 76/75+1**：算术 `mul/div/rem/neg/saturating_add/sub`（div/rem 除零响亮）、
  `minimum/maximum_number_nsz`（宿主 min/max=minnum 在 nsz 允许集合内——首轮探针即
  发现该名漏臂，"实现-重跑-再补"再应验）、浮点单目全套（fabs/fsqrt/ceil/floor/round×2/
  trunc + 超越族 fsin/fcos/fexp/fexp2/flog×3 逐 lane 宿主 libm，与 native scalarize 同源）、
  位族 `ctlz/cttz/ctpop/bswap/bitreverse`（与标量 BitUn 共用 helper）、`fma/relaxed_fma`
  （宿主 mul_add）、`funnel_shl/shr`（u128 拼接窗口，移位越界响亮）、cast 族
  `cast/as/cast_ptr/expose_provenance/with_exposed_provenance`（float→int 饱和=宿主 `as`；
  指针族按整数位透传）、`select/select_bitmask`（mask 符号位判）、
  `gather/scatter/masked_load/store`（**假 lane 绝不佯读/佯写**——哨兵探针实证）、
  `extract_dyn/insert_dyn`（运行期索引越界响亮；insert 整体 memmove 防同址）、
  `arith_offset`（pointee stride 逐 lane wrapping）、归约族 `reduce_{add,mul}_{ordered,
  unordered}/and/or/xor/min/max`（顺序折叠恒在 unordered 允许集合内；float min/max=
  minnum/maxnum 链）。`simd_geom` 统一取几何 + f16/f128 lane 拒绝（D8c 接入点）；
  `select_bitmask` 第一泛参是标量掩码，前置特判。饱和逻辑提炼 `int_saturating`
  与标量 IntSat 共用。
- **验收**：`demo/simd_probe.rs`（float NaN/−0.0/inf 比较与算术、i8 饱和边界、符号
  除法/移位、funnel、饱和 cast、gather/scatter 哨兵、masked、dyn lane、全归约）
  native/mirvm **逐字节一致**；`corpus/c_portable_simd.rs`（点积 mul_add/字节扫描
  bitmask/clamp/rotate/cast 真实画像）两侧一致进 corpus（26→27）；diff.sh 22→23。

### 片 4：D8j atomic 序贯通 —— 完成（2026-07-14）

- IR 五形态补序：`AtomicLoad/AtomicStore + order`、`AtomicCxchg + succ/fail 双序`、
  `AtomicRmw + order`、`Fence + order`；`MemOrd` 五值枚举，引擎 `host_ord` 1:1 映射
  宿主 `Ordering`——Relaxed store 回到 plain mov，弱序可见性与 native 同源恢复。
  旧"全折 SeqCst"虽合规（强化序=允许集合子集）但违 concurrency-arch 承诺。
- lower 读 const 泛型 `ORD`（cg_ssa parse_atomic_ordering 同构：valtree 分支[0] 判别
  叶 → to_atomic_ordering）。**漂移陷阱实测踩中**：本 nightly atomic intrinsic 类型
  参数量异构（`atomic_xadd<T,U,ORD>` 双类型参 vs `atomic_load<T,ORD>` 单参），硬编码
  `const_at(1)` 在 xadd 上 ICE——改为**按位置收集全部 const 泛参**（序是唯一 const，
  cxchg 两个按序取 succ/fail），下标漂移免疫。load/store/cxchg-fail 的非法序组合
  lower 期拒绝。
- **验收**：`demo/atomic_order_probe.rs`——①组合矩阵（每操作×每合法序真实执行，
  错误映射会被宿主原子 API panic 当场抓住；`fence(Relaxed)` 非法性由 native 侧 panic
  实证）②Acquire/Release 消息传递不变式 ③4 线程 Relaxed 争用计数——native/mirvm
  逐字节一致；diff.sh 23→24；TSan gate 全绿（弱序错映射=真数据竞争会被抓）。

### 片 5：D8c f16/f128 —— 完成（2026-07-14，零残留）

- **实现路线小改（同源性论证不变）**：设计写"手写 libgcc FFI"；实测本 nightly 宿主
  `f16`/`f128` 类型全套可用（算术含 `%`、比较、全部数学函数、`mul_add`、宽整数互转）
  ——引擎直接骑宿主类型 + `#![feature(f16, f128)]`，rustc 把引擎自身的 f16/f128 运算
  下降到与 native guest **同一批** compiler-builtins `__*tf*`/`__*hf*` + glibc `*f128`
  libm 符号。同源即位同，少一层手写 FFI 与 libffi float128 形态问题（libffi 的
  longdouble 在 x86-64 是 80 位，接不了 binary128）。
- **f16 = 标量通道**：`is64: bool` 全面重构为 `FloatW{F16,F32,F64}`（FloatBin/FloatCmp/
  FloatNeg/FloatCast/FloatToInt/IntToFloat/MathUn/MathBin/MathFma/Wide128ToFloat 十形态），
  FloatCast 扩为 3×3 全组合。intrinsic 名后缀解析扩 f16/f128（`FloatSuffix` 四值）。
- **f128 = 16 字节宽通道**（骑 u128 的 Bytes/place 基建）：`F128Bin`（四则+Rem=
  fmodf128）、`F128Cmp`、`F128Un`（Neg+14 数学单目）、`F128MathBin`（pow/powi[标量
  rhs]/copysign/min/max）、`F128Fma`、`F128From/ToScalar`（f16/f32/f64/≤64 整数互转，
  `as` 饱和）、`F128From/ToWideInt`（i128/u128 互转）。BinOp/Neg/三类 cast/数学/fma/
  fast-math 臂全部四宽路由。
- **验收**：`demo/float_wide_probe.rs`（f16 subnormal/位模式/饱和边界、f128 精度证明
  1e30+1、NaN 语义、宽整数往返、to_bits 十六进制位断言）native/mirvm **逐字节一致**；
  `corpus/c_float_wide.rs`（f16 量化误差界 + f128 高精度累加画像）进 corpus（27→28）；
  diff.sh 24→25。D8c 无残留（i128↔f128 也已覆盖）。

### 片 6：D8g atexit + D8h global_asm/naked/asm 操作数 —— 完成（2026-07-14）

- **atexit 家族**（D8g）：`atexit`/`__cxa_atexit`/`on_exit` builtin。glibc 不导出
  `atexit` 供 guest dlsym（探针实证），故引擎自持 LIFO 注册表 + 一个 native
  trampoline（经引擎自身链接的 libc `atexit` 挂载，**非** dlsym）；进程收尾时 libc
  在主线程调 trampoline，逐条 LIFO 解释执行 guest 回调（fresh Ctx attach——退出线程
  可能非 guest 执行线程）。回调必须是已知 guest fn 条目（防非 guest 地址）。Shared
  裸指针在 run_main/run_export attach 时存入静态，供 trampoline 找回引擎。
- **global_asm! + naked fn**（D8h）：新 `src/lower/global_asm.rs`——收集
  `MonoItem::GlobalAsm` + naked `MonoItem::Fn`，渲染成单个 `.s`（global_asm 用 HIR
  模板、naked 用 MIR InlineAsm 终止子 + `.globl/.type/.size` 包装，cg_ssa
  prefix_and_suffix 精简版）→ `cc -shared -nostartfiles` → `.so` → required_native_lib
  （dlsym 前 RTLD_NOW 就位）。naked fn 调用点（resolve_call 检 `CodegenFnAttrFlags::
  NAKED`）改走 foreign 直调其 mangled 符号。cg_clif global_asm.rs 同构。
- **inline asm const/sym 操作数**（D8h）：M5.0 工厂加 `AsmOperand::Inline{text}`——
  const/sym 在 lower 期渲染成字面文本（asm_const_to_str / symbol_name），占位符直接
  展开该文本，寄存器分配天然跳过（cg_clif 同款"const 格式化进模板"）。
- **诚实边界**（D8l 登记）：naked/global_asm 的 `sym` 操作数只能指向**机器码**符号
  （另一 naked/global_asm 或动态库导出）；指向**解释执行的 guest fn** 无机器码入口，
  .so 会留未解析符号——`nm -D -u` 审计在 lower 期响亮拒绝，指明这是 JIT 期能力
  （从机器码 jmp 进解释器需 per-fn trampoline）。att_syntax/label/may_unwind 维持拒绝。
- **验收**：`demo/asm_extras_probe.rs`（global_asm 定义符号 + naked fn + inline asm
  const + atexit LIFO）native/mirvm **逐字节一致**；`corpus/c_atexit.rs`（三回调 LIFO）
  进 corpus（28→29）；diff.sh 25→26（asm_extras_probe）。

### 片 7：D8e backtrace 影子帧 —— 完成（c_backtrace XFAIL→绿，2026-07-14）

- **影子帧栈**：Ctx 加 `shadow: Vec<u64>`，interp_frame enter 时 push 合成 IP
  （`FUNC_IP_BASE=0x5f5f… + func×64`，每 FuncId 唯一/非零/不可执行 opaque token），
  FrameGuard::drop 时 pop（与 depth 同 RAII 生命周期，unwind 安全）。
- **四个 unwinder builtin**（原 `Unsupported`）：`_Unwind_Backtrace(trace_fn, arg)`
  逐影子帧（栈顶→底、跳过自身）调 guest trace_fn(synth_ctx, arg)，synth_ctx 是存 IP
  的栈缓冲；`_Unwind_GetIP`/`GetIPInfo` 读它；`_Unwind_FindEnclosingFunction(ip)` 返
  ip 自身（合成 IP 即函数入口）；`_Unwind_GetCFA` 复用 GetIp（backtrace 用作 sp 帧
  身份，每帧唯一即够）。其余 context/state API（CFA 除外的 SetGR/GetGR/Resume…）
  维持响亮 `Unsupported`。
- **诚实符号化边界**：合成 IP 在用户地址空间之上、非页对齐 → dladdr 找不到 → 符号
  解析诚实产出 `<unknown>`（禁止伪造宿主符号）。**因此 backtrace 精确文本非
  well-defined**（ram-spec §2），c_backtrace 的 oracle 改为**影子帧不变式**：
  ①status=Captured ②非空 ③递归深度如实反映（`deep(30)` 比 `deep(0)` 多 ≥30 帧，
  black_box 夹递归两侧阻 TCO）——native 与 mirvm 都满足，两侧打印同一确定行。
- **验收**：c_backtrace 从原因锁定 XFAIL **转绿**（gate5 XFAIL 2→1）；native（去
  frontmatter）与 mirvm 同输出 "backtrace: captured, non-empty, depth reflected
  (+30 frames)"。后续可选：物化符号 ELF 让 dladdr 报真 guest fn 名（非本期）。
