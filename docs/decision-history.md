# 关键架构决策与演变

> 本文不是“永远不许改”的 ADR 集，而是可逆决策的索引。它保存当时的备选项、证据、当前选择和
> 重开条件。新证据可以推翻旧选择；推翻时追加记录，不删除旧论证。当前实现事实见
> [current-status.md](current-status.md)。

## 1. 状态词

- **active**：当前开发默认遵守，但仍可被新证据推翻。
- **provisional**：已有倾向，等待真实负载或实现验证。
- **historical**：曾采用或认真评估，现不作为默认方案。
- **open**：尚无足够证据裁决。

每次改变至少记录：日期、旧选择、候选项、触发证据、新选择、兼容/迁移代价和再次重估条件。

## 2. Frame model：A、B 与局部存储不是同一轴

**当前状态：active（控制流 Model A）；解释态局部使用 slaved ByteRegion。**

### 演变

1. tier-0 继承 rustc `InterpCx` 的 `Vec<Frame>`，接近 B1。它是 bootstrap 的实现事实，
   不是经 greenfield 比较后选出的终局。
2. 2026-07-05 的初版比较曾给 B 的深递归和 unwind 简单性较高权重，也受“现有实现更接近 B”
   影响。后续评审明确这两点在方法级 JIT 是硬约束时不能如此计权。
3. [frame-stack-models.md](frame-stack-models.md) 保留完整 A1/A2/B1/B2 比较。当前裁决选择 A：
   guest 调用活动随宿主递归进入 native 栈，解释帧和未来编译帧可在同一 unwind 链上互调。
4. M4 实现采用 A1/tree-walking：每个 guest activation 对应一个 `interp_frame` 宿主调用；
   guest 局部字节不直接内联 native 栈，而放在随递归 LIFO 推进的 mmap ByteRegion。
   [frame-abi-bytecode.md](frame-abi-bytecode.md) 已解释“调用活动”与“局部存储位置”是正交轴。

### 为什么当前选 A

- Cranelift 是方法级 JIT，编译帧天然在 native 栈；同栈可简化 i2c/c2i 和 unwind。
- 项目选择 1:1 OS 线程，Rust async 是无栈状态机，因此 B 的栈式协程优势当前用不上。
- A 更接近 native 的栈深与溢出行为。

### 保留的 B 价值与重开条件

B 仍在以下条件下值得重评：产品明确需要栈式协程/可保存 continuation；独立 VM 栈显著改善
嵌入、调试或资源治理；生产 JIT 证明 A 的 unwind/适配成本高于预期；或可测性能数据表明 B2
的连续帧栈收益超过混合栈代价。`slaved` 与 `alloca` 只是局部存储轴，切换它们本身不等于 A↔B。

## 3. vmctx：边界 TLS、T 骨架与 R 缓存层

**当前状态：边界 TLS active；生产 JIT 尚未实现；内部 T/R 为 provisional 分层方案。**

### 演变

1. Spike 2 为验证再入和 i2c/c2i，使用显式 `*mut Ctx` 参数（旧称 P）。这是合适的实验骨架，
   但 plain-C 函数指针逃逸时会产生签名错位。
2. [vmctx-passing.md](vmctx-passing.md) 比较 P、thread-local（T）和固定寄存器（R），得出
   native→guest 回调必须按**当前线程**查 ctx；跨线程回调与 signal 使捕获创建线程 ctx 的 thunk
   原理上不正确。因此边界 TLS + lazy attach 被确定。
3. Spike 5 的窄 fib 微基准中 R 比 P 快约 8%，证明 R 可行，但没有覆盖真实寄存器压力，不能据此
   终裁生产约定。
4. 2026-07-11 的 [m5-design.md](m5-design.md) D5 不再做 P/R 二选一：生产 M5 先采用
   **T 骨架**——纯 guest fast 签名，需要 ctx 时做 TLS 获取；R 保留为 ABI 兼容的可选缓存层。
   旧 P 不再是生产候选。

### 为什么当前先 T

当前编译码中需要 ctx 的热内联点尚未存在：分配和 TLS 仍经过助手；真实地址访问、冻结 statics
和大多数纯计算不需要 ctx。此时全程征用一个寄存器是提前付租金，而 T→R 的差异可收敛到
`get_ctx()`、Cranelift pinned-reg 开关和边界 save/set/restore，不要求改变 guest fast ABI。

### 重开 R 的明确触发器

当分配快路径或 guest TLS 被内联进编译码，或 profiling 显示 ctx 获取成为可见热点时，用该真实
负载重测 T 与 R，包括寄存器压力、跨 FFI 回调、递归和 rayon 类并发。R 的切换粒度是整个 JIT
代码缓存重编，不能在同一缓存中任意逐函数混用两种制度。

## 4. 其他已发生的替代

| 决策 | 早期判断 | 后来证据与当前选择 |
|---|---|---|
| 执行核心 | `InterpCx` tier-0 可作为长期 oracle/tier | 2026-07-09 已删除；当前唯一产品引擎是 M4 typed-bytecode interpreter |
| 单态化闭包 | 只依赖 rustc mono collector | collector 不含所有解释可达调用；改为 collector 作种子 + call-site worklist 扩集 |
| panic | lower 特判 panic 入口 | 过于 ad-hoc；改为照常解释 guest 链，只在 runtime/linker/foreign 原语边界接管 |
| inline asm | 曾写成“Cranelift 能降低 inline asm” | Cranelift 本身不处理 asm；M5.0 采用 cg_clif 风格 GAS wrapper + 外部汇编器 + dlopen |
| guest TLS dtor | M4.4 计划不运行 dtor | 实现中加入 pthread key 与最多三轮延迟析构；实例块回收仍是生命周期债务 |
| `spread_arg` | 初判可忽略 | Rust-call ABI 真实需要 tuple 字段展平，M4.4 已实现 |
| signal | “有 thunk 即可直通”曾被当成接近完成 | thunk 不是异步信号安全 trampoline；静默 StubZero 已移除，guest handler 当前明确 Trap，SIG_DFL/SIG_IGN 才受限直通；真实注册/投递仍未实现 |
| guest backtrace / unwinder context | 移除 StubZero 后曾以“可 dlsym + 可回调 guest thunk”为由让 `_Unwind_Backtrace` 等走通用 host FFI | thunk 只解决调用方向，宿主 unwinder context 仍只含 libffi/解释器帧。当前除已专用实现的 Raise/Delete 外，Backtrace、Get/Set context、Resume/ForcedUnwind 家族全部明确 `Unsupported`；只在有 guest frame/IP/LSDA 翻译层及 differential probe 时重开 |
| volatile 宿主载体 | 第一版按 layout size 把 1/2/4/8/16-byte 值转成整数或对齐 8 的 `Volatile16` | `[u8;16]` alignment=1 反例触发宿主对齐 UB，含 padding 聚合值还会把未初始化字节解释为整数。现用 alignment=1 `MaybeUninit<[u8; N]>` 作 opaque 整值 volatile 事件，只作位型搬运；若未来扩展其他宽度，仍不得拆成多次 MMIO 访问 |
| M5.0 范围 | 预期 div/cpuid/syscall 后多个 corpus 直接变绿 | 修完 asm 后暴露 `llvm.x86.*` 和静态归档下一层；实际结果以 m5-log 为准 |
| SIMD / x86 intrinsic 路线 | M5 总设计倾向常见操作走 CLIF、异类走 asm stub；M5.1 初稿进一步提出 pshufb lane + SHA asm-stub，并扩通用向量 wrapper ABI | 标量 addcarry/subborrow/xgetbv 走 builtin；解释器向量路线改为 tcx-free stdarch target-feature helper（更小、隐式寄存器由 stdarch 处理），已由 native vector probe + c_sha2 验收为 active；旧通用 asm ABI 在 helper 面膨胀/缺 stdarch 表达时重开。JIT 的 CLIF/asm 选择不受此解释器决定封死 |
| ecosystem 滚动面 | 初始静态盘点把 movemask/pshufb 写成可能尾巴 | xgetbv 后实际依次暴露 `simd_insert/extract`、`simd_shl/shr`、`vzeroupper`；按 probe 补齐后 ecosystem debug/release 绿，再次证明 expected-red 必须随真实前沿滚动 |
| Static native archive | M5 初稿把 `.a→.so` 描述成通用转换，第一版又把物化 `.so` 放进可选 dlopen 列表 | 实测 `--whole-archive` 必须用 `--no-whole-archive` 闭合；最终只承诺 Linux/ELF、单 archive、PIC、依赖闭合、无 `.init/.fini`/ctor/dtor/跨 archive 或 RTLD_DEFAULT 重名导出的垂直切片。物化产物现是 required library，必须在 dlsym 前 `RTLD_NOW` 加载，失败带 `dlerror` 终止；扩面须先定义 link plan/生命周期 |
| 沙箱层级 | RAM spec 曾把进程 containment 写为 L4 方案 | 后续范围裁决砍掉 L4、MPK，当前只保留 L1 结构隔离 + 可选 L3 checked 的长期方向；正式沙箱未实现 |

## 5. 尚未兑现或需要重新验证的架构承诺

- P7 设想独立 `src/os/` 物理层；当前 OS/FFI/builtin 逻辑仍分布在 lower、interp、ffi、heap。
- “engine 是 library”目前只是 crate 结构；进程退出、全局 TLS key、泄漏式生命周期使其还不是稳定
  多 Engine 嵌入 API。
- `.mirvm` mode B、fat target artifact、checked 模式、alloca 局部、方法级 JIT 均仍是设计，不是现状。
- static `.a`→`.so` 的受约束 Linux/ELF 切片已实现；非 PIC、跨 archive 依赖/顺序或重名、
  RTLD_DEFAULT 重名、constructor、thin、export-symbols 仍是明确拒绝面。它们需要新 link plan/
  生命周期设计，不能从 blake3 外推通用。

## 6. 改变决策时的记录模板

```markdown
### YYYY-MM-DD：<决策名>

- 旧状态：
- 候选项：
- 新证据（命令、probe、源码或生产负载）：
- 新选择：
- 被替代文档/章节：
- 迁移与兼容影响：
- 再次重估触发器：
```
