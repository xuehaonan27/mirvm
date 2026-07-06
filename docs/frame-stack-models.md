# 帧栈模型 A vs B —— 详细对照（M4 字节码 VM 帧布局的地基决策）

> 目的：把"guest 调用帧放在 native 栈上（模型 A）"还是"放在独立的 VM 帧栈里（模型 B）"
> 两条路的**具体做法**讲透，供 M4 设计前研判。本文只讲机制与权衡，不下最终裁决。
> 配套：DESIGN.md §5/§6、账本 C8。调研来源见文末。

---

## 0. 术语与问题定义

### 两个栈

任何解释器运行时，一条线程上都存在**两个概念上的栈**：

- **native 栈**：真实 OS 线程的 C 调用栈。解释器自己的代码（dispatch 循环、FFI 调用、thunk）在这上面。
- **guest 帧**：被解释程序的调用记录（activation record）。**放哪，就是本文的问题。**

### 一个"帧"包含什么

guest 的每次函数调用产生一个帧（activation record），至少含：

```
- 局部变量槽 (locals)          ← MIR 的 _1, _2, ...
- 操作数/临时槽 (operand slots) ← 表达式求值中间值
- 指令指针 (IP / bytecode ptr)  ← 当前执行到哪
- 返回信息 (caller 帧引用、返回值该写回哪)
- unwind 信息 (cleanup/catch 边)
```

### 两条正交的轴（常被混为一谈）

1. **帧放哪**：native 栈（A） vs 独立 VM 栈（B）—— 本文主轴。
2. **派发方式**：host 递归 vs flat dispatch 循环（while + computed-goto）—— 影响速度，与轴 1 相关但不等同。

**速度主要由轴 2 决定**（好的 dispatch 让 B 也能顶级快，见 LuaJIT），别把"B=慢"当成结论。

---

## 1. 模型 A：guest 帧在 native 栈上

核心：解释一个 guest 调用时，**在真实 native 栈上产生对应的帧**。两种实现变体。

### 1.1 变体 A1：树遍历 / host 递归

最朴素：每个 guest 调用 = 一次宿主函数递归调用。guest 帧的状态就是宿主函数的栈帧。

```rust
// guest 调用 = 宿主递归调用；guest 帧 = 宿主帧
fn interp_call(f: &Body, args: &[Val]) -> Val {
    let mut locals = vec![Val::Uninit; f.num_locals];   // ← 这些局部在 native 栈上
    load_args(&mut locals, args);
    let mut ip = 0;
    loop {
        match &f.code[ip] {
            Op::Add(a, b, dst) => locals[*dst] = locals[*a] + locals[*b],
            Op::Call(g, argregs, dst) => {
                let sub = gather(&locals, argregs);
                locals[*dst] = interp_call(g, &sub);   // ★ HOST 递归：guest 帧嵌在 native 栈
            }
            Op::Ret(r) => return locals[*r],
            _ => { /* ... */ }
        }
        ip += 1;
    }
}
```

native 栈实况（guest 调 main→foo→bar）：

```
 native 栈 (向下增长)
 ┌────────────────────────────┐
 │ interp_call(main) 的帧      │  ← 含 main 的 locals
 │   interp_call(foo) 的帧     │  ← 含 foo 的 locals
 │     interp_call(bar) 的帧   │  ← 含 bar 的 locals  ← 当前
 └────────────────────────────┘
```

- guest 深递归 → 宿主深递归 → **native 栈溢出**。
- 实现最简单；但每个 guest 帧背着一个完整的宿主帧（interp_call 的局部、match 开销），较臃肿。
- Crafting Interpreters 的 tree-walker、许多教学解释器是这一类。

### 1.2 变体 A2：模板解释器（HotSpot 式）

工业级：解释器本身是**启动时生成的机器码**，把 guest 帧按**VM 定义的布局**直接摆在 native 栈上，像真编译函数一样。

native 栈上的一个 guest 帧布局（HotSpot 风格）：

```
 native 栈 (向下增长)          一个 guest(interpreter)帧:
 ┌───────────────────────┐    ┌──────────────────────────┐
 │ ...caller guest 帧...   │    │ 局部变量区 (locals)         │
 ├───────────────────────┤    │ 监视器/锁记录               │
 │ 本 guest 帧 ──────────▶ │    │ 帧元数据: method*, 常量池*   │
 │                        │    │ 上一帧 fp、返回 bytecode ptr │
 ├───────────────────────┤    │ 操作数栈 (operand stack)     │
 │ ...callee guest 帧...   │    └──────────────────────────┘
 └───────────────────────┘    (native fp/rbp 指向本帧)
```

- **CALL 字节码的模板**：在 native 栈上下压一个新 guest 帧（调 rsp），从操作数栈搬实参到 callee 的 locals，跳到 callee 第一条字节码的模板。
- **派发**：每条字节码模板末尾"取下一条字节码、跳其模板"（threaded / computed-goto），不回宿主循环。
- **关键**：帧布局是 VM 与 **JIT 共享**的——所以编译帧和解释帧在同一条栈上能互相调用、能 OSR（见 §3）。
- guest 深递归 → native 栈（有界，HotSpot 靠 guard page + handler 抛 StackOverflowError）。
- 实现最复杂（近汇编级：栈帧布局、寄存器约定、GC map、与 JIT 的 ABI 协调）。

### 1.3 模型 A 的共性

- guest 帧 = native 栈上的真实内存，地址天然真实、可取址。
- guest 栈深受限于 native 线程栈大小（8MB/2MB 级）。
- guest 调用控制流走 native 栈（递归或模板跳转）。
- **代表**：HotSpot（A2）、V8 Ignition（A2 风格）、教学 tree-walker（A1）。

---

## 2. 模型 B：独立的 VM 帧栈

核心：guest 帧放在**解释器自管的、与 native 栈分离的栈**里；dispatch 循环在 native 栈上保持**扁平**。

### 2.1 变体 B1：堆帧对象 / 帧数组

每个 guest 帧是独立的结构（堆分配对象，或数组里的元素）。**我们当前 tier-0（InterpCx）就是这个**：`stack: Vec<Frame>`。

```rust
struct Frame { locals: Vec<Val>, ip: usize, body: BodyId, ret_to: (FrameIdx, Slot) }

struct Machine { stack: Vec<Frame> }   // ★ guest 帧在这，不在 native 栈

fn run(m: &mut Machine) {
    'outer: loop {
        let top = m.stack.len() - 1;
        loop {
            let op = m.stack[top].current_op();
            match op {
                Op::Add(a, b, dst) => { /* 改 m.stack[top].locals */ }
                Op::Call(g, argregs, dst) => {
                    let args = gather(&m.stack[top], argregs);
                    m.stack.push(Frame::new(g, args));   // ★ push 到 VM 栈，非 native 栈
                    continue 'outer;                     // 循环拾取新栈顶
                }
                Op::Ret(r) => {
                    let v = m.stack[top].locals[r];
                    m.stack.pop();                       // ★ pop VM 栈
                    if let Some(c) = m.stack.last_mut() { c.write_ret(v); continue 'outer; }
                    else { return; }                     // 根帧返回 = 结束
                }
                _ => {}
            }
            m.stack[top].ip += 1;
        }
    }
}
```

native 栈实况（guest 调 main→foo→bar）：

```
 native 栈 (浅、扁平)          VM 帧栈 (Vec<Frame>, 堆上)
 ┌──────────────┐            ┌──────────────┐
 │ run() 一个帧  │            │ Frame(main)   │
 └──────────────┘            │ Frame(foo)    │
   ↑ 永远就这么浅              │ Frame(bar) ◀─ │ 当前
                             └──────────────┘
```

- guest 深递归 → `Vec<Frame>` 在堆上增长，**native 栈不动**，可递归极深。
- 每帧一个 `Frame` 结构（`Vec<Val>` locals 各自堆分配）→ 分配开销、指针间接、cache 不友好。**这是 B1 的慢点，也是 fib(27) 慢的一部分。**
- CPython <3.11 的 frame 对象、Miri、我们 tier-0 属此类。

### 2.2 变体 B2：连续帧栈（CPython 3.11 / Lua / LuaJIT）

B 的高性能形态：一整块**每线程连续缓冲**，帧在里面 bump 分配；dispatch 扁平。

```
 每线程一块连续 buffer (data stack / value stack):
 ┌─────────────────────────────────────────────────────────────┐
 │ [main 的 locals+operands][foo 的 ...][bar 的 locals+operands..│  ← SP 在此
 └─────────────────────────────────────────────────────────────┘
        ▲base(main)      ▲base(foo)    ▲base(bar)   ← 各帧起点
 帧元数据(ip/body/prev-base) 另存 CallInfo 栈(Lua) 或内联(CPython _PyInterpreterFrame)
```

CALL / RETURN（无 malloc、无宿主递归）：

```rust
fn call(vm: &mut Vm, g: BodyId, argc: usize) {
    let new_base = vm.sp - argc;              // ★ 实参已在栈顶 → 直接当 callee 的 locals 前缀(零拷贝重叠)
    vm.push_callinfo(CallInfo { base: new_base, ret_ip: vm.ip, ret_body: vm.body });
    vm.sp = new_base + g.frame_size;          // bump：分配 callee 帧区
    (vm.body, vm.ip, vm.base) = (g, 0, new_base);   // 切"当前帧"
    // 回到 flat loop，从 callee 第一条指令继续，没离开循环
}
fn ret(vm: &mut Vm, retslot: usize) {
    let v = vm.stack[vm.base + retslot];
    let ci = vm.pop_callinfo();
    vm.stack[ci.base /* 或返回槽 */] = v;
    vm.sp = ci.base;                          // pop：降 SP
    (vm.body, vm.ip, vm.base) = (ci.ret_body, ci.ret_ip, ci.prev_base);
}
```

- **拆两个栈**（Lua/LuaJIT）：**value 栈**（locals+operands，连续 `TValue` 数组）+ **CallInfo 栈**（帧元数据）。数据局部性好、职责分离。
- CPython 3.11 把 `_PyInterpreterFrame` 连续摆在 per-thread data stack，**Py→Py 调用不再递归 C 栈**——实测递归密集 **1.7×**、通用 3-7% 提速。
- 深递归 → buffer 增长（realloc / chained chunks），native 栈不动，且可**优雅检测溢出**（每次 CALL 查 SP 上限）。
- 分配 = bump（无 malloc），cache 友好。**这是 B 能做到顶级快的形态。** LuaJIT 的解释器（手写汇编 + 此结构）是最快解释器之一。

### 2.3 模型 B 的共性

- guest 帧在 VM 自管内存里；native 栈只放 dispatch 循环 + 跨边界帧。
- guest 栈深与 native 栈解耦（可控、可优雅检测）。
- 帧是"VM 拥有的对象/区域" → 可保存/恢复/自省（协程、调试器的基础）。
- **代表**：CPython 3.11+（B2）、Lua/LuaJIT（B2）、BEAM（B2 极致）、CPython<3.11 与 Miri/我们 tier-0（B1）。

---

## 3. 横切场景：两模型分别怎么处理

下表是全景，逐项详解在后。★ = 该模型明显更优。

| 场景 | 模型 A（native 栈） | 模型 B（独立 VM 栈） |
|---|---|---|
| FFI 出（guest 调 C） | 自然嵌套 | 自然嵌套（无硬桥接） |
| 回调入（C 调 guest, thunk） | ★ 帧统一，天然 | 可行，需标记 VM 栈"段边界" |
| 真 OS 线程 | ★ 复用 OS native 栈，零额外分配 | 每线程一个 VM 帧栈（小额外分配） |
| 深递归 | native 栈界，难优雅捕获 | ★ 可控增长、优雅检测溢出 |
| 栈溢出忠实性 | ★ ≈ native 深度 abort | 需人为设界才忠实 |
| `&local` 传 C | ★ 天然真址 | 真址（可能需"spill 到内存"） |
| unwind / panic | 需显式退 native 帧（难，尤其 A1） | ★ 纯 VM 栈操作，宿主栈不动 |
| JIT 集成（M5） | ★ 帧统一、OSR 无缝 | 需 interp↔compiled 桥（LuaJIT 已验证） |
| 协程 / 栈式挂起 | 极贵（要拷 native 栈，见 Loom） | ★ 廉价（切 VM 栈段） |
| async（Rust 无栈） | 无关 | 无关 |
| 调试 / backtrace | 靠帧布局元数据走 native 帧 | ★ 帧是对象，易走 |

### 3.1 FFI 出（guest → C）

两者都：dispatch（在 native 栈上跑）发起一次真 native 调用，编组参数、call、取返回值。
- A：C 帧压在 guest 帧正下方，同一条 native 栈，天然。
- B：guest 帧在 VM 栈里"暂停"在原位；C 帧压在 dispatch 之上的 native 栈。返回后 dispatch 续跑。
- **差异：几乎没有。** 出方向两者都只是"native 栈上多压一个 C 帧"。

### 3.2 回调入（C → guest，thunk / libffi closure）

C 代码调一个指向 guest 函数的指针（qsort 比较器、pthread thread_start、signal handler）。
- A：thunk 直接在 native 栈上建 guest 帧、进解释（A1 是 `interp_call` 递归，A2 是模板入口）。帧与其它 guest 帧**同构**，天然。
- B：thunk 调一次**嵌套的 dispatch**，guest 回调帧 push 到 VM 栈（续长）。native 栈上是 `[dispatch][C 帧][thunk][嵌套 dispatch]`；VM 栈上回调帧是**一个新逻辑段**，与之前的段被 C 帧隔开。unwind 时要认得这个段边界。
- **差异**：A 更自然（帧统一）；B 要在 VM 栈标记"此处上方是被 native 帧隔开的新段"。都可行，B 多一点簿记。

### 3.3 真 OS 线程

- A：每 guest 线程 = 一 OS 线程，其 guest 帧就用**那条 OS 线程的 native 栈**。零额外分配。HotSpot。
- B：每 guest 线程有**自己的 VM 帧栈**（每线程连续 buffer / Vec）；OS 线程的 native 栈只放 dispatch + FFI。多一份每线程 VM 栈分配（不大）。
- **差异**：A 复用 native 栈；B 多分配一个 per-thread VM 栈。小。注意：**两者都与"1:1 真 OS 线程"兼容**（B 不是为了省线程，那是 BEAM 的 M:N 才需要的；我们不需要）。

### 3.4 深递归 + 栈溢出

- A：受限于 native 线程栈（8MB/2MB 级）。guest 深递归 → native SO，是**信号/崩溃**，优雅捕获难（HotSpot 用 guard page + 信号处理器抛 StackOverflowError，很精细）。深度**≈ native Rust**。
- B：受限于 VM 帧栈上限（可配）。每次 CALL 查 SP，可**优雅检测**并 abort（像 Rust 栈溢出那样）或抛可捕获错误。可动态增长。
- **差异**：A native 界、难捕获、但天然忠实；B 可控、优雅、需主动设界才忠实。

### 3.5 栈溢出忠实性（对"事实标准实现"重要）

一个 native 下会栈溢出 abort 的程序，mirvm 也该在 ≈ 同深度 abort（否则"能在 mirvm 跑 ≈ 能 native 跑"的承诺破了）。
- A：guest 栈就是 native 栈 → 天然 ≈ 同深度溢出。★
- B：VM 栈可无限长 → 默认会"跑得比 native 深"（发散）。**须把 VM 帧栈上限设成 ≈ native 线程栈**才忠实。是个便宜旋钮，但必须做。

### 3.6 `&local` 取址传给 C（极常见：`libc::read(fd, &mut buf, n)`）

- A（模板）：local 在 native 栈真址 → `&local` 直接是真址 → 传 C。天然。（A1：local 在宿主数组里，`&arr[i]` 也是真址。）
- B：local 在 VM 帧栈（若以真址内存承载则）也是真址。**但**若 VM 把 local 当"immediate（寄存器式槽，无地址）"优化，取址时要**spill 到内存**（我们 tier-0 现在就有这个"force to memory"）。
- **差异**：A local 恒在内存可取址；B 可能保 immediate、取址时 spill。小的性能细节，都正确。

### 3.7 unwind / panic（含跨 FFI）

guest panic 要退 guest 帧、逐帧跑 Drop。
- A1：guest unwind ≠ 想用宿主 panic（语义不同）→ 得靠"向宿主递归上层返回特殊 unwinding 结果"显式退帧，与宿主栈纠缠，麻烦。
- A2：显式走 native 帧找 handler（HotSpot 式），要与编译帧协调。
- B：guest unwind = **显式 pop VM 帧、跑 Drop**，宿主栈（flat loop）完全不动。**干净**——这正是我们 D4"panic/catch_unwind 跨平台无痛"的原因（帧是 VM 自己的）。★
- 跨 FFI（panic 要穿过 C 帧）：**两者都 abort**（Rust ABI：unwind 穿 C = UB → abort）。相同。
- **差异**：B 的 guest unwind 明显更干净（与宿主栈解耦）；A 要显式退 native 帧（难，A1 尤甚）。

### 3.8 JIT 集成（M5，Cranelift）——A 的看家优势

Cranelift 生成 native 机器码，其帧**必在 native 栈**。
- A：解释帧与编译帧**同在 native 栈、共享布局** → interp↔compiled 调用近乎无缝、OSR（运行中把解释帧原地换成编译帧）自然。HotSpot/V8 选 A 主要就为这个。★
- B：编译码用 native 栈，解释帧在 VM 栈 → compiled→interpreted 要**桥**（建 VM 帧、进 dispatch）；interpreted→compiled 要**桥**（call native）；OSR 更难（VM 帧 ↔ native 帧互建）。**LuaJIT 证明可行**：它编译整条热 trace（不是单函数），把边界跨越降到最少，trace 要么跑完要么在定义好的 exit 掉回解释器。
- **差异**：A JIT 无缝（其存在理由）；B 需桥、但 LuaJIT 级已验证，靠"编译热区/trace 而非碎调用"控制边界成本。

### 3.9 协程 / 栈式挂起——B 的看家优势（但对我们无关）

- B：VM 帧栈段可**保存/恢复** → 协程 yield = 停在本段、切"当前帧"到另一段。廉价、无 native 拷贝。Lua 协程、Python 生成器、BEAM 进程、Loom 虚拟线程都靠这个。★
- A：要挂起就得存/取 native 栈——标准 C 不能可移植地做。所以 Loom 要一整套 continuation（把 native 帧拷到堆）才做出虚拟线程，重。
- **对 mirvm**：**无关**。我们用真 1:1 OS 线程（不做绿色线程/M:N）；**Rust async 是无栈的**（future 是堆 struct，`.await` 是状态机转移，不经过调用栈）。所以 B 的这个大优势我们用不上，A 的这个大劣势我们也不吃。**这是整件事的枢纽。**

### 3.10 调试 / introspection / backtrace

- B：帧是 VM 对象 → 走栈做 backtrace/调试器/`sys._getframe` 容易。（CPython 需要这个 → 选 B。）
- A：帧在 native 栈 → 靠 VM 掌握的帧布局元数据走 native 帧（HotSpot 能做，较繁）。
- 对 mirvm：Rust backtrace 是 best-effort，两者都够。小。

---

## 4. 真实实现对照

| 系统 | 模型 | 具体 | 为什么这么选 |
|---|---|---|---|
| **HotSpot (JVM)** | A2 | 模板解释器，帧在 native 栈；C1/C2 编译帧同栈 | 要 interp↔JIT 帧统一、OSR 无缝 |
| **V8 Ignition (JS)** | A2 风 | 寄存器字节码，帧在 native 栈；TurboFan 同栈 | 同上（JIT 无缝） |
| **CPython <3.11** | B1+A 控制流 | 堆 frame 对象，但 eval 循环每次 Py 调用**递归 C 栈** | 历史；受 C 栈界（RecursionError） |
| **CPython 3.11+** | B2 | `_PyInterpreterFrame` 连续 per-thread data stack，flat loop | 主动改：更快（1.7× 递归）、帧须活过调用（生成器/introspect） |
| **Lua / LuaJIT** | B2 | value 栈 + CallInfo 栈；LuaJIT 解释器手写汇编 + JIT | **协程**必须独立栈；顶级速度 |
| **BEAM (Erlang)** | B2 极致 | 每进程堆+栈 | 百万级轻量进程（M:N） |
| **Miri / mirvm tier-0** | B1 | `Vec<Frame>`，flat step 循环 | 建在 rustc InterpCx 上，快速起步 |

---

## 5. 对 mirvm 的意义 —— 倾向 A（greenfield + JIT 硬约束下）

> 更新（2026-07-05，用户校正后）：本节初版把两个偏差混了进来——① 用"现有是 B1，升 B2 更近"当技术论据（错：现有 tier-0 是要推倒的错误实现，应 **greenfield 评估**）；② 过度给 B 记了"unwind 干净/深递归"两分（错：那是**无 JIT 的便利**，一旦 JIT 硬约束就蒸发）。去掉偏差、把 **JIT 当硬需求**，天平倒向 **A**。

### 5.1 greenfield + JIT 硬约束下逐条重判

| 我们的约束 | 判定 |
|---|---|
| **JIT 必做（Cranelift = 方法级编译器）** | **A 决定性优势**。编译帧必在 native 栈；A 下解释帧同在 native 栈 → interp↔compiled 是廉价 i2c/c2i 适配器（栈上挪参数）。B 下要建/拆 VM 帧、OSR/deopt 难、且**两条栈要联合走**（unwind/backtrace）。 |
| 真 1:1 OS 线程 + Rust async 无栈 | §3.9 **B 的看家优势（协程/挂起）对我们完全无关**；A 的对应劣势也无关。B 失去存在理由。 |
| 事实标准实现（忠实性） | §3.5 A 天然 ≈ native 深度溢出；§3.4 B"能递归更深"对我们是**发散/不忠实**。→ A |
| unwind 自实现 | §3.7 **有了 JIT，穿越 native 编译帧的 unwind 无论如何都要做**（Cranelift 2025 已支持 landingpad）；B 省不掉还多"两栈联合"。A 只走一条栈。→ 偏 A（B 的干净只在无 JIT 的 tier-0 成立） |
| `&local`→C、回调入、真线程 | §3.6/3.2/3.3 均 A 略优（天然真址、帧统一、复用 native 栈） |
| 中央双向软边界 FFI | §3.1 出方向中立；入方向 A 略优 |

### 5.2 关键再认识：JIT-VM 里解释器是"冷层"

A 的最大代价一直是"解释器更难写（native 栈帧布局）"。但 **JIT-first 的 VM 里，热代码都被编译，解释器是冷层**（HotSpot 解释器也"慢"，无所谓）。所以：

> A 的"解释器更复杂/更慢"这个代价**权重大降**——力气花在 JIT 集成，解释器可起步简单（甚至 tree-walking），因为它不是热点。这消解了 A 的主要缺点。

### 5.3 结论

**greenfield + JIT 硬约束下，mirvm 选 A（guest 帧在 native 栈）。** 佐证：最成功的两个 JIT VM（HotSpot、V8）都是 A，正因 JIT-first。B 的存在理由（协程）我们没有、B 的弱点（跨边界挂起）我们不碰，但 B 的**代价（JIT 难集成）我们全额承担**——B 在有 JIT 的世界对 mirvm 是纯负担。

**A 对 mirvm 的具体形态**：每 guest 线程用其 OS 线程的 native 栈放 guest 帧；解释器按**与 Cranelift 共享的帧布局/调用约定**摆帧，起步可简单（冷层）；interp↔compiled 走 i2c/c2i 式廉价适配器（注意 A 也非零边界，HotSpot 也有适配器，只是同栈故便宜）；unwind 走一条 native 栈（解释帧+编译帧都带 unwind info），跨 FFI panic = abort；栈溢出 ≈ native（忠实）；OSR 可选、后置。

### 5.4 诚实的剩余代价（A 不免费）

- **M4/M5 强耦合**：帧布局 + 调用约定必须**解释器与 Cranelift 共同设计**。→ "帧/调用约定规格"要在动 M4 前和并发 RFC 一起定，不能先写解释器再回头适配 JIT。
- native 栈帧管理比 B2 flat loop 麻烦；tree-walking 起步则每 guest 帧背一个宿主帧（冷层可接受）。
- backtrace/introspection 走 native 帧（靠帧元数据），比 B 的帧对象繁——但我们需求低。

### 5.5 何时才重估 B

几乎不会。唯一情形：若将来要**栈式协程/绿色线程**（我们明确不做，Rust async 无栈）。JIT 的边界成本不是重估理由（那正是 A 解决的）。

---

## 参考

- CPython 帧设计：[cpython/InternalDocs/frames.md](https://github.com/python/cpython/blob/main/InternalDocs/frames.md)、[faster-cpython true inlining #653](https://github.com/faster-cpython/ideas/issues/653)、[3.11 性能分析](https://blog.codingconfessions.com/p/are-function-calls-still-slow-in-python)
- Lua/LuaJIT 栈设计：[Lua 解释器剖析](https://thesephist.com/posts/lua/)、[Coco: LuaJIT 的 C 协程](https://coco.luajit.org/)
- Loom 虚拟线程（native 栈为何贵、continuation 拷贝）：[Rock the JVM: Virtual Threads](https://rockthejvm.com/articles/the-ultimate-guide-to-java-virtual-threads)
- 栈 vs 堆帧一般权衡：[Cornell CS3410 内存笔记](https://www.cs.cornell.edu/courses/cs3410/2025sp/notes/mem.html)
- HotSpot i2c/c2i 适配器、模板解释器：OpenJDK HotSpot `sharedRuntime`/`templateInterpreter`（解释器用表达式栈、编译码用寄存器，故即便同在 native 栈也需廉价参数搬运适配）
