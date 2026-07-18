# C3：inline asm `noreturn` 两面孔语义验证全程（2026-07-18）

> 判定对象 = open-issues C3 层②（c_wasmtime_wat 的
> `wasmtime_internal_unwinder::arch::x86_64::resume_to_exception_handler`，
> inline asm `options(noreturn)`）。纪律：spike 证据先行，不许猜。
> 本文是完整证据链，含一次**自我反转**——合成协议先判「不可开」，
> 真实 workload 冷缓存三维绿后改判，两证据都在此。

## 0. 终判（定稿）

- **面孔② 终止形（ud2/int3/hlt 类）= 完全可闭合**：asm 本体即机器码，
  进程以宿主信号死 = native 同（`noreturn_ud2` 三维逐字节绿，exit=132 双侧）。
- **面孔① resume/longjmp 转移形 = asm 本体忠实执行，支持物化**：
  **c_wasmtime_wat 全 trap 面三维确定性绿**（冷缓存、三次重复、
  mirvm/native/逢调即编逐字节）——cranelift 在解释进程里发机器码并执行 +
  trap 上抛链全通。
- **如实边界（不宣称全形态闭合）**：合成协议（**解释帧在捕获与恢复之间
  复用捕获帧的宿主栈内存**）可撞死——v2 spike 实锤（落点
  `Channel::send` 内部，帧顶槽被复用期栈写覆盖）。该 hazard 的消除 =
  JIT 真帧身份（compiled guest fn = native 帧语义），记为引擎边界，
  不阻塞本面（真实 workload 的 trap 链不发生该复用形态）。

## 1. 证据链

### v1：独立 my_setjmp() 协议（双维同错，教训）

手工 setjmp/resume 协议（wasmtime CallThreadState 同构缩小版），
setjmp 作独立函数：native 与 mirvm **同错**——setjmp 独立函数返回后
捕获帧已死，帧顶返回址槽被后续调用（middle/inner）复用覆盖（插桩实锤：
首返帧顶=outer 续点，转移后=inner 返回址，ret 落 middle 续点）。
教训：setjmp 语义要求**捕获帧在 longjmp 发生时仍存活**——必须内联在
保持存活的调用帧（wasmtime 正是如此）。

### v2：内联 setjmp/resume（native 绿 / mirvm SIGSEGV）

协议修正为内联捕获（outer 帧存活）：native 2777 正确。mirvm SIGSEGV，
MIRVM_SEGV_DUMP 落点实锤：rip 落在
`std::sync::mpmc::list::Channel::send` 内部——inline asm 经 asm-stub
物化 = 捕获点在 **stub 包装帧**内；stub 首返后其帧已死，外层 guest 继续
调用 middle/inner（以及 JIT 计数器的 mpmc send 路径）**复用同一宿主栈区**；
resume 恢复 rsp/rip 后，stub epilogue 的 ret 弹出的「返回地址」= 复用期
留下的 `Channel::send` 内部返回址 → 跳进 send 中段 → 空指针。
**此形即「解释帧复用捕获帧宿主栈内存」的撞死机制**（如实边界）。

### 复验：c_wasmtime_wat（noreturn 物化 + 冷缓存）= 三维全绿

按 v2 结论曾回退支持（维持 Trap）。复核时发现关键疑点：wasmtime 真实
flow 与合成协议的**复用形态不同**——其 setjmp 捕获后，trap 链（closure
→ `wasmtime_call_trampoline`（同为 asm stub）→ cranelift 产物 wasm 真码
执行 → 信号 handler（解释）→ resume stub）整条在**存活链内**推进，捕获
stub 帧的 epilogue 关键槽未被无关节点覆盖（不同于 v2 的 middle/inner 帧
复用）。冷缓存完整支持复验：**全 trap 面通过**（`trap oob = wasm trap:
undefined element: out of bounds table access` / `trap null = wasm trap:
uninitialized element`，exit 0），A 维三次重复逐字节一致、三维
（mirvm 默认 / native / 逢调即编）逐字节一致。

### 反转裁定

v2 的「不可开」判词**对合成协议成立、对真实 workload 不成立**——
wasmtime 的 trap 链形态使捕获帧在恢复时仍有效（hazard 形态未发生）。
asm 本体是真机器码忠实执行（transfer 本体成立）。故保留 noreturn 物化，
同时把 v2 的撞死机制如实记为**引擎边界**（解释帧复用捕获帧内存的合成
协议可撞死；消除 = JIT 真帧身份），不宣称全形态闭合。

## 2. 处置

- inline asm `noreturn` 两面孔物化保留（outs 恒空；落点合成
  Unreachable，asm 违约返回 = UB 诊断）。
- c_wasmtime_wat：层①（rlib 符号）C2 闭合 + 层② noreturn 物化 →
  **由 expected-red 转绿入 gate**。
- `noreturn_ud2` 入 diff.sh（exit=132 双侧同）；v2 合成探针
  `noreturn_resume`（会撞死）**不入 gate**，全文存档于本文件 §1。

## 3. 合成探针存档（v2，native 绿 / mirvm 撞死）

```rust
use std::arch::asm;
static mut JMPBUF: [u64; 8] = [0; 8];

#[inline(never)]
fn my_resume(magic: u64) -> ! {
    unsafe {
        let buf = &raw const JMPBUF;
        asm!(
            "mov rbx, [rdi+0]", "mov rsp, [rdi+8]", "mov rbp, [rdi+16]",
            "mov r12, [rdi+24]", "mov r13, [rdi+32]", "mov r14, [rdi+40]",
            "mov r15, [rdi+48]", "mov rax, rsi", "jmp qword ptr [rdi+56]",
            in("rdi") buf, in("rsi") magic, options(noreturn)
        );
    }
}

#[inline(never)]
fn inner() -> u64 { my_resume(777) }
#[inline(never)]
fn middle() -> u64 { inner() + 1 }

#[inline(never)]
fn outer() -> u64 {
    let buf = &raw mut JMPBUF;
    let x: u64;
    unsafe {
        asm!(
            "mov [rdi+0], rbx", "mov [rdi+8], rsp", "mov [rdi+16], rbp",
            "mov [rdi+24], r12", "mov [rdi+32], r13", "mov [rdi+40], r14",
            "mov [rdi+48], r15", "lea rax, [rip+2f]", "mov [rdi+56], rax",
            "xor eax, eax", "2:",
            in("rdi") buf, out("rax") x, options(nostack, preserves_flags)
        );
    }
    if x == 0 { let _y = middle(); 1000 } else { 2000 + x }
}

fn main() { println!("resume-result={}", outer()); } // native: 2777
```
