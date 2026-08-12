# C 与 C-unwind 跨语言异常合同

> **状态：2026-08-12 已实现并进入 `fast`。** 本文规定 mirvm 在 Linux/ELF/x86_64
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
`MIRVM_JIT_SYNC=1`、`MIRVM_JIT_THRESHOLD=1` 的强制同步 JIT。九个执行场景都要求
JIT 日志中包含对应目标函数名的同一发布行出现 `release=true`，且不得出现
`release=false`，防止“名叫 JIT 实际全程解释”的假绿；两个不支持 ABI 场景在
lowering 阶段就应拒绝，不会进入 JIT。

| 场景 | native | 解释器 | 强制同步 JIT |
|---|---|---|---|
| `C-unwind` 无异常返回 + cleanup，30,000 次 | `value=42 drops=30000` | 同左 | 同左，已发布机器码 |
| C++ typed exception 往返 | `result=1073 caught=73 drops=1` | 同左 | 同左，已发布机器码 |
| Rust panic 经 C++ 重抛 | `payload=51 caught=888 drops=1` | 同左 | 同左，已发布机器码 |
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
| Rust panic 越过普通 C callback | `C` | 在该 ABI 边界终止 | 非零退出、终止原因 | stderr 逐字节相同 |
| C++ 异常试图越过 guest 普通 C wrapper | `C` wrapper 内调用 `C-unwind` | 按 rustc 的 `Terminate` 边终止，不让外层 C++ catch 收到 | 非零退出、外层不得正常返回 | UB 情形的具体信号和文字 |
| foreign 异常到达 Engine 顶层 | 无稳定嵌入合同 | 当前由宿主 Rust 边界终止 | 进程不误报成功 | 异常原样越出公开 Engine API |

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

## 6. 回归入口

```bash
./tests/run.sh suite runtime.c-unwind
```

夹具 `tests/fixtures/c_unwind_contract/` 由固定 Cargo+rustc+C++ 先生成 native oracle，
再用默认 cargoless 分别跑纯解释器和强制同步 JIT。十一项覆盖正常返回、typed C++
exception、Rust panic 重抛、吞 panic、普通 C callback panic，direct/函数指针两种
`Terminate` 边上的 foreign exception 和 Rust panic，以及外层调用/回调参数两种非
C/System ABI 的明确拒绝。它已进入 `fast`，不再扩成通用 FFI harness。

## 7. E13 裁决

guest panic 继续使用宿主 Rust panic 作为运输层。guest catch 点只 downcast
`GuestPanic`，其余载荷继续展开；Engine 顶层把未捕获的 `GuestPanic` 映射为退出码
101，把 `EngineFault` 映射为 `RunError`，只有其余宿主 panic 原样重抛。独立
exception class/personality 不作为架构终态：它不能让标准 Rust `catch_unwind` 捕获
C++ 异常，反而会先被当作
foreign exception 终止，还会把平台展开、对象所有权和两阶段 personality 的维护责任
全部转给 mirvm。

只有以下证据之一出现才重开：稳定嵌入 API 要求异常按 mirvm 自有类别越出 Engine；
固定工具链升级使现有 guest panic 或 C-unwind 回归变红；或实测证明宿主 panic 与 guest
panic 共轨产生无法在局部 guard/pad 修复的错误。性能本身不足以触发。
