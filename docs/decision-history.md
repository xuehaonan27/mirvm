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
3. [designs/frame-stack-models.md](designs/frame-stack-models.md) 保留完整 A1/A2/B1/B2 比较。当前裁决选择 A：
   guest 调用活动随宿主递归进入 native 栈，解释帧和未来编译帧可在同一 unwind 链上互调。
4. M4 实现采用 A1/tree-walking：每个 guest activation 对应一个 `interp_frame` 宿主调用；
   guest 局部字节不直接内联 native 栈，而放在随递归 LIFO 推进的 mmap ByteRegion。
   [designs/frame-abi-bytecode.md](designs/frame-abi-bytecode.md) 已解释“调用活动”与“局部存储位置”是正交轴。

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
2. [designs/vmctx-passing.md](designs/vmctx-passing.md) 比较 P、thread-local（T）和固定寄存器（R），得出
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

## 5. 2026-07-13 真实项目 TDD 追加替代

本节是晚于 M4/M5.1 施工日志的新证据。上表和历史日志中的旧选择保持原文；发生冲突时，以本节、
[current-status.md](current-status.md) 和当前代码/测试为准。

### direct dyn 尾字段：静态 offset → vtable runtime alignment

- **旧状态**：M4 place 设计把 Field/Downcast 普遍折成 lower 期静态 offset；这对 sized 字段和
  slice/str 尾成立，但没有单独建模 direct `dyn` 尾可能提高外层字段 alignment。
- **新证据**：ripgrep 的 `Arc<dyn Prefilter>` 使 16-byte sized prefix 后的 dyn value alignment
  达到 32。旧 offset 16 指向 padding，最终把零读成 `memchr::memmem::Searcher::find` 的间接目标。
- **新选择（active）**：direct dyn 尾使用 `PlaceStep::VTableAlignOffset`，从 vtable alignment 槽
  读取运行期 alignment，考虑 `repr(packed)` 上限后对 u64 prefix offset 向上取整，并检查算术
  溢出。slice/str 仍走静态公式；尚未建模的其他嵌套 DST 响亮拒绝。
- **被替代范围**：`designs/frame-abi-bytecode.md` 的“投影全为冻结 offset”是历史设计基线，
  `m4-log.md` 的当期实现记录不回写；本例说明冻结后的 place step 也可含执行期 vtable 元数据运算。
- **重开条件**：需要支持其他 metadata 形态、一般嵌套 DST 或更复杂 packed layout 时，重新评估
  是否应冻结一份通用 DST layout expression，而不是继续增加特例 step。

### i128/u128 `SwitchInt`：target 宽、discriminator 窄 → 两路判别值

- **旧状态**：IR target 已用 u128 保存，但 discriminator 强制通过 u64 scalar operand；M4.5 的
  “128 位判别式”主要兑现 TagInfo/Niche/SetDiscriminant，不能据此外推任意
  `SwitchInt(i128/u128)` 已经可执行。
- **新证据**：tokei/log 的合法 enum 路径产生完整 u128 producer 与 `SwitchInt(u128)`；原 lower
  把它报成“非标量”或会面临截断。
- **新选择（active）**：`SwitchDiscr::Scalar` 保留普通整数快路，`SwitchDiscr::Wide` 从 place
  完整读取 128 位后与 u128 target 比较。公开回归为 `demo/u128_switch.rs`。
- **兼容边界与重开条件**：这只证明 i128/u128 SwitchInt，不声明所有 128-bit ABI 形态都已
  标量化；若新的 MIR 整数表示或更宽判别值出现，应重新扩展 discriminator 载体。

### `track_caller` fn pointer：直接 item entry → rustc Reify shim

- **旧状态**：M4.1 把 `ReifyFnPointer` 一律写成 item entry 地址；M4.2 又把 track_caller ABI
  一致性总结为普通 Call、Virtual 和 fallback intrinsic 三处。
- **新证据**：tokei/clap 把 `#[track_caller]` 泛型函数交给 `Iterator::map`。fn-pointer ABI 不编码
  隐藏 Location，直接调用仍要求 Location 的 item entry 会少一个实参槽。
- **新选择（active）**：使用 rustc `Instance::resolve_for_fn_ptr`；需要 caller location 时选择
  Reify shim，由 shim 以普通 fn-pointer ABI 接参并补 Location。公开回归为
  `demo/track_caller_fn_ptr.rs`。
- **被替代范围与重开条件**：`m4-log.md` 保留“当时直接取 entry/三处一致性”的历史事实；
  ClosureFnPointer、其他 pointer adjustment 或 nightly `InstanceKind` 再变化时重新核对 rustc codegen。

### 宽 volatile：不得拆 → memory-repr 后端分块

- **旧状态**：第一次实现把值解释成宿主整数；第二版改用 alignment=1 opaque
  `MaybeUninit<[u8; N]>` 后，为避免虚构单次 MMIO 语义，只接受 1/2/4/8/16-byte，其他宽度 Trap，
  即上表仍保留的“不得拆”结论。
- **新证据**：tokei 的 `MaybeUninit<ignore::walk::Message>` 合法产生 136-byte volatile store；
  这种 memory-repr 值在 native 后端本来就必须分解为机器可承载的访问，要求不存在的 136-byte
  单指令事件反而拒绝了真实 Rust 程序。
- **新选择（active，推翻旧结论）**：1/2/4/8/16-byte 继续保持单个后端 volatile 事件；更宽值
  先把 source/destination 快照，再按 16/8/4/2/1-byte 块分解。padding 始终 opaque，重叠不会让
  后续块读取已被前序写覆盖的数据，并且明确**不承诺原子性**。公开回归为
  `demo/volatile_wide.rs`，另有 31/137-byte 与重叠单元测试。
- **重开条件**：真实 MMIO/device-register workload、目标相关访问宽度或 native codegen 差分证明
  当前块大小/顺序不满足所需语义时，必须引入目标相关 codegen 规则；不能把分块包装成原子整值事件。

### real-project oracle 路径：宿主随机绝对路径 → namespace-private 逻辑身份

- **旧状态**：两侧物理 checkout/HOME/TMP/target 不同；即使 cwd 逻辑相似，build diagnostics 与
  程序观察到的绝对路径仍可能制造假差异。第一版固定 `/tmp` 逻辑根又会信任宿主可预测路径。
- **新证据**：tokei 的 build-script 诊断包含 target 绝对路径；同时直接设置
  `CARGO_ENCODED_RUSTFLAGS` 虽能 remap，却会覆盖项目 `.cargo/config.toml` rustflags，破坏 native baseline。
- **新选择（active）**：每次 sandbox 在 namespace-private `/run` tmpfs 内创建
  `/run/mirvm-project-side`，两侧使用同一逻辑 cwd/HOME/TMP/XDG/target，diagnostics remap 到
  `/mirvm-project`；RUSTC proxy 只在 rustc argv 末尾追加 harness remap，保留 Cargo 已解析的项目
  rustflags。物理写隔离继续存在。控制器还会在启动时拒绝位于 `/run` 下、会被私有 tmpfs 隐藏的
  workspace/suite/toolchain/cache 宿主依赖。
- **当前兼容边界**：产品 Cargo shim 对非空 `RUSTC_WRAPPER` / `RUSTC_WORKSPACE_WRAPPER`，以及有效的
  `build.rustc-wrapper` / `build.rustc-workspace-wrapper` 一律 fail-closed；当前不尝试组合 wrapper 链。
  这是可重开的实现选择，不是永久禁止 Cargo wrapper。
- **重开条件**：如果 workload 必须观察真实路径，或真实固定项目需要自定义 rustc/wrapper 链，
  应先定义 wrapper 顺序、参数/env 传递与 provenance，再增加精确差分测试；不得用宽泛 normalizer
  隐藏差异。

### Cargo runner 收尾：callback 内执行 → 提前 exit（已撤回）→ compiler 收尾后执行 VM

- **旧状态**：Cargo target runner 重建 rustc 会话并执行 guest 后，把退出码带回共享
  `run_driver`，再让 rustc driver 正常完成收尾。纯单文件和 runner 使用同一种生命周期。
- **新证据**：带一个有意未使用函数、但 `main` 正常返回的 Cargo script 表明，Cargo 已在编译阶段
  完成诊断协议后，runner 再回到 driver 收尾会泄漏一条 native `cargo run` 没有的 warning summary，
  使严格 stderr oracle 失败。最小回归是 `tests/fixtures/cargo_warning_return.rs`，并已进入
  `diff_cargo.sh`。
- **中间选择（已撤回）**：仅 target runner 在 guest code 已取得后直接
  `process::exit(code)`。它使 warning fixture 转绿，却跳过 pinned rustc 的 `tcx.finish()`、incremental
  dep-graph 保存/校验、`Session::finish_diagnostics()`、`abort_if_errors()`、`flush_delayed()` 和
  compiler drop，可能把 lower 后迟发诊断掩盖成 guest success；guest 自身 `process::exit` 在 callback
  内也有同样问题。因此该方案只保留为失败过的历史候选，不能恢复为 active。
- **新选择（active）**：callback 只 lower tcx-free `Module`。runner 在 lower 后安装
  `TRACK_DIAGNOSTIC` filter；只有同时无 lint/code/span/children/suggestions、level 为
  `ForceWarning` 且消息精确为 `N warnings emitted` 的结构化 count-summary 才不给 emitter，所有诊断
  仍委托 rustc 原 tracking hook。`run_compiler` 完整收尾后恢复 hook；compiler code 成功才在 callback
  外执行 VM。这样真实 warning、迟发 error、guest 同文 stderr 与 guest exit 行为都不被误吞。
- **迁移与重开条件**：当前 hook 是进程全局单槽，单次 CLI 进程只有一个 compiler session 时安全；
  daemon、嵌入或并发/nested compiler 必须先改成有所有权校验的 guard 或串行化。若 pinned rustc 改变
  summary 结构或提供正式的诊断协议接口，应重开实现选择，但仍须保留完整 finalization 与严格
  native stderr/退出码差分。

### tokei 广覆盖 oracle：并行 JSON reports → 稳定 compact aggregate

- **旧状态**：原始 tokei case 对单个 `Cargo.toml` 输出 JSON；扩展到整个 `tests/data` 时曾尝试沿用
  JSON，并考虑用 `--sort` 或 normalizer 稳定顺序。
- **新证据**：广覆盖 JSON reports 的并行收集次序不稳定，而 tokei 在处理 `--sort` 前已从 JSON
  分支 early exit，因此该选项不能稳定报告。宽泛 normalizer 会把真实输出差异一起隐藏。
- **新选择（active）**：新增 `tokei_languages` workload 使用
  `--columns 160 --compact tests/data`。它覆盖 206 个 tracked fixtures，native/mirvm 均输出 205 行，
  SHA-256 同为 `0f5058901e0042629c9ff02714134206ca417235ebf6f892aeccec03a34d64c4`；
  `normalizers` 继续为空。原始单文件 JSON case 保留为另一条稳定切片。
- **兼容边界与重开条件**：这验证 compact aggregate，不证明 JSON report 顺序语义；若未来要把
  JSON 本身作为 oracle，应先在上游/工作负载层获得确定排序并精确测试。当前同一 revision 的多个
  workload 仍靠独立 case name 管理；case digest 与 suite inventory 是下一步身份模型候选，尚未实现。

### real-project evidence：name-only 可变目录 → 分层内容身份与不可变对象

本节晚于上一个小节，明确推翻其中“case digest 尚未实现”的当期状态；原文保留，用来说明决策
发生的先后。suite inventory 仍未实现。

- **旧状态（historical）**：`$NAME/check` 与 `$NAME/bench` 是可变目录；`name` 同时承担人类标签、
  identity 和 current location。没有同名 workflow lock，也没有把 benchmark 固定到它实际消费的
  correctness evidence。schema-1 summary 能记录 provenance，但目录本身不是可验证的历史对象。
- **认真比较的候选**：
  1. 维持外部接口，只增加一个扁平 case digest 和原子覆盖目录；实现最小，但 check/bench/tool
     identity 继续耦合，benchmark 配置变化会不必要地使 correctness 身份失效。
  2. 建立完整 suite graph、fixture registry、GC 与迁移协议；长期表达力最好，但在只有三个
     workspace-local workload 时引入过多尚无 consumer 的模块和接口。
  3. 保持 `prepare|check|bench CASE.toml` 调用接口，在内部只引入 Case/Check/Bench/Evidence 四层
     identity、不可变 object store 与 current view；suite inventory 延后。
- **触发证据**：同名但 args 不同会覆盖 oracle；同一工具内容换路径不应制造新正确性身份，而工具
  字节变化必须制造；benchmark schedule 变化不应重跑成另一个 CaseID，但必须产生新 BenchID；
  failed check 不能遗留旧 benchmark current；并发 workflow、hard crash 和后台 workload 后代会造成
  staging 可见、锁泄漏或 committed payload 被继续写；consumer 只信 metadata 又无法发现篡改。
- **新选择（active）**：选择候选 3。`name` 只作人类 namespace；CaseID 绑定 workload 语义，
  CheckID 再绑定 host/tool/controller/sysroot 内容，BenchID 绑定 exact Check EvidenceID 与 schedule，
  EvidenceID 绑定 envelope/payload。object store 路径为
  `objects/{check,bench}/<phase-id>/<evidence-id>`，`check`/`bench` 是原子 current symlink。
  consumer 重算 payload 与内容地址；发布对象只读封存并执行本地 fsync 步骤。
- **并发与失败语义**：per-name workflow lock 防止同名 oracle 串线；suite-global cache lock 让 prepare
  独占、check/bench 共享。固定顺序 name → cache，workload 不继承锁 FD。新 check 先撤销 current，
  crash staging 由下一持锁 workflow 回收；failed check 保留历史 object 但不发布 current。
- **迁移/兼容**：外部 CLI 与 case TOML 不变，调用者不填写 digest。旧扁平 check/bench 移到
  `legacy/*.pre-identity.*`，不会冒充 schema 2。现有 object 历史不做自动 GC；只读 mode 是防误写，
  完整性依据仍是 consumer hash，不宣称抵抗 owner/root 或证明真实断电恢复。
- **暂缓与重开条件**：suite inventory/集合身份、fixture registry、保留策略/GC、跨主机共享 cache、
  NFS/对象存储 durability 或远程持续 gate 成为真实需求时，重开候选 2。Python 内容哈希与 sysroot
  marker 只是有界执行闭包；若动态 runtime、完整 sysroot Merkle identity 或 Cargo fingerprint 输入
  成为差异来源，应扩展 CheckID，而不是把当前 ID 宣称为通用可复现构建证明。

### 2026-07-13：real-project evidence consumer/publish 加固（active，晚于上一节）

本节细化而不删除上一节的四层身份选择；上一节保留当时从 name-only 迁移到内容寻址对象的理由。

- **旧状态（historical）**：第一版内容寻址 writer/consumer 使用 schema 2。consumer 能重算内容地址，
  但没有从 payload 重新证明 PASS/XFAIL correctness 语义；bench summary 的 host/tool provenance 也没有
  与 exact check result 交叉核对。对象先以最终目录名出现、再变只读，会留下一个短暂的可见可写窗口；
  新增 Git controller 身份后若仍沿用 schema 2，还会让历史解码规则发生漂移。
- **触发证据**：对篡改后的 mirvm stdout 或 benchmark tool descriptor 同步更新 payload hash 与
  EvidenceID 时，旧 consumer 仍可能接受；hard-kill/并发回归又要求最终对象路径一出现就已经封存。
  同时，既有 schema-2 object 是审计材料，不能用新字段要求使其突然不可读，也不能把新保证追认为
  旧证据当时已有。
- **新选择（active）**：writer 升为 schema 3，CheckID 明确绑定受控 Git/controller 内容；check
  consumer 从 evidence 重新证明 expected native exit、PASS 两侧 exit/stdout/stderr 完全一致，或
  XFAIL 的 exit 与 canonical diagnostic。bench consumer 必须读取 exact linked check result，交叉核对
  case/host/Cargo/rustc/mirvm provenance，解析全部 sample 并复算 median/p95；对象还必须只含 schema
  规定的 regular files，额外 sidecar 一律拒绝。benchmark 在最终发布前再次验证 exact check，关闭
  correctness 准入到发布之间的 TOCTOU 窗口。
- **发布协议**：payload 先写入 `results/<name>/.staging`，stage 校验后移到对应 phase-ID parent 下的
  隐藏 `.staging.*`；在隐藏名称下封为文件 `0444`、目录 `0555` 并完成 fsync，最后才在同一 parent
  rename 为 EvidenceID。`prepare|check|bench` 取得锁后都会回收两层 staging 和临时 current symlink；
  `prepare` 不撤销仍有效的 current。最终 object rename 与 current symlink 仍是两个提交点，因此
  中间崩溃至多留下完整、只读、不可见的孤儿历史对象。
- **兼容与边界**：validator 对 schema 2 保留独立历史 dispatch，schema 3 是唯一当前写格式；
  pre-identity 扁平快照继续只放在 `legacy`/历史位置，不冒充 schema evidence。该协议提供本地同文件
  系统原子可见性与耐久化步骤，不证明断电恢复，也不抵抗 owner/root 主动改写。suite inventory、
  集合身份、GC、完整 Python 动态闭包和 sysroot Merkle identity 仍按上一节的重开条件处理。

## 6. 2026-07-14：M5.2 立项——非 JIT 语义补全插入，JIT 顺延

- 旧状态：M5 双轨设计中 M5.2–M5.4 = 方法级 JIT 三期；轨 A 以 M5.1 收口视为"corpus 全绿"。
- 候选项：直接进 JIT（M5.2 原案）vs 先补非 JIT 语义面。
- 新证据（2026-07-13 实测，[m5.2-design.md](m5.2-design.md) §1）：主动圈定（拒绝面全量
  清点 + rustc intrinsic 权威差集 + 14 个 native 差分探针）发现 corpus/真实项目全绿只覆盖
  "已踩过的面"——`f64::abs()` 即 Trap（`fabs` 泛型名漂移）、递归 8000 帧上限、`std::simd`
  55/75 缺失、`fetch_max`/`mul_add` Trap、atomic 序整体折叠 SeqCst、fork/global_asm/naked/
  atexit/f16/f128 缺口、`#[global_allocator]` 分配计数不一致。
- 新选择：插入 M5.2 = 非 JIT 语义补全期（D8a–D8l，用户 2026-07-14 全批）；JIT 三期顺延为
  M5.3–M5.5。理由：JIT 的差分 oracle 是 JIT-on/off，解释器语义面越干净，JIT 期误差归因越
  单纯；本期探针/用例直接成为 JIT 期回归网。
- 被替代文档/章节：m5-design.md §2 双轨图与施工顺序表（编号已同步）；各文档 M5.2+ 指称
  已全局改指新编号。
- 迁移与兼容影响：代码内 `M4.6+`/`M5.x` 预标注释随各分片实现改指 M5.2；gate 判据随
  XFAIL 转绿滚动（滚动记账纪律不变）。
- 再次重估触发器：M5.2 收口时若发现新的大面（探针再撞新层），先记账再决定是否二期，
  不无限扩期挡 JIT。

## 7. 尚未兑现或需要重新验证的架构承诺

- P7 设想独立 `src/os/` 物理层；当前 OS/FFI/builtin 逻辑仍分布在 lower、interp、ffi、heap。
- “engine 是 library”目前只是 crate 结构；进程退出、全局 TLS key、泄漏式生命周期使其还不是稳定
  多 Engine 嵌入 API。
- `.mirvm` mode B、fat target artifact、checked 模式、alloca 局部、方法级 JIT 均仍是设计，不是现状。
- static `.a`→`.so` 的受约束 Linux/ELF 切片已实现；非 PIC、跨 archive 依赖/顺序或重名、
  RTLD_DEFAULT 重名、constructor、thin、export-symbols 仍是明确拒绝面。它们需要新 link plan/
  生命周期设计，不能从 blake3 外推通用。

## 8. 改变决策时的记录模板

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
