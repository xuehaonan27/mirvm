# 帧布局 · 调用约定 · 字节码格式 —— M4 设计草图（模型 A）

> **状态：M4 的历史设计基线，主体已实现；`.mirvm` 分发已实现，alloca 必迁承诺已撤销。**
> Model A tree-walking、slaved ByteRegion、冻结元数据、真线程和 M4 unwind 已落地；
> 方法级 Cranelift 与 i2c/c2i 产品适配器已由 M5.3–M5.5 兑现；`.mirvm` mode B 分发
> 已经实现。解释态局部正式保留 slaved ByteRegion，`alloca` 只在
> 真实负载证明性能收益时重开（decision-history §7.49）。实际状态见
> [current-status.md](../current-status.md)，A/B 与局部存储两轴的演变见
> [decision-history.md](../decision-history.md)。下文保留原始方案，不能把未来段落当成现状。
>
> **2026-08-12 unwind 勘误**：下文历史段落把“跨 FFI”一概写成 abort，范围过宽。
> 现行规则是普通 C 边界终止，C-unwind 边界允许原异常穿过并跑 cleanup；出向
> `ffi_call`、callback/P1 wrapper、解释器与 JIT 均已按此实现。规范见
> [c-unwind-contract.md](c-unwind-contract.md)。

---

## 0. "模型 A for mirvm"具体是什么

模型 A 的 JIT 关键性质 = **每个 guest 函数活动记录（call activation）对应一个 native 栈帧**，于是
interp↔compiled 调用是 native call、unwind 走一条栈。有两类 native 帧：

- **解释帧**：一次 `interp_frame(instance)` 的 Rust 调用（tree-walking 控制流：guest 调用 = 宿主递归调用）。
- **编译帧**：Cranelift 生成的 native 帧（纯 guest，无解释器开销）。

> **澄清（重要）**：JIT 互操作只要求**调用活动**在 native 栈上（控制流 + unwind），**不要求 guest 局部数据
> 也内联在 native 栈**——编译代码从不读解释帧的局部，跨函数只按调用约定传参/返回。所以"局部数据放哪"
> 与 JIT 互操作**正交**（见 §2）。模型 B 之所以 JIT 难，是因为它的 guest 调用**不递归 native 栈**
> （flat loop + Vec<Frame>），编译帧的解释态 caller 不在 native 栈上，unwind 走不过去。

**为什么 tree-walking（宿主递归）而非 HotSpot 的汇编模板解释器**：后者显式操作 native SP 压/弹解释帧
（汇编级），太重；tree-walking 用安全 Rust 就把 guest 调用映射成 native 调用，同样达成模型 A 的 JIT
互操作。代价是每个解释帧背一个宿主 `interp_frame` 开销——但**解释器是冷层**（热代码走编译帧），可接受。
汇编/alloca 方案只保留为证据触发的性能候选（§10、decision-history §7.49）。

---

## 1. 两类 native 帧

```
 一条 OS 线程的 native 栈（向下增长）：
 ┌─────────────────────────────────────┐
 │ interp_frame(main)   [解释帧]         │  Rust 帧: 含 dispatch 状态 + 指向 main 的操作数区
 ├─────────────────────────────────────┤
 │ <compiled> foo       [编译帧]         │  Cranelift 帧: foo 的 locals/spill，纯 native
 ├─────────────────────────────────────┤
 │ interp_frame(bar)    [解释帧]         │  ← 当前
 └─────────────────────────────────────┘
 调用链: main(解释) → foo(编译) → bar(解释)，全在一条 native 栈；
 每次跨越是 i2c/c2i 适配器（§3）。unwind 走这一条栈（§7）。
```

- 解释帧 = `interp_frame` 的一次 Rust 调用；guest 调用 → 宿主递归。
- 编译帧 = Cranelift 帧；guest 调用 → 直接 native call。
- 两者混在一条 native 栈，通过适配器相连——这就是模型 A。

---

## 2. 帧内布局与 guest 局部存储

### 2.1 编译帧

Cranelift 自管（locals、spill 槽、callee-saved），我们只定**它的调用约定**（§3）和 **unwind info**（§7）。
无需我们摆布局。

### 2.2 解释帧的 guest 局部：slaved 操作数区（Rust 可行，避 alloca）

guest 帧大小是**动态的**（取决于函数的 locals 数与类型），Rust 局部是定长——不能直接内联到 native 栈。
方案：**每线程一块"解释器操作数区"，slaved 于 `interp_frame` 递归**：

```
 每线程操作数区 (连续 buffer, SP slaved 于 interp_frame 递归):
 ┌──────────────────────────────────────────────────┐
 │ [main 的寄存器槽 r0..rn][bar 的寄存器槽 r0..rm]... │  ← 区 SP
 └──────────────────────────────────────────────────┘
   进 interp_frame: 按帧描述符 bump 区 SP 预留本帧寄存器槽
   出 interp_frame / unwind: 恢复区 SP
```

- **不是模型 B**：这块区 slaved 于 native 递归（LIFO，与 interp_frame 进出同步），**不可独立挂起**
  （我们不要协程，§async）。控制流仍在 native 栈上（模型 A）。
- **寄存器槽 = MIR 局部**：`r0..rn` 对应 MIR 的 `_0.._n`，每槽按其类型的冻结 layout 定大小/对齐。
- **`&local` → C 天然**：槽在区里是真地址（§4 真实地址）。
- 编译帧不用这块区（用自己的 Cranelift 帧）。

> **2026-08-12 终裁（替代 2026-07-05 的迁移承诺）**：slaved ByteRegion 是解释器的
> 正式帧局部方案，不再要求换成 alloca。热函数已经由 Cranelift 使用 native 帧与 SSA；
> alloca 只改变冷解释器，而且必须额外承担栈探测、清零、unwind 和 checked 地址追踪。
> 只有真实解释器负载证明端到端收益时才重开（decision-history §7.49）。
>
> **仍有效的解耦要求**：帧局部存储与安全模式（fast/checked，轴 S）是
> 两根【正交轴】，实现上【不得耦合】。** 二者只在一个抽象处相遇：`GuestMemory::contains(addr)->bool`
> （"是否合法 guest 内存"谓词）。fast 从不调它；checked 在 raw 解引用前调它；FrameStorage 提供它。
> 当前 slaved 区可用廉价范围比较；未来若以性能证据重开其他存储，也不得改变 checked 的语义合同。

### 2.3 帧描述符（lowering 时算好，冻结）

每个函数一份，供 interp_frame 建帧用：

```
FrameDescriptor {
    reg_count,                       // 寄存器(=MIR 局部)数
    reg_slots: [{offset, size, align, ty_layout_id}],  // 每寄存器在操作数区的槽
    total_frame_size,                // 区里预留多少
    drops: [{reg, drop_glue_instance, cond}],          // 需 Drop 的寄存器 + drop glue（冻结）
    cleanup_edges,                   // unwind 目标（catch/cleanup 块），冻结自 MIR
}
```

---

## 3. 调用约定 + i2c/c2i 适配器

目标：compiled↔compiled 是纯 native call；interp↔compiled 廉价适配。

### 3.1 编译代码的调用约定

Cranelift 编译的 guest 函数用一个**定义好的 mirvm 调用约定**（可基于平台 C ABI 或自定义 Cranelift
calling convention）。标量/指针按寄存器传（真实地址下指针就是真址）；聚合按 rustc 的 ABI（复用 rustc
layout，与 native 一致）。**函数身份 = 单态化 Instance**（惰性单态化）→ 编译后是一个 code ptr。

### 3.2 四种转移

```
compiled → compiled : 直接 native call（Cranelift 按约定）。零适配。
interp   → compiled : i2c 适配——interp_frame 把 guest 实参从操作数区槽搬进约定寄存器, native call code ptr。
compiled → interp   : c2i 适配——编译码 call 一个桩 c2i(instance, args...); 桩把参数搬进新解释帧的
                       操作数区槽, 调 interp_frame(instance)。
interp   → interp   : interp_frame 直接递归调 interp_frame(callee)（宿主递归）。
```

- 适配器只是**参数搬运**（槽 ↔ 寄存器），因同在 native 栈，便宜。HotSpot i2c/c2i 同构。
- **未编译的热 Instance**：interp 调它时，要么继续解释，要么请求编译（编译服务线程，C8）——tiering
  策略本草图**不定**（M5 事），此处只保证"编译后可无缝接入"。

---

## 4. 字节码格式：寄存器式，MIR 派生

**寄存器式**（非栈式），寄存器 ≈ MIR 局部。理由：MIR 本就是寄存器/place 式（`_0.._n` + projection），
寄存器式**从 MIR 近乎机械降低**、指令更少更快（Lua/Dalvik 同选）。字节码 = "**MIR 把元数据解析/展平后的形态**"。

### 4.1 与 MIR 的对应

| MIR | mirvm 字节码 |
|---|---|
| 局部 `_i` | 寄存器 `ri`（操作数区一个槽） |
| place projection `_3.2`、`(*_4)[i]` | 解析成**具体偏移算术**（layout 冻结 → 编译期算好偏移） |
| rvalue（BinaryOp/Ref/Cast/Aggregate…） | 对应字节码指令，写入目标寄存器 |
| `SwitchInt`（match/判别式/**async 状态机**） | `switch ri -> [值:目标]` |
| `Call(f, args, dest, unwind)` | `call <InstanceId/code ptr>, [arg regs], dest reg, unwind blk` |
| `Drop(place, unwind)` | `drop ri`（用冻结的 drop glue instance） |
| `Return` | `ret r0` |
| foreign call | `call_foreign <os::handler id>` 或经 §os 边界 |

### 4.2 指令集草图（示意，非全集）

```
# 算术/逻辑
bin  <op> rd, ra, rb          # op: add/sub/mul/... 带 overflow 语义(冻结 overflow-checks)
un   <op> rd, ra
# 内存（真实地址，§4；无 AllocId 元数据，裸访问）
load  rd, [rbase + off]       # off 编译期算好
store [rbase + off], rs
ref   rd, rplace              # 取址(真址)
# 聚合/投影
field rd, rbase, off          # 已解析偏移
index rd, rbase, ridx, elem_sz
discr rd, rbase, disc_enc     # 读判别式(冻结编码/niche)
setdiscr rbase, variant, disc_enc
# 控制流
jump   blk
switch ri -> [v0:blk0, v1:blk1, ...]   # ← async 状态机派发就是这个
call   <target>, [args], rd, unwind=blk # target: 直接 InstanceId / dyn: vtable slot
ret    ri
# 内建
intrinsic <id>, [args], rd    # 引擎实现或转 os::
```

- **投影/判别式全是冻结偏移**——运行期不查 tcx（C8）。
- `switch` 直接服务 async（§async 已证：状态机就是 discr + switch，无特殊支持）。

---

## 5. 元数据冻结（lowering 时解析，运行期不触 tcx —— C8 核心）

MIR → 字节码（针对一个单态化 Instance）时解析并冻结：

- **layout**：每类型的 size/align/字段偏移/判别式&niche 编码 → 具体数字。
- **调用目标**：直接调 → 单态化 Instance 的 BytecodeBodyId / code ptr；dyn 调 → vtable slot 号。
- **vtable**：dyn 类型的 vtable 布局（方法槽）。
- **drop glue**：每个要 Drop 的位置 → 具体 drop instance。
- **常量**：intern 进本函数常量池。
- **intrinsic**：标记为内建 op 或解析。

产物 = 不可变的 `BytecodeBody`。**发布后只读、线程共享**（C8：降低时冻结）。降低本身在编译服务线程或惰性
加锁下做（≈ HotSpot 的 resolved constant pool / class loading），发布后各线程 lock-free 读。

---

## 6. 执行：interp_frame 与转移

```rust
// 一次调用 = 一个解释帧（native 栈上）
fn interp_frame(body: &BytecodeBody, args: Args, region: &mut OperandRegion) -> Value {
    let base = region.reserve(body.desc.total_frame_size); // slaved bump
    load_args_into_slots(region, base, args, &body.desc);
    let mut blk = 0; let mut ip = 0;
    loop {
        match body.code[blk][ip] {
            Bin(op, rd, ra, rb) => { /* 读写 region[base+slot] */ }
            Field(rd, rb, off)  => { /* 已冻结偏移 */ }
            Switch(ri, targets) => { blk = targets[read(ri)]; ip = 0; continue; }
            Call(target, aregs, rd, unwind) => {
                let a = gather(region, base, aregs);
                let r = match target {
                    Interp(callee) => interp_frame(callee, a, region),   // 宿主递归(模型 A)
                    Compiled(ptr)  => i2c_call(ptr, a),                  // native call + 适配
                    Foreign(h)     => os::dispatch(h, a),               // §os 边界
                };
                write(region, base, rd, r);
            }
            Drop(ri) => run_drop(body.desc.drop_of(ri), region, base),
            Ret(ri)  => { let v = read(region, base, ri); region.restore(base); return v; }
            ...
        }
        ip += 1;
    }
}
```

- 纯 Rust、tree-walking；guest 调用 → 宿主递归（模型 A）。
- `region.reserve/restore` = slaved 操作数区。
- 编译 callee 走 native call；解释 callee 走递归；foreign 走 os:: 边界。

---

## 7. Unwind（**最硬的部分，spike**）

模型 A 的代价：guest 帧在 native 栈上，unwind 要**走一条混着解释帧 + 编译帧的 native 栈**，逐帧跑 guest Drop。
这比 tier-0（模型 B，我们自己 pop Vec<Frame>）难。老实说这是模型 A 换 JIT 无缝的账。

需求：guest panic → 沿 native 栈退帧、按 guest 顺序跑 Drop、被 catch_unwind 帧接住或穿出 main（退 101）。

### 候选机制（待 spike 定）

- **A. 复用平台 unwinder（libunwind + personality）**：
  - 编译帧：Cranelift 发 landing pad（2025 已支持），跑 guest Drop——**和 native Rust 编译码同款**。
  - 解释帧：`interp_frame` 是 Rust 函数，用 landing pad / catch 参与，捕获 unwind → 跑本帧 guest Drop → 重抛。
  - 优点：与 Cranelift 天然一致、普通 C 的 abort 语义天然（穿 C = abort）。难点：guest unwind 与"宿主 Rust
    unwind"要共用一套 personality，得设计 guest 异常对象 + personality routine。
- **B. 自研栈行走器（HotSpot 式）**：我们按帧元数据自己走 native 栈、自己跑 Drop。
  - 优点：完全掌控。难点：要能识别并走过 Cranelift 帧（读它的 unwind info），工作量大。

**定为候选 A（2026-07-05，因 JIT 已定 Cranelift，见 §7.5）**：借 Cranelift 已有的 landing-pad 机制，
少造轮子——cg_clif 对全 Rust 已趟通 MIR→Cranelift+unwinding，风险大降。仍需 spike 验证"解释帧 + 编译帧
混合栈上 guest 异常的传播 + Drop 顺序 + catch_unwind"。**这是 M4 前置 spike 的头号项。** 候选 B（自研
栈行走）作为兜底保留。

**Spike 3 验证通过（2026-07-07，history/spike3-mixed-stack-unwind.md）**：宿主 panic 机制（= 同一平台
unwinder + Rust personality，候选 A 的具象）在混合栈上传播 + Drop 顺序（内层先）+ catch_unwind +
当时探针覆盖的普通 C 跨界 abort 全部与 native 逐位一致，含 landing pad 内再入混合执行（cleanup 链调编译 helper）。
**候选 A 坐实，候选 B 退役为纸面兜底。** 帧 ABI unwind 维度封版雏形：解释帧 = CleanupGuard + 动态
unwind_edge（动态 LSDA）+ region 恢复；编译帧 = 静态 LSDA + landing pad；单条 native 栈 ⇒ unwinder
天然逐帧内层先，VM 侧零协调。残余：真 Cranelift LSDA 发射留 M4 复核（与 vmctx 内部约定同一检查点）；
JIT 调用约定必须 unwind-capable（plain "C" = abort shim，给普通 C abort 兜底）。

**Spike 5 收窄残余（2026-07-07，history/spike5-cranelift-adapters.md）**：**CFI 传播已用真 Cranelift
验证**——`create_unwind_info` → gimli .eh_frame → `__register_frame` 自注册后，guest panic 正确
穿过真 JIT 帧（裸跑如预期 SIGABRT：cranelift-jit 不注册系统 eh_frame；其 wasmtime-unwinder 异常
路线与宿主 unwinder 不互操作，**正式不采**）。M4 仅剩 **landing pad/LSDA**（JIT 帧内跑 drop glue
+ catch，cg_clif personality/异常表先例）。i2c/c2i/cc→cc 直调也已真 Cranelift 坐实（§3 适配器
模型从替身升级为实证）。

跨普通 C 边界：**abort**；跨 `C-unwind` 边界则允许传播并跑 cleanup。现行分治合同
见 [c-unwind-contract.md](c-unwind-contract.md)。

---

## 7.5 JIT 后端与字节码分发（2026-07-05 定）

### JIT 后端 = Cranelift，藏在 `JITBackend` trait 后

- **选 Cranelift**：为 JIT 而生（≈10× 快于 LLVM 的编译，代码质量 ≈V8 慢 2%/比 LLVM 慢 ~14%），正合我们
  目标（JIT ≈ debug build，编译延迟重要、峰值不重要）。**cg_clif 已把 MIR→Cranelift 全 Rust 映射好**
  （Rust ABI、layout、2025 unwinding），复用其知识。Wasmtime/Wasmer/SpiderMonkey baseline 生产验证。
- **`JITBackend` trait 抽象**：`compile(BytecodeInstance) -> (code ptr, unwind info, ...)`；VM 核心只对
  trait 说话，Cranelift 是第一个 impl（os:: 同一纪律 P7）。copy-and-patch（CPython 3.13 式，运行期无后端
  依赖、更可移植但代码较差/内存膨胀）记为**备选**，将来想更少运行期依赖再评估。
- **耦合可控**：真正抽不掉的只有①调用约定②unwind 模型，且**都不是 Cranelift 特有**——①我们本就用
  Rust ABI（tier-0 fn_abi），cg_clif 也用 → 共享约定 = "Rust ABI"，co-design 负担小；②landing-pad unwind
  就是 Rust 原生方式，绑它 ≈ 绑"Rust 怎么 unwind"，逃不掉。**不为 Cranelift 牺牲**内存/线程/元数据/真实地址模型。

### 字节码贴近 MIR + 两级结构

字节码贴近 MIR（不下沉到 CLIF 级），让解释器与 Cranelift JIT **共享同一 MIR 级真理源**、复用 cg_clif 的
MIR→Cranelift。分发与运行两级：

```
mirvmc:   rustc 前端(全 check) → Stable MIR(rustc_public + serde) → 序列化为 .mirvm 分发件
                                （版本化，= .class/.jar 类比；建在 Stable MIR 而非裸内部 MIR，有稳定性故事）
mirvm 运行期"class loading":  .mirvm → 按 target 冻结 layout(C8) → 降低为
                    ├─ 解释器的已解析寄存器字节码（偏移/调用/vtable 全解析，§4/§5）
                    └─ 喂 Cranelift 的 JIT 输入（复用 cg_clif MIR→CLIF）
                    （每平台一次，缓存；≈ Java classfile→verify→interpret/JIT、CPython .pyc→specialize→JIT）
```

- **基座 = Stable MIR / `rustc_public`**：跑 rustc 全分析（所有 check），serde 序列化"单态化体 + 带 layout
  的类型元数据 + 符号名"成自包含文件，消费端**不链接 rustc**。它专门解决"MIR 版本绑定"（SemVer 转换层）。
- **版本绑定诚实**：mirvm 字节码像"classfile 有版本号"——runtime 要匹配或转换（可管理，非"任意 mirvm 跑
  任意字节码到永远"）。

### 分发格式：多 target 打包（**定，2026-07-05 用户确认**）

**单产物"跑任意 target"对完整 Rust 理论上不可能**：根本障碍是 `cfg`——rustc 编译期按 `--target` 剪枝
cfg，不同 target 是**字面不同的程序**，rustc 无 target 无关 MIR 输出（叠加 usize/可观测 layout/const-eval）。
Java 能是因为它无编译期 target cfg、layout 由 JVM load 时定、基本类型定长——Rust 三条全违反，是语言固有性质。

**分发格式 = 多 target 打包（fat artifact）**：mirvmc 对一组选定 triple 各跑一次前端（rustc 交叉），
打包 N 份 target 特定 Stable-MIR 段；运行期挑匹配段 load。**消费端零工具链、一文件覆盖常见平台**（.jar 的
实际价值），复用前端 + 跳 codegen/链接（快）+ 运行期还能上 JIT。代价：mirvmc 交叉编译 N 次、artifact ×N
（仅元数据，可压缩/按需下载段）。os:: 是**每平台 build 时选定**（Linux/macOS impl），与分发格式无关。

- **段格式**：`.mirvm` = 一个容器，头部 target 索引（triple → 段偏移），各段 = 该 triple 的 Stable-MIR
  序列化 + 冻结元数据。运行期按自身 triple 查索引，命中则 load，未命中报"不支持的平台"。
- **按需**：段可各自压缩、可支持"只下载匹配段"（网络分发时）。
- **默认 triple 集**：x86_64/aarch64 × linux(gnu/musl)/darwin/windows 的常见组合；mirvmc 可配。

---

## 8. 真 OS 线程集成（C8）

- 每 guest 线程 = 一 OS 线程；其解释帧用**该 OS 线程的 native 栈**，其 slaved 操作数区是**该线程私有**的一块。
- **thunk 重入（native→解释，C8）**：pthread thread_start / C 回调的 thunk = 一个 native 桩，等价于 c2i：
  在**当前 OS 线程**上 `interp_frame(thread_start_instance, args, thread_local_region)`。因操作数区每线程私有、
  native 栈每线程私有，天然并发安全。
- 编译帧同理跑在各自 OS 线程 native 栈上。
- **Sync 要求**：BytecodeBody / 冻结元数据发布后只读共享（C8）；操作数区每线程私有；Rust Heap 分配走 per-thread
  arena（C8）。→ 引擎 Sync，无 GIL（VM tier）。

---

## 9. 深递归 / 栈溢出

- guest 深递归 → 宿主 interp_frame 深递归 + 操作数区增长 → **native 栈界**（≈ native Rust，忠实，§3.5）。
- 编译帧比解释帧小 → 热递归编译后能更深。
- 优雅捕获：可在 interp_frame 入口查 native 栈剩余（guard page / 栈指针阈值），到界抛 guest 栈溢出（≈ native abort）。

---

## 10. 开放问题 / spike 清单

1. **Unwind 机制（§7）**：候选 A（复用 Cranelift landing pad + Rust personality）vs B（自研栈行走）。**头号 spike**：
   混合栈 guest 异常传播 + Drop 顺序 + catch_unwind + 普通 C abort / C-unwind 传播。
2. **帧局部存储（已裁决）**：解释器正式使用 slaved ByteRegion；alloca 不预设更快，只有
   真实解释器负载证明端到端收益时重开（decision-history §7.49）。
3. **调用约定细节**：基于平台 C ABI 还是自定义 Cranelift CC；聚合传参与 rustc ABI 对齐的具体做法。
4. **JIT tiering 策略**（M5）：何时编译、OSR 要不要（先不做，调用边界处编译整方法）、去优化。本草图只保证"编译后可无缝接入"，不定策略。
5. **thunk/closure 生成**：libffi closure 还是自生成小桩；与 c2i 适配器合并。
6. **字节码验证/降低管线**：MIR→字节码 pass、冻结元数据的缓存与内容寻址（与 sysroot 缓存呼应）。
7. **vmctx 传递机制**（→ docs/designs/vmctx-passing.md，2026-07-07）：**边界已被逼定**——FFI 逃逸指针/回调/
   信号的入口必须 TLS 按当前线程查找执行态 + 惰性 attach（JNI AttachCurrentThread 同款，归 os::thread）；
   被三条约束逼死：plain-C 逃逸（签名不能带隐藏参）、ctx 每线程一份（捕获式 thunk 跨线程原理错）、
   信号在任意线程跑。**内部约定待 M4 定**：显式 vmctx 首参 vs Cranelift pinned reg（r15），配多入口
   （f_boundary 读 TLS → tail-call f_fast(ctx,…)，HotSpot verified/adapter entry 同构）。红利：thunk
   收窄回本职（仅解释态逃逸需要）。spike 阶段暂用显式参。

---

## 11. 建议的 spike 顺序（M4 前，验证地基）

1. **最小模型 A 骨架**：`interp_frame` tree-walking + slaved 操作数区 + 寄存器式字节码，跑通纯计算（fib）。
   与 tier-0（InterpCx）差分对拍（tier-0 转 oracle）。
2. **interp↔compiled 适配 spike**：手编一两个函数为 native code（先手写/或最小 Cranelift），验证 i2c/c2i +
   混合栈调用跑通。
3. **Unwind spike（头号）**：混合栈上 guest panic + Drop + catch_unwind + 普通 C abort / C-unwind 传播，选定机制 A/B。
4. **并发 spike**（并入并发 RFC）：N 宿主线程各跑 interp_frame，共享只读字节码 + per-thread 区/arena，**过 TSan**。

过了这 4 个 spike，模型 A 的地基就验证了，可进 M4 正式实现。
