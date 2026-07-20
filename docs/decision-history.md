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
4. 2026-07-11 的 [m5-design.md](designs/m5-design.md) D5 不再做 P/R 二选一：生产 M5 先采用
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
- 新证据（2026-07-13 实测，[m5.2-design.md](history/m5.2-design.md) §1）：主动圈定（拒绝面全量
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
- **收口（2026-07-14 完成）**：M5.2 十片全部落地，gate5 40 PASS/2 XFAIL → **46 PASS/
  0 XFAIL/0 FAIL**。施工中的设计偏离/意外见下方 §6.1；未做项以 D8l 响亮 Trap 登记。

### 6.1 M5.2 施工中的设计偏离与意外（记录不失踪）

- **D8c f16/f128：手写 libgcc FFI → 直骑宿主类型**。设计写"dlsym libgcc `__*tf3`"；实测
  本 nightly 宿主 `f16`/`f128` 全套可用，引擎加 `#![feature(f16,f128)]` 直接用——rustc 把
  引擎自身的 f16/f128 运算下降到与 native guest **同一批** compiler-builtins/libm 符号，
  同源即位同，少一层手写 FFI（且 libffi longdouble 在 x86-64 是 80 位，接不了 binary128）。
- **D8d/D8e 机制沿用而非新造**：signal 的 AS-trampoline 直接复用 M4.4 thunk 工厂（handler
  = `extern "C" fn(c_int)` 与逃逸 guest fn 同构）；backtrace 影子帧的 IP 是合成 token
  （非真 fn 条目地址），dladdr 诚实 miss → oracle 用不变式而非 native 逐字节。
- **D8f fork 守卫的 TOCTOU 教训**：初版用 Ctx 计数判 guest 线程数，实测**漏放**多线程
  fork——pthread_create 返回后新线程即存在，但其 Ctx 要 trampoline attach 才建，有窗口。
  改用真 OS 线程数（`/proc/self/task`）对 guest-main 基线判定。
- **D8b 顺带根治潜伏静默错值**：旧 `SimdBin` 对全部 lane 按整数位运算，float lane 的
  add/cmp（±0.0、NaN）是**静默错值**，仅因 corpus 全整数 lane 未爆雷。引入 `LaneKind`
  使"忘带元素类别"类型层不可表示。
- **D8i fabs 泛型名漂移**：本 nightly `fabs` 去后缀化为泛型名，"剥后缀匹配"模式 miss →
  `f64::abs()` 曾一调即 Trap（corpus 恰好无人调的暗洞）。全 math 表加泛型兜底。
- **被替代文档**：m5.2-design.md D8c 的"libgcc FFI"、D8b 的"合成 asm-stub 向量 ABI"（M5.1
  已先偏为 stdarch helper）保留原文作历史；实现事实以 m5-log.md M5.2 节 + 本节为准。

## 7. 2026-07-14：轨 C 分发与产品面立向（D9）——先缓存后打包，发行先 miri 式

- **旧状态**：mode B（`.mirvm` 分发）只有 DESIGN.md / m4-plan 尾部雏形；无缓存分层
  账本；toolchain 耦合模型与工具链入口形态未成文。
- **新证据**：摄入链路盘点（`src/cargo_shim.rs` 三阶段、`-Zscript` frontmatter、
  MIR sysroot、五个内容哈希缓存**均已实现**）+ 实测账本（std-only 热跑 0.40s；
  ecosystem 四依赖热跑 3.07s，且为**每跑必付**的加载相成本——冷启动痛点不在依赖
  解析/编译，在叶前端+单态化+lower）。
- **新选择**（全批，细节与风险见 [distribution-design.md](designs/distribution-design.md)）：
  - **D9a** 统一入口 = `mirvm` 单二进制 + 子命令；`mirvmc` 至多别名硬链。
  - **D9b** 拒绝 StableMIR-as-format（进程内 API 非格式）；先做 **L2 post-mono
    engine-IR 缓存**，mode B 包 = 缓存可移植化；对外格式冻结推迟 M5.3 后。
  - **D9c** 缓存 L0–L3 分层；L2 key = mirvm build id + sysroot hash + crate 图
    内容哈希，失配整体重建。
  - **D9d** target 依赖构建剪 codegen（check 形态，落地对照 cargo-miri 实证）。
  - **D9e** toolchain 模型 = **自带编译器**（运行时发现架构上不可能：nightly-only
    rustc_private / ABI 锁死 / rmeta 跨版本不稳）；发行**先 miri 式**按 toolchain
    出构建，成熟后 JDK 式自包含 tarball。
  - **D9f** 施工顺序 ①相位计时 → ②L2 缓存 → ③剪 codegen → ④mode B+`mirvm pack`
    （M5.3 后）→ ⑤发行/命名收尾；MRsDK 命名否决，kit 命名推迟到 mode B 实物。
- **被替代**：无推翻；m4-plan ".mirvm 化 sysroot" 雏形被 D9b 细化吸收。
- **迁移影响**：本次仅方向文档，无代码变更；里程碑编号立项时另定（候选 M6），
  不占 M5.3–5.5。
- **重估触发器**：M5.3 收官后重估对外格式冻结；pinned toolchain 升级时重测 cargo
  emit 剪枝容忍度。

### 7.1 M6 片1/片2 施工偏离与新发现（2026-07-14，细节见 m6-log.md）

- **固定基址替代重定位重建**：设计稿 §4 写"dlopen 句柄/FnPtr/常量池指针载入时重建"；
  实做发现冻结区绝对地址已内嵌进字节码 const 与 fn_addrs 键，逐点重建需要 lower 全程
  记录重定位边表（侵入极大）或范围启发式改写（静默错值风险，违纪律）。改为 **JVM CDS
  同思路的固定基址映射**（`0x6800_0000_0000` + MAP_FIXED_NOREPLACE，被占响亮回退
  动态基址+不缓存）——地址不重建而是天然稳定；唯一真正的活体重建仅剩 asm-stub 表
  （配方 `Module.asm_sites` 幂等重物化）与 required .so（引擎侧本就每跑 dlopen）。
- **告警不入缓存**（设计稿未涉及的真语义边界，diff 通道当场抓获）：warm 跳过 rustc
  会话 ⇒ 编译诊断无法重演；v1 契约 = 有告警/错误的会话不 store（告警程序每跑重演，
  语义与 native 差分口径逐字节一致），诊断回放留作升级路径。计数钩必须经
  `psess_created` 安装（rustc_interface::setup_callbacks 会覆写 TRACK_DIAGNOSTIC，
  psess_created 在其后、首次解析之前——直接在 run_driver 前装会被吞，实测踩中）。
- **environ 类宿主地址直嵌不入缓存**（gate corpus c_process 热路径 SIGSEGV 抓获）：
  非 weak extern static 的 dlsym 宿主真地址烤进 const/冻结区，ASLR 跨进程无效且
  固定基址救不了（那是 libc 的地址不是我们的）。v1 契约 = `Module.foreign_static_syms`
  非空即拒 store；GOT 式 Operand 间接留作升级路径。**教训（可缓存性三判据）**：
  快照可回放 ⇔ ① 区内地址定基稳定 ② 进程级活体有重物化配方（asm_sites）③ 无第三方
  地址直嵌（environ 类）——新的 lower 期 dlsym/地址烤入必须同步登记不可缓存标记。
- **argv 从 lower 迁出**：EntryPlan.argc/argv_ptr 原为 lower 期烤死——缓存下会回放
  上次运行的 argv（错值级）。迁 `Module::finalize_entry_argv`，冷/热每跑在快照语义
  之后终结化，单一路径。
- **materialize_script 幂等化**：原每跑无条件重写物化文件 ⇒ mtime 漂移 ⇒ 清单必
  失配（缓存永 miss），同时一直在扰动 cargo 指纹（此前 native/mirvm 双方"恰好都
  重建"而互相掩盖）。write_if_changed 修复。
- **输入清单实现升级**：设计稿 §4 只写"key 含 crate 图内容哈希"；实做采 rustc
  dep-info 同构清单（source_map + file_depinfo + used_crate_source + env_depinfo）
  ——比设计更精确（env! 依赖到值、include! 文件、sysroot rlib 全覆盖）。

### 7.2 2026-07-14：M5.3 Pending 与轨 C 施工顺序修订（用户裁定）

- **决策**：M6 ①② 收官后不默认进入 M5.3。用户裁定：冷启动（lower 相）是一等问题，
  先调研（[coldstart-research.md](history/coldstart-research.md)，commit 27f0001）再按新顺序施工：
  **S1**（V1 sysroot 仪式 stamp 化 + 调研抓到的三个缓存盲区 P1/P2/P3 修缮）→
  **S2**（V2 依赖 codegen 剪枝 = 原 D9f③）→ **S4**（V4 std 预降低底座，大件先设计简报
  过审）→ 之后 **S3 懒降低与 M5.3 JIT 合并出联合分层设计，过审后才动工**。
- **为什么**：调研实证 lower 是近常数 std 税（0.10ms/instance；执行集仅占降低集 7–29%），
  懒降低（S3）与 JIT 首调编译是同一根"首调触发 per-fn 物化"管线，分开定型会重蹈
  M4.1"ABI 未一次做全"的教训；其余杠杆彼此独立、可先行兑现。
- **被替代**：D9f 施工顺序"①计时 ②L2 ③剪枝 ④mode B（M5.3 后）"中"②之后默认进
  M5.3"的隐含节奏被替代（①② 已完成不动；③=S2 提前于任何 JIT 工作；④ 位次不变，
  S4 底座是其 sysroot 侧特例、联动设计）。
- **重估触发器**：S1/S2/S4 完成、S3+JIT 联合设计出稿时；若 S4 设计发现底座强依赖
  懒降低机制，则 S4 并入联合设计（届时在此记录）。

### 7.3 2026-07-15：S4 施工偏离——偏移合并取代 FuncId 域位（M6 片6）

- **偏离**：简报（s4-base-image-design §3）设计 = FuncId 最高位分域 + 引擎双函数表。
  施工通读引擎后改为**偏移合并**：delta 降低时 fn/TLS/asm-stub 的 id 直接从底座
  计数起编（Linker delta_first_*），装载 absorb = base++delta 拼单表。
  **解释器热路径零改动零新分支**——域位方案要动 run_blocks/interp_frame 两处热点
  取址，且 asm_stub_addrs/tls/fn_addrs 等 per-module 表全要域位路由双份化。
- **代价与抵扣**：delta 与底座实例级耦合（底座换代必须重降 delta）——本就由分层
  缓存键管着（ircache Header.base_key 精确相等，含 None 侧；单测锁定），无新增约束。
- **验收替换**：简报"域位编解码单测"作废，替换为 base_key 失配拒载单测 +
  双域冻结区跨引用往返单测（片6a）。
- **附带事实**：debug/release 二进制共享同一底座（MIRVM_BUILD_ID=源树内容哈希，
  两 profile 同值；底座文件是 postcard 编码非内存转储，跨 profile 安全）。
- **降低指纹的会话内验证**（简报 §5 的实现精化）：lower 烤入字节码的会话布尔仅三个
  （ub/overflow/contract checks，func.rs RuntimeChecks 处），底座文件存构建会话
  三元组，程序会话 after_analysis 比对，失配即弃底座走全量降低——cargo runner
  可能带自定义 profile 旗标，装载期（会话外）无法预知。
- **COW 定基映射按实测裁剪**（简报 §2 承诺废弃 v1）：装载相 33ms 里冻结区 memcpy
  只占 ~0.2ms（~2MB），大头是 3000 个 FuncBody 的 postcard 解码——file-backed
  MAP_PRIVATE 只优化前者，v1 无收益。真正的零装载 = 字节码零拷贝布局（rkyv 类），
  与 mode B `.mirvm` 外部格式冻结同题，排 M5.3 后（重估触发器：mode B 立项时）。
- **底座字节确定性**（验收"连续两建 cmp 一致"抓获真缺陷）：Module.exports/fn_addrs
  是 std HashMap（RandomState 每进程随机迭代序），postcard 随序落盘 ⇒ 两建不同字节。
  修 = 底座文件将两表摘出为**排序 Vec**（module 内清空，装载端重建）；L2 条目无
  确定性契约不受影响。

### 7.4 2026-07-15：S3′b 依赖成像暂停——线性链证伪，待裁定方向

- **决策**：S3′b（依赖成像）施工中，per-crate 链方案的装载命中率被实测证伪（eco 4/19
  依赖，lower 988→~850ms 基本没降），暂停施工、把设计岔路写成
  [s3b-design-fork.md](history/s3b-design-fork.md) 交后续 session 裁定；工作树回退 S3′a 绿态
  （commit b4ed691），已探索代码存 `docs/parked/s3b-chain-wip.patch`。
- **为什么证伪**：链是**线性**结构，依赖是**非线性 DAG**。cargo 并行构建下多数依赖
  建于 `[std 底座]`（上游 image 未就绪），装载贪心拼链时装完首个 `[std]`-built 依赖后
  前缀就移过 `[std]`，其余全部 below_key 失配 → 只能装出一条穿过 DAG 的线性链。定点
  装载（反复扫描）已是链方案最优，仍 4/19。**这是数据结构层的固有信息损失，非实现 bug**
  ——m5.3-design §3.3 已预警为"最尖风险"并留"实测不达标再议"，本次即触发。
- **正确性未破**：链方案本身airtight（image 只在 below_key 精确等于前缀键时装，错配退
  delta，绝不腐坏）；证伪的是**价值**（命中率），不是正确性。任何后续方案都必须守此红线
  ——绝对 FuncId 偏移/跨域地址只在"装载栈下 == 构建栈下"时有效。
- **被替代**：m5.3-design §3.3 的"per-crate 链 + 定点装载"作为 v1 主实现被替代；四条
  出路（A 单 deps-image 跨运行 / B chain+屏障 / C reloc / D chain+reloc / E 接受低命中）
  取舍见 s3b-design-fork，当前推荐 **A**（最简、风险最低、兑现主收益 edit-rerun 988→
  ~150ms，代价=放弃跨项目共享 S3′c）。
- **重估触发器**：裁定 A–E 之一时；若选 A 后确有"多项目同 lockfile 共享"的实需，再上
  B/D 补跨项目。
- **施工副产物（已解，记备后用）**：① 依赖成像必须门控
  `should_codegen()`=true（cargo 流水线 metadata-only 趟 reachable_non_generics 空 ⇒
  lib 自身函数不成 mono root ⇒ 空 image；symbol_export.rs:53）。② fn-entry cell 内容
  （FuncId）是**调试用**，运行期派发走 fn_addrs 反查 ⇒ relocation（C/D）不必碰冻结区字节，
  只改 Call.callee/fn_addrs 值/exports 值 3 处类型化字段。

### 7.5 2026-07-15：S3′b 裁定 = A2 纯化聚合 deps-image（purity 账本证伪 A1）

- **旧状态**：§7.4 暂停待裁定；s3b-design-fork 推荐方案 A（口径：非 LOCAL_CRATE 全进
  image，"键必须含本项目依赖集实际实例化"）。
- **候选项**：A1（fork 原推荐口径）/ A2（A 的改良：purity split）/ B chain+屏障 /
  C reloc / D chain+reloc / E 接受低命中。
- **新证据（探针实测）**：`MIRVM_PURITY_STATS=1`（src/lower/mod.rs，worklist 逐
  instance 分类+计时，env 门控默认零开销）对 eco 冷会话：
  - 底座在场（A2 真实配置）：local 11 inst/1.0ms，**tainted 72 inst/1.9ms**，pure
    10593 inst/1042.7ms ⇒ A2 每编辑重降 = local+tainted = **83 inst/2.9ms**（总
    10676 inst/1045.7ms，99.7% 的 lower 成本 bin 无关）。
  - 底座旁路对照：tainted/local 不变（72/11），pure 13150 inst——tainted 与底座无关；
    纯组间差 2557 inst 即底座覆盖量。
  - tainted top = serde derive Visitor（Task）、`Vec<Task>`/`RawVec<Task>` 系、本地闭包
    迭代器适配器；符号内嵌本地 crate disambiguator（`CskivyEyr2QPT_9ecosystem`）。
  - 结论 1：tainted 仅占 0.7%，**A1 相对 A2 只多买 1.9ms**——而 A1 要付两个结构性
    成本：① 键含"实际实例化指纹"，算它需先做单态化收集（正是要跳过的成本）；
    ② tainted 符号内嵌本地 disambiguator，跨编辑身份稳定性是额外风险。
  - 结论 2：purity **向下封闭**（callee substs 派生自 caller substs；依赖源码命名不了
    bin 类型；trait 求解选中 impl 只来自类型的 crate 或 trait 的 crate）⇒ deps-image
    不会有指向 delta 的吊引用。
  - 验证：gate0 9/9 + 纯度门禁、Clippy `-D warnings`、fmt、diff.sh 30/30 全绿。
- **新选择 = A2（纯化聚合 deps-image）**：deps-image = std 底座未覆盖的 bin 无关实例；
  delta = LOCAL_CRATE + tainted；键 = std 底座键 + 各依赖 rlib 指纹 + 降低指纹（**不含
  bin 派生数据**，会话开始即知，结构上不可能腐坏）。2 元素固定栈（S3′a ImageStack 直接
  承载），无链/屏障/relocation/膨胀；跨项目共享（S3′c）因键无项目身份**自动复活**。
  退化方向只有"少装"（缺实例退 delta 现降），chain 时代"错配装载=全盘错值"的红线在
  A2 结构上不存在。
- **被替代**：s3b-design-fork §4 方案 A 的切分口径（非 LOCAL_CRATE 全进 image）与
  "键必须含本项目依赖集实际实例化"的结论被 A2 替代；A 家族的其余判断维持（单 image、
  bin runner 会话内成像、放弃 per-crate 链——DAG 问题结构性地消失）。B（屏障活性/复杂度）、
  C（每依赖带完整传递闭包 2-3× 膨胀）、D（跨依赖 relocation 分类=静默腐坏面最大）、
  E（≈没做）在本账本下均不采纳。
- **迁移与兼容影响**：无产品面变更；探针 `MIRVM_PURITY_STATS=1` 留 src/lower/mod.rs
  （后续 ripgrep/tokei 施工顺手测 purity 账本用）。
- **再次重估触发器**：① 实测出现 tainted 集巨大的项目（clap-derive 类；每编辑重降
  >100ms 量级）→ 评估第三层"项目本地 tainted image"（那时才面对 A1 的键问题，范围限
  tainted 集）；② 跨项目共享被证实无实需 → 简化回单项目键；③ ripgrep/tokei 的 purity
  账本与 eco 显著不符 → 复核切分口径。

## 7.5b. 2026-07-16：地址模型盘点后的按账手术单（真实模型保留 + P1/P2 根治立项）

> 背景：机器迁移后的架构盘点会话。用户裁定——**保持真实地址模型**（FFI 零编组、
> panic 穿 native 帧、fork COW、JIT 直通 intrinsic、对拍地基，全是它买的，本周
> 99 个真实 crate driver 全绿即证据）；不按账换虚拟模型。逐项判定的根源记录
> 在会话与 docs/corpus.md §5/§6、docs/m4-debt-map.md §6/§7。

1. **P1（thunk 盲区）原理根治立项**：fn 条目可执行化——fn-ptr 值从「冻结区装
   FuncId 的数据槽」改为「i2c 蹦床码址」（未 JIT 时落 mirvm_c2i，已 JIT 直进
   fast 码；HotSpot i2c/LuaJIT 同构）。宿主 C 库经任何形态拿到 fn-ptr（显式
   实参/结构体内嵌），跳过去必落可执行入口——盲区类**结构性消失**（非协议
   表白名单治标）。代价：fn-ptr 表示层重构（反查表改「码址→FuncId」、W^X 页、
   第三固定代码样条域入 L2 键链）。**未动工**。
2. **P2（宿主地址烤模块→缓存挑刺）根治立项**：GOT 式 Operand 间接——foreign
   符号经运行期解析表寻址，模块本体位置无关（L2/image 全量可缓存目标）。
   替代现行 foreign_static_syms 拒缓存判据。**未动工**，IR 设计变更。
3. **P3（VM 层内存隔离）冻结**：题设不可兼得（零拷贝 FFI 与内存隔离互斥），
   沙箱贡献全归 OS 层（seccomp/进程隔离），VM 不掺和。已定场，不立项。
4. **P4（指针非确定）判定为非问题**：native rustc 二进制同漂（Linux ASLR），
   差分 oracle 从不比地址值；「字节级回放」若将来要，是线性内存+JIT 影栈
   +FFI 指针纳管+单线程前提的四联工程（**彼时按 workload 需求再立**，明确
   知道只根治堆侧一半）。
5. **P5（固定基址样条）维持现工程**（0x68/69/6a00 三域 + is_valid_home 白名单
   +动态回退降级为少赚缓存），增量（扩域/回收）记 M7+。

**虚拟地址模型本身的判定**：纯计算语义面「只亏性能」成立（Miri 为证），但
FFI 轴有原理性障碍（native 库留存指针的习惯与 marshal 语义冲突，i 契约 ii
共享内存两种出路都未中 mirvm 使命）——不作方向。

## 7.5c. 2026-07-17：P2 定案——GOT 槽 = 冻结区普通格 + 启动相统一重填（零 IR 变更）

§7.5b 项2 的 P2-1 片调研收口。原计划设想"新 Operand 变体（ForeignSym）+
interp/JIT 逐变体接线"，勘探后发现**根本不需要新变体**：

1. **GOT 槽 = 冻结区普通 8 字节格**（按当前上下文开本侧域：delta 0x69 域 /
   image 0x6A 域）。固定基域下槽地址本身跨进程稳定——字节码烤**槽地址**
   不烤**槽内容**（宿主符号真地址）。
2. **立即数通道**（func.rs `ConstValue::Scalar(Ptr)` / ReifyFnPointer /
   ClosureFnPointer）：foreign 分配走 `Operand::Mem{PlaceBase::Static(slot), W64}`
   ——interp eval_operand 与 JIT 编译道（`jit_compile.rs:1284` 绝对地址
   iconst + load）**两边都现成**，零改动；非零 addend 以
   `SubImm{base, sub: 0-addend}` 精确等价（mod 2^64 算术恒等，非 hack）。
3. **冻结字节通道**（materialize_in 重定位）：初填仍写真值（冷路径逐位不变），
   同时登记修补点 `{addr, sym_idx, addend}`。
4. **槽位本体**以 addend=0 登记进同一修补表；`Module` 新增
   `foreign_syms: Vec<{name, weak}>` + `got_fixups: Vec<{addr, sym, addend}>`，
   随模块序列化（image 侧各表落 image 模块，随 depsimage 文件走）。
5. **启动相统一重填**（run_vm_engine 与 finalize_entry_argv 并列，冷/热
   单一路径）：以与运行期 foreign 调用**同一解析序**（FfiState::resolve：
   hidden 兜底→归档句柄→RTLD_DEFAULT→可选句柄）重解析全部符号，逐修补点
   写 `resolved + addend`。非 weak 未命中 = 响亮退出（陈旧地址是 SIGSEGV 级
   静默错值源；唯一分歧 = 构建期可解析、运行期丢失且路径永不执行的符号，
   接受为可诊断性收益）；weak 未命中写 0（extern weak 缺席语义）。
6. **serde 兼容零代价**：ircache/depsimage 双头验（build_id + 精确回比）+
   `.ok()?` 优雅 miss——旧条目全部自然失效重缓存；含宿主地址的模块原本
   就从未被存（三判据），不存在"读旧快照拿到陈旧地址"的窗口。
7. **分片边界**：P2-1 只建机制（行为逐位不变为验收）；拒缓存三判据
   （`ircache.rs:147` / `baseimage.rs:371` / `depsimage.rs:206`）与 image
   absorb 的 GOT 按名合流（sym idx 重编）留 **P2-3** 退役与验收——届时
   c_process 类从"永不缓存"变 warm 可回放。

**省掉新变体的账**：operand_ok / jit_read / scan_op / collect_ssa_offs /
eval_operand / serde 六个消费点接线全省；冷路径性能零影响（槽读 = 一次
内存 load，常量折叠前的 Imm 读本来就对应一条 movabs）。

## 7.5d. 2026-07-17：P2 收官（§7.5c 实现）——拒缓存判据全退役 + 纯 std 会话不 split

P2-1 机制验收全绿后落地 P2-3：

1. **三判据退役**（`ircache.rs` / `baseimage.rs` / `depsimage.rs`）：宿主地址
   直嵌不再是缓存障碍；`Module.foreign_static_syms` 字段整列删除（机制证明
   冗余后不留死账），lower 三处记账点、image 模块字段、absorb 合并环全清。
2. **P2 目标兑现**：c_process（environ 用户）463ms 冷 → **30ms 热**
   （cache-load 24ms）；zstd/rusqlite/openssl/gix 四个外来符号密集用例
   冷/热输出逐字节一致，deps-image 写盘/回读全通——热回放经启动相 GOT
   重填把上进程 ASLR 地址换成本进程真值，正是 M6 片2 c_process 热路径
   SIGSEGV（§7.3 记账）的根治。
3. **连带修复（先存的 A2 沉默债）**：纯 std 脚本会话（`--extern` 为空）
   按 v1 边界 `pre_key` 恒 None，但 cli `want_split` 只看"未旁路+未装载
   +底座在场"，照样 split → 产物写不了盘 → 键退化 `a2-unstable-{pid}` →
   L2 键链按设计永 miss——**所有纯 std corpus 用例的 L2 从来都是死在
   这里的**（此前被外来符号门闩挡住，从未暴露）。修法与 v1 意图一致：
   `pre_key(...) == None ⇒ want_split = false`（残余全进 delta，键回底座；
   语义不变，单 id 空间 = 经典非 split 路）。deps 会话行为不变
   （rusqlite/openssl 等照常 split+写盘+热读）。
   验证：4 例外来符号三维 + 冷/热×2 一致，`a2_deps_image` 闸 PASS，
   gate5 117/0/0，cargo test 66/66，diff 30/30，diff_cargo 5/5。

## 7.6. 2026-07-17：P1 定案——fn 条目可执行化 = 固定基代码域 stub + libffi Closure 复用

§7.5b 项1 的 P1-0 调研收口（debt-map §6 thunk 盲区的结构性根治）：

1. **病因回顾**：fn-ptr 值 = 冻结区数据槽（内装 FuncId）——解释器反查派发没问题，
   但一旦以**非显式实参**姿势流给 native（结构体内嵌，flate2 C-libz 的
   zalloc/zfree 实锤），native 回调即跳进不可执行数据地址静默 SIGSEGV。
   thunk 机制只覆盖"FFI 声明里显式写出来的 fn-ptr 形参位"，逃逸面天然覆盖不全。
2. **定案**：`freeze_c_fnptr_sig`（extern "C"/System·非变参·全标量类已有）可从
   Instance 的 MIR fn 类型派生 cif 的条目，取址时直接物化**可执行入口**；
   不可派生（Rust ABI / 聚合按值 / 变参）保持数据槽——此类 fn-ptr 被 native
   调用本来就是 UB，无盲区内损失。
3. **形态（复用最大化）**：
   - 每实例一条 16B 手写 stub（`movabs rax, <closure 码址>; jmp rax`）落在
     **新第三地址域族**（固定基代码样条域：delta 0x6C00 / 底座 0x6D00 /
     image 0x6E00+k，与冻结区三域同构），**stub 地址即 fn-ptr 值**；
   - stub 背后的 marshaling 直接用 **thunks.rs 现成 libffi Closure**
     （attach→搬参→call_guest 全现成），不新写 ABI 汇编；
   - 每实例唯一条目值——顺带修复 thunk 按逃逸签名多址的 fn-ptr 相等性小坑。
4. **稳定性纪律（与 asm_sites/GOT 同契约）**：stub 偏移 = 降低期分配位序
   （确定性纪律与 FuncId/asm_sites 相同）；域域由 **instance 类**定（image 类
   恒 image 域——跨运行稳定域；delta 类恒 delta），单一地址身份不破；
   模块只序列化**配方**（FuncId + ForeignSig 有序表，像 asm_sites），
   启动相（run_vm_engine/absorb，GOT 重填的同一点）重建 closure → 填字节 →
   整域 mprotect RX（W^X）；域被占 = 响亮失败按 cache miss 处理。
5. **逃逸物化收编**：interp 的 CallForeign thunk_args 替换环对已是 stub
   码址的值恒等直寄（0/已是 native 真码的直传语义不变）；数据槽条目维持
   原按需 thunk（Rust-ABI 逃逸的唯一可行路径，保留无回归）。
6. **消费面零改**：fn_addrs 反查键统一换成"条目值（stub 码址或数据槽址）"，
   CallIndirect/atexit/catch_unwind/backtrace/main 启动链全是反查语义。
7. **分片**：P1-1（代码域 + 配方 + fn_entry_addr 发放 + 启动相物化 +
   反查换键 + absorb 合流）行为保持"未逃逸调用语义逐位不变"；P1-2
   （负对照 + debt §6 关闭落档）。

**实施确认（2026-07-17 收官，commit `4202317`）**：全项按上落地。验收：
负对照（flate2 C-libz 后端 zalloc/zfree 结构体内嵌回调）完整往返，三维 +
L2 热一致；gate5 117/0/0。debt §6 关闭（残余边界 = 签名不可派生条目，
无实质盲区剩余）。**顺带实得**：每实例唯一条目值修复了 thunk 按逃逸签名
多址的 fn-ptr 相等性小坑；`SIGSEGV 诊断化`（debt ②阶梯）降为可选后补。

## 7.7. 2026-07-17：custom #[global_allocator] 致 __rust_* 跨堆撕裂 → 运行期统一路由

corpus 批7 c_mimalloc（波2，自定义分配器边界探针本意）撞出的产品 bug，
两个实锤实例（退出段 stdout 缓冲、Vec<String> 末档 6144B 缓冲跨堆 free，
*mimalloc 元数据 SIGSEGV*）：

1. **根因**：分配系 builtin 的路由是**按 lower 会话**决定的——
   `engine_builtins` 只在 `allocator_kind == Default` 时注册
   RustAlloc/Dealloc/Realloc/AllocZeroed（引擎堆接管）；但分配语义是
   **程序级**的：驱动会话 kind=Global 时，base/deps image（早前的 Default
   会话烘的 `CallBuiltin(Rust*)`）与 delta/image 的 HIR 展开器生成
   `__rust_*` guest shim（→ 用户 GlobalAlloc/FFI mimalloc）**两台分配器并存**，
   互穿 free = mimalloc `mi_validate_ptr_page` 野读。
2. **修法**：lower 在 kind=Global 时按 `CodegenFnAttrFlags::{ALLOCATOR,
   DEALLOCATOR, REALLOCATOR, ALLOCATOR_ZEROED}` 找到 AST 展开器生成的
   四只本地转发 fn，登记 `Module.custom_alloc_shims: Option<AllocShims>`
   （FuncId 四件套，随模块序列化）；interp 的 `CallBuiltin(Rust*)` 臂
   **在运行期**统一路由到 shim（NULL 时保持引擎堆）——字节码烘在哪个
   会话不再重要。分配由此与 JIT/GOT/stub 的其它"程序级量"同格。
3. **实现自伤一记顺手**：shim 的首个实现漏了 A2 rebase（FuncId 未随
   image_fns 移位），运行期 call_guest 打到野 id 报"ABI 不匹配"错调
   insert_entry/from_iter——补 `rb.fn_id` 同表后正。教训：FuncId 消费
   面任何一个都要过 rebase 清单（exports/fn_addrs/ids/sites/shim 五处，
   设计文档盘点口径自此为"五处"）。
4. **验证**：两最小复现（ga_p_only/ga_vecstr）转绿；c_mimalloc 三维
   逐字节一致（D8k 窗口对账四相位 live_delta=0；线程相位 53110 calls
   无分歧）；gate5 139/0/0。**边界**：kind=Global 的
   `__rust_alloc_error_handler` 路由未动（OOM 冷路径，仍按既有
   ③/Trap 语义；首次 workload 触达时再立项）。

## 7.8. 2026-07-17：native-archive 生命周期分治 + LLVM `\x01` 前缀剥除（corpus 批8 c_aws_lc 撞开锁系列）

1. **constructor 分治解码**：originally 一切生命周期段（.init/.fini/
   .init_array/.fini_array/.preinit_array/.ctors/.dtors）全拒（"dlopen
   生命周期语义尚未定义"）。批8 c_aws_lc 实锤：aws-lc-sys 全量无条件带
   `.init_array`（do_library_init→OPENSSL_cpuid_setup）与 `.fini_array`——
   与批7 c_mimalloc 的 `mi_process_attach` 同族（当时 CFLAGS 绕行）。
   语义判定：**动态加载器的 DT_INIT 就是在 dlopen 时执行它们**，与 native
   进程启动期 constructor 完全同构；mirvm 从不 dlclose ⇒ fini 永不执行 =
   native exit 由 OS 回收。定案：`.init_array`/`.fini_array`/ctors/dtors
   （含优先级）**放行**，旧式裸注入 `.init`/`.fini` 段 **仍拒**（执行语义
   不可靠——仓库内单测实测即 DL 期 SIGSEGV；真实 workload 不供养）。
   配套单测：拒绝例改验收例（DT_INIT 执行置位可证）。
2. **LLVM `\x01` verbatim 前缀剥除**：aws-lc-sys 的 BORINGSSL_PREFIX 全符号
   经 `#[link_name = "\u{1}aws_lc_..."]` 声明——`\x01` 是 LLVM 的 verbatim
   标记，**物化（目标文件/动态符号表）只存去前缀名**；rustc 的
   `symbol_name` 返回带前缀原名。lower 各 dlsym 口径（resolve_call /
   foreign_fn_entry_addr / extern static / naked）此前逐字直查必然全域未
   命中——统一 `canonical_link_name` 剥除一次。
3. **P2 GOT 的键名去重盲点（自伤一记，P2 竣工当晚的漏网）**：
   `foreign_fn_slot` 与 `foreign_alloc_sym` 的键名**未**走同一个剥除——
   带前缀家族的 fn-ptr 条目槽查找 hit=None → func.rs Reify/Closure/const
   全部退回烤 `Imm{lower 期 dlsym 地址}`——**跨进程腐旧 fn-ptr 值常驻字节
   码**：运行撞死面呈 ASLR 运气（Heisenberg：同 driver 同缓存，两跑一崩
   一活），崩点 = EVP_AEAD 派发表真实函数入口。修复 = 两处键名同剥。
   教训：带 `\x01` 的 crate 系属稀有家族（aws-lc-sys/bindgen 产物），普通
   corpus 全不命中——P2 的五例外来符号验收全绿而本例独漏，恰说明"用例
   形态分布"保护性。**诊断链条记录**（对照今后同类调查）：segv-trace
   preload（siginfo+ucontext）→ 栈顶两帧锁到 libffi 调用 → CallIndirect
   native_sig 地址对表 nm 证实目标合法 → fn-ptr 值做同 run/跨 run 分型 →
   lower 发射口径 GOT 槽查找 hit=None 一生成。结账一次到位，中间 4 次错误
   分支（缓存/闭包/启动相/JIT）逐一实证排除。
4. **验证**：c_aws_lc（含 238-crate sequoia 同波）冷/热×3 逐字节一致；
   native-archive 12/12；gate5 全绿。c_mimalloc 的 ctor 绕行 CFLAGS 保留
   （kernel：确定性无必要改）。

### 7.9 2026-07-18：第 0 步收口三单（E27 / G4 / C7，按开放问题闭合性总判排单）

依据 [open-issues.md](open-issues.md)「原理闭合性总判」排的第一批（全部判定
「原理可完全闭合 → 做彻底」）：

1. **E27 weak 符号真地址化（关闭，实修隐藏缺陷）**：验收探针暴露 weak extern
   **static** 长期走 M4 遗留「判空 cell 恒 0」路径（never 进 GOT，std 全部弱探测
   被伪装成缺席走回退；**fn 侧** P2 早已命中=真址/缺席=0）。修复 = weak static
   改走 `foreign_slot(name, 0, true)` GOT 槽，启动相与所有 foreign 符号同一
   重填序真解析；**引擎接管语义符号强制缺席**（非纯直通内建/DENY/
   `__cxa_thread_atexit_impl`——std 对这些走回退，引擎接管语义不被真符号绕开，
   与 fn 取址①同纪律）。探针 `demo/weak_extern.rs`（Option<extern fn> 类型
   `#[linkage="extern_weak"]` static 官形：缺席=None / 命中=Some+可调）三维绿。
   行为变化面：std 弱探测（statx/getpid 等）自本单起解析为真、走主径——gate5
   全量复绿确认无回归。
2. **G4 stale 绕行双钉回摘（关闭）**：`vcvtps2ph.128/256`（批内已内建，软件
   模型与 F16C 硬件逐位单测在案）、psad.bw/pclmulqdq（七族 `2b4766b`）三项
   intrinsic 实证在码。c_exr_image 摘 `half =2.2.1` 钉（两侧同走 F16C 通道，
   sNaN 静默化两侧同发生）；c_flate2 手工 gz/zlib 容器退役、原生
   ZlibEncoder/GzEncoder 回归（simd-adler32 crc32fast 硬件路径双维同选）。
   双 driver 三维逐字节绿。
3. **C7 global_asm/naked `sym` 指向解释态 guest fn（主面闭合）**：渲染期识别
   「非 foreign、非 naked」的 SymFn → 预算 P1 可执行条目（`fn_entry_addr`，
   签名可派生为前提）→ .s 头部导出同名**函数跳板**（`movabs rax, <待码址>;
   jmp rax`；`.set` ABS 形式在 GAS Intel 模式下 `call` 不可发码，实锤后改真
   跳板）。机器码 call → 跳板 → 条目 stub → 蹦床回解释器 = native 链接期
   绑定同构。未定义符号审计口径随更新。探针
   `demo/global_asm_guest_fn.rs`（global_asm 函数经 sym 调 guest fn，返回值 +
   指针回写）三维绿。
   **如实保留的拒绝面（open-issues R16）**：①sym 指向签名不可派生的 guest fn
   （聚合/Rust ABI/变参）——机器码调此类同形本即 UB，响亮拒绝；②SymStatic
   指向 guest static 未接（mangled 名审计仍会命中），按 workload 再立。
4. **方法注**：三条均先「闭合契约」后施工；验证 = 各 driver 三维逐字节 +
   `cargo test --locked` 67/67 + `diff.sh` 33/33（weak_extern、
   global_asm_guest_fn 入册）+ gate5 全量复绿。

### 7.10 2026-07-18：C1 FFI 按值聚合封送闭合（旧 debt §9 / open-issues C1）

- **实锤**：c_tree_sitter 全 parse 路径汇到按值 TSInput（内嵌 read 回调）；
  `ts_node_*` 按值传/返 TSNode(32B)/TSPoint(8B)。`ffi_kind_of` 只收标量，lower 期
  冻入口 Trap「非标量（按值聚合）」。
- **设计**（[designs/c1-ffi-agg-design.md](designs/c1-ffi-agg-design.md)）：
  `FfiKind::Agg(FfiAgg)` 冻结布局（rustc layout 声明序递归展开）+ libffi
  `ffi_type_struct` 全聚合编组（eightbyte/sret 语义不自证）+ 全局一条约定
  「FFI 边界两侧聚合一律按真地址交接」：出向 avalue 直指 guest 内存零拷贝、
  返回强制 `RetDest::Indirect`（结果缓冲 memcpy 至 dst）；入向 closure avalue
  字节地址经新 `interp::call_guest_ffi` 按 callee ParamAbi 展开（Indirect 传址
  / Scalar·Pair 按 FfiAgg 声明序读值），返回 = Indirect sret 直传 / Pair 小档
  `FfiAgg` 重打包。伴生效应：聚合签名 guest fn 自此入 P1 可执行条目候选
  （TS 的 read 回调家族全体可派生、可经 P1 stub 被 native 回调）。
- **施工修出的一只实雷**：≤16B 聚合实参经 `lower_operand` 拆成 scalar/pair
  槽，与 kinds 槽数不齐（av 与 sig 错位 → 首跑 SIGSEGV）；修法 = foreign/
  native_sig 按位置的 `Agg` 实参改取 place 真地址（字节连续在 guest 帧）。
- **边界（如实响亮拒绝，open-issues R17）**：union 按值、SIMD 向量按值、
  变参尾参位聚合、align>8 聚合、multi-variant enum 按值。
- **验证**：合成矩阵探针 `demo/ffi_agg_probe.rs`（出向 8B/24B/32B 按值参 +
  出向 8B/32B 聚合返回 + 入向聚合回调参数（P1 条目）+ 入向 Pair 重打包/32B
  sret 直传，内嵌 fn-ptr 成员 TSInput 同形）三维绿；**c_tree_sitter 原样
  三维逐字节转绿**（其 expected-red pattern「非标量（按值聚合）」XPASS 退役，
  接线 gate5 corpus 段）；`cargo test` 67/67、`diff.sh` 35/35、gate5 全量复绿。
- **方法注**：本单即「原理可完全闭合 → 无论工程量做彻底」的第一例大工程
  （open-issues 闭合性总判 § 可闭合清单 C1 位）。

### 7.11 2026-07-18：C2 native-archive 闭包「符号在 rlib」闭合（旧 debt §10①）

- **实锤两族**：wasmtime `libwasmtime-helpers.a` 蹦床调 `resolve_vmctx_*`
  （`#[export_name]`，定义在自身 rlib）；bzip2-sys vendored BZ_NO_STDIO 断言桩
  `bz_internal_error`（`#[no_mangle]`，定义在 Rust rlib）。`-z defs` 单归档
  自闭合够不着 → 拒「无法安全转换为共享库」。
- **路线**（[designs/c2-rlib-symbols-design.md](designs/c2-rlib-symbols-design.md)）：
  首链失败救援链——`src/elfsym.rs` 二进制级静态枚举归档全部成员的
  `SHN_UNDEF` 全局/弱符号（**无工具文本解析**；ad-hoc 自查 §5 修订：初版
  ld stderr 解析作废）∩ `exported_defs()`（rustc `exported_non_generic_symbols`，
  native final link 集符集本机权威）⇒ `fn_entry_addr` 预算 P1 可执行条目 ⇒
  `.hidden` 跳板（`movabs+jmp`，C7 同形制，防 RTLD_GLOBAL 插桩）并入重链。
  首链成功路径 cc 行与缓存键逐字节不变；注入路径键含排序 (name,addr) 对。
- **施工实雷一只（单测反咬）**：GNU ar 对 >15 字符成员名用**字符串表引用
  `/N`**（如 `/0`），初版「名字以 `/` 开头即元数据」把真对象成员整个误判
  跳过——真实归档 undefs 恒空；修订元数据判定为精确名单（`/`、`//`、
  `__.SYMDEF`、`/SYM64/`），`/#1/len` BSD 内嵌名另剥。教训：单测只覆盖
  短名 ar，真实世界 ar 是长名——测试矩阵教训同「用例形态分布」一条。
- **验收**：MRE（`/tmp/mre`）转绿（`mre ok`）；`c_bzip2_csys`（bzip2 0.4.4
  vendored C 后端 write/read/mem 全矩阵 + 损档错误路径）三维逐字节绿；
  **c_wasmtime_wat 换面**：层①消失，锁层② `inline asm noreturn`/70
  （= C3 入口，gate5 expected-red 接线，C3 落地 XPASS 强制转绿）。
  断言桩运行期路径（BZ_PANIC can't-happen 族）如实不覆盖（native 同）。
- **边界（如实拒绝另立）**：rlib 数据符号（static 被 C 引用）。
- gate5 157 → **158 pass + 1 expected-red**，cargo test 68/68（elfsym ar
  枚举单测），diff.sh 35/35。

### 7.12 2026-07-18：C3 inline asm `noreturn` 两面孔（含一次自我反转）

- **对象**：c_wasmtime_wat 层② `resume_to_exception_handler`（asm
  `options(noreturn)`；asm-stub 工厂只覆盖 call-return 形，遇即 TRAP 70）。
- **合成 spike 先判「不可开」**：手工 setjmp/resume 协议（CallThreadState
  同构）——v1 独立 setjmp 帧已死（双维同错，协议自伤教训：setjmp 必须
  内联在存活帧）；v2 内联后 native 绿、mirvm SIGSEGV，MIRVM_SEGV_DUMP
  实锤落点 = `Channel::send` 内部（asm-stub 捕获帧已死且宿主栈区被后续
  解释帧复用，epilogue 弹出复用期栈写 → 跳入 send 中段）。
- **真实 workload 复验反转**：c_wasmtime_wat 冷缓存完整支持后**全 trap
  面三维确定性绿**（三次重复 + mirvm/native/JIT=1 逐字节）——wasmtime 的
  trap 链（closure → trampoline stub → cranelift 真码 wasm → 信号 handler
  → resume stub）整条在存活链内推进，捕获帧在恢复时仍有效，与合成协议的
  复用形态不同。asm 本体 = 真机器码忠实执行，transfer 本体成立。
- **终判**：noreturn 两面孔物化保留（outs 恒空 + 落点合成 Unreachable）。
  ① ud2/终止形 = 完全可闭合（`demo/noreturn_ud2.rs` 三维绿入 diff.sh，
  exit=132 双侧）；② resume/longjmp 转移形 = 真实 workload 三维绿支持
  物化；**如实边界（不宣称全形态闭合，转 open-issues E32）**：解释帧在
  捕获与恢复之间复用捕获帧宿主栈内存的合成协议可撞死（v2 实锤），消除
  = JIT 真帧身份。
- **验证**：c_wasmtime_wat 由 expected-red **转绿入 gate**（VM-in-VM 旗舰
  全程：cranelift 编译/实例化/表/global/宿主回调/rayon/trap 上抛全通）。
- 全程证据链：[parked/c3-resume-spike.md](parked/c3-resume-spike.md)。
- gate5 → **159 pass / 0 expected-red**，cargo test 68/68，diff.sh 36/36。

### 7.13 2026-07-18：C5 dyn 上溯 vtable 变换闭合（批10 波1 三修收官）

- **对象**：open-issues C5（M4.2 欠账）——`Arc<dyn Source> →
  Arc<dyn Any+Send+Sync>` 类 principal 变换的 dyn 上溯（trait upcasting）。
  批10 波1 由 c_datafusion_sql 撞红、c_typst_pdf 同修供养。
- **目标语义**（cg_ssa base.rs `unsized_info` 同构，rustc-src 实证）：dyn→dyn
  upcast 时 `tcx.supertrait_vtable_slot((src_dyn, dst_dyn)) → Option<usize>`，
  目标 vtable = `*(源 vtable + slot×8)`；None = auto trait 差 → vtable 不变
  pair 位拷；同 principal = pair 位拷。
- **导航 = `dyn_unsize_tails` 统一递归判据**（src/lower/func.rs），每级四路：
  ① builtin_deref 直达双 Dynamic（Ref/RawPtr/Box/DerefPure）；
  ② **解引用落点再判**——`*const ArcInner<T>` 落点非 Dynamic 时对落点再跑
  struct_lockstep 取尾对（→ data 字段双 Dynamic）。本片是收官关键：此前
  ping-pong 根因 = 只判 builtin_deref 直达、未在落点再判（Arc → NonNull →
  Pat → `*const ArcInner` → ArcInner → data 的 fresh 内部 lockstep 由
  `struct_lockstep_tails_for_codegen` 实锤，arc_up MIR 实证
  `PointerCoercion::Unsize` 直出）；
  ③ Pat 壳剥除（本 nightly NonNull = `pattern_type!(*const T is !null)`，
  base = `*const T`）；
  ④ Adt 同构结构体唯一非 ZST 字段递归（cg_ssa unsize_ptr Adt 臂同构）。
- **chase 物化**（PC::Unsize 臂）：源胖指针对 data 半位拷 + meta 半
  `PlaceExpr{steps:[Deref, Offset(slot×8)]}` + `Operand::Mem{W64}` 读目标
  vtable；Arc/&/Box 胖布局（data, meta）一致。place 形（resolve_place +
  half_operand）与非常量 Slot meta 半两形均接；常量胖指针上溯如实报错未接
  （未遇真实 workload）。
- **伴生两修（批10 波1 红三同修，各配 driver 转正）**：
  ① **weak/COMDAT 跨归档碰撞**（c_risc0_run）：`reject_symbol_ambiguity`
  纳入 nm posix 类型字母——W/w/V/v/u 全放行（首件胜出，native link 语义）、
  恰一 strong 放行、双 strong 仍拒；含 GNU unique（`u`，`_ZGVZ*` 族）与
  weak object（`V`）。单测 `duplicate_weak_symbols_follow_native_link_semantics`。
  ② **asm-stub xmm 16B 槽**（c_typst_pdf 过 `__m128i`）：`AsmIoVal`/`AsmIoDst`
  = Scalar + VecBytes(PlaceExpr, u32)，`Terminator::InlineAsm` ins/outs 改型；
  lower 按 layout>8B 走向量字节通道；interp 与 jit `analyze_frame` 同步。
- **验证**：合成探针 `demo/dyn_upcast_probe.rs`（Arc 包装链上溯 + & 直达上溯
  两形，入 diff.sh 36→37）；**c_datafusion_sql 三维 95 行逐字节**（Q1–Q7 +
  plan；A==B==C）；**c_typst_pdf** PDF `fnv=a07292af73881d72` 锚点无回归
  （A==C）；**c_risc0_run** journal `fnv=fbb47fc52544af18` 三维绿。
  gate5 → **162 pass / 0 expected-red**，cargo test 69/69，diff.sh 37/37。
- **教训落档**：diff.sh 默认 `target/debug/mirvm`——`cargo test` 不重建 bin
  目标（无 Rust 集成测试），debug 陈旧二进制致 5 假 fail（五只新探针恰好
  全踩在缺失特性上）；复跑须 `MIRVM=target/release/mirvm` 或先 cargo build。
  gate5 内部默认 release 不受影响。

### 7.14 2026-07-18：缓存重审——根迁 `$HOME/.mirvm` + `mirvm cache` 管理命令

- **缘起**：`~/.cache/mirvm` 快速胀大（批10 期间十几 GB）。实证解剖
  （空根跑 ethers_evm 单 driver 两维）后用户裁定：① 内容不是随手可弃的
  cache，是 mirvm 统一管理的本地仓库，迁 `$HOME/.mirvm`（`MIRVM_HOME`
  可整体改址，不再读 XDG_CACHE_HOME）；② 补管理命令；③ 重审必要性。
- **增长机理（实证）**：主项 = 每个 frontmatter 程序养两套完整 cargo
  target——mirvm shim 一套（ethers 96 包 424M：host 侧 proc-macro/build
  script 全量 314M + target 依赖 metadata-only rlib 81M）+ B 维 native
  对拍一套（+313M）；重型树（polars 351 crate/datafusion/typst/swc/
  wasmtime/rustpython）每套 1.5–4G，corpus 170+ driver 全跑即几十至上百
  GB。次项 = `MIRVM_BUILD_ID`（src 树内容哈希）每次 src 改动令
  base/deps/ir 全体换代且旧代无人删。
- **必要性审计（全组件判定）**：sysroot（229M 一次性）= MIR-rich std 唯一
  来源，必要；`scripts/<hash>/target/mirvm` = 不打包世界的本地依赖库
  （deps image 盖戳源与真相源），必要——`.mirvmar` 落地后打包形态可豁免；
  `scripts/<hash>/target/debug` = 仅 B 维 native 对拍需要，非引擎必需；
  deps/base/ir = 性能加速器（lower 9.6×、热加载 11–15×），必要但陈代无
  GC 是真浪费；native-archives/global-asm/asm-stubs = 运行期 dlopen 对象，
  内容键控去重稳定，必要。**结论：无一体可裁，缺的是管理面。**
- **管理面落地**：`mirvm cache status`（各族体量 + 陈代体量）与
  `mirvm cache purge`（默认 = 清陈代，保守面；`--deps/--base/--ir`
  整族；`--scripts` 最大件；`--all [--sysroot]`；`--dry-run` 全局旗）。
  陈代判定：三族文件首字段均 build_id（postcard varint + UTF-8），只读
  头 32 字节 peek——**禁用整包解码**（ir::Module 反序列化触发冻结区定基
  mmap，purge 工具不可承受其副作用）；条目扩展名（ir=bin、deps/base=img）
  过滤，`build.log` 等构建副产不碰不报（首版实测咬出此洞）。
- **与 `.mirvmar` 愿景的关系**：`$HOME/.mirvm` 同时是未来 mirvmar 相关
  本地解析的自然家；本片只备好根目录形态，不动 mirvmar 本体。**统一依赖
  cache 方向已裁定（用户 2026-07-18）**：近期 = 共享 cargo target dir
  （cargo fingerprint 即编译键内容寻址）；终态 = mirvm 原生内容寻址 store
  （登记 open-issues D14，与 mode B/D1 合并评审立项）。
- **跟迁面**：tests/diff_cargo.sh（SCRIPT_CACHE）、a2_deps_image.sh、
  real_projects.sh（XDG 隔离位全部换 MIRVM_HOME 指向同名目录，prepare/
  marker/fake-mirvm 回归面零改动——新变量与旧布局同路径）。
- **验证**：cargo test 73/73（cachectl 四测：peek 往返/分类/清陈代留当代
  +dry-run/--all 与 --sysroot 并集）；手动矩阵（空根、MIRVM_HOME 覆盖、
  陈代 fixture、扩展名过滤、purge --all/sysroot）；新根冷启动自愈
  （sysroot 重建 + fib 全链）；diff.sh 38/38；diff_cargo 5/5；gate5
  **167/0/0**。
- 旧根 `~/.cache/mirvm` 无迁移无兼容读（全组件自愈），文档提示可手删。
  自动 LRU/容量上限不立项（手动 purge 够当前节奏；open-issues E17② 更新）。

### 7.15 2026-07-18：统一依赖存储近期片——共享 cargo target dir（D14）

- **裁定回顾**（用户 2026-07-18）：cache 应机器级统一维护——多脚本/多项目
  共引 X@V 时其编译产物只存一份（去重单位 = 版本×features×依赖闭包×
  cfg/flags×toolchain 的完整编译键，cargo fingerprint 天然即此键的内容
  寻址）。并发模型 = 发布一次后续只读命中、无大锁常驻；清理粒度粗可接受；
  终态原生 store 登记 D14 与 mode B 合并评审。
- **实施**：`cargo_project_command --target-dir` 由 per-project
  `target/mirvm` 改 `$MIRVM_HOME/target/mirvm`（`MIRVM_TARGET_DIR` 可改址）。
  **产物定位零改动**——runner 协议本由 cargo 把假二进制路径传给 runner，
  从不扫 target dir。共享化唯一新增碰撞面 = 无指纹的 `debug/<binname>`
  （同 stem 不同路径脚本），解 = `[[bin]] name` 带路径哈希短缀（package
  名保持 stem，onboarding grep recipe 不动）。B 维同族：materialize_script
  增写 `.cargo/config.toml`（target-dir = `$MIRVM_HOME/target/native`），
  shim 显式旗覆盖不受影响、native cargo run 吃文件配置进共享。
- **附带收益**：deps image 的 extern 路径指向共享目录 → 依赖闭包相同的
  脚本/项目 deps image 天然共享（a2_deps_image S3′c 手工驱动侧同址
  --target-dir 跟迁，测试语义不变）。real_projects 诊断 remap 补共享路径。
- **实测**（purge 后两树）：ethers 21.5s → 二跑 0.66s（fingerprint 新鲜）；
  ethers(96 包)+xlsx(35 包)并集 525M（旧模式 ethers 单树 mirvm-side 即
  424M）；script dir 缩至 KB 级（manifest+lock+src+config）。
- **验证**：cargo test 73/73、diff.sh 38/38、diff_cargo 5/5、
  a2_deps_image PASS、gate5 **167/0/0**。
- **配套**：cachectl 族谱加 target 族 + `purge --target` 旗；USAGE 补
  MIRVM_HOME/MIRVM_TARGET_DIR；cachectl du 硬链接重复计数（cosmetic，
  记此不修）。

### 7.16 2026-07-18/19：结构重构战役——os/arch 双 leaf + 巨型文件治理（E21 闭合）

- **缘起与北极星**：用户判代码库渐成屎山，裁定对标 OpenJDK JVM 分层
  （share/vm 可携核心 + os/ OS 族 + cpu/ 架构族），兑现 DESIGN.md P7 与
  open-issues E21。战前两路 explore 全图侦察（OS/arch 触点分布 + 八大
  文件职责解剖与拆缝判定），铁律 = **纯搬移、零行为变化；每片 green
  后才许进下一片**（验证纪律后段放宽为：cargo test + diff 双态 +
  点测，gate5 战役收尾一轮——用户裁时裁盘）。
- **片1 os/ 层**（`0215d4d`）：`src/os/mod.rs`（边界契约 = leaf/原语不
  裁决/直通优先 + 非 linux compile_error!）+ `linux/{mem,thread,signal,
  dll,process}`——mmap/TLS/信号/dlopen/fork/syscall 全部触点归并，
  **非 os 域 `libc::` grep 机械清零**；guest 语义裁决（fork 守卫、信号
  白名单、sigaction 改拷贝、栈放大策略）全留引擎业务侧。配套 =
  `vm/engine/addrlayout.rs`（固定基址三方共享常量层）+ tsan 同源门禁
  扩 `#[path]`。
- **片2 arch/ 层**（`190e51d`）：`src/arch/mod.rs`（同款契约 + 非 x86_64
  compile_error!）+ `x86_64/{intrinsics,asmstub}`——x86.rs 硬件执行体
  整搬（920 行硬件交叉验证测试随迁）、stub 字节工厂与 int3/xgetbv
  归位；`is_x86_feature_detected`/`core::arch`/`asm!` 非 arch 域清零。
- **片3-6 巨型文件治理**（`40b107b`/`3318900`/`a22420d`/`ab77de5`）：
  func.rs(5226)→`lower/func/` 八件（LowerCx 与工具带留 mod.rs，cast/
  unsize/term/asm/call/intrinsic/simd 单入口簇成子文件 impl 块）；
  interp.rs(3642)→`engine/interp/` 七件（地基与异常展开 edge 协议、
  call_guest 发布协议读侧锚点留 mod.rs；volatile/rvalue/stmt/services/
  call/runblocks 整函数切割，**无一处臂级手术**）；
  jit_compile.rs(4302)→`engine/jit/` 七件（与原 jit.rs J1 状态基座
  合并：state feature-free + compiler/admit/helpers/translate/frame/
  lsda_probe 门内）；
  lower/mod.rs(2765)→`lower/linker/` 五件（Linker 结构 + entries/
  alloc/got/calls 字段分组带）+ builtins/ffi_sig/purity/rebase 四自由
  模块——**lower/mod ⇄ native_archive 文件级环解**（依赖变 lower/mod →
  native_archive → lower::{linker,ffi_sig} 单向无环）。
- **质量定义（DoD 可验口径）**：每模块 `//!` 契约头（25 个拆分文件
  补写）；可见性最小化（树内 pub(super)、跨树 pub(crate)、字段分组
  精准放宽）；`cargo build --release --locked` 零警告；cargo test
  74/74；diff.sh 38/38（含 JIT=1 双态）；diff_cargo 5/5；tsan 同源
  编译过；gate5 **167/0/0**（战役收尾一轮）。
- **记档接受面**：lower/asm.rs 寄存器分配与 llvm.x86 名表 = rustc 类型
  耦合，留 lower 域（arch/ 禁 engine/rustc 类型的 leaf 纪律不硬搬）；
  interp SIMD/128 子带与 CallBuiltin 臂的臂级再拆不做（整函数切割已够，
  手术风险不值）；spikes 冻结原型与 ir.rs 全仓契约不动。
- **伴生两件**：`lower_for_image_build` 零调用方——用户裁定标注预留
  （依赖 image 构建侧未接线，勿再提删除）；gate/corpus 逐驱动清
  deps/ir 缓存（本机 98% 磁盘用率实锤，`e1445b9`）。

### 7.17 2026-07-19：R1 重构定稿——同步故障信号 = 崩溃期语义，非 handler 支持

- **用户裁定**：同步故障信号（SEGV/BUS/FPE/ILL/TRAP）本就是进程崩溃信号，
  mirvm 不应追求执行 guest handler 代码，而应「适当插入清理的东西」——
  落定为**崩溃诊断化**（T4 泛化并入总案）。
- **实证钉清（两个反转）**：① sigread 三维对拍（native/mirvm 均装
  SA_SIGINFO|SA_ONSTACK handler，mirvm 侧是**宿主 std 启动时装的那只**）
  ——guest std 的 `stack_overflow::init` 条件安装（仅 SIG_DFL 才装）
  读回宿主 handler 非 DFL → **静默跳过**，corpus 全绿从未触发 R1 拒绝；
  ② 深递归实测 `thread 'mirvm-guest' has overflowed its stack` + SIGABRT
  ——宿主 std handler 代打，与 native stack overflow 同死亡形态。
  **崩溃期退出语义已忠实**（真故障 = 同信号死亡），native 信号死亡同样
  无 atexit/析构——「清理」在 native 语义里本不存在。
- **三边界定稿**：已忠实 = 退出信号与死亡形态（无需动）；可闭合 =
  崩溃诊断化（故障落点归属判定：guest 冻结域/代码域/帧区 → guest 化
  崩溃行 → 同信号终止；T4 并入）；永不可闭合 = guest handler 代码执行
  （宿主/guest 故障不可分辨；解释器深度不可重入、信号帧内跑解释态代码
  原理性非 async-signal-safe；handler 返回 = 无限再故障）。
- **同场钉清**：E19 syscall 三形态与 chokepoint（FFI/变参可虚拟化，
  硬编码 inline-asm syscall 唯 seccomp 可兜）；E11 栈深度逐字节一致
  = UNSPECIFIED 不追求（用户确认）；R8 asm goto 证据级记档（Cranelift
  对一切 inline asm 均 fatal 的 issue 素材 + 自研 JIT 设计注脚）。

### 7.18 2026-07-19：syscall 全通道拦截定稿与使能片排期（E19 降级 + T5 立项）

- **用户问**：mirvm 自扫代码找 syscall 指令、用 GOT 类方法替换为 wrapper，
  是否可行？**答：可行且更强——不需要扫二进制**：guest 全部 inline-asm /
  global_asm 的 GAS 文本都由 mirvm 自己的 asm-stub 工厂拼出（生成点在手），
  文本改写即可；JIT 与解释共用同一批 stub 入口，拦截点天然唯一且全覆盖。
- **全通道定稿**（「拦截一切 syscall」真实生态成立）：① FFI libc 包装 =
  builtin 注册表现成挂载点（HostWrite/HostGetenv/HostFork/HostSignal/
  HostSyscall 在产拦截中）；② `libc::syscall` 变参 = HostSyscall 单点已内建；
  ③ inline-asm 裸 syscall（rustix linux_raw）与 ④ global_asm/naked 内 =
  **T5 使能片（本日排期）**：文本检测 `syscall` 助记符 → `call
  *mirvm_syscall_slot(%rip)` 改写（间接槽随 .so 物化 + dlopen 后重填，
  P2 同款启动相哲学）→ trampoline 保 syscall 全契约（整数寄存器/flags/
  xmm/mxcsr 全保——真 syscall 不动向量态，wrapper 必须同纪律）→
  `mirvm_syscall_dispatch` v1 直通 + TRACE 旋钮；⑤ vendored C：常态经
  native_archive 链接序插桩可闭合，罕见叉（C 内联汇编裸 `syscall`）不透明；
  ⑥ JIT 与①③同入口；⑦ 对抗式自修改无真实形态。**唯一如实残余 = ⑤罕见叉
  与⑦，只有 seccomp 能兜**（原判不掺水）。
- **边界纪律**：本片只备**钩子**（直通即零行为变化 + TRACE 实证）；虚拟化
  语义（统一 fd 空间/假 FS/路径重定向/计费）属 D10 本体——没有它，拦截是
  空钩子；有了它，三形态+⑤常态无旁路。
- **覆盖边界（如实）**：`sysenter`/`int $0x80` 与 `.byte 0x0f,0x05` 对抗
  书写不接（无真实形态，重开需实锤 crate）。
- **T5 施工实录（2026-07-21，`d0470fc`）**：一次实锤反转——初版
  `call [rip+slot]` 被 ld 拒（全局符号 PC32 重定位不可用于 shared
  object），改 **GOT 两级间接**（`mov r11,[rip+slot@GOTPCREL]; call
  [r11]`：GOT 项装载期填 → 命名 .data 槽 dlopen 后重填；r11 恰为
  syscall 契约可坏寄存器）；trampoline 增补 xmm0-15+mxcsr 无条件保全
  （TRACE 打印会碰向量态，诚实纪律）；global_asm! 在本 toolchain 为
  Intel 语法（AT&T 初版被拒实锤）。验收全绿：76/76、探针三维一致 +
  TRACE 双拦截实证、diff.sh 39、rustix 系零回归。

### 7.19 2026-07-19：T1 战役收口——M5.4c/d 全落地（T1/E1 闭合）

- **战役地图**（m5.4-design §3.2-3.4，四片各 commit 全绿）：
  - T1-a ABI 泛化（`7d21ad6`）：CalleeAbi 全形态（sret 前插 / Scalar /
    Pair(lo,hi) / Indirect + track_caller 幻影尾参），镜像 interp ABI v2
    展平序（frame-abi §10-3 冻结面，JIT 只实现同一展平）。
  - T1-b 五调用助手（`4ffbb35`/`7102d2b`/`6a444b1`）：CallIndirect /
    InlineAsm / TlsRef / CallForeign / CallBuiltin——630 行 interp
    CallBuiltin 臂提取 `exec_builtin` 共享本体，「助手调 interp 本体、
    不复制逻辑」自此成为 JIT 助手族定式。
  - T1-c unwind 产品化（`83e168d`）：四调用臂三向全开（Cleanup =
    try_call + `emit_cleanup` 公共 pad 发射器；Continue = PLT/c2i 纯
    穿透；Terminate = 边界助手）+ Resume = `exception_var` →
    `_Unwind_Resume`（cg_clif 同构）+ TerminateAbort 助手；双 CIE
    （plain + personality CIE，DW.ref 间接 `rust_eh_personality`）+
    全覆 LSDA（无 handler 站点发 lpad=0 项——rust personality 对无
    call-site 项的 ip 返回 Terminate，probe 实证准则）。
  - T1-d 准入放开（`5252a89`/`5a1bcbe`）：admit 三表（stmt / rvalue /
    terminator）从「白名单 + catch-all 拒」改为**映射表穷尽**——SIMD
    15 族 + Sat128 + rvalue 三件经 `mirvm_simd_stmt` / `mirvm_simd_rv`
    统一助手调 interp `simd_exec` 共享本体（19 个本体函数整搬，
    abort 文案逐字保留）；IntCmp3 / NicheDiscr CLIF 内联；Trap/Nop
    产品化（`mirvm_jit_trap` 与 interp `engine_abort` 同文案同
    exit(70)，区别于 TerminateAbort 的 134 通道）。
- **f156 序坑（T1-c 唯一设计外实锤）**：`std::panicking::panic_handler`
  闭包的 bb 序里 Resume 先于其 pad——cleanup 链尾经链内**正常边**落
  resume（「cleanup 块只能由 unwind 边进入」只对链头成立），懒声明的
  exception_var 在 use 点未定义。修 = 入口预声明 + def 0 兜底，pad 的
  def 经支配关系覆盖真用点。
- **鉴出并修既有 bug 四件**（逢调即编 + 热循环探针显形，均非 admit
  漏臂）：
  ① call_foreign 导入签名 7 参 vs 实传 8 参（T1-c 加 terminate 旗漏
     改 `sig_cf` 与 Cleanup 支内联签名）→ 含 CallForeign 函数全静默
     留解释（`10390cb`）；
  ② 寄存器 bitcast 喂 `MemFlagsData::trusted()` 被 cranelift 0.133
     verifier 拒 → 凡含标量 f32/f64 rvalue 的函数 define 全失败
     （`10390cb`）；
  ③ Bin128 with_overflow 旗标槽 SSA 误提升（frame.rs dst 恒扫 16B，
     (u128,bool) 旗标在 +16 → 读侧取 SSA 零值 = 假阴性 →
     saturating_mul 给包绕值，`aca8eb6`）；
  ④ frame.rs mask/ptr 落帧区间欠覆盖（mask 按 mask_bytes、
     gather/scatter 指针 lane 恒 8B，`5252a89`）。
  ③④同族：frame 区间模型「interp 足迹 > 标记区间」是系统性盲区，
  鉴出路径 = 逢调即编全量 + 真实函数热循环探针（sat128_probe 入库
  锚定）。
- **设计偏离一处（如实）**：Q4 原案「SIMD 净映射家族 CLIF 内联 + 其余
  助手」未取——T1-d 一律全助手先行（语义权威唯一、零漂移优先），
  CLIF 向量内联降为 E7 纯性能项；T2 语义面随之闭合。
- **锚点全绿**：gate2 unwind 九用例 9/9、cargo test 76、diff 双态
  45/45（新增 jit_unwind_probe / sat128_probe）、diff_cargo 5/5、
  m51 simd_insert/simd_shift/x86_vectors 全过、gate5 **167/0/0/0**
  （fib(32) JIT **54ms** ≤80ms 硬门维持，较 M5.4a/b 的 74ms 还收）。
- **连带闭合/进展**：T1、E1（间接调用准入）闭合；R3 的
  `_Unwind_Resume` 移出 Unsupported 面；E32 记进展（JIT 帧 = 真
  native 帧含真 unwinder 穿透/着陆，setjmp/longjmp 所在函数一旦发布
  即脱出 hazard 面；interp 帧路径维持原记账）。

## 8. 尚未兑现或需要重新验证的架构承诺

- ~~P7 设想独立 `src/os/` 物理层~~（**2026-07-18/19 已兑现**：`src/os/` + `src/arch/` 双 leaf 建成，E21 闭合，见 §7.16）。
- “engine 是 library”目前只是 crate 结构；进程退出、全局 TLS key、泄漏式生命周期使其还不是稳定
  多 Engine 嵌入 API。
- `.mirvm` mode B、fat target artifact、checked 模式、alloca 局部、方法级 JIT 均仍是设计，不是现状
  （mode B 的路线 2026-07-14 已由 §7 D9 定向：先 L2 缓存，包=缓存可移植化）。
- static `.a`→`.so` 的受约束 Linux/ELF 切片已实现；非 PIC、跨 archive 依赖/顺序或重名、
  RTLD_DEFAULT 重名、constructor、thin、export-symbols 仍是明确拒绝面。它们需要新 link plan/
  生命周期设计，不能从 blake3 外推通用。

## 9. 改变决策时的记录模板

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
