# Rust 无栈 async 与 mirvm —— 实证调研

> 文档状态：**历史调研**。async 降为无栈状态机的结论仍有效；本文对 signal/thunk 或 OS
> handler 的机制描述不证明这些可观察语义已在当前 M4 实现，现状见
> [current-status.md](../current-status.md)。

> 目的：搞清 Rust async 怎么实现（无栈状态机）、mirvm 怎么跑、对帧模型 A/B 有无诉求、
> 以及 async I/O（epoll 等）在 mirvm 上是"直通"还是"emulate"。
> 结论：async 编译期降低成普通 MIR 状态机，**mirvm 零特殊支持即可解释**；对帧模型**无诉求**
> （确认 C11 选 A 不因 async 付代价）；async I/O 是**纯直通真内核**，非 emulation。
> 全部有真 MIR dump + mirvm 实跑对拍佐证。

---

## 1. Rust async 怎么实现：编译期降低成状态机（实证）

`async fn` 被 rustc 的 `StateTransform` MIR pass 在**编译期**转成一个**状态机 struct**。
下面是 `outer()`（await 两个子 future）的真实 MIR dump 前后对比。

### 1.1 变换前（coroutine 语义，有 yield）

await 点的形状：poll 子 future → 看 Ready/Pending → Pending 则 `yield` 挂起。

```
_9 = <inner as Future>::poll(_10, _13)          // poll 子 future
_16 = discriminant(_9)
switchInt(_16) -> [0: bb9 /*Ready*/, 1: bb8 /*Pending*/]
bb8: { ... _19 = yield(()) -> [resume: bb10, drop: bb29] }   // ← Pending：yield 挂起
bb9: { _17 = ((_9 as Ready).0); _3 = _17 }                   // ← Ready：取值，赋给局部 a
```

### 1.2 变换后（纯状态机，yield 消失）

签名变成标准 `poll`，带 coroutine layout（判别式 + 跨 await 存活的字段）：

```
fn outer::{closure#0}(_1: Pin<&mut Future>, _2: &mut Context) -> Poll<u32> {
  coroutine layout {
    field _s0: u32;                 // 跨 await 存活的局部 a
    field _s1: inner future;        // 第一个被 await 的子 future
    field _s2: inner future;        // 第二个
    variants = { Unresumed(0), Returned(1), Panicked(2), Suspend0(3): [_s1], Suspend1(4): [_s0,_s2] }
  }
  bb0: {
    _43 = discriminant((*_44));                       // 读当前状态
    switchInt(_43) -> [0,1,2,3,4 各恢复点]              // ← 状态机派发
  }
  // 挂起点（原 yield）：
  (((*_44) as variant#3).0) = move _4;                // 把子 future 存进自己的 Suspend0 变体
  _0 = Poll::Pending; return;                          // ← yield 变成"存状态 + return Pending"
  // 恢复点：从 bb0 switchInt 跳来，重新 poll 保存的子 future，Ready 则取值继续
}
```

### 1.3 关键结论

- `yield` → "把跨 await 的局部**存进 future 自己的判别式变体字段** + `return Poll::Pending`"。
- 恢复 → "bb0 按判别式 `switchInt` 跳回保存的恢复点"。
- 全程只有 **switchInt / discriminant / 字段读写 / call / return** —— **纯普通 MIR，无 coroutine 专属执行语义**。
- async fn 的"栈"（跨 await 存活的局部）被编译成 **struct 字段**，不是调用栈。**这就是"无栈"的确切含义。**

---

## 2. mirvm 怎么跑：零特殊支持（实证）

future 是内存里一个 struct，`poll` 是普通函数，`.await` 是"call poll + switchInt"。mirvm 当普通 MIR 解释即可。

`demo/async_suspend.rs`（自定义 future 先返回 Pending 一次，逼出真实挂起/恢复循环）实跑：

```
mirvm : result = 25, polls = 3
native: result = 25, polls = 3     ← 逐位一致
```

3 次 poll 对应完整状态机：
- poll 1：outer 进入 → await 第一个 future → Pending（存 Suspend0）→ 返回 Pending。
- poll 2：bb0 判别式跳回 Suspend0 → 子 future Ready 拿到 a=10 → await 第二个（存 Suspend1）→ Pending。
- poll 3：判别式跳回 Suspend1 → Ready 拿到 b=15 → 算 a+b=25 → Poll::Ready(25)。

mirvm 正确解释了"**存判别式 → return Pending → 按判别式恢复**"的全过程，无一行 async 专属代码。

---

## 3. Pin / 自引用 future —— mirvm 真实地址天然处理

future 可自引用（借用跨 await 存活 = 指向 struct 内部的指针），所以 `!Unpin`、必须 Pin 住不动。
mirvm 真实地址模型下：future 在真地址上，只要 guest 遵守 Pin（不移动它），内部自引用就是
**指向该真地址内部的真指针**，天然成立。**模型 A/B 不改变这点**（future 都是真地址上的内存分配）。

---

## 4. 对帧模型 A/B 有无诉求：没有（这是重点）

| | 有栈协程（Lua / goroutine） | 无栈 async（Rust） |
|---|---|---|
| 挂起是什么 | **冻结一个调用栈**、切到另一个栈 | `return Poll::Pending`，状态存进 struct |
| 挂起时解释器调用栈 | 必须**保存 / 可切换** → **强制模型 B** | **彻底 unwind 回 executor**（栈清空） |
| 状态存哪 | 被冻结的那个栈上 | 堆/内存里的 future struct |
| 对帧模型要求 | 强制 B | **A/B 皆可，运行完全相同** |

深层原因：无栈 async 每次 poll 结束，**解释器调用栈彻底退回 executor**——不存在"被挂起的解释器栈"，
状态全在 future struct 里。所以它对"帧放 native 栈还是 VM 栈"**零要求**。有栈协程才需要"保存并切换
调用栈"，那才逼你上模型 B。

**注**：深度嵌套 await（foo 内 await bar 内 await baz）在编译期变成**嵌套 struct**（子 future 存在父
的变体字段里），poll 时是一串 poll 调用（foo.poll→bar.poll→baz.poll），**用调用栈是暂态的**（一次
poll 遍历），每次 Pending 就整串退回，从不冻结。递归 async 需 `Box::pin`（否则无限大小），poll 链走堆
指针。仍是普通 MIR，仍不挂起栈，仍与模型无关。

**→ 确认账本 C11**：我们不做有栈协程（Rust 生态不需要），Rust 无栈 async 对帧模型无诉求 →
选 A（为 JIT）不因 async 付任何代价。

---

## 5. 完整 async 生态（tokio）在 mirvm 上：I/O 是纯直通，不是 emulate

真跑 tokio 程序时，各部分归宿：

| 部分 | 是什么 | mirvm 归宿 |
|---|---|---|
| 任务（future） | 无栈状态机 = MIR struct + poll 调用 | 普通 MIR 解释（已能） |
| executor（tokio 调度器） | 普通 Rust 代码 | 普通 MIR 解释 |
| worker 线程 | 真 OS 线程 | 真线程模型（C8） |
| I/O 就绪（epoll/mio） | 真内核 epoll + 真 fd | **纯直通真 OS**（见下） |
| 任务级 M:N（多 future:少线程） | executor **调 poll** 实现 | **无 VM 栈切换**（无栈），与"逼上模型 B 的 M:N"无关 |

### 5.1 epoll 为什么是"纯直通"而非 emulate

guest（tokio reactor）调 `epoll_create1`/`epoll_ctl`/`epoll_wait`：
- 操作的是**真内核资源**：epfd 是真 fd，被监视的 fd 是真 socket/pipe（guest 从真 `socket()`/`open()`
  拿的——我们本就直通成真 fd）。
- `epoll_event` 结构体：guest 传真地址上的结构体，**target==host 布局一致**，真指针直接递给真 epoll。
- 所以 handler 就是 **FFI 转发到真 `epoll_wait`，在真 fd 上等**。零重实现。
- 阻塞语义：`epoll_wait` 阻塞就阻塞**那条真 OS 线程**（VM tier，同 native）；tier-0 GIL 下"阻塞前放锁"
  （和 read/futex_wait/pthread_join 一样，非 epoll 特有）。

**这和 futex/read 同理——真资源、真线程、只转发。是"用真 OS"，不是 emulate。**

### 5.2 整个 async I/O 栈都是直通

- epoll / socket / bind / accept / recv / send → 真 fd 上直通。
- eventfd（tokio 跨线程唤醒阻塞中的 epoll_wait）→ 真 fd，直通。
- timerfd（定时器）→ 真 fd，直通；clock_gettime 已直通。
- io_uring（tokio-uring 可选）→ io_uring_setup + mmap 共享环；mmap 是真内核内存、真地址，guest 裸访问，直通。
- signal handler（sigaction 注册 Rust 函数，内核信号到达时调）→ **native→解释 回调**，走**和
  pthread_create/thread_start 同一个 thunk 机制**（C8），**不是自建信号系统**，也非 emulate。

### 5.3 术语澄清（避免"shim"再滑动）

`shims.rs` 里的 foreign-call handler 有三种归宿（= C10）：

| 归宿 | 做什么 | 例子 | 占比 |
|---|---|---|---|
| **纯直通** | 转发真 OS、真资源 | **epoll**、socket、read/write、clock_gettime、futex(VM tier)、pthread(除 create) | 绝大多数 |
| **VM 内建** | 因触虚拟化状态才自己实现 | `__rust_alloc`→Rust Heap、pthread_create→thunk、intrinsics | 少数 |
| **合成/适配** | inline asm 没法解释，宿主直算 | libm 数学 | 小批 |

用户厌恶的 emulation 特指**第二类里的过度重实现**（如 tier-0 自建线程调度器）。**epoll 是第一类（纯直通）**。
"async I/O 需要写一批 handler"是**工作量（薄直通）**，不是 emulation。

---

## 6. 结论

- Rust async 编译期降低成**无栈状态机（普通 MIR）**，mirvm **零引擎侧特殊支持**即可解释（实证：与 native 逐位一致）。
- async **对帧模型 A/B 无诉求**（无栈 → 不冻结解释器栈）→ 确认 C11 选 A 不因 async 付代价。
- async I/O（epoll/socket/eventfd/timerfd/io_uring）是**纯直通真内核 fd**，signal handler 走 thunk，**全无 emulation**。
- mirvm 支持完整 async 只欠：这批**直通 I/O handler**（薄，roadmap）+ 真线程（已有）+ tier-0 GIL 的"阻塞前放锁"纪律。

---

## 附：复现命令

```bash
# dump async 状态机 MIR（StateTransform 前后）
rustc --edition 2021 -Zdump-mir='outer' -Zdump-mir-dir=/tmp/mirdump --crate-type lib async_demo.rs
ls /tmp/mirdump | grep StateTransform
# mirvm 实跑 async（手动 poll，无 tokio）
target/release/mirvm run demo/async_suspend.rs   # result = 25, polls = 3，与 native 一致
```
