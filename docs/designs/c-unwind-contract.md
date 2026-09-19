# C 与 C-unwind 跨语言异常合同

> **状态：2026-08-12 已实现并进入 `fast`；2026-08-13 补齐嵌入关闭、signal 安全点
> activation 与 native fini 不可展开边界（E13 现行裁决见 §7）。** 本文规定 mirvm 在 Linux/ELF/x86_64
> 基线上如何处理 Rust panic 与 C++ 异常。`C-unwind` 的白话含义是：这段外部函数
> 边界允许系统展开器带着异常穿过去；普通 `C` 边界不允许。

## 1. 为什么要区分两种 ABI

`extern "C"` 和 `extern "C-unwind"` 的机器传参方式相同，异常规则不同：

- 普通 `C` 边界不允许异常穿过。Rust panic 从这里逃出会终止进程；外来异常反向
  穿入 Rust 属于未定义行为，mirvm 不为它建立成功语义。
- `C-unwind` 允许异常穿过。mirvm 必须执行沿途 Rust `Drop`，并保持异常对象原样，
  使外层 C++ 仍能按原 C++ 类型捕获它。
- `std::panic::catch_unwind` 只保证捕获 Rust panic，不保证捕获 C++ 异常。固定工具链
  当前会在 C++ 异常到达它时终止；这不是把 C++ 异常转换成 Rust panic 的理由。

因此，mirvm 不能把任意 C++ 异常统一转换成 `RunError` 或 guest panic。那样会丢掉
C++ 的类型、对象身份和析构责任，也会偏离 native。

## 2. 修复前实测矩阵

环境为 rustc `nightly-2026-07-02`、g++ 13.3、项目内 libffi 5.1.1（捆绑
libffi 3.6.0）、Linux x86_64。表中“默认”只表示默认配置；短调用未越过热阈值时，
它仍可能由解释器完成，不能当作 JIT 机器码证据。

| 场景 | native | MIRVM `JIT=off` | MIRVM 默认 | MIRVM `SYNC=1, threshold=1`（修复前） | 当时判定 |
|---|---|---|---|---|---|
| `C-unwind` 无异常返回，调用点有 cleanup | 返回 42，Drop | 返回 42，Drop | 返回 42，Drop | JIT 编译在 `try_call` 结果读取处 panic | JIT 真实 RED |
| C++ `Marker{73}` 穿 guest 回调，外层 C++ typed catch | 原类型捕获，Drop | 在 `extern C` callback wrapper 终止 | 同左 | 同时受 JIT RED 与 wrapper 阻断 | callback 真实 RED |
| Rust panic 穿 C++ `catch(...); throw;` 回 guest catch | 原 payload，Drop | 在 `extern C` callback wrapper 终止 | 同左 | 同上 | callback 真实 RED |
| C++ 异常经 direct `C-unwind` 到 Rust 线程根/catch | Drop 后终止，提示 foreign exception | 同 native | 同 native | JIT RED | 不承诺由 Rust catch 捕获 |
| C++ 吞掉 Rust panic | 终止，要求重抛 | callback wrapper 更早终止 | 同左 | 同上 | 必须终止 |
| 异常穿普通 `C` | Rust panic 终止；C++ 异常反向穿入为 UB | 终止 | 终止 | 终止 | 不建立传播合同 |
| 独立 libffi closure，真实 callback 编译为 `C-unwind` | 异常可双向穿过 | 同一宿主探针可穿过 | 同左 | 不涉及 JIT | 推翻“libffi closure 原理不可穿” |

最后一行还核对了捆绑 libffi 的 x86_64 汇编：closure 帧有可供系统展开器使用的
栈信息。旧问题不是 libffi 天然阻断，而是 mirvm 自己把 Rust wrapper 编译成了普通
`extern "C"`。

## 3. 修复后实测结果

正式夹具使用同一 native oracle，并分别运行纯解释器和 `MIRVM_JIT=on`、
`MIRVM_JIT_SYNC=1`、`MIRVM_JIT_THRESHOLD=1` 的强制同步 JIT。需要发布 guest
机器码的执行场景都要求
JIT 日志中包含对应目标函数名的同一发布行出现 `release=true`，且不得出现
`release=false`，防止“名叫 JIT 实际全程解释”的假绿；两个不支持 ABI 场景在
lowering 阶段就应拒绝，不会进入 JIT。

| 场景 | native | 解释器 | 强制同步 JIT |
|---|---|---|---|
| `C-unwind` 无异常返回 + cleanup，30,000 次 | `value=42 drops=30000` | 同左 | 同左，已发布机器码 |
| C++ typed exception 往返 | `result=1073 caught=73 drops=1` | 同左 | 同左，已发布机器码 |
| Rust panic 经 C++ 重抛 | `payload=51 caught=888 drops=1` | 同左 | 同左，已发布机器码 |
| C++ typed exception 穿出整个 Engine | 外层 C++ 捕获 `Marker{73}` | 同左，值仍为 73 | 同左，已发布机器码 |
| C++ exception 到达 guest `catch_unwind` | 提示不能捕获 foreign exception 后终止 | 同左，guest catch 函数不运行 | 同左，已发布机器码 |
| C++ 吞 Rust panic | 终止 | 终止 | 终止 |
| 普通 C callback 中 Rust panic | 终止 | 终止 | 终止 |
| 普通 C wrapper 内 direct `C-unwind` C++ throw | 终止，外层 catch 不返回 | 引擎 `Terminate` 守卫终止 | 同左 |
| 普通 C wrapper 内 fn-ptr `C-unwind` C++ throw | 终止，外层 catch 不返回 | 引擎 `Terminate` 守卫终止 | 同左 |
| 普通 C wrapper 内 direct `C-unwind` Rust panic | 终止 | 引擎 `Terminate` 守卫明确终止 | 同左 |
| 普通 C wrapper 内 fn-ptr `C-unwind` Rust panic | 终止 | 引擎 `Terminate` 守卫明确终止 | 同左 |
| direct foreign 使用非 C/System ABI | 不执行（该声明若实际调用即 ABI 不匹配） | lowering 明确拒绝 | 同左 |
| foreign 回调参数使用非 C/System ABI | 不执行（该声明若实际回调即 ABI 不匹配） | lowering 明确拒绝 | 同左 |

## 4. 收口后的产品合同

| 边界/流向 | 合法 ABI | mirvm 必须做什么 | 稳定观察量 | 不承诺什么 |
|---|---|---|---|---|
| 无异常的出向调用 | `C` 或 `C-unwind` | 按原签名返回；有 cleanup 的 JIT 调用正确取回返回值 | 返回值、Drop 次数 | 无 |
| C++ 异常经 guest 回到 C++ | 全链 `C-unwind` | 保留原异常对象和类型；解释帧/JIT 帧执行 cleanup | typed catch 的值、Drop 次数 | Rust `catch_unwind` 能捕获它 |
| guest panic 经 C++ 回 guest | 全链 `C-unwind`，C++ 只重抛 | 保留原 Rust panic payload；执行 cleanup | payload、C++ 已重抛标记、Drop 次数 | C++ 可以吞掉 Rust panic 后继续 |
| C++ 吞 Rust panic | `C-unwind` | 与 native 一样终止 | 非零退出、终止原因 | 正常返回 |
| C++ 异常到达 guest `catch_unwind` | `C-unwind` | 与固定 rustc 一样终止；guest catch 函数不得运行 | 非零退出、明确的 foreign exception 原因 | 把 C++ 异常伪装成 guest panic |
| Rust panic 越过普通 C callback | `C` | 在该 ABI 边界终止 | 非零退出、终止原因 | stderr 逐字节相同 |
| C++ 异常试图越过 guest 普通 C wrapper | `C` wrapper 内调用 `C-unwind` | 按 rustc 的 `Terminate` 边终止，不让外层 C++ catch 收到 | 非零退出、外层不得正常返回 | UB 情形的具体信号和文字 |
| C++ 异常到达 Engine 顶层 | 全链 `C-unwind` | 不消费、不改写，交回外层系统展开器 | 外层 C++ 仍按原类型和值捕获 | Rust `catch_unwind` 可以接住它 |
| guest panic 逃出 unsafe raw export | guest Rust unwind | 先在 guest 标准库中降低 panic 计数、析构并释放 payload，再报告 `RunOutcome::GuestPanic` | payload `Drop` 恰一次，Engine 可继续调用 | 由宿主猜测 guest std 私有布局 |
| `main` panic 被 `lang_start` 捕获 | guest std 内部 catch | lowering 精确标记包住用户 main 的调用；该次 `run_main` 单独记录 catch 结果 | `GuestPanic` 与正常 `Termination` 返回 101 分立 | 根据数值 101 反猜是否 panic |
| 异步 signal 到 guest handler | 传统的进程定向或 `SI_TKILL` 线程定向 signal | 内核 frame 只原子登记；进程事件归 owner inbox，线程事件归目标 pthread 稳定槽；普通安全点用新租约和新 activation 执行 handler | handler 属于注册它的 Engine；线程事件仍在目标 pthread；不污染外层 main catcher | 在 signal frame 进入 libffi/guest/展开；给进程定向外部事件承诺即时延迟 |
| Engine close 中的 native fini | 不可展开的拆除边界 | 捕住任何 MIRVM、foreign 或宿主 Rust 异常，固定诊断后 `abort` | 非零退出、`native finalizer unwound during Engine teardown` | 把 `EngineFault` 续传回嵌入方；异常逃出后继续关闭 |

终止诊断不是 Rust ABI 的稳定组成，所以负向测试不逐字比较 native 与 mirvm 的
stderr；它锁定非零退出、明确终止原因，以及 C++ handler 不得吞掉后正常返回。

## 5. 实现纪律

1. lowering 从 rustc `ExternAbi::C/System { unwind }` 冻结该位，不得默认为 false。
   其他 ABI 不得伪装成普通 C，必须在进入 libffi 前明确拒绝。
2. direct foreign 的普通 C 调用沿用 libffi 的普通 C 声明；只有 `unwind=true` 才以
   本地 `extern "C-unwind"` 声明调用同一个 `ffi_call` 符号。
3. callback thunk 与 P1 可执行条目按 `ForeignSig.unwind` 选择普通 C 或 C-unwind
   wrapper；两者共用同一搬参和执行本体。
4. MIR 的 `UnwindAction::Terminate` 高于内层 callee 的 ABI。direct foreign 与 native
   fn pointer 都必须经过终止守卫，不能让异常越过 guest 的普通 C wrapper。
5. JIT 的 `try_call` 是终结指令，返回值经正常分支的 `TryCallRet` 块参数取得，不能从
   `inst_results(try_call)` 读取。
6. cleanup 对异常来源一视同仁。不得为了只认 guest panic 而漏掉 C++ 异常下的 Drop。
7. guest panic 和引擎故障使用 MIRVM 自有异常类；捕获点直接取得系统展开器交来的原始
   异常指针并分类。MIRVM **不自研 personality**（决定每一帧如何展开的函数），解释帧
   和 JIT 帧继续使用现有 Rust personality/LSDA 执行 cleanup。
8. 分类自有异常时必须同时核对异常类、ABI cookie、进程内 canary，以及异常内保存的
   `Arc<Shared>` 是否与当前 Engine 指向同一对象。仅看异常类或 Engine 数字 id 不足以
   决定归属；另一个 Engine 的 guest panic 必须继续展开。
9. `EngineFault` 是引擎自身故障，不是 guest panic。每个解释器 raw catch 和 JIT landing
   pad 都必须查看**本次实际收到的异常指针**，只在该对象确为 `EngineFault` 时跳过 guest
   cleanup。不得用“本线程还有某个 fault 未结清”代替异常对象分类：native catch 可以
   暂停外层 fault，然后在同一线程重入一个需要正常 cleanup 的 guest panic。
10. MIRVM 外层异常只保存 guest 标准库原始异常指针。guest catch 时把该指针交回 guest
    catch 函数；未捕获时调用固定工具链的 guest `std::panicking::catch_unwind::cleanup`
    和对应 drop glue。引擎只搬运两个不透明机器字，不读取 std 的私有异常、Box 或
    vtable 布局。
11. 宿主线程 TLS 中保存的是带 nonce 的 `EngineFault` token LIFO 栈，只验证 owner 与
    消费顺序，不决定帧是否 cleanup。外层 fault 被 native catch 暂停后，可以重入 guest
    panic，也可以压入并先结清一个内层 fault；最后再由外层 owner 按栈序结清。
12. 真实 main panic 已被 guest `lang_start_internal` 消费，不能等它越出 Engine 再分类。
    lowering 从固定 std 的真实 MIR 调用图唯一定位包住用户 main 的 catch，冻结为
    `CallRole::MainPanicBoundary`；解释器和 JIT 都在该调用期间认领紧随的第一层
    `catch_unwind`，并写入本次 `run_main` 的 LIFO 状态。可执行模块必须恰有一个该边界且
    unwind action 为 `Continue`；IR 序列化、image 合并、Package owned snapshot 逐函数验证都保留并
    复核此合同。固定 std 改形时 lowering/验证必须响亮失败，不能退回函数名或 101 猜测。
13. native constructor 还处于可失败的启动边界，受控 MIRVM 异常可分类成
    `Result` 失败后走关闭协议。native fini 已进入不可回滚的拆除阶段；专用 raw
    guard 必须对 guest panic、`EngineFault`、`EngineClosed`、foreign exception 和
    宿主 Rust panic 统一诊断后 `abort`。不得复用 guest `Terminate` 守卫对
    `EngineFault` 的续传特例。
14. signal 的固定 22 字节桩与 adapter 只能用固定 TLS 读取和原子操作登记事件：进程定向
    事件写 owner inbox（注册 Engine 的待处理信号箱），`SI_TKILL` 写目标 pthread 按本次
    registration 建立的稳定 cell。它不能 attach `Ctx`、取锁、分配、调用 libffi/guest 或
    展开。普通安全点派送时必须建立全新 activation；线程事件只能在目标 pthread 派送。
    activation 的 nonce 与 `run_main` 状态不得复用被中断执行，避免 handler 的 catch 认领
    外层 `MainPanicBoundary`。handler 异常按普通 C signal 回调的不得展开边界终止。

## 6. 回归入口

```bash
./tests/run.sh suite runtime.c-unwind
```

夹具 `tests/fixtures/c_unwind_contract/` 由固定 Cargo+rustc+C++ 先生成 native oracle，
再用默认 cargoless 分别跑纯解释器和强制同步 JIT。十三项覆盖正常返回、typed C++
exception 往返、Rust panic 重抛、整个 Engine 的 C++ typed passthrough、guest catch
遇 C++ exception 时终止、吞 panic、普通 C callback panic，direct/函数指针两种
`Terminate` 边上的 foreign exception 和 Rust panic，以及外层调用/回调参数两种非
C/System ABI 的明确拒绝。它已进入 `fast`，不再扩成通用 FFI harness。

未捕获 guest panic 的资源归还和真实 main 结果由标准 `runtime.semantics unwind` 段
另外锁定：解释器和强制同步 JIT 各跑一次 payload Drop 中的普通 guest 调用与第二次
panic，再各跑一次真实 lowering + `lang_start_internal` 的 main panic/正常 101 对照，
连同原有九个 unwind 语义用例共 **13/13**。这证明 panic 计数已在 guest 一侧复位、两个
payload 各析构一次、同一 Engine 清理后仍能继续执行，并且相同 OS 退出码没有抹掉 library
API 的结果类别。

## 7. E13 裁决

真实嵌入入口并不需要等“稳定嵌入 API”发布后才出现：当前 library API、P1 可执行入口和
libffi callback 已经能让一个 Engine 的异常穿过另一个 Engine。实测因此推翻 §7.50 的
降级结论；后续严格审查又推翻 §7.51 的线程级 cleanup 判据。完整证据和决策演变保留在
git 历史。

现行选择是：**采用 MIRVM 独立异常类，但不自研独立 personality。** 自有异常只负责给
系统展开器中的对象标明“guest panic 或 EngineFault、属于哪个 Engine”；每一帧如何执行
cleanup 仍交给已有 Rust personality 和 JIT LSDA。原始 guest panic 对象继续属于 guest
标准库，MIRVM 不复制、不解析它；guest catch 或 Engine 顶层消费自有外壳后，必须把内层
指针交回 guest 侧完成捕获或释放。

cleanup 判据来自当前异常对象，而不是 TLS 全局状态。解释器逐帧 raw catch 当前对象，JIT
landing pad 直接分类展开器交来的指针；FrameGuard 只恢复操作数区、影子帧和深度。TLS 的
带 nonce token 栈只保证 `EngineFault` 由正确 owner 按 LIFO 次序消费，因此不会让一个被
native catch 暂停的外层 fault 污染随后重入的 guest panic 或内层 fault。

Engine 执行出口是结构化结果：正常返回为 `RunOutcome::Returned(value)`，未捕获 guest
panic 为 `RunOutcome::GuestPanic`，缺入口/导出或引擎故障为带 `RunErrorKind` 的
`RunError`。真实 main panic 不会越出 guest std，所以 lowering 用
`MainPanicBoundary` 标出固定启动链中的精确 catch，每次 `run_main` 用独立状态栈记录；
CLI 仍把 `GuestPanic` 映射为 101，但正常 `main` 返回 101 不再与 panic 混成同一个库 API
结果。宿主 Rust panic 和 C++ foreign exception 都不被冒充成上述两类；
前者原样续传，后者可穿出整个 `C-unwind` Engine 并由外层 C++ typed catch 接回。

异常本身也参与 Engine 生命周期。MIRVM 异常可以被 native catch 暂停，此时没有活动
Engine 调用栈，但异常以后仍可重抛给原 owner。异常外壳因此持有 `DeferredHold`（延迟
持有），也就是“仍有一件已经发生、尚未消费或删除的工作”的计数凭据；Engine close 必须
等它离开，不能在暂停窗口释放 `Shared`。这个持有只决定对象寿命，不决定帧 cleanup；
cleanup 仍严格按本次实际异常指针分类，保持 §7.52 的结论。

2026-08-13 已继续闭合 E22 的实际生命周期：Engine 用执行租约处理并发 close/执行，
pthread start 与线程私有析构回调用延迟持有覆盖“已登记但尚未开始/撤销”的空窗，`CtxSlot`
让长期宿主线程不再钉住 Shared，逐实例 native constructor/finalizer 也纳入 Closing。
constructor 的受控失败转成 `Result` 并完成关闭；finalizer 是不可展开边界，任何异常
逃出都固定诊断后 `abort`，不返回嵌入方。
完整合同见 git 历史。

公开面仍不能误写成全 safe API。`Package::load` 是 safe 的 owned snapshot 校验；
`Package::instantiate` 是 `unsafe`，因为字节码验证不能证明包内 native 库、宿主符号和 FFI
签名相符。`run_main` 对既有 Engine 是 safe；手工 Module 和无类型 raw export 位于
`vm::engine::raw` 的 unsafe 面，内部 `Shared` 不公开。已发布 callback/JIT/MC/native
地址可能被任意原生代码保存，关闭后以进程期代码和小型 owner 墓碑保持稳定，不声称可以
从第三方库中主动撤销所有裸指针。
