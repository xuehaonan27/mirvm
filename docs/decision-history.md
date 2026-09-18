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
   普通 native→guest 回调必须按**当前线程**查 ctx；跨线程回调使捕获创建线程 ctx 的 thunk
   原理上不正确。因此边界 TLS + lazy attach 被确定。当时也把 signal 纳入这条入口；§7.54
   后来证明信号帧根本不应 attach ctx，而应只登记、再由 owner Engine 安全点建立新 activation。
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
| signal | “有 thunk 即可直通”曾被当成接近完成 | §7.54-§7.55 已落地固定 22 字节原子登记桩、进程定向 owner inbox（待处理信号箱）、`SI_TKILL` 目标 pthread cell（线程槽）与普通安全点派送；oldact/非 LIFO close/线程退出收口/在途 frame、受控 `raise` 和自产 archive 三符号桥已闭合。同步故障、realtime、高级 flags 与进程定向外部信号的安全点延迟仍是 R1/R21 的明确边界 |
| guest backtrace / unwinder context | 移除 StubZero 后曾以“可 dlsym + 可回调 guest thunk”为由让 `_Unwind_Backtrace` 等走通用 host FFI | thunk 只解决调用方向，宿主 unwinder context 仍只含 libffi/解释器帧。当前 Raise/Delete 有 guest 专用语义，Backtrace/GetIP/GetIPInfo/FindEnclosingFunction/GetCFA 由影子帧实现；其余 11 个 context/state/Resume/ForcedUnwind 符号明确 `Unsupported`，只在有相应 guest frame/IP/LSDA 翻译与差分探针时重开 |
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
- **D8d/D8e 机制沿用而非新造**：signal 当时的 AS-trampoline 直接复用 M4.4 thunk 工厂
  （handler = `extern "C" fn(c_int)` 与逃逸 guest fn 同构）；这段信号帧直执行机制已被
  §7.54 推翻，现只作历史。backtrace 影子帧的 IP 是合成 token
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

### 7.19 2026-07-21：T1 战役收口——M5.4c/d 全落地（T1/E1 闭合）

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
- **连带闭合/进展**：T1、E1（间接调用准入）闭合；JIT 内部 MIR `Resume` 已能用
  `_Unwind_Resume` 续传宿主 unwinder，但 guest 可见的同名符号仍在 Unsupported 面；
  E32 记进展（JIT 帧 = 真
  native 帧含真 unwinder 穿透/着陆，setjmp/longjmp 所在函数一旦发布
  即脱出 hazard 面；interp 帧路径维持原记账）。

### 7.20 2026-07-21：M5.5 收口——vmctx 终裁落笔（T3 闭合，M5 战役全收）

- **范围裁定**（用户 2026-07-21）：按 m5-design 原案收口（计量 + 落笔 +
  gate6），不融合 E6（分配/TLS 内联是另一个战役，应由 corpus 性能数据
  驱动立项）；**复测触发器并列双闸**：E6 进场 或 多 Engine 嵌入立项
  （SHARED-static 是多实例真 blocker，与性能无关），先到先裁。
- **交付**：
  - 片1 `05021d2`：`MIRVM_JIT_STATS=1` 助手频度统计（12 桶原子计数 +
    atexit 单行 dump，关闭时零观测成本）。ctx 触碰桶 = alloc/tls_ref/
    c2i/call_indirect/call_foreign/call_builtin/call_terminate；纯计算
    桶（simd_stmt/simd_rv/bin128_ovf/volatile×2）不取 ctx，仅作密度参照。
  - 片2 计量跑批：fib(32) 61–80ms（**全空桶**——数值内核零 ctx 站点
    直证）；rayon 冷 506ms / 热 189ms（tls_ref=1392、alloc=0）；
    **corpus 全量 129/0 约 6.3min**：110 crate 有非零桶、19 全空；
    格③ 候选总量 = alloc 7.82M（59 crate）+ tls_ref 0.38M（17 crate）；
    c2i 586.6M 与 call_terminate 229.98M 属格② 固有形状（解释兜底
    边界 / ABI 边界守卫），不作用 T/R 之差。
  - 片3 `aae9aec`：vmctx-passing §7 落笔（T 骨架生产实证 + 基线 +
    双触发器 + T→R 单开关路径与诚实条款 + 挂载点评审两项不动留 E7）
    + tests/m5_gate6.sh（判据①-⑤复用 gate5 全量，增量 = ⑥ §7 核查
    + stats 冒烟 + ⑦ 本条核查）。
- **终裁结论（与 2026-07-11 分层判断互证）**：**T 骨架 = 生产定稿**。
  准入三表穷尽的今天，编译码 ctx 站点仍为零——格① 架构承诺结构性
  不存在、格② 助手形状与实现无关、格③ 未进场；没有为假想负载预付
  寄存器租金。R 保持 ABI 兼容缓存层身份，挂双闸触发器（见
  vmctx-passing §7）。
- **锚点**：cargo test 76、diff 双态 45/45、corpus 129/0、gate6 全绿
  （gate5 167/0/0/0 + 增量三查）。M5 战役（M5.0–M5.5）自此全收口；
  下一战役候选 = E6（性能轴，触发复测闸①）或 corpus 扩编/C4（功能
  轴），排序听用户。

### 7.21 2026-07-22：外部审核驱动的稳定化战役——「M5 全收」重新定性

- **起因**：[history/development-status-audit-2026-07-22.md](history/development-status-audit-2026-07-22.md)
  （GPT 外审，证据分级 A 实跑/B 源码确定/C 静态盘点/D 历史）裁定：M5 功能施工
  基本完成，但**正确性验收与稳定化未完成**——CI 三连 RED、默认 JIT 存在宽浮点
  静默错值、threshold=1 差分证明不了机器码执行、FFI 聚合/变参静默 ABI 错调、
  文档权威链失守。用户裁定冻结功能扩面，四波稳定化全做。本节是该战役的
  canonical 承接（审核 §13 迁移清单的落点）。
- **「M5 全收」重新定性（取代 §7.19/§7.20 的措辞强度）**：§7.19/§7.20 的
  「全收/全落地」在审核证据下不成立，应读作**功能施工完成**——翻译器三表
  穷尽、unwind 产品化、vmctx 终裁落笔均属实，但当时缺少「可准入函数编译
  失败即 RED + 证明编译码真被执行」的验证强度，四处静默错码与三处验证
  盲区因此在全绿账面下隐身。恢复「全收」措辞的验收条件 = 审核 §12 七条，
  本次终验按此执行（结果见本条末）。
- **波1 错码清零**（全部源码确定级真 bug，默认 JIT 开启下静默错值）：
  - F-02 f16 Div 按 % 算（`44ed11e`）；F-03 f128 powi 把 i32 指数当 f128
    位读（同）；F-04a FloatToWide128 kind/v 实参互换（同）；F-04b
    i128/u128→f32/f64 按 I64/RAX 读 XMM0 返回（同，改宿主 `as` 助手）。
    热探针实证四路径所在函数自身发布后 digest 与 native 逐字节一致。
  - F-06 聚合布局冻结全丢（`4164ea6`：`validate_agg_natural`，packed/
    align(N) 从静默错调改 freeze 响亮拒绝）；F-07 变参固定聚合切尾
    （同：tail_kinds 位置占位 + call_addr 等长不变量）。
  - F-08 GOT weak/strong 首现定强弱（`142fe67`：三处合并 = 任一 strong
    即 strong）；F-09 C-unwind 静默压 nounwind C（初判两冻结表面精确
    拒绝；**当日下午实锤反转**——c_mlua_lua 的 lua_Alloc 是真实
    C-unwind 回调，冻结拒绝把既有绿项打红；改为接受并保全属性
    （`ForeignSig.unwind`），残余边界记 R18（callback 内 panic 仍
    abort 于 nounwind trampoline——libffi 闭包无 unwind info 原理
    阻塞；longjmp 形可用）。教训：冻结拒绝的适用面必须先过 corpus
    全量再定型。
- **波2 验证方式**（`51cff03`，本战役枢纽）：`MIRVM_JIT_SYNC=1`——
  call_guest 投递后等待发布/失败哨兵，threshold=1 从「首调请求编译」
  升为「首调同步编译发布」；可准入编译失败 = FAIL 哨兵响亮 abort；
  fork 子进程重启编译服务。**模式上岗首日显形三件潜伏**：f16 数学
  MathUn/Bin/Fma 无护栏（panic 杀编译线程）、Bin128 Div/Rem 走 ISLE
  未实现的 I128 除法、emit_cleanup 把 try_call 发回 blocks[bi]（延续
  块前移时 verifier 拒收）——与审核四件同根同因：静默回退吞掉一切。
  阴性对照（种入 F-02 → SYNC 差分抓获）证明模式有效。gate5 逢调即编
  行自此 = 阈值=1+SYNC。
- **波3 仓库健康**：fmt 一次清（`d489402`，50 文件）；clippy 32 诊断
  清零（`0044694`，cmp_ps 谓词 4 条按语义 allow）；gate-truth fake
  runner 补 a2 嵌套（`efad1bf`，12/12）；gate6 默认 release + CI 接
  gate6（同）。
- **波4 文档真值**：open-issues 移出 11 个闭合条目（T1–T3/T5/C1–C3/
  C5/C7/E1/E21，本规则早立未守）+ T4 归并 R1；新登记 R17 第六形态/
  R18/E33/E34/G7；current-status 精修；本 §7.21 收束旧「当前摘要」；
  README/CLI/设计档头部对齐；docs/README 索引补齐 + D-08 关账清单。
- **旧摘要收束**（append-only 纪律，以下为本文更早期的「当前」断言，
  自此以本条为准）：§6 前后 vmctx 章头的「生产 JIT 尚未实现」、当前
  选择表的「唯一产品引擎是解释器」、§8 清单的「方法级 JIT 是设计不是
  现状」——M5.3–M5.5 已兑现（§7.19/§7.20 + 本条）；§8 同清单的
  constructor 拒绝面已由 §7.8 分治放行、RTLD_DEFAULT 重名已由 `fb0b204`
  归档句柄优先处置——维持其余条目原样（mode B/checked/alloca/自研 JIT
  仍属设计）。
- **终验（审核 §12 七条，2026-07-22 实跑，结果随本 commit 入账）**：
  ①CI 同构 fmt/clippy/单测/gate-truth/release/TSan/runtime gate 全绿；
  ②F-02～F-04 修复并有 compiled-entry 回归锁定；③strict 模式上岗
  （不准入记录 + 可准入失败 RED）；④F-06/F-07 修复 + packed 负例
  响亮拒绝；⑤weak/strong 与 C-unwind 定型有测试；⑥文档权威链交叉
  核对无冲突；⑦代表 workload（45 demo 双态 + SYNC + ffi/变参/宽浮点
  探针族）可复现逐字节一致。**「M5 全收」措辞自此恢复**，含义 = 功能
  施工完成 + 本验收矩阵持续绿。
- **排序裁定（用户 2026-07-22）**：稳定化先于一切扩面；其后回
  corpus/C4 功能轴，E6 性能轴待固定 workload 收益证据（审核 §11 P2
  同判——无 wall-time 主因证据不启 E6，与 §7.20 双闸触发器一致）。
- **终验入账（2026-07-22 当日晚，F-09 反转修正后复跑）**：CI 同构七步
  全绿（fmt/clippy/cargo test 76/gate-truth 12/release 零警告/TSan/
  gate6 4/4 含 gate5 167）；diff 45/45 ×3 口径（默认、阈值=1、
  阈值=1+SYNC）；diff_cargo 5/5；corpus 全量 129/0（mlua 复绿）；
  宽浮点/变参聚合/weak/C-unwind 四族探针逐字节一致。**审核 §12 七条
  全部落齐，「M5 全收」按本条前节的重新定性恢复**——功能施工完成 +
  本验收矩阵持续绿。战役 commit 链：`44ed11e`/`4164ea6`/`142fe67`/
  `51cff03`/`efad1bf`/`d489402`/`0044694`/`e87fdc0`/`ea2b780`。

### 7.22 2026-07-22：C4 两轮绕行被否与「不绕行」方法论确立——mode B 定向 + 砍 cargo 日程

- **经过**：C4（dep crate global_asm）初提 env 名单（`MIRVM_DEP_CODEGEN`）
  被否——「这是绕过问题，不是解决问题」；改提按需救援链（欠账 crate
  自动重放编译）再被否——「不要 99% 能做 1% 靠救援降级的思路，要数学
  和架构上干净的解决」。用户当场钦定两条工作纪律（`3e08df3`，已入
  AGENTS.md）：**① 永远直面问题，而不是绕过问题；② 讲话不要讲黑话，
  要讲人能听懂的话。**
- **分析框架（本战役的方法论产出）**：任何 Rust 项目的内容物只有三类——
  ① Rust 代码（主 crate + dep）→ 字节码 + 热点 JIT（现状即此，无争议）；
  ② 预编译外国库（glibc 类）→ .so/.a 链接（不可约，唯一配谈 .so 的类）；
  ③ **嵌在 Rust 源码里的手写汇编**（`global_asm!`/裸 `asm!`）——既非
  Rust（MIR 无其位、永远变不成字节码）也非外国库，是「以文本形态躲在
  源码里的机器指令」，归宿只能是 **mirvm 自行物化的机器码**；包装
  （.so 还是包内节）是次要的壳问题。按此判据过全家族：dep asm!/naked
  fn 已在 MIR 内解决；build.rs cc 静态库已在 native-archive 解决；
  cargo 外 C 工具链 = 真边界记档。**本题（③ 的 dep 形）可解，不是
  理论边界。**
- **关键架构事实（两轮绕行共同盲视的）**：mirvm 的 cargo shim 对每个
  dep crate 在进程内跑 rustc_driver——**mirvm 就是每个 dep crate 的
  编译器**；`after_analysis` 的 HIR 里就有 global_asm 模板文本
  （mono 收集对「本次编译的 crate」恒成立）。「rmeta/产物里没有」
  从来不是问题——**编译期就在手里**。正确动作 = dep 编译管线多产
  一类东西（抽取+物化），不是从产物里抠、更不需要救援与名单。
- **理论可解性判定（用户要求的证明形式）**：本题可解——数据源在编译期
  确定存在（HIR），物化通道在仓（asm-stub 工厂/cc+dlopen），装载与
  解析链在仓；无任何信息缺口。cargo 外形态（make/cmake 手搓库）才是
  真边界（mirvm 任何阶段均不可见，记档不绕）。
- **方向裁定（用户 2026-07-22）**：**mode B 是干净归宿**——打包期
  rustc 产字节码 + 机器码，运行期 mirvm 只读自有包 + FFI 真外国库；
  运行期工具链/ELF 依赖随 mode B 消除。C4 本体 = dep 编译期抽取
  （数据源修复，与包装无关）；机器码节纳入 mode B 包格式（与 D1 同
  设计）。**砍掉 cargo 纳入日程**（之后要做）：自有依赖解析 + 编译
  调度战役（登记 open-issues D15）——动机不是本题（本题不需要它），
  是运行期去工具链化与调度自主权，以及制度性消除「吃 cargo 产物
  就得绕」的处境。
- **反面对照（勿重提）**：env 名单（人工报名 = 绕行）；救援链
  （失败驱动重放 = 降级）；读 OUT_DIR 生成文件（输入追踪外）；
  whole-archive 抽 rlib（Rust CGU 闭包必炸）；auto-detect（元数据里
  extern 声明与 global_asm 定义不可区分，实证不可能）。

### 7.23 2026-07-22/23：C4 闭合施工实录（dep global_asm 编译期抽取 + 两个伴生实锤）

- **施工（`176ae20`，decision-history §7.22 定稿方向的一次落地）**：
  dep 编译回调在 mono 收集时顺带把本 crate 的 global_asm/naked 从
  HIR 渲染成 `.s` 文本，落 **rlib 旁挂清单**（`lib*.mirasm.s`）；bin
  加载相按 crate 图序发现清单，经既有 assemble 通道（内容寻址 cc +
  T5 syscall 改写 + 未定义符号审计）物化挂进 required_native_libs
  装载链。**清单存文本而非 .so**——缓存自愈免费（purge 后 bin 侧重编，
  dep 无需重编）。渲染器 sym fn 臂参数化：bin 侧 = C7 P1 条目预算原路，
  dep 侧片① = foreign/naked 早退、其余响亮拒绝（C7 跨 crate 条目预算
  属片②，见 open-issues R16）。
- **验收**：`corpus/c_faer_lu.rs` 复原 `faer = "0.21"` 默认特性——
  pulp V3 检测命中，`static LD_ST[544]` 的 544 个 extern fn 取址经
  dep 清单 .so 全解析，**native / mirvm 默认 / mirvm SYNC(阈值=1 同步
  发布) 三维逐字节一致**；cargo test 76、diff 双态 45/45、corpus
  faer_lu/tree_sitter/mimalloc/mlua_lua/rayon 5/5、gate5 复绿。
- **伴生实锤①（C6 按需队列）**：faer V3 内核触发 llvm.x86 cmp.pd
  (128/256)、max/min.pd(128/256)、max/min.sd 六件缺失，按四触点法补齐，
  谓词表与 maxmin 语义（NaN/±0/双源选择）经原生 intrinsic 探针逐位验证。
- **伴生实锤②（JIT 帧对齐潜伏错值，SYNC 显形后同役修复）**：cranelift
  x86_64 栈基只保证 16 对齐（源码实证：compute_frame_layout 一律
  16-align、prologue 无动态重排），frame_align > 16 的 JIT 帧按调用链
  奇偶错位——nano-gemm 的 `__m256d` 局部撞 `mem::zeroed` 的 32 字节
  precondition（write_bytes 对齐检查）。此前此类帧「编译成功且按运气
  对齐」，是**默认值域级潜伏错值**。修 = 槽内补 (align−16) 字节余量 +
  入口代码级 `(addr+align−1)&−align` 抬基（frame_addr 第三态），不依赖
  cranelift 任何保证。教训同 F-05：验证强度（SYNC 同步发布）把「按运气
  正确」变成「必然显形」。
- **NaN 符号处置（用户 2026-07-22 裁定）**：faer 奇异矩阵 inverse 的
  8 个 NaN 符号位属**实现定义域**——IEEE 允许自选；同一 native 下
  O0==mirvm==7ff8、O3==fff8（LLVM 优化级改符号）；rustc const-eval 一律
  +nan。不追 LLVM 相位签：driver dump 对 f64 NaN 掩符号位（payload 与
  inf/-inf 区分保留，有穷值全位照旧）——这也方便开发者暴露自己的问题。
- **遗留边界（如实）**：dep global_asm 的 `sym` 指向 dep 自身 guest fn
  的操作数片①响亮拒绝（C7 跨 crate 条目预算，见 open-issues R16，遇
  真实 workload 再立）；C4 片② = mode B 机器码节（与 D1 同设计）。
- **gate5 首轮抓获两枚边界并同役修正（`444ca31`）**：
  ① sym 指向 guest fn 的 dep 清单**拒收改跳过**（三态 DepAsmText）——
     wasmtime `fiber_start` 实锤：fiber 面不被 c_wasmtime_wat 触达，「因
     可能不用而拖垮整个 dep 构建」是把惰性失败错误提前；跳过清单 = C4 前
     状态（符号维持未解析，被使用时按既有 TRAP 响亮），真渲染失败仍 panic；
  ② assemble 通道**剥 `//` 行注释**——GAS/LIVE 语义差实锤：rustc 的目标
     汇编器 LLVM MC 把 `//` 当行注释，GNU as 把 `//` 当除法运算符
     （wasmtime fiber 大量 `//` 注释令 cc 报 invalid use of register）；
     global_asm 文本是 LLVM 语义域，馈 GAS 前剥除（引号态跟踪，串内不剥）。
  修正后 gate5 **167/0/0/0** 复绿。

### 7.24 2026-07-23：mode B 片②落地——`.mirvm` 包格式 v0 + pack/run

- **设计**：[modeb-mirvmar-design](designs/modeb-mirvmar-design.md)（用户
  补裁定：**格式当前不定死**——随片③/D15/C12 可变动，fmt_ver 仅同代
  区分，对外冻结归 D4 独立评审）。
- **实现（`254692c`）**：
  - 容器 = magic + fmt_ver + build_id + 节表（META/STAMPS/MODULE/
    NATIVELIBS/RELOC；BASE/MC 预留）+ fnv1a-128 节哈希与全文哈希；
    校验全链 refuse-loud（绝不静默重建——包是分发物不是缓存），
    `MIRVM_PACK_NOSTAMP=1` 旁路输入戳（无源环境分发用）。
  - `mirvm pack`：cargo 两形态经 MIRVM_PACK 环境传入 runner 的
    pack_driver（空 image 栈 + `MIRVM_NO_BASE_IMAGE`/`MIRVM_NO_DEPS_IMAGE`
    强制全量冷路径——单模块自包含，不付 BASE 节形态），纯单文件直驱；
    `callbacks.pack_out` 在 after_analysis 末尾落包（与 L2 store 同一
    洁净快照时机）。
  - `mirvm run x.mirvm`：magic 嗅探先于文本读取 → load_package →
    asm_sites 幂等重物化 → run_vm_engine——**零新执行路径**（全复用
    warm 后半段机件）。
- **施工实证（含两处自伤修正）**：
  ① 节表项宽度按 20 字节写、实为 36 字节——首包全哈希不符，按实宽修正；
  ② 盖戳相对路径（cargo 会话给本地 crate `src/main.rs`）在异 cwd 装载
     失配——收集器改绝对化（L2/包共用 `collect_input_stamps`）;
  ③ **git checkout 误伤**：探针命令误带 `git checkout .` 清掉全部未提交
     改动（pack.rs 未跟踪幸免）——教训：探针命令与仓库操作严格分行，
     大改动先 commit 再跑探针。
- **验收**：fib/eco(serde)/jsonschema(目录形态)/faer(dep global_asm)/
  wasmtime(84MB) 五负载 pack+run 与直接 run **逐字节一致**；拒绝探针
  （字节篡改→全文哈希、源变→盖戳并指名失配文件、库缺→NATIVELIBS）
  均响亮；cargo test 76、diff 双态 45/45、diff_cargo 5/5、gate5 复绿。
- **包的真实含义（用户原话校准）**：打包期 rustc 产字节码，运行期
  mirvm 只读自有包 + FFI 真外国库——本片兑现了「除 glibc 类外运行期
  不读缓存/rustc/cargo 痕迹」；自产机器码的 cc/ELF 依赖消除属片③
  （MC 节 + 进程内装载）。

### 7.25 2026-07-23：mode B 片③落地——MC 机器码节 + 进程内装载（自产码去 cc/ELF）

- **实现（`1697c82`）**：
  - `vm/engine/mcload.rs`：进程内 ELF64 装载器——自解析 PT_LOAD 映射
    （BSS 清零、按 flags 分段 mprotect）、自重定位（RELATIVE/64/PC32/
    GLOB_DAT/JUMP_SLOT；内部符号自 .symtab、外部符号 RTLD_DEFAULT、
    弱缺席 0）、eh_frame 逐 FDE `__register_frame`、符号表注册进解析链
    ①′位（`ffi.rs` FfiState::resolve，先于归档句柄）。**对系统链接器零
    依赖**（无 ld.so/ld.so.cache 概念，kernel mmap/mprotect + 自解析）。
    边界一律响亮拒绝：非 ET_DYN x86_64、PT_INTERP、TLS/COPY 重定位、
    非弱未定义外部符号、IFUNC——不属自产 global_asm 族。
  - pack 期：global_asm 族字节入 MC 节（NATIVELIBS 退化索引 + fnv 互证；
    `MIRVM_PACK_NO_MC=1` 退化纯文件引用）。load 期：MC 逐条自装载 +
    注册，并从 required_native_libs 剔除（不再 dlopen）；无 MC 的包
    （片②/NO_MC）维持文件引用全兼容。
  - 实锤一误：符号表先存 base+value、resolve 又加 bias = 双重加基址
    → SIGSEGV（faer LD_ST 首调即崩）；改存 vaddr 统一 bias+value
    （与 archive_fallbacks 同形）后三维一致。
- **真自包含酸试（本片的核心验收）**：`rm -rf ~/.mirvm/global-asm` 后
  faer 包**仍逐字节跑通**——pulp LD_ST 544 例程全走包内字节；自此
  运行期对自产机器码 **零 cc/ELF/.so/缓存依赖**，唯一剩 dlopen 的 =
  FFI 真外国库（glibc 类与静态归档）——§7.22 用户划的线完全兑现。
- **验收**：faer/wasmtime/eco MC pack/run 逐字节一致；cargo test 76、
  diff 双态 45/45、diff_cargo 5/5、gate5 复绿。
- **如实边界**：装载器只覆盖自产 global_asm/dep_asm 族（重定位面实测
  几乎全零、零星 JUMP_SLOT/GLOB_DAT）；静态归档（vendored C）与系统库
  维持 dlopen（「FFI 真外国库」类）；格式演进候选 = MC 预消化 blob
  （去 ELF 解析形态，随 D4 冻结评审）。

### 7.26 2026-07-23：测试管线整顿——manifest 唯一真源 + 分层套件 + 真项目对拍 + 三维计量

- **动机（用户四条）**：① tests/ 22 脚本平铺、里程碑代号命名（m4_/m5_/m51_）
  不成套件；② corpus 名单两处内联且已漂移——实锤：gate5 内联 147 /
  corpus.sh 内联 129，**13 个批6 条目两边都没接线**（创建时三维验收过却
  从不进任何 gate），zstd_stream 只在一侧；③ gate5 全量太慢、cache 无
  计量无预算（~/.mirvm/target 实测 17G）；④ corpus 全是单文件脚本，
  缺真 cargo 项目形态。
- **裁定（四连）**：代号名照方案退役改名；真项目 **vendor 进仓**；
  real_projects 重型 harness（71K+118K+32K，仓内零 case、不进 CI）挪
  `tests/parked/`；smoke 层 ~24 个跨类目代表（每类目≥1 + 历史功勋条目
  优先——修出过产品 bug 的先入）。
- **落地（结构批）**：
  - `tests/corpus.manifest` **唯一真源**：name/tier(smoke|full|manual)/
    timeout/mode(exit|oracle:<名>|diff)/env/needs/xfail 六列，
    `lib.sh::manifest_rows` 解析（非法字段/非法枚举响亮报错）；
    164 脚本驱动全接线（137 full + 24 smoke + 3 manual）；
    内联六 oracle 抽 `fixtures/oracles/`。
  - 改名归位：m4_gate0/1/2/4 → `runtime_gates.sh`（pure/digest/unwind/
    threads 四段可单段）；m51_* → `probes.sh`（输出行形状不变，gate_truth
    锁语义不锁文件名）；m4_gate5+m5_gate6 → `gate.sh`（并入 JIT stats
    冒烟；gate6 两条文档 grep 时点检查化石退役——落档纪律由评审承担，
    不由门 grep）；`spike4_tsan.sh`/`a2_deps_image.sh`/`diff.sh`/
    `diff_cargo.sh`/`gate_truth_regression.sh` 叶位不动；
    `project_suite_rustc_proxy.sh` → `fixtures/rustc_proxy.sh`
    （仍是 diff_cargo 活动依赖）。
  - `tests/run.sh` 统一入口：**fast**（逢提交：fmt/clippy/test + diff 双态
    + diff_cargo + gate_truth）/ **smoke**（fast + corpus smoke 层 +
    probes + runtime_gates）/ **gate**（收尾级）/ corpus / perf。
  - `tests/lib.sh`：记账/计时/manifest 解析/`corpus_run` 执行器
    （env `;` 分隔 + `%20` 空格解码、needs SKIP、逐驱动清 deps/ir、
    计时榜）+ **磁盘护栏**（disk_guard 见底两级升级清理仍不足响亮
    exit 3；target 预算闸 MIRVM_TARGET_BUDGET_GB 默认 24G 超即
    purge --target 并报告；cache_snapshot 跑前跑后 du）。
  - `tests/perf.sh`：三硬门（load<1s / rayon<5s / fib32≤80ms 三跑取最小）
    + 资源计量；SKIP_PERF 只跳时序门（gate_truth 锁 SKIP 不冒充 PASS）。
  - CI 改跑 `gate.sh`（+ MIRVM_TARGET_BUDGET_GB=12）。
- **片④ 真 cargo 项目对拍**：hexyl 0.17.0 + tokei 14.0.0 vendor 进
  `corpus/projects/`（.crate 全量解包 + sha256 钉 + 许可证随包；
  provenance 与升级纪律见其 README）。manifest `mode=diff` 判绿 =
  **mirvm warm 三维 == native 三维 + warm stdout == cold stdout**。
  两个机制实锤：
  ① **cargo 会回放缓存告警**（warm 构建 stderr 仍含依赖告警，对拍被
  噪音炸）→ 两侧同帽 `--cap-lints allow`（stderr 只承载程序自身输出；
  manifest env 列 `%20` 编码空格）；
  ② **mirvm 项目模式 guest cwd=项目目录**（cargo run 从不 chdir，
  argv 探针实锤）→ 夹具路径 `{ROOT}` 占位绝对化（lib.sh::parse_args）
  绕行于 harness 层；产品侧是否对齐 cargo 语义（cwd=调用者 cwd）
  待裁定 → [open-issues.md E36](open-issues.md)。
- **附带实修/自抓**：mode B 三片遗留 fmt/clippy 破窗（`22d89de`，
  run.sh fast 首跑即抓）；新脚本自抓虫两例均被排练抓出——probes.sh
  shift 后误用 `$1`（无 vm-call 探针 unbound）、gate.sh 汇总 `xfail`
  计数器被 manifest 字段撞名（read 覆盖）。
- **验收**：gate_truth 12/12、`run.sh fast` 7/7、gate.sh 排练
  （hexyl+tokei 三维对拍）17/0/1/0、**gate.sh 全量 179/0/0/0**——唯一红
  c_jiff_time 实锤上游 jiff-core 0.1.0 debug_assert 破洞（native 同文
  panic 非分叉，孤儿条目接线即立功），钉 `=0.2.32` 复绿
  （84fa00b 结构批 + jiff 钉版 + 本档）。

### 7.27 2026-07-23：D15（砍 cargo）立项与四决策点裁定

- **调研（三探针 + 主文件通读）**：cargo 今日职责全清单定案
  （manifest/版本/registry/feature/build.rs/proc-macro/rustc 参数/指纹/
  runner 协议九项，证据见 [designs/d15-cargoless-design.md](designs/d15-cargoless-design.md)
  §2 表）；mirvm 已有其半（`run_dep_compiler`/`MirvmCallbacks`/ircache/
  D14 store）；**build.rs 普遍性实锤**（共享 target 内 151 个 crate 有
  build.rs 输出——连 anyhow 都有），故 build.rs 全生命周期是核心硬骨头、
  不可后置；proc-macro 机制直白（host/target 二分）；profile 语义叉钉死
  （debug-assertions/overflow-checks 进 MIR 语义，jiff 判例）。
- **四决策点裁定**：① HTTP/解包 = **纯 Rust crate**（ureq+flate2+tar，
  自包含优先）；② registry store = **自有 ~/.mirvm/registry + 读穿
  ~/.cargo/registry**（只读不污染）；③ lock 缺席求解 = **pubgrub crate**
  （0.4，原理闭合）；④ 分期轴 = **P1→P5**（地基→机制全→迁移→退场→
  按实需）。
- **闭合契约**（每期可观察判据，设计 §5）：P1 解析库 + 审计工具
  （lock 在场自解 == lock 逐条对账；lock 缺席自解落 lock 后 cargo
  --locked --offline 反证接受）；P2 corpus smoke 24 零 cargo 跑通
  （self vs cargo 双路径逐字节 + 原三维判绿）；P3 DEPS=self 全量
  gate 179/0/0/0 同构绿；P4 sysroot 自管 + 默认翻转 + cargo 显式
  compat 双轨（非救援）；P5 复杂语义按实需（事先明说的不闭合面）。
- **附带红利**：假二进制与 runner 协议随新路径退役，E36（项目模式
  guest cwd=项目目录 vs cargo run 语义分叉）在新路径顺带闭合
  （guest cwd=调用者 cwd）。

### 7.28 2026-07-27：D15 P1 收口——cargo 解析语义全实证与 cargoless 解析库落地

- **落地（`aaba3ff`/`8944229`/`f827ec6`/`d2590b5`/`29f0f39`/`61596b3`/
  `4b8b450`/`9ef2019` + 修边）**：`src/cargoless/` 五件——manifest
  （Cargo.toml 模型 + cfg 平台求值 + frontmatter 伪包）、lockfile（v1–v4
  读写 + canonical v4 序列化）、registry（自有 index/cache/src 三层 store
  + 读穿 cargo 缓存只读 + sparse index + .crate sha256 自实现校验 + tar.gz
  解包防护）、resolve（双模式求解 + feature 统一 + 单元装配）、audit
  （`mirvm deps audit`：项目等值对账 + 脚本 cargo `--locked --offline`
  验收链 + corpus.manifest needs/env 联动）。
- **cargo 解析语义实证清单**（全部对拍实锤，设计档 §8 同步落笔）：
  - **resolve 图 ∪ build 图分裂**：Cargo.lock 是全平台并集（cfg(any())
    照进、windows-sys 在 Linux 入锁），build 图按 host `rustc --print cfg`
    过滤；`unify_features(include_weak)` 双态承载。
  - **lazy-bucket 多版本 fork**：pubgrub 单版本模型装不下 cargo 的
    同名多版本并存（hashbrown 0.14/0.15、ark 全家 0.3/0.4/0.5/0.6 同图）；
    包 id 加 bucket 维度（边到达能并入既有 bucket 则并、否则开新），
    pubgrub 按 bucket 独立回退；可达集过滤清回退孤儿 bucket。
  - **optional 门按（父包, 父版本, 依赖键）**：全局包名/版本盲门两连炸
    （zerovec 的 yoke → litemap 的 yoke ^0.8；ark-ff 0.6 的 derive →
    ark-serialize-derive 全系四版）。
  - **?/ 弱引用级联**（resolve 图语义）：被启用 feature 的 ?/ 弱形引用
    把被引用包收进解析图与 lock 依赖行，特征照常下发、可与强激活级联
    （rust_decimal std → borsh?/std → bytes?/std；yoke alloc → serde?/alloc；
    tracing-core default → valuable?/std）；build 图仅强激活。
  - **pre 精确规则**：pre 版仅当 major/minor/patch 全同且带 pre 的
    comparator 点名（ark-ff-asm 0.5.0-alpha.0 误选修复）。
  - **exact 钉兼容 build**：`=M.m.p` = [M.m.p, M.m.(p+1))——semver crate
    的 Ord 比 build 元数据（Eq 不比），singleton 会误杀带 build 的候选
    （libgit2-sys 0.18.5+1.9.4 实锤）。
  - **req_to_ranges Less 臂**：`<3` 一度错成 `<4.0.0`（brotli
    alloc-no-stdlib 2.0.4 被 3.0.0 顶包）。
  - **lock canonical 硬判据**：依赖行每行尾逗号（cargo `--locked` 对非
    canonical 一律判"需重写"拒收）。
  - **同名多 req 条目**：按 (父, 依赖键, req串) 分立（ruint 四个
    ark-ff 系列各带 hint）；lock 行 hint 消歧。
  - **rename/下划线键**：lock 依赖行写真包名，index 键可能是下划线键
    （rustix libc_errno→libc-errno、grep-searcher memmap→memmap2）——
    边查找双路匹配。
- **上游破洞实锤六枚**（均验证 cargo 自家 fresh 解析同撞，非 mirvm
  分叉；钉版对齐 driver 验收时代，头注记恢复条件）：jiff-core 0.1.0
  debug_assert（批11 已钉）、datafusion 54.1.0 internal API（钉 54.0.0
  全家 28 枚）、pest_generator/meta 2.8.8（train 四钉 2.8.7）、uuid
  getrandom feature 移除（钉 1.6.1）、libc POSIX_SPAWN_SETSID 变窄
  （钉 0.2.186）、（wincode git 源 = P5 合法响亮拒绝，非破洞）。
- **验收**：29 单测绿 + corpus 全量 audit 166 目标（两项目等值对账、
  其余 cargo 验收链）。
- **如实边界（记账）**：rust-version-aware 版本偏好未实现（cargo 1.84+
  fallback 语义，与 D14 store 合并评审时补）；git 源/alt registry/
  workspace 多包图/source replacement 归 P5 响亮拒绝。

### 7.29 2026-07-27：D15 P2 收口——编译调度全生命周期零 cargo 化，smoke 24 双轨逐字节闭合

- **落地（`f74233c`/`9d2eda6`/`d0b9d1a` + 切④修复）**：`src/cargoless/`
  新增 schedule（拓扑 + 内容指纹 + 每 crate rustc 参数）、driver
  （`MIRVM_DEPS=self` 的 `mirvm run` 新路径，替代 cargo_shim 三阶段；
  假二进制与 runner 协议在新路径整体退役，E36 以构造闭合——guest
  cwd = 调用者 cwd）、buildrs（build.rs 全生命周期）；proc-macro
  host 编译并入 schedule/driver。验收轴：`tests/corpus_deps_pair.sh`
  （每条目 cargo 腿 vs self 腿 stdout/stderr/exit 逐字节）+
  `tests/diff_cless.sh` 六夹具（self 腿 PATH 只含 mirvm +
  `MIRVM_OFFLINE=1`，实证零 cargo 进程）。
- **cargo 编译调度语义实证清单**（全部探针实锤）：
  - **host/target 二分**：proc-macro 闭包 ∪ build-deps 闭包真 rustc
    真 codegen；target 侧照旧 `-Zno-codegen` metadata-only rlib；同
    unit 双用（bin 与 proc-macro 共引）双侧各编，产物分目录
    （deps / host-deps）。
  - **proc-macro 五钉**（serde_derive 实锤）：`--crate-type proc-macro`、
    `-C prefer-dynamic`、`--emit=dep-info,link`、末尾裸
    `--extern proc_macro`、消费方 `--extern` 指 `.so`；host rlib
    （proc-macro2 实锤）无 prefer-dynamic 无 debuginfo、dep 边指
    `.rmeta`。
  - **build.rs 生命周期**：编译形态（`--crate-name build_script_build`、
    `--crate-type bin`、`--emit=dep-info,link`）+ 执行 env 全集
    （CARGO_CFG_* 按 `rustc --print cfg` 原子通用映射 +
    **CARGO_FEATURE_\<NAME\>=1 逐启用 feature** + CARGO_PKG_* +
    OUT_DIR/HOST/TARGET/PROFILE 等）+ cwd=包根 + stderr 仅失败回吐。
  - **指令传播规则**（probe_link 实证，cargo 1.98）：`-l` 只进本包；
    `-L` 进本包 + 传递依赖者；rustc-cfg/check-cfg/rustc-env/link-arg
    只进本包；metadata 只给**直接依赖者**的 build script
    （`DEP_<LINKS>_<K>`）；cargo **不**自动注入 `DEP_*_ROOT`（-sys
    自发 metadata=root 惯例）、**不**对 rustc-cfg 自动补 check-cfg；
    legacy `cargo:` 未知键按 metadata；warning 只 path 包显示。
  - **links 互斥**（cargo 同）与 `build = false` 语义（切①，cfg-if
    实锤：键在场 ≠ 有 build.rs）。
  - **指纹 v1 粗粒度**：内容寻址 fp 含**排序后各 dep fp**（传递传播
    = depsimage pre-key 不变量）；build.rs 每次重跑（rerun-if 精细化
    归 P3），build script 二进制按 fp 缓存。
- **对拍暴露的修复四枚**：
  - StrongDep 强形 `x/y` 激活可选依赖须视同指定其同名隐式 feature
    （k256 `ecdsa-core/signing` ⇒ `#[cfg(feature = "ecdsa-core")]` 实锤；
    cargo「视同指定 foo feature」语义）。
  - build.rs env 缺 `CARGO_FEATURE_<NAME>=1`（cranelift-codegen 按
    `CARGO_FEATURE_PULLEY` 决定生成 pulley_inst_gen.rs 实锤）。
  - 隐式 feature 裸名引用激活可选依赖时同名 cfg 旗必补（切③，serde
    facade `#[cfg(feature = "serde_derive")]` 实锤）+ facade 再导出
    proc-macro 时 rustc 按 hash 找 `.so` 需 `-L host-deps`（E0463）。
  - **cargo 腿既有 bug**：phase_wrapper 劫持 ad-hoc 探测编译（rustix
    1.1.4 build.rs 读 RUSTC_WRAPPER 后 spawn `$WRAPPER $RUSTC
    --emit=metadata -o <f> -` 探 nightly 特性）→ run_dep_compiler 缺
    `--out-dir` panic 与探针 stdin writeln 竞态成 EPIPE（负载高炸、
    空载假否，两态皆错）；修 = 无 `--out-dir` 或 stdin 源一律透传
    真 rustc（探针语义 = 这套工具链认不认 X，只能真 rustc 回答）。
- **P2 闭合验收**：`corpus_deps_pair --tier smoke` **24/24** 双腿
  stdout/stderr/exit 逐字节一致；diff_cless 6/6；cargo test 127；
  run.sh fast 8/8。P2 整期按设计档 §5 闭合。
- **边界记账（不冒充闭合）**：RUSTFLAGS / CARGO_ENCODED_RUSTFLAGS 未接
  （hexyl/tokei 类 full 层归 P3）；`.cargo/config.toml` rustflags 子集
  未接（P3）；编译调度串行 v1（并行归 P3）；build script 的
  check-cfg feature 值表填启用集（registry cap-lints 兜底；path 包
  build.rs 用未启用 feature 的 cfg 比 cargo 多一条 unexpected_cfgs
  lint）；v1 粗指纹固有限度（build.rs 输出随环境漂移而源未变时 rlib
  可能陈旧——与 cargo rerun-if-env 同类问题，P3 rerun-if 闭合）。

### 7.30 2026-07-28：D15 P3 收口——迁移全量 corpus 与双轨 gate

- **落地（`de33733`/`6f4094f`/`a43698f`/`239677f` + 收官修复）**：
  - **切⑤a rustflags 子集**（新 `rustflags.rs`）：
    CARGO_ENCODED_RUSTFLAGS / RUSTFLAGS / config 三键，优先级与发现
    规则按 cargo；落点实证（13 条 rustc 行逐类核对）——有 --target
    时 rustflags 只落 target 单元（host 侧签名级不吃）；进全 unit
    指纹。伴生修两枚 full 层基线缺口：`proc_macro` 下划线归一化
    拼写双收（tokei 的 derive_arbitrary 实锤）、根包 [lib]+[[bin]]
    双 target 的根 lib 编译接线（hexyl 实锤）。
  - **切⑤b build.rs rerun-if 精细增量**：cargo 同语义重跑判定
    （默认面 registry 源不可变永不重跑 / path 树快照、
    rerun-if-changed 按 (len,mtime_ns)、env-changed 按值、links
    直接依赖传递、存档缺席损坏自愈）；存档 = build/<pkg>-<fp>/
    {output.txt, rerun.txt}，跳过执行则原始 stdout 重解析回放
    （零序列化失真，warning 同门控回放）；MIRVM_DEBUG_BLDRS=1
    观测行。提速实锤：libgit2 第二腿 20s→0s。
  - **切⑤c 编译调度并行化**：`schedule::run_scheduler`——Kahn
    就绪队列 + std-only worker 池；完成表只归主线程、派发时算好
    依赖侧输入捎进 WorkMsg（零锁）；jobs=1 与 Kahn FIFO 逐位一致
    的对拍锚；FpLocks 互斥同 fp 的 Normal/Build 双 unit（并行才
    暴露的既有雷）。冷跑提速 wasmtime_wat 152s→79s（1.9×）。
  - **切⑤d full 层迁移**：137 条目双腿对拍 120/19 起，分诊修复
    四枚——**extern 命名无 rename 时按 dep 包 lib target 名**
    （tendril→new_debug_unreachable 等 9 条目）；**StrongDep 强形
    在有同名显式 feature 定义时被 dep: 遮蔽也置旗**（zerotrie
    litemap 探针实证三态：dep: 永不置、x/y 有显式定义置、无定义
    未遮蔽走隐式旗）；**build script env 补 CARGO_MANIFEST_LINKS**
    （ring 0.17.14 build.rs unwrap 实锤）；**bin 会话
    --remap-path-prefix + 脚本正文物化改 <cache>/src/main.rs**
    （file!()/panic Location 路径形态与 cargo 逐字节）。
- **P5 单列制度化**：对拍轴遇「归 P5」响亮拒绝且 cargo 腿通过时
  单列 p5 计数（mirvm deps audit 同款先例；miden_prove 的 wincode
  git 源归列）——设计档 §5 明说的不闭合面不冒充闭合。
- **双轨 gate**：`tests/diff_cargo.sh` 恒钉 MIRVM_DEPS=cargo
  （cargo compat 轨不缺席）；`MIRVM_DEPS=self bash tests/gate.sh`
  = corpus ① 全量走零 cargo 自有调度的 DEPS 轴验收。
- **P3 闭合验收**：`corpus_deps_pair --tier full` **138 pass,
  1 p5, 0 fail**；cargo test 153；run.sh fast 9/9；
  `MIRVM_DEPS=self SKIP_TSAN=1 bash tests/gate.sh` **177 pass,
  1 p5, 1 fail**——corpus ① 含 P5 单列（miden_prove 的 wincode git
  源归 p5 列）；hexyl/tokei native 基线修复 = gate.sh 显式钉 RUSTC
  （rustup 代理按每次调用 cwd 解析：registry 依赖编译 cwd 在仓外落
  rustup default stable，混合工具链 E0514 实锤）；唯一 fail =
  runtime_gates 纯度门禁（harness 编译 mirvm-tsan，被工作树内并行
  的 mirvm_log 重构卡住——与 D15 无关，重构收尾后回启复验）。
  P4（sysroot 自管 + 默认翻转）待施。

### 7.31 2026-07-29：D15 P4 收口——sysroot 自管与默认翻转，cargo 退场为 compat

- **切⑥a sysroot 自管（`5c2e9bf`）**：MIR sysroot 构建从
  rustc-build-sysroot 驱动 cargo 换成 cargoless 自有调度——
  `cargoless/vendor.rs`（VendorDir 通用 vendored-dir PkgSource：
  `vendor/<name>-<ver>` 扫描合成 + overrides 精确映射，未来 P5
  source replacement 复用）+ 伪根（std/test/proc_macro 三 path 边，
  std 带 panic-unwind/backtrace）+ `library/Cargo.lock` 增广 lock
  模式 + compile_plan 复用（driver 纯抽取，行为零变）+ tmp 目录原子
  发布 + cargo 轨 dep 缓存换代连坐 purge（防 E0463 混代）+ 内容键
  不含 BUILD_ID（防重编乒乓）+ 顶层哨兵盖戳（保 fib 硬门）。
  **意外 crates.io 依赖根除**：sysroot .d 引用 `~/.cargo` 数 = 0
  （全指 rust-src library/ + vendor/）。冷建 **27.2s**（旧 cargo 构建
  数分钟；metadata-only 无对象码——双轨 gate 全消费面实证无炸），
  25 crate 集与旧构建对齐（custom_local_sysroot/sysroot 两枚伪壳
  消失属预期），PATH strip 零 cargo 实证。
- **切⑥b `--bin` 多目标选择（`271425b`）**：cargo run --bin 语义
  （按名精确选，错名响亮列可选名单；多 bin 无 default-run 的拒绝
  去 P5 化——--bin 已可解）；compat 轨 --bin 直通 cargo run；
  脚本/单文件/包形态响亮拒绝（cargo script 同无此概念）。
- **切⑥c 默认翻转**：`MIRVM_DEPS` 缺省 = self（零 cargo 自有调度），
  `=cargo` 显式 compat。USAGE/gate 头注同步。
- **compat 评审（设计档 §5 P4 定案）**：compat 不是救援是双轨——
  两条路径各自完整（cargo 三阶段 vs cargoless driver），gate 双轨
  保留（默认 gate = self 轨 corpus ① + diff_cargo 恒 cargo 轨冒烟；
  `MIRVM_DEPS=cargo bash tests/gate.sh` = compat 全量）。**删除条件
  另行评审**：compat 的剩余独占价值 = ①cargo 行为差异的对照 oracle
  （P5 语义扩展期的对拍基准）②`mirvm pack`（mode B 打包仍走 runner
  协议，翻 self 属后续评审）③`mirvm deps audit` 的验收链（设计上就
  是 cargo 对拍工具）。任一条存续期间 compat 不删；删除评审在 P5
  边界按需扩张完成后重启。
- **P4 闭合验收**：sysroot 冷建 27.2s 零 cargo（PATH strip 实证）；
  翻转后 `mirvm run` 默认路径全程零 cargo（脚本/项目手测）；
  run.sh fast 与 corpus smoke（默认 self 轨）全绿；
  `--bin` 三行为实证（diff_cless 7/7 含 binsel 双腿）。
  **D15 战役至此主体收官**：P5 边界（git 源/alt registry/workspace
  多包图/source replacement）按实需逐项立项。

### 7.32 2026-07-29：冷启动/test/env-GC 方向裁定——懒降低维持否决、GC 取标记-清扫、日志收回自造

- **背景**：用户提出四组方向——①冷启动性能（dev 循环：改一点 →
  快速跑）：entry 先跑 + 按预测访问顺序并发 lower + guest 卡住时
  抢占 lower 线程保存断点改降 on-demand 单元 + cache 写盘等重 IO
  交专门 service 线程（兼任 log 落盘）；②cargo test / cargo login
  支持；③包获取与运行分离 + uv 式环境 + cache 引用计数清理；
  ④日志系统（[designs/mirvm_high_performance_log.md](designs/mirvm_high_performance_log.md)，
  v1 施工在外）。逐条对照在案否决/杠杆后裁定如下。
- **模式 A 懒降低维持否决（用户裁定，不复活 V3）**：「entry 先跑、
  其余后台按需 lower」即 2026-07-15 J2 否决的懒降低 V3；dev 循环
  不构成重启触发器——deps 字节码已由 S3′b image 缓存，编辑只重
  lower 根 crate，lower 不是 dev 循环瓶颈。「按需/并发」改钉进 D3：
  模式 B 包布局改 mmap 直读 + 逐函数惰性解码（无 tcx，合法），
  入口段先映射先跑、后台线程按预测序（上次运行真实触发顺序落
  cache）预取、demand 单插队队首——三件套在 D3 形态全部成立；
  预测错只慢不错，正确性不依赖预测，明写。**「抢占保存断点」判
  不可行且不需要**：Cranelift 无中断续跑接口；函数是编译的天然
  最小单元，demand 插队的等待上界 = 一个在跑函数编译完成，数学上
  等价于抢占延迟有界，无需断点机制。另记架构事实防再提抢占模型：
  guest 永不卡住等编译——解释器是零等待地板，JIT 是后台计数触发
  的升级层，未发布走解释。
- **dev 循环真瓶颈排序（裁定按此排兵）**：①**JIT 码每进程重烧 =
  最大单根杠杆** → D5/L3（禁令条件「M5.3–M5.5 定型前禁做」已随
  M5.5 收官消失；D1 片③ MC 机器码节 + 进程内 ELF 装载器已证 JIT
  产物可序列化再装载）；②字节码 postcard 整包解码 → D3 零拷贝；
  ③rustc 前端重跑（改一行也要重取 MIR）→ D12 -Cincremental
  （触发器「大用户 crate 编辑-重跑」在案）。
- **冷启动战役（D16）立项**：profile 先行——MIRVM_TIMING 相位账本
  现成 + dev 循环基准场景（corpus/projects 改一行重跑计时）+ 日志
  设计 §7 三组基准顺带实测（该文档数字全系量级估算非实测，其 §6
  自述）。后台服务线程 = cache write-behind + log 落盘**合一**
  （单线程多优先级队列，防每种杂活各起一线程）。线程池不拍固定
  配比：guest 优先、编译（JIT/解码/预取同池）吃剩余核；优先级
  三级 demand > 预测序 > 闲时回填；具体比例是 profile 后的调参
  产物，不是设计产物。
- **mirvm test（D17）立项**：cfg(test) 重编本包 + libtest（sysroot
  伪根本含 std/test/proc_macro，test crate 本地原料在）+ test 目标
  调度 + harness argv 透传；**doctest 明说不做**（rustdoc 是另一个
  前端，闭合成本单独评估）。cargo login = 只读复用
  ~/.cargo/credentials.toml，并入 P5 alt registry 子项备案，不单独
  立项；publish/login 不做进 mirvm（发布侧非运行侧）。
- **env/GC（D18）**：uv 式环境 = 全局 store（D14 已有）+ 环境
  （lock 物化的引用集，`~/.mirvm/envs/` 登记为根）。**GC 裁定
  登记根 + 标记-清扫（用户裁定，否决裸引用计数）**：落盘计数崩溃
  半截即永久不一致，是「大部分时候没问题」的脏设计；sweep = env
  创建登记为根、purge 环境 = 摘根、从根集合可达性扫描回收不可达
  ——崩溃安全、无计数一致性、语义一句话说清。缺包行为：默认自动
  拉取（日志明示在拉什么），--offline/--locked 下响亮报错 + 提示
  fetch 指令。
- **D4 排序裁定**：对外格式冻结评审排在 D3 零拷贝布局评审之后，
  否则冻结后必为布局改版。
- **日志系统归属（用户裁定）**：收回自造，不再视同他人领地；v1
  有问题即推翻重写。v1 在施已知两处红：stdout 臂误用不存在的
  `std::io::println`；tsan crate 根缺 `mirvm_log` 宏（纯度门禁
  SKIP_TSAN=1 绕行中，修复后摘钉复验并回改 §7.30/§7.31 记档）。
  v2（ring + 消费者线程 + feature 闸门）归 D16 后台服务线程同
  设计。

### 7.33 2026-08-07：现状复核纠偏——JIT 展开整段注册、包格式 v2 真自包含、产品能力排单

- **JIT 旧选择被推翻**：产品编译器、spike5 和活跃设计一直把同一
  `FrameTable` 写出的 `.eh_frame` 拆成 FDE，逐条调用 `__register_frame`。
  单 JIT 帧测试能通过，掩盖了多个 FDE 共享 CIE 的事实。稳定复现是
  `cfi_only_passthrough` 的两层 JIT 调用：宿主报
  `failed to initiate panic, error 5`；产品侧 `jit_builtin_probe` 与
  `jit_unwind_probe` 同样 abort。把完整、零结尾的 `.eh_frame` 一次注册后，
  `_Unwind_Find_FDE` 能命中两函数，五个 unwind probe 和两个产品差分用例全绿。
  新规则集中在 `jit::register_eh_frame_section`，compiler/probe/spike 共用；历史
  spike 文档保留当时记录，active 的 m5-design/onboarding 已更正。
- **TSan 红的性质与修复**：独立 tsan crate 经 `#[path]` 同源复用 `src/os`，但日志
  改造后没有把 `mirvm_log!` 定义带入 crate 根，导致编译失败而非数据竞争。只补
  `src/utils/logs.rs` 的同源模块依赖；原 `tests/spike4_tsan.sh` 随后四个 spike case
  与 engine 多线程真身全部通过，零 TSan warning。
- **`threads_sync` 处置**：审查首轮曾在 L2 warm 复跑观察到一次 exit 139；JIT 修复后，
  默认差分通过，并在 `MIRVM_JIT_SYNC=1`、阈值 1、热缓存、SEGV 诊断开启下连续
  100 次通过。没有独立复现和故障地址，因此不做猜测性产品改动；最终全量差分继续
  作为判据。
- **release unwind 伴生红与构建约束**：打包器 v2 增量本身不参与 `catch` 执行，却
  改变了主 crate 的代码布局，使 pinned nightly-2026-07-02 / LLVM 22 在无完整 DWARF
  的 release 构建里生成断裂的解释器递归清理链：即使 `MIRVM_JIT=off`，libgcc 也会在
  `_Unwind_Resume` 第二阶段 abort。HEAD 对照通过、仅换入 pack/ircache 即稳定复现；
  禁内联捕获点/`interp_frame`/`run_blocks`、拆参数 `Vec` 所有权、强制 unwind table、
  codegen-units=1 和 opt-level 1/2 均无效，均已撤回。完整 DWARF 参与代码生成可消除
  误编译，链接后剥离调试段仍通过且成品约 14.4 MiB，因此 release profile 固定
  `debug=2` + `strip="debuginfo"`。移除触发器 = 升级工具链后先用 debug=0 A/B 复跑
  `catch`、默认 45/45 和 SYNC+阈值1 45/45；未过之前不得为了缩短构建时间摘掉。
- **`.mirvm` 旧自包含断言被推翻**：v1 只将 role=global_asm 的 `.so` 放入 MC；
  role=static_archive 仍只保存绝对路径+哈希，运行时读取原缓存。默认 STAMPS/env
  门控还要求分发机器保留源码和编译环境，`MIRVM_PACK_NOSTAMP` 把负担转给用户，
  与“单文件分发”目标矛盾。
- **新选择 = 格式 v2**：NATIVELIBS 每项携带原始 bytes；MC 命中者进程内装载，
  其余从包内按哈希自动物化到当前 MIRVM_HOME 后 dlopen。旧 path 只作 MODULE 顺序
  互证和诊断。STAMPS/envs 保留来源信息但不参与运行许可。关闭 MC 打包、移走原
  global-asm 缓存并换全新 MIRVM_HOME 后，真实包输出仍为 `chain=29 slot=41`，
  且新目录自动生成两个内容寻址 `.so`。v1 包由 fmt_ver 精确拒绝；格式本来未冻结，
  不提供迁移兼容，D4 仍必须排在 D3 零拷贝布局之后。
- **坏包边界**：容器解析改为 checked cursor；节数量先受剩余节表约束再尝试分配，
  offset+len 检查溢出，禁止节越界/重叠/重复 tag，并校验所有节（含未知节）哈希。
  七个单测覆盖正常写读、截断 build_id、巨大 count、溢出 range、重叠、重复 tag、
  节 hash 和不读旧路径的 native blob 物化。
- **产品能力记账**：仍明确缺失 `mirvm test`、P5 复杂依赖来源、正式沙箱与资源治理、
  稳定多 Engine/daemon API、稳定可移植包格式和跨平台支持。建议顺序与每阶段验收已写入
  [designs/product-capabilities-plan.md](designs/product-capabilities-plan.md)；这只是规划，
  不把未施工项写成现状。

### 7.34 2026-08-08：`mirvm test` 单包主链与 Cargo 合同

- **权威选择**：锁定 toolchain 的 Cargo 实际行为是 cargoless 的外部权威；不冻结
  自造 Cargo 快照，也不跟随未审核的 Cargo 主线。升级 toolchain 时先审 `-vv`
  rustc 行，再同步实现和合同。
- **实现选择**：测试解析复用 D15 manifest/resolve/schedule，根 Dev 依赖只加入测试
  单元；每个 artifact 用独立 mirvm 子进程执行。integration test 的
  `CARGO_BIN_EXE_*` 是配方启动器，执行的仍是 VM bin，不生成本机目标程序。
- **compat 修正**：Cargo wrapper 原先写不可执行 JSON 占位，Cargo runner 能读，
  integration test 直接启动则 EACCES。现改为可执行脚本加旁置 JSON；Cargo runner 与
  直接执行共用同一记录。
- **验收选择**：固定夹具做 Cargo native / compat / self 三腿结果比较，再以 Cargo
  `-vv` 锁目标形状；self 腿用 PATH 哨兵和 execve 审计抓相对/绝对 cargo 调用，热复跑
  以 build script 计数判定。合同当前 20/20。workspace/package 不做命令层特判，等待 D15 P5 正确多包图；
  doctest 继续归 rustdoc 专项。

### 7.35 2026-08-08：`mirvm test` resolver 2 工作区合同

- **闭合范围**：固定 Cargo 1.98 nightly 是唯一行为权威。支持 virtual/root-package
  workspace、`members`/`exclude`/`default-members` 与 `*` glob、工作区内 path
  依赖自动入成员、workspace.package/dependencies/root profile 继承、根统一 lock、
  默认/当前成员、`--workspace`/`--all`、`-p`/`--package`、`--exclude` 和 feature
  选择。实现入口是 `workspace.rs`，先把成员清单物化成完整包模型，再交给既有
  manifest/resolve/schedule；不是为命令行另造包名单。
- **多根特性选择**：同一次命令先分别求出各根图，再按（包、版本、normal/build
  类别）把 feature 结果反灌到所有根，单调迭代到不再变化后才编译。这样 app 直接
  依赖 shared、tool 经 bridge 间接依赖 shared 时仍只得到 Cargo 的统一 feature 集；
  build 类不与 normal 类误合并。所有选中包先完成准备，再开始执行，保持 Cargo 的
  编译失败与 fail-fast 边界。
- **锁文件选择**：所有成员只读写 workspace 根 `Cargo.lock`。无锁时按 Cargo 的最大
  解析图规则，以全成员全部 feature 可达依赖做一次 PubGrub 求解并生成 canonical v4；
  实际编译图仍只启用用户选择。结果再由 pinned Cargo `--locked --offline` 反向验收；
  `--locked` 缺锁直接失败。路径先 canonicalize，
  防止 `tool/../bridge` 把同一个成员登记两次。
- **兼容层修正**：Cargo wrapper 在 workspace cwd 下记录的相对 `.rs` 输入会被 runner
  从成员目录重复拼接。runner 现在把真实输入绝对化，同时注入 remap 保留 Cargo 的
  workspace 相对诊断与 `file!()` 结果。
- **严格边界**：edition 2024 隐含 resolver 3，不得误当 resolver 2；resolver 1/3、
  `**`/`[]`/`?` 成员 glob、复杂 package ID spec/同名成员、嵌套 workspace、
  workspace lints、`[patch]`/`[replace]` 均响亮拒绝。缺失成员/
  default-member 报错，从 excluded 包启动则按独立包处理，不能误跑默认成员。
- **验收**：新增 `tests/cargoless_workspace_contract.sh`，Cargo native / compat / self
  三腿覆盖 12 组选择与 feature 场景、两种失败策略、零 Cargo execve、fresh lock、
  build.rs 编译键和 `-vv` 结构，共 **27/27**。既有单包合同 **20/20**、
  `diff_cless` **7/7**，Rust 测试 **176/176**；隔离 Cargo home 与全新 target 的 release
  `tests/run.sh fast` **11/11**。

### 7.36 2026-08-10：D15 P5 第一批完成——resolver 3、rust-version 与 package lints

- **权威和探针**：继续以 pinned Cargo 1.98 nightly 为裁判。最小项目实证：edition
  2024 隐含 resolver 3；显式 resolver 3 可用于旧 edition；未声明 rust-version 时
  以当前 rustc 为比较基准；混合 workspace 以全部成员最低 Rust 版本为基准。
- **候选选择**：读取 package/workspace 继承和 registry index 的 `rust_version` /
  `rust_version2`。resolver 3 默认 `fallback`，先取满足 semver、非 yanked 且与工作区
  Rust 版本兼容的最高版；一个兼容版都没有时仍取通常的最高版，让后续诊断指出真实
  Rust 版本要求。resolver 2 默认 `allow`；Cargo config 和
  `CARGO_RESOLVER_INCOMPATIBLE_RUST_VERSIONS` 按 Cargo 层级覆盖。
- **命令边界**：`--ignore-rust-version` 同时关闭候选回退偏好与编译前 Rust 版本拒绝，
  compat 轨透传 Cargo 同名参数。正常路径只检查实际要编译的根包/单元，不因仅在 lock
  中但当前不可达的包误报。
- **lock 版本**：Cargo 对最低 Rust 版本不高于 1.82 的项目写 lock v3，1.83 起写 v4；
  serializer 改为忠实写模型版本，不再把 v3 错写成 v4。真实 `home = "0.5"` workspace
  探针中 Cargo 与 self 同选 0.5.9；当时 self lock 只与 Cargo 相差生成器注释，并被
  Cargo `--locked --offline` 接受。§7.40 为来源合同对拍进一步统一生成器头，当前新
  lock 已逐字节一致。
- **合同升级**：workspace 夹具改为 resolver 3，并让全部成员继承
  `workspace.package.rust-version = "1.85"`；原有 27 项结果/机制合同无需新增 harness
  即可判断，仍为 **27/27**。resolver 2 回归、单包合同和 lock 双向验收继续保留。
- **真实 RED 带出的伴生修复**：从空 `MIRVM_HOME` 跑 fast 时，当前 rust-src 的
  `proc_macro` 因 `[lints.rust]` 被旧解析器拒绝。Cargo `-vv` 探针确认 lint level 先按
  priority 稳定排序，随后追加 `unexpected_cfgs.check-cfg`，且 build.rs 与普通 target
  同样消费。现已严格校验 level/priority/check-cfg，将参数传给 root/path/registry、
  host/proc-macro/build.rs 全部编译入口并写入指纹；空目录 sysroot 冷建成功。
- **剩余边界和下一步**：resolver 1、workspace lints、复杂成员/package spec 继续响亮
  拒绝。P2 主队列进入 Git 依赖，之后才是 alt registry/config、replacement/patch、
  pack self 与总验收。最终验收为 Rust 测试 **186/186**、resolver 3 workspace 合同
  **27/27**、隔离可写 Cargo home/target 的 release `tests/run.sh fast` **11/11**。

### 7.37 2026-08-10：Cargo compat 改为长期双轨，不再安排删除评审

- **用户裁定**：mirvm 同时保留 Cargo 依赖路径和 cargoless 自有路径。缺省继续使用
  cargoless；用户可用 `MIRVM_DEPS=cargo` 显式回退。
- **原因**：Cargo compat 不只是故障救援，还承担 Cargo 行为裁判、版本迭代差异定位和
  持续对拍。删除它会降低 cargoless 对齐 Cargo 的可验证性，也会拿走用户处理暂未覆盖
  Cargo 构造的直接出口。
- **长期合同**：两条路径各自完整并持续维护；默认 self 的零 Cargo 机制测试继续保留，
  compat 的 Cargo native/runner 对拍也继续保留。compat 不得成为静默回退：只有用户
  显式选择时才进入，self 遇到不支持构造仍应响亮报错。
- **替代旧决策**：§7.31 的“P5 后重启删除评审”失效。后续 `mirvm pack` 翻 self 只是
  改默认实现和减少强依赖，不构成删除 compat 的前置步骤。

### 7.38 2026-08-10：P5 第二批 Git 依赖闭合

- **获取与凭据**：新增 cargoless Git store。可变的默认分支、`branch`、`tag`、
  `rev` 只在无锁求解时解析，系统 Git 负责网络与认证，直接复用 credential helper、
  SSH agent 和 known_hosts；没有用户报名表或私有仓库特判。checkout 按仓库身份和精确
  commit 分目录，submodule 按树中钉住的提交初始化。
- **锁与离线**：Git package 的 lock source 必须含 40/64 位精确 commit 且无 checksum；
  已有锁不重新解释浮动分支。暖缓存可离线复跑，冷缓存离线明确指出缺少哪个 commit；
  缓存 origin 不符、checkout 文件或 submodule 被改动都拒绝继续。
- **来源身份**：解析节点不再只按包名合并。本地内部身份包含精确 Git source，lock
  依赖行保留 Cargo 的 `name version (source)` 消歧信息；因此同名同版本包来自同仓库
  两个 commit 时，feature、依赖边、编译单元和缓存指纹都保持分立。修复过程中真实
  抓到“fresh lock 已前进但误用旧编译产物”，根因正是旧指纹只写了 registry 常量；
  现已由 Git commit 指纹回归钉死。
- **验收**：默认分支、branch/tag/rev、仓库内 workspace/package、feature/path 边单测
  通过；`cargoless_git_contract` **9/9** 覆盖 fresh、分支移动、locked/offline、冷缓存、
  双 commit、Cargo lock 互认、execve 零 Cargo 和篡改拒绝。真实双 commit 程序 self
  在线/离线均输出 `17 19`，其 lock 被固定 Cargo `--locked` 接受。cargoless Rust 测试
  **105/105**，全量 Rust 测试 **193/193**，隔离可写 Cargo home 的 release fast
  **12/12**。下一批按计划转向 alt registry 与 Cargo 配置合并。

### 7.39 2026-08-10：P5 第三批——替代 registry 与 Cargo 配置合同闭合

- **裁判不漂移**：当前合同固定
  `cargo 1.98.0-nightly (a335d47ff 2026-06-26)`；新 Cargo 只做差异观察，差异解释并
  更新合同后才能升级裁判。Cargo config 按 `$CARGO_HOME`、项目祖先从浅到深、
  `include` 先于包含文件、同目录 extensionless `config` 优先及环境变量覆盖合并。
  只建模 resolver、registries、registry credential、source replacement 所需键，不把
  “依赖来源够用”冒充完整 Cargo config。
- **来源和认证**：registry 身份进入解析节点、lock 与自有 store；支持 sparse HTTP 与
  Git registry index。认证复用 token/credentials，并实现 `cargo:token`、
  `cargo:token-from-stdout`、credential alias 和 Cargo credential protocol v1；外部
  provider 只接收 `--cargo-plugin`，配置参数放协议 JSON 的 `args`，不另设 mirvm 名单。
- **验收**：本地认证 sparse registry 同时由固定 Cargo 和 self 首次获取；self fresh
  lock 与 Cargo 逐字节一致，被 Cargo `--locked --offline` 接受；暖缓存离线复跑成功，
  execve 审计为零 Cargo。认证日志同时钉住 registry 名、provider argv 与协议参数。

### 7.40 2026-08-10：P5 第四批——source replacement、patch 与 replace 闭合

- **替换链**：逻辑 lock 来源与实际取包后端分开。支持 registry 镜像、Cargo
  local-registry 和 vendor directory，replacement 可连成链但形成环或引用不存在来源时
  直接报错；directory 的 package 与逐文件 checksum 均校验，源码篡改不能进入构建。
- **求解语义**：registry 目标上的 path/Git/registry `[patch]` 作为版本候选参与
  PubGrub 求解，而不是求解后偷换源码；直接依赖和传递依赖共同选择补丁，未被约束选中的
  版本写 Cargo `[[patch.unused]]` 且不进入构建图。`[replace]` 按精确名称和版本处理，
  lock 保留原 registry package 行及 `replace` 指针，再记录真实替换 package。
- **验收**：新增标准来源合同 **30/30**，覆盖 alternate registry、三类 replacement、
  direct/transitive/unused patch、replace、替换环、离线复跑、篡改拒绝和 execve 零
  Cargo。六类 self fresh lock 与固定 Cargo 逐字节相同，并全部通过 Cargo
  `--locked --offline`。Git source replacement 与以 Git URL 为目标的 patch 未纳入本批
  合同，仍是明确边界。

### 7.41 2026-08-10：P5 第五批——pack 缺省 cargoless，Cargo 回退长期保留

- **默认路径**：项目和 frontmatter 脚本的 `mirvm pack` 缺省调用 cargoless driver；
  run/test/pack 因而共享 manifest、依赖图、lock、build.rs、proc-macro、native library
  和编译调度。最终根 rustc 会话只在落 `.mirvm` 包的回调处分流，不复制依赖机制。
- **双轨合同**：`MIRVM_DEPS=cargo` 仍显式进入原 Cargo runner，继续承担用户回退和行为
  裁判；非法值直接拒绝，self 失败不会静默转 Cargo。pack 合同 **6/6**：默认生成包且
  execve 零 Cargo，两轨产物都能在全新 `MIRVM_HOME` 运行，Cargo 轨由进程审计证明确实
  启动固定 Cargo。
- **伴生正确性修复**：空 home 验收暴露 rust-src workspace 使用 resolver 1。顶层
  resolver 1 仍因 feature 统一语义未实现而拒绝；但路径依赖只需按其 workspace 物化包
  元数据，不能把依赖所在 workspace 的 resolver 当成根 resolver。读取入口现已分开，
  全新 home 的真实 sysroot 重建并运行 `fresh-sysroot-ok` 成功。
- **总回归**：Rust 格式、严格 Clippy 与单元测试全绿（Rust 测试 **199/199**）；
  `tests/run.sh fast` **12/12**，其中 workspace 27/27、Git 9/9、来源 30/30、pack
  6/6、harness 自检 16/16。该档不运行性能基线。
- **下一步**：按用户裁定不恢复、不运行性能基线；先做 P2 总 corpus 验收，据结果更新
  剩余 P5 分类，再评审 D17 余项。

### 7.42 2026-08-10：E 批第一轮——多 Engine、验证器与故障边界

- **Cargo 双轨边界先收口**：Cargo compat 现在把调用者目录通过内部协议交给 guest，
  编译仍在项目目录进行，因此 `--manifest-path` 从别处启动时与 Cargo 的运行目录一致；
  `MIRVM_ENCODED_RUSTFLAGS_APPEND` 的内容进入独立 target 分区，改值不会复用旧产物。
  rustc 所需构建环境与 guest 运行环境分开保存和恢复。`differential.cargo` 8/8，覆盖
  异目录运行、暖缓存运行环境变化和追加 rustflags 变化。
- **多 Engine 触发器已经命中，选择 T**：旧设计的进程级 `SHARED` 单例无法表示同一
  进程里的两个 Engine。选择 T，即按当前宿主线程记录“正在运行哪个 Engine”；每个
  Engine 以 `Arc<Shared>` 持有程序，线程表按 Engine id 保存独立 `Ctx`，嵌套进入会在
  返回时恢复外层。保留寄存器方案 R 只能缓存一个已经选定的 ctx，不能回答“当前是哪
  个 Engine”，所以不是这次身份问题的解法；它只保留为 E6 性能触发后的候选优化。
- **所有权与失败返回**：JIT worker、发布槽、fork 基线和 guest `atexit` 登记按 Engine
  隔离；Engine 析构会停止并等待自己的 worker、注销入口并清理未执行的退出回调。
  每线程 `Ctx` 持有 Shared，最终析构时释放 guest TLS 实例。反序列化后的 builtin 名称
  改为 `Box<str>` 随 Module 回收，不再泄漏成 `'static`。机器码符号表也从进程全局表
  移到各 Module：foreign 解析只搜索当前模块的镜像，同名 `global_asm` 不会跨 Engine
  串用。rustc driver 的全局诊断状态由编译会话锁保护。引擎产生的 Trap、非法调用、
  栈耗尽和 JIT 错误不再在库路径直接 `exit`，而是退到 `run_main`/`run_export`，以
  `RunError` 返回消息和退出码；真正的 guest panic、Terminate double-panic 和宿主自身
  panic 仍按各自 Rust 语义处理。
- **字节码验证器 E20 完成**：新增穷尽匹配的验证 pass，检查函数/TLS/asm/块引用、
  frame 和 slot 范围、SIMD 形状、inline asm 缓冲、GOT/frozen 地址及 FFI 布局。pack
  写入、pack/基础镜像/依赖镜像/L2 读取和最终合并执行前都验证；坏缓存按 miss 处理，
  外来包或镜像在产生 native 副作用前拒绝。
- **栈与内存第一层保护**：操作数区现在两端各有一页 `PROT_NONE` guard。每个已发布
  JIT 函数先进入无 frame 的 guard wrapper，在 Cranelift prologue 真正扣大栈帧前按
  `MIRVM_STACK_SIZE` 检查并返回可诊断的错误；深递归回归要求退出码 70 和函数名，
  不再接受裸 SIGSEGV。guest TLS 实例也在 Ctx 析构时回收，因此 E10、E11 可闭合部分、
  E20、E25、E36 关闭。
- **checked 模式重新划界**：L1 guard 已完成，但 L3 checked 不能用“地址当前已映射”
  冒充。现有 `PlaceStep::Deref` 已抹掉 raw pointer、引用和所有者来源；合法地址又可能
  来自 frame、frozen、mimalloc、用户 allocator、FFI、mmap 或 libc。只查映射会把 VM
  元数据也误判合法，要求用户登记地址则把正确性负担转嫁给使用者。后续必须让 IR 保留
  指针来源并由运行时自动维护所有权范围，或直接使用 P3 worker 的 OS 隔离；此前 E23
  只关闭 L1，不宣称 checked 或沙箱完成。
- **仍未宣称 E22 全闭合**：libffi closure 必须保持代码地址有效，native 已保存的回调
  目前无法主动撤销；JIT 与自装载 MC 镜像也因活动代码指针不能安全卸载，当前只把
  符号可见范围收回 Module。长期宿主线程直接执行 Engine 时的 TSD 释放边界、稳定公开
  嵌入 API、动态库/回调注册撤销和进程级隔离仍需后续工作。当前单测覆盖双 Engine
  ctx、嵌套恢复、析构注销、模块内 MC 符号隔离和引擎错误返回；
  `runtime.semantics` 的 pure/digest/unwind/threads（含 TSan 与小栈 JIT）已通过本轮定向回归。

### 7.43 2026-08-10：D15 P2 总 corpus 验收完成，默认 cargoless 主线闭合

- **第一轮不是假绿**：`corpus.deps-pair --tier full` 得到 **131 pass / 1 skip /
  7 fail**。三项真实差异来自 lock 图的同名多版本边：旧键只含 registry source，
  `getrandom 0.2/0.3` 这类边会互相覆盖，实际撞出 `polodb`、`polars_lazy`、
  `polars_frame`。最终键同时保留已选子包的 source 与 version；这样 registry 多版本
  不覆盖，Git URL/commit 也不丢。新增单测同时锁住 registry 同源多版本、Git 仓库内
  路径包与同包双 revision。
- **其余失败逐项归因**：`faer_lu` 定点复跑直接通过，是同轮并发构建的瞬态
  `ENOENT`；`risc0_run` 与 `datafusion_sql` 是外部获取超时，缓存就绪后 Cargo/self
  定点均通过，未为它们改 harness。`miden_prove` 原钉的 0.25.5 已被 registry yanked，
  Cargo 自己也不能 fresh 解析；迁到同发布线 0.25.8，并删除该版本不再需要的 wincode
  Git patch。Cargo/self 的证明长度、摘要、验证与篡改拒绝输出逐字节一致，原 P5/XFAIL
  正式摘除。
- **总验收结果**：第二轮在统一入口完成 **138 pass / 1 skip / 0 expected-fail /
  0 fail**，耗时 2,900,921ms。唯一 SKIP 是宿主缺少 `/usr/lib/x86_64-linux-gnu/libffi.so`
  开发链接名的 `rustpython_mini`，不是两轨行为差异。由此 D15 的默认 cargoless 主线和
  Cargo compat 裁判轨完成当前 corpus 总验收；resolver 1、Git source replacement、
  以 Git URL 为目标的 patch 等仍按公开边界保留，不借总验收声称完整 Cargo。
- **同轮非性能收口**：最终代码 `cargo test --locked --all-features` **208/208**；
  `runtime.semantics` 四组全绿（threads 12/12，含 TSan 和 JIT 栈诊断）；pack **6/6**；
  `tests/run.sh fast` **12/12**。fast 还撞出 Cargo compat 测试内再执行
  `CARGO_BIN_EXE_*` 时内部 sysroot 被用户环境恢复清掉的问题：runner 现从启动器旁置
  配方恢复内部 sysroot，无需用户保留变量；单包 **20/20**、workspace **27/27** 复绿。
  按用户裁定，本轮未运行 `performance.limits` 或完整 `gate`。

### 7.44 2026-08-11：D17 余项闭合——bench、根 proc-macro 与 resolver 1

- **旧状态**：`mirvm test` 已覆盖普通单包和 resolver 2/3 常见工作区，但 bench、根
  proc-macro、resolver 1、复杂成员 glob、workspace lints 和完整 package ID spec 仍被
  列作缺口。doctest 也在同一张表里，容易被误解成可以用普通 test target 顺手补上。
- **Cargo 实证先行**：固定 toolchain 的 `cargo test/bench -vv --no-run` 证明：bench 是
  带 `--test` 的测试 artifact 并使用 Dev 依赖；根 proc-macro 的普通库必须在宿主侧产出
  动态库，根库 unit test 仍是 VM 的 `--test` artifact，integration test 则通过
  `--extern` 加载宿主 proc-macro。resolver 1 会把 normal/dev/build 用途的 feature 合并，
  但宿主和目标产物仍是不同编译单元。旧 edition 或虚拟工作区没写 resolver 时，Cargo
  默认采用 resolver 1。
- **新选择**：target 模型加入 Bench；manifest 自动发现 `benches/`，test CLI 支持
  `--bench`/`--benches`/`--all-targets`。host closure 可以从根 proc-macro 的普通依赖出发，
  根动态库、unit test 和 integration test 各走 Cargo 对应形状。resolver 1 的 feature、
  optional 激活和弱 feature 引用在依赖用途间统一；`**`/`?`/`[]` glob、workspace lint
  继承与路径/版本 package spec 在工作区机制内实现。没有 resolver 的旧工作区直接按
  Cargo 默认使用 resolver 1，不要求用户补清单。
- **验收**：单包合同 **24/24**，覆盖 bench、all-targets 和根 proc-macro；工作区合同
  **31/31**，覆盖未显式声明的 resolver 1、normal/build feature 合并、复杂 glob、
  workspace lint 和完整 path package ID。两脚本都以固定 Cargo `-vv` 为编译形状裁判，
  self 腿继续由 PATH 哨兵和 execve 审计证明不启动 Cargo。
- **边界重分配**：D17 的普通 test/bench Cargo 合同闭合。doctest 需要 rustdoc 处理代码块
  提取、临时 crate、源行号和 compile-fail 诊断，另记 D19；不能把它伪装成 integration
  test。嵌套 workspace 与同名成员由 Cargo 自身拒绝，不再误记成 mirvm 应当放行的余项。

### 7.45 2026-08-11：D3 核心落地——mmap 容器、逐函数惰性驻留与热序调度

- **旧状态**：格式 v2 用 `fs::read` 把整包复制到 Vec，再用 postcard 一次解码整个
  `Module`；所有函数体在启动时同时物化并常驻。D3 原计划要求 mmap、逐函数惰性解码、
  上次真实访问顺序预取和需求优先，但不能为节省启动工作绕过 E20 字节码验证器。
- **新布局 = 不稳定格式 v3**：文件以只读 mmap 打开。MODULE 只保存非函数元数据；
  新 FUNCS 节先存函数数目和固定大小的 offset/length/hash 索引，再存每个独立 postcard
  `FuncBody`。解析器检查表长、整数溢出、边界、重叠和逐体哈希；需求解码时再次核对该
  函数哈希。`Module.funcs` 改为同时支持原 Vec 形态和 mmap 惰性形态的 `FuncTable`，
  既有 L2/base/deps 序列化仍保持原 sequence 编码，不被包格式改版带偏。
- **调度选择**：单个 `mirvm-decode` worker 维护 demand 和 predicted 两个队列，永远先取
  demand；已在预测队列的函数被访问时提升为 demand。预测不能中断一个已经开始的
  postcard 解码，因此一次需求的等待上界是当前解码的一项加它自己。实际首次访问顺序
  自动记录，Module 释放时按 FUNCS 内容哈希原子写入
  `$MIRVM_HOME/package-heat/<hash>.order`，下一次装载据此预取；用户不维护热函数名单。
- **E20 约束与剩余项**：现有验证器检查的是解码后的 IR 语义，不只是容器边界。为保证
  任何 MC/native 副作用前完整拒绝坏字节码，v3 首次装载仍逐函数临时 postcard 解码、
  运行 E20 验证并立即释放；执行期随后按需重新解码并驻留。这已经消除整文件复制和全函数
  长期驻留，但不等于“首次装载完全不反序列化”。D3 仍保留档案直接语义验证余项，完成前
  不启动 D4 格式冻结。
- **验收与范围**：Rust 单测覆盖独立函数索引/逐体损坏拒绝和 demand 提升队首；pack
  合同覆盖首次运行产生一个非空 heat 文件、同 home 第二次运行输出不变，自包含与 Cargo/
  self 双轨继续通过。性能基线按维护者要求暂缓，本条只声明结构和正确性结果，不声明
  启动时间或内存数字。OS 沙箱不在本轮施工范围。

### 7.46 2026-08-11：D3 余项定路；D19 rustdoc doctest 合同闭合

- **D3 可以完成，但不能在 v3 上伪造“直接验证”**：v3 的 postcard 函数体是顺序编码，
  E20 必须看到 block、slot、地址、FFI 布局和每条操作的结构关系；只检查容器哈希或另存
  一份验证摘要，会允许外来包同时伪造摘要与执行体。后续应新建不稳定格式 v4，以带边界
  检查的偏移式只读归档作为唯一函数体真源；E20 直接借用遍历它，运行时按首次访问从同一
  已验证表示恢复 owned `FuncBody`。这会同时改包编码和验证器访问层，是 D4 前独立工程，
  不是本轮 D19 的附带小改；性能基线仍按维护者要求暂缓。
- **rustdoc 继续做它擅长的前端工作**：固定 rustdoc 自己提取 Markdown、生成临时 crate、
  保持源码行号并裁判 `no_run`、`ignore`、`compile_fail`/错误码和
  `should_panic(expected)`。MIRVM 通过固定工具链的 `--test-builder` 接口接管生成物：
  合并库产带 MIR 的 metadata-only rlib，可运行 bin 先用 rustc 检查，再发布为旁置配方的
  VM 启动器。这样没有重写 Markdown 解析器，也没有把 doctest 冒充 integration test。
- **双轨一致**：默认 self 直接组织 rustdoc 参数；Cargo compat 用内部 rustdoc 包装入口
  保留 Cargo 参数，只把 sysroot 和 test builder 改为 MIRVM 同源版本，并移除 Cargo 注入的
  `--test-runtool`，避免启动器被重复套 runner。两轨都支持默认/`--doc`、根库、Dev 依赖、
  build.rs cfg/env、过滤、静默、状态文字和退出码；`--doc --no-run` 与混合 target 选择按
  Cargo 以 101 拒绝。
- **验收**：新增 `cless_doctest_contract`，固定 Cargo/compat/self 三腿逐字比较正常、失败、
  默认选择和参数拒绝；固定 Cargo `-vv` 钉住 rustdoc 根库、Dev 依赖和 build cfg；self
  用 PATH 哨兵与 execve 审计证明零 Cargo。`contracts.cargoless-test` **34/34**。本条闭合
  D19 的 `mirvm test` 范围，不据此宣称已经提供独立 `mirvm doc` 或 HTML 文档生成命令。

### 7.47 2026-08-12：E3/E8/E24/E28/E31 闭合；接受项退出开放账

- **E31 内容身份**：依赖镜像和 IR 缓存旧键只看路径、长度与纳秒 mtime。相同长度的
  内容替换再恢复 mtime 时，两条路径都会误命中。现统一从同一文件描述符读取元数据和
  BLAKE3 内容摘要，哈希前后元数据漂移则拒绝入键；回归明确构造“同长度、恢复 mtime”并
  要求键变化。mtime 仍用于快速身份信息，但不再承担内容正确性，因此 E31 关闭。
- **E3/E8 混合栈与符号**：解释器影子帧现在记录客体函数 IP 和对应宿主栈位置；JIT
  发布时登记机器码地址范围和函数身份。`_Unwind_Backtrace` 先用宿主 unwinder 收集当前
  JIT 帧，再按栈位置与解释帧合并。Engine 装载时还会从轻量函数名表生成只读最小 ELF，
  通过 `dlopen` 纳入标准 Rust backtrace 的 ELF 符号查找；惰性包无需为此解码全部函数体。
  `c_backtrace` 在纯解释与强制 JIT 下都要求捕获、深度增加和文本出现
  `c_backtrace::deep`。E3/E8 关闭；尚未实现的 unwinder 上下文写入/Resume 家族仍按原
  `Unsupported` 边界保留。
- **伴生 JIT 正确性修复**：debug Cargo 差分暴露 Cranelift 基本块断言。根因是 cleanup
  发射器在当前块尚未由 `try_call` 终结时先切去异常 pad；release 只是关闭断言，并不让
  构造顺序正确。现先准备异常表和块，发 `try_call` 后再填 pad 与 normal continuation；
  原始 ecosystem 同步 JIT 负载和完整 Cargo 差分均通过。
- **E24 wrapper 组合**：固定 Cargo 探针确认普通 wrapper 在外、workspace wrapper 在内，
  后者只用于 workspace 成员。MIRVM 不再占用或清空任何 wrapper 槽，而是占据 Cargo 的
  `RUSTC` 编译器槽；因此环境变量、Cargo config、普通依赖/workspace 选择、两层顺序和
  wrapper 对参数的修改仍完全由 Cargo 执行，MIRVM 作为最内层编译器捕获最终参数。真实
  path 依赖项目分别用环境变量和 `.cargo/config.toml` 对拍固定 Cargo，输出及链顺序
  **4/4**，`differential.cargo` 总计 **12/12**。E24 关闭。
- **E28 旧前提失效**：固定 rustc 1.98 的 `allocator_kind=Global` 实测表明，最终 crate
  已生成并解析 `__rust_alloc_error_handler`，它自然调用 std 的全局错误处理器，并不走
  旧记录所称的未知符号 Trap；四个普通分配入口仍需既有程序级重路由，两者不能混为一谈。
  新差分以自定义 `#[global_allocator]` 直接调用 `handle_alloc_error(123)`，native 与 MIRVM
  冷/热都打印同一诊断并以 SIGABRT 结束。顺带删除 HostAbort 额外打印的 MIRVM 私有行，
  恢复 libc abort 的 stderr 行为。E28 作为过时欠账关闭。
- **从 open issues 移出的定型项**：E4 的 JIT 原子统一 SeqCst 是 Rust 内存模型允许的
  合规强化；E18 的 Cranelift 自有内联明确保持关闭，只在真实性能问题中重新评估；E29
  所谓“全改通用直通”不是产品能力，普通符号已有 dlsym+libffi，带 guest 语义或热路径的
  专用 shim 应继续专用；E35 在退出时取消未发布优化任务，解释器仍给出同一程序结果，
  不等待队列是已接受的短程序性能选择。四项转入 H 节，不再以未解决缺陷重复计数。
- **验证范围**：本节不恢复性能基线，不运行 OS 沙箱或完整 gate。格式、严格 Clippy、
  release 构建与 Rust 单测 **213/213** 通过；程序差分默认/同步 JIT 各
  **46 PASS / 2 SKIP**，`differential.cargo` **12/12**，其余非性能 `fast` 产品叶
  全绿。首次聚合仅有 `harness.truth` 的假 Cargo 不认识新 wrapper 项目；最小同步替身后
  该叶 **16/16** 单独复绿。E31 两条内容变更单测、符号 ELF 单测、强制 JIT/纯解释
  backtrace 负载均在上述结果中通过。

### 7.48 2026-08-12：E9 闭合；E 区性能项与接受限制归位

- **E9 不再靠“没有观察到错误”**：新增 `demo/dl_iterate_phdr_probe.rs`，由 glibc
  调用 guest 回调；回调先按 `size` 检查 `dl_phdr_info` 的公开前缀，在第一份有效 ELF
  映像上返回固定值 73。native、纯解释和强制同步 JIT 的 stdout/stderr/退出码一致；
  标准 `differential.programs` 默认与同步 JIT 两种入口各 **2/2**，都包含冷、热复跑。
  地址、映像数量和路径不是稳定合同，测试不比较这些环境量。E9 关闭。
- **E2/E5 是已知产品边界，不是待修错误**：当前不做 OSR（运行中的函数现场替换）、
  deopt（从优化代码退回解释器）和生产级分层编译；无调用边界的长跑单循环因此不会中途
  转 JIT。Cranelift JIT 也不支持逐函数释放，机器码随 Module 常驻。两项移入 H；前者
  由真实长跑负载触发，后者由稳定嵌入 API 的资源有界验收触发。
- **性能只保留一个施工入口**：原 E6/E7/E30/E37 都不描述错误语义，分别是快路径内联、
  JIT 优化候选、旧 regex 基线复测和日志 v2。它们连同旧计量、97ms/80ms 红线、vmctx
  复测闸门与日志估算一并并入 D16；没有删掉触发器，也没有用文档归类冒充性能已经改善。
  按维护者现行裁定，性能基线仍延期统一恢复。

### 7.49 2026-08-12：撤销 E12 的 alloca 必迁承诺

- **旧选择**：2026-07-05 把 slaved 操作数区定义为 v0，承诺解释帧局部以后必须改放
  native 栈上的动态 `alloca`；当时预期它更接近未来编译帧且局部性更好。
- **后来证据**：生产 JIT 已经落地。热函数由 Cranelift 使用真正的 native 帧；未逃逸值
  还能保留在 SSA/寄存器中，只有必须有地址的局部才落栈。alloca 因而只会改冷层解释器，
  不会让解释器自动获得编译码的固定栈偏移寻址。当前 ByteRegion 也早已不是 Spike 1 的
  `Vec<u64>`：它每线程一次 mmap、按需提交物理页、LIFO 水位分配、地址稳定且两端有
  guard page。
- **成本重估**：生产可用的动态 native 栈方案必须同时实现栈余量检查、跨页探测、帧清零、
  正确的 unwind 信息和 checked 模式的逐帧地址追踪；它还让 guest 局部与解释器自己的
  宿主栈竞争同一额度。只比较移动栈指针的微基准会漏掉这些必需成本。
- **新选择**：解释态局部正式使用 slaved ByteRegion；编译态局部继续使用 Cranelift
  native 帧与 SSA。撤销“alloca 是架构终态”和“后续必须迁移”，E12 从开放账移入 H。
  这不改变 Model A：guest 调用活动仍逐层位于 native 调用栈。
- **重开条件**：必须有可复现的真实解释器负载，证明局部存储是端到端主瓶颈；候选实现
  计入清零、栈探测、unwind、深递归和 checked 成本后仍有显著收益，并保持现有正确性与
  诊断合同。仅有合成微基准或“更纯”的实现偏好不足以重开。

### 7.50 2026-08-12：R18 C-unwind 闭合，E13 定稿为宿主 Rust panic 运输层

- **推翻的旧前提**：R18 曾认为 libffi closure 没有 unwind 信息、异常原理上不能穿过，
  并据此把 callback panic 和 direct `C-unwind` 记作永久残余。项目实际捆绑的 libffi
  3.6.0 在 Linux/x86_64 为 `ffi_closure_unix64` 提供 FDE；临时宿主探针让 Rust panic
  与 C++ 异常双向穿过 closure，证明真正阻点是 mirvm 的 Rust wrapper 被写成普通
  `extern "C"`。同样，libffi 的 `ffi_call` 机器层可传播，缺的是 Rust 侧合法的
  `C-unwind` 声明。
- **native 合同实测**：全链 `C-unwind` 时，C++ typed exception 穿 Rust cleanup 后仍由
  C++ 按原类型捕获；Rust panic 经 C++ `catch(...); throw;` 后由 Rust catch 收回原 payload；
  C++ 吞 Rust panic 必须终止。普通 C callback 的 panic 必须终止。Rust `catch_unwind`
  遇到 foreign exception 在固定工具链上会终止，标准库也不保证捕获它。
- **实现**：lowering 保全 direct 和 fn pointer 的 `C/System { unwind }` 位；出向调用只在
  `unwind=true` 时用本地 `extern "C-unwind"` 声明调用同一 `ffi_call`；callback/P1 入口
  按位选择 C 或 C-unwind wrapper。JIT 的 `try_call` 返回值改由 `TryCallRet` 正常块参数
  承接。解释器 direct/native-indirect 调用补上 `UnwindAction::Terminate` 守卫，避免内层
  C-unwind 异常越过 guest 普通 C wrapper。非 C/System ABI 不再默认为 plain C，而是在
  lowering 阶段明确拒绝。
- **验收**：标准 `runtime.c-unwind` 以固定 rustc+C++ 为 oracle，解释器和强制同步 JIT
  十一项全绿，覆盖 C++ 类型身份、Rust payload、Drop 次数、吞 panic、plain-C panic、
  direct/fn-pointer 两条 Terminate 边上的 foreign exception 与 Rust panic，以及两种
  非 C/System ABI 拒绝。
  `jit_foreign_probe` 另锁住有 cleanup 的标量返回。
- **E13 裁决**：guest panic 正式继续用宿主 Rust panic 作运输层。guest catch 点只按
  `GuestPanic` downcast，其余载荷继续展开；Engine 顶层把未捕获的 `GuestPanic` 映射为
  退出码 101、把 `EngineFault` 映射为 `RunError`，只有其余宿主 panic 原样重抛。独立
  exception class/personality 不能让标准 Rust catch 捕获 C++ 异常，反而会把 guest
  异常先变成 foreign exception，并引入
  自有 personality、两阶段展开和对象所有权的整套平台责任，因此不作终态承诺。
- **重开条件**：稳定嵌入 API 明确要求分类异常越出 Engine；固定工具链升级让当前合同
  回归变红；或实测证明 host/guest panic 共轨产生无法用局部 guard/pad 修复的错误。
  详细合同和修复前矩阵见 `designs/c-unwind-contract.md`。

### 7.51 2026-08-12：真实嵌入入口推翻 E13 降级，采用自有异常类与原始分类

- **为什么立即重开**：维护者指出“稳定嵌入 API 尚未发布”不等于没有真实嵌入需求。
  当前 crate 本来就是 library，P1 可执行入口和 libffi callback 已经能让 native 代码回调
  Engine，也能让一个 Engine 的入口穿过另一个 Engine。能立即构造的生产路径不应被
  延期成未来触发器，因此 §7.50 的 E13 降级结论只保留为历史，不再代表现行设计。
- **三个真实 RED**：① Engine A 的 C-unwind thunk 在 Engine B 内调用时，B 的 guest
  `catch_unwind` 会把 A 的 guest panic 当成自己的异常并运行 B 的 catch 函数；
  ② A 的 `EngineFault` 穿过 B 时会被 B 顶层错误消费，A 的 fault 状态留在在途状态；
  ③ Engine 顶层使用标准 Rust `catch_unwind` 时，C++ typed exception 到达这里会以
  `Rust cannot catch foreign exceptions` 终止，外层 C++ 无法按原类型和值接回。
  同一轮还确认旧 `run_main` 会把正常返回 101 和未捕获 guest panic 都表示成同一个
  `Ok(101)`，library 调用者无法分辨程序结果。
- **新选择：自有异常类，不自研 personality**：MIRVM 用独立 exception class 包住
  guest 标准库给出的原始异常指针，或包住 `EngineFault`。捕获边界直接取得系统展开器的
  原始对象并分类；分类时核对 class、对齐、ABI cookie、当前进程 canary，再用
  `Arc::ptr_eq` 核对对象内保存的 `Arc<Shared>` 所属 Engine。MIRVM 没有另写决定每帧如何
  展开的 personality；解释帧继续借宿主展开，JIT 帧继续使用已有 Rust personality 和
  LSDA cleanup。这只增加可靠身份和所有权判断，没有接管两阶段栈展开算法。
- **guest payload 仍归 guest std**：自有异常只是外壳，不读取 std 的私有
  Exception/Box/vtable 布局。guest catch 消费本 Engine 外壳后，把内层原始指针交给
  guest catch 函数；未捕获时，Engine 调用固定工具链的 guest
  `std::panicking::catch_unwind::cleanup` 降低 panic 计数，再由同一 guest 类型的 drop
  glue 析构 payload 并走 guest allocator。payload Drop 再 panic 时按 double-panic 终止，
  不能伪装成普通 `RunError`。
- **EngineFault 单独传播**：每个宿主线程只允许一个在途 `EngineFault`，异常携带所属
  Engine 和发起 Ctx。它穿过另一个 Engine 的 guest catch、顶层和 `Terminate` 守卫时
  继续展开；解释器 FrameGuard 与 JIT cleanup pad 都查询线程级标记并跳过 guest cleanup。
  只有所属 Engine 的执行边界可以消费它、清除标记并生成
  `RunErrorKind::EngineFault`。同一线程出现第二个在途 fault，或错误所有者试图消费，都会
  响亮终止。
- **Engine 顶层合同**：`run_main`/`run_export` 现在返回
  `Result<RunOutcome<T>, RunError>`。`RunOutcome::Returned(value)` 与
  `RunOutcome::GuestPanic` 分立；缺入口/导出和 `EngineFault` 由 `RunErrorKind` 分类。
  CLI 才把 guest panic 映射为 101，正常返回 101 在 library API 中保持正常返回。未识别
  的宿主 Rust panic 原样续传；C++ foreign exception 也不转换，沿 C-unwind 原样穿出
  整个 Engine，由外层 C++ typed catch 接回。相反，C++ exception 到达 guest
  `catch_unwind` 时仍严格按固定 rustc 终止，guest catch 函数不得运行。
- **验收**：解释器和强制同步 JIT 的多 Engine 定向测试锁住 guest panic 所属、
  `EngineFault` 跨 Engine 时不运行 guest cleanup/不被异主消费、owner 清理状态后可继续
  调用，以及宿主 Rust panic 原 payload 续传。`runtime.c-unwind` 扩为 **13/13**，新增
  “C++ typed exception 穿出整个 Engine”和“foreign exception 到达 guest catch 必须
  终止”；`runtime.semantics unwind` **11/11**，新增两种执行模式下未捕获 payload 的
  guest 侧计数复位、恰一次 Drop、Drop 内再次 guest 调用与第二次 panic。
- **仍开放的边界**：本条闭合执行期间的异常身份、归属、资源回收和结果分类，不关闭
  E22。与执行并发发生的 Engine drop、native 已保存 libffi callback 的撤销、JIT/MC
  活动码卸载、长寿命宿主线程 TSD 和稳定公开生命周期协议仍需独立解决。

### 7.52 2026-08-12：逐异常对象决定 cleanup；真实 lang_start main 结果闭合

- **保留并推翻 §7.51 的错误机制**：§7.51 把“线程上存在在途 `EngineFault`”写成解释器
  FrameGuard 与 JIT cleanup pad 的共同跳过条件，并禁止同线程第二个 fault。严格审查指出，
  native catch 可以暂停外层 `EngineFault` 而不消费它，随后在同一宿主线程重入 Engine。
  此时新发生的 guest panic 是另一只异常，必须正常运行 guest cleanup；重入代码也可以
  产生并先结清一只内层 `EngineFault`。线程布尔无法回答 landing pad 当前收到的是哪只
  异常，因此 §7.51 该段只保留为错误结论的历史证据，不再代表现行实现。
- **FrameGuard 回滚与正确分类点**：解释器不再让 `FrameGuard::drop` 查询线程状态并执行
  cleanup。每个 `interp_frame` 在 `run_blocks` 外设置 raw catch，直接分类本次捕获的异常
  对象；只有当前对象不是 `EngineFault` 时才进入 MIR cleanup，随后原样续传。FrameGuard
  退回单一职责：恢复操作数区、影子帧与深度。JIT landing pad 同样把展开器交来的实际
  exception pointer 传给只读 classifier，只为这只对象决定进入 cleanup 还是立即 resume。
- **TLS 只做 owner 清账**：线程上下文保存带单调 nonce 的 `EngineFaultToken` LIFO 栈。
  token 同时记录 owner id 与发起 Ctx；消费时必须由当前 owner 匹配栈顶 token。它不参与
  cleanup 分类。native catch 暂停外层 fault 后，重入 guest panic 会照常 cleanup；重入的
  内层 fault 可 push/finish/pop，外层 token 仍存活，最后再由外层 owner 结清。
- **新增回归证据**：解释器和强制同步 JIT 都覆盖两条原先会失败的重入路径：暂停外层
  fault 后，独立 guest panic 的 cleanup 必须执行一次；暂停外层 fault 后，内层
  `EngineFault` 必须独立返回 `RunErrorKind::EngineFault`，结清后外层 fault 仍可由原 owner
  消费。原有跨 Engine fault 测试继续要求异主 catch/顶层不能消费，且当前 fault 自身不跑
  guest cleanup。
- **真实 main 暴露第二个 101 缺口**：§7.51 只让 Engine 顶层区分“越出顶层的 guest
  panic”和正常返回值；真实 `std::rt::lang_start_internal` 会先在 guest 内部捕获 main
  panic，再以 `Termination` 路径返回 101，所以顶层看到的仍是正常整数。以数值或符号名
  猜测都不可靠。
- **精确框定固定 std 的 main catch**：lowering 从 lang item `start` 的真实 MIR 调用图
  出发，验证唯一 `lang_start_internal`、外层 `std::panic::catch_unwind` 闭包和其中唯一的
  main catch；不依赖 DefId 数值或人工函数名单。该直接调用冻结为
  `CallRole::MainPanicBoundary`。固定 std 调用图改形时 lowering 响亮失败，不把升级风险
  转嫁给嵌入者。
- **每次 run_main 独立记账**：每个 Ctx 为嵌套 `run_main` 保存 LIFO 状态。进入标记调用
  后，只允许紧随的第一层 guest `catch_unwind` intrinsic 认领该边界；它捕获本 Engine 的
  guest panic 时标记本次运行。`lang_start` 正常返回后，`run_main` 据此返回
  `RunOutcome::GuestPanic` 或 `RunOutcome::Returned(101)`。用户 main 内自己的嵌套 catch
  不会误认领，嵌套 run 也不会串状态。
- **解释/JIT/包合同**：解释器在 `CallRole` 处建立作用域；JIT 对该调用使用专门 helper，
  被调函数内部仍可正常发布机器码。role 随 IR/postcard、image rebase 与 `.mirvm` 函数体
  保存；验证器要求完整可执行模块恰有一个 `MainPanicBoundary` 且 unwind 为 `Continue`，
  mmap pack 装载的逐函数验证也汇总同一计数。部分 image 可把边界留在前层，但合并后的
  最终可执行模块必须重新满足唯一性。
- **验收**：`runtime.semantics unwind` 在原九个语义用例和两种执行模式的未捕获 payload
  清理之外，又用真实 lowering + `lang_start_internal` 在解释器/强制同步 JIT 各对拍一次
  main panic 与正常 `Termination` 101，现为 **13/13**；JIT 腿还要求启动闭包真实发布。
- **E22 当时口径（已由 §7.53 推翻）**：本条曾把 `Engine::shared` 和
  `run_export` 一并写成 safe Rust 公开入口，并把并发 close、callback 生命期和活动码
  回收一并留给未来的稳定 API。这个公开面描述不准：现行 `Shared` 不公开，无类型
  `run_export_raw` 是 `unsafe`；并发关闭与每 Engine 资源隔离已在 §7.53 立即实现。

### 7.53 2026-08-13：真实嵌入立即施工；Package v4 与 Engine 关闭协议

- **为什么不是未来项**：§7.51/§7.52 已证明当前 library、P1 函数指针和 native callback
  能立即形成真实嵌入调用。维护者进一步指出，可重复创建同一程序实例也不需要等待外部用户
  报名：现有 `.mirvm` 包和几行宿主代码就能制造需求。旧 E22 把并发 close、回调生命周期和
  多实例资源隔离推迟到“稳定 API 立项”以后，是把当前机制问题改名成未来产品问题，现废止。
- **公开面先说准**：新增 `Package::load(path)` 安全地复制并完整校验包，返回不再依赖源
  inode 的不可变对象。它不是“安全执行任意包”的证明：容器/字节码验证无法证明内嵌 native
  库、宿主符号与 FFI ABI 声明相符，所以 `Package::instantiate` 必须是 `unsafe`。同理，
  `Engine::from_module_unchecked` 与无类型的 `vm::engine::raw::run_export_raw` 保持 `unsafe`；
  后者只返回两个机器字。`Shared` 与 engine 内部模块不再公开。已有 Engine 上的 `run_main`、
  状态和 close/wait 操作是 safe，但这不能冒充完整的 safe typed export API。
- **Package v4 是映像，不是实例**：v3 的 Module 仍混有固定运行地址，一份包只能占用一套
  frozen/P1 地址。v4 把 artifact 身份改为 `LinkAddr`（逻辑链接地址），另存 P1
  `EntryStubSite` 配方与 `FrozenReloc`（冻结指针重定位）。`Package::load` 持有 owned byte
  snapshot；每次 instantiate 独立映射 frozen/TLS、恢复函数表、装载 MC/native 映像，先建
  `LoadMap` 的 `LinkAddr -> 本实例真实地址`，再统一修补 Static、AddrImm、entry、GOT 和
  frozen 内指针。同一 `Package` 可以并发重复实例化；load 后改写或删除源包不影响对象。
- **P1 改成每 Engine 运行身份**：P1 的白话含义是“交给原生代码调用的 guest 函数入口”。
  每个 Engine 由配方生成独有 libffi closure；global_asm 与 C2 archive 不再烤固定入口，
  而是经 RIP 相对隐藏槽跳到本 Engine closure。自产 archive 每实例使用唯一文件身份，避免
  动态加载器合并 mutable global。旧 P1 closure 与 owner 墓碑不回收、不复用地址，所以关闭
  A 后的陈旧指针不会因地址复用误调后来创建的 B，这闭合了 ABA（旧地址先失效、又碰巧代表
  新对象）问题。
- **关闭状态机和执行租约**：Engine handle 可 clone；显式 `close` 或最后一个 handle drop
  把 `Running` 原子改为 `Closing`。`ExecutionLease`（执行租约）是每个公开调用、native
  回调、构造/析构执行仍在使用 Engine 的计数凭据。Closing 拒绝新的普通入口，但允许当前
  native 调用链重入和已经登记的回调完成。计数归零后才进入内部 `Finalizing`，释放重资源并
  发布 `Closed`。空闲 Engine 同步 finalise；活动 Engine 由收尾线程等待。`wait_closed` 在
  当前线程仍位于该 Engine 调用链时返回 `ActiveOnCurrentThread`，避免等待自己释放租约；
  外层退出后可正常等待。
- **延迟回调不能只看“现在有没有线程在跑”**：native `pthread_create` 可能已经收下 guest
  start 地址、但新线程尚未进入；pthread 线程私有数据（TSD）析构器也可能等到线程退出才调。
  新 `DeferredHold`（延迟持有）复用生命周期计数，覆盖“已登记，尚未执行或撤销”的空窗。
  direct foreign 与自产 native archive 的 pthread_create/key_create/setspecific/key_delete
  都进入同一登记表；guest closure 和纯 native 间接 start/dtor 都覆盖。close 同步按 POSIX
  四轮上限清理当前线程 TSD，远端线程有值则保持 Closing 直到线程退出或 key 删除；删除后
  已取出的旧 void destructor 只走稳定 no-op 墓碑。构造/析构期间新建的这类工作也必须再
  清账一次，不能在第一轮归零后漏过。
- **暂停异常也属于延迟生命期**：MIRVM 异常可能被 native catch 暂停，活动 Engine 调用栈
  此时已经退去，但异常以后仍可重抛给原 owner。异常对象持有 `DeferredHold`，直到被消费、
  删除或继续抛出，防止 close 在空窗中释放其 `Shared`。每帧 cleanup 仍按 §7.52 的实际异常
  指针分类；生命周期持有不重新引入线程布尔判据。
- **长寿命宿主线程不再钉住 Engine**：每线程 `Ctx` 改由 `CtxSlot`（上下文槽）间接持有，
  Shared 只登记弱引用。Finalizing 已证明全部租约和延迟持有退出，此时可从收尾线程清空所有
  槽内 Ctx，归还 guest TLS、mimalloc 对象、1 GiB 虚拟 ByteRegion 和 `Arc<Shared>`。长期
  不退出的宿主 worker 只留一个空槽；首次再次 attach 会重建 Ctx，弱槽表也会顺手剪掉已退出
  线程的条目。
- **native 生命周期显式分阶段**：自产 `.so`/MC 先映射和重定位，保持可写以填 P1、GOT、
  pthread bridge，再封最终页权限；任何 constructor 之前所有入口已可用。生命周期是
  `Unstarted -> Starting -> Completed -> Finalized`，只对 Completed 映像在 close 时按逆序
  执行一次 fini，部分 constructor 失败不伪装成完整实例并运行 fini。ctor 收到真实
  `argc/argv/envp`，可回调 guest；它的 guest panic、`EngineFault`、`EngineClosed` 或可删除的
  foreign exception 在启动边界分类成 `Result` 失败，再走关闭协议。宿主 Rust panic 和
  不能安全删除的 foreign exception 保持原对象续传，但也必须先完成关闭。fini 则是
  **不可展开的拆除边界**：它一旦开始，无论 guest panic、`EngineFault`、`EngineClosed`、
  foreign exception 还是宿主 Rust panic 逃出，都固定诊断
  `native finalizer unwound during Engine teardown` 后 `abort`；不沿用 guest
  `Terminate` 边界中对 `EngineFault` 的续传规则，也不允许异常逃出后把 Engine 永久留在
  `Closing`。进入 constructor 之前的失败由 RAII 回收 closure、MC frame/mapping 和
  动态库句柄；一旦 constructor 可运行，地址可能已经逃逸，后续失败也走关闭墓碑
  而非冒险卸载。
- **什么按 Engine 回收，什么保留到进程结束**：close 会 join JIT worker，释放 Shared、
  Module/frozen、Ctx/guest TLS、未发布的 closure 与映像，并刷新 heat 顺序。已发布的普通/P1
  libffi closure、JIT 机器码及系统 unwinder 使用的 `.eh_frame`、committed MC 映像和自产
  动态库映像保留到进程结束。理由不是“暂时没写 drop”，而是任意 native 代码可以复制裸
  函数地址、休眠在已发布栈帧里，通用 FFI 没有枚举并撤销所有副本的协议。关闭后的 callback
  只持小型 `EngineControl` 墓碑，不再持有 Module；C-unwind 入口稳定报告 `EngineClosed`，
  普通 C 入口按不得展开的 ABI 终止。
- **不让未知期限反过来卡死 close**：只有 pthread/TSD 这类具备可观察完成或撤销事件的 API
  获得 DeferredHold。任意第三方库若无限期保存 callback，不得让 `wait_closed` 永久等待，
  而走进程期 closure + 关闭墓碑。真实 workload 撞到另一种有明确完成事件的注册 API 时，
  为该 API 补最小登记/撤销合同；若产品要求任意 native 库资源严格有界，只能采用子进程隔离
  一次性回收，不能要求使用者人工报名回调，也不能假装通用裸指针可安全卸载。
- **验证合同**：标准 pack 合同覆盖 load 后改写/删除源包、同一 Package 并发建多 Engine、
  static/TLS 与 P1 地址隔离、global_asm/C2 owner、每实例 ctor/fini、关闭旧指针和新实例不
  复用地址。Engine 单测覆盖 close/execute/reentry 竞态、same-thread wait、pthread start、
  TSD 四轮/远端退出/delete 竞态、暂停异常持有、constructor 回调/失败和长寿命宿主线程
  Ctx 释放。最终通过数字只记实际验收结果，不在本决策条目预填。
- **仍开放的边界**：E22 现在只保留 safe typed export 与进程期裸地址两件真边界；E23
  checked 指针来源、D10 进程隔离/资源治理、跨平台与格式冻结各自不变。本节不声称容器验证
  能证明 native ABI，也不声称 Engine close 可从任意第三方库手中收回裸函数指针。

### 7.54 2026-08-13：signal 改为原子登记与安全点派送；多 Engine 关闭闭合

- **推翻 M5.2 信号帧直执行**：M5.2 的 AS-trampoline 会在内核任意打断点进入 libffi、TLS
  attach 和 guest 解释/JIT。函数签名相同不等于异步信号安全：这些路径会取锁、分配、读取
  可被中断的 Engine 状态，也可能展开；被打断线程当前激活的 Engine 还未必是该 handler 的
  owner。Package v4 又使同一进程存在多 Engine 和非 LIFO close，旧实现没有进程级
  disposition 所有权、`oldact` 反译或在途 frame 关闭协议。因此 §6.1/M5.2 的“直接复用
  callback thunk”只保留为历史证据，不再代表现行机制。
- **内核帧只登记，不执行 guest**：每次 guest handler 安装都创建不可移动、进程期存活的
  `SignalRegistration` 和独有的固定 22 字节 RX 桩。桩只把 registration 地址装入寄存器并
  尾跳固定 adapter；adapter 校验 signal 后只读进程期内存并做原子操作，把实际事件计数写进
  owner `EngineControl` 的 inbox。它不查 TLS、不拿 mutex、不分配、不调用 libffi/guest，
  也不发起 unwind。允许新 frame 的 active 位与在途 frame 计数合在同一个原子字中，close
  清 active 位后不会发生“已观察为零、随后又进来一帧”的竞态。
- **普通安全点恢复完整执行语义**：解释器块入口/返回与 JIT helper 等普通执行位置排空当前
  Engine inbox；每个 handler 取得 Closing 仍允许的已登记回调租约，并建立全新 activation，
  再应用 handler 自身和 `sa_mask` 的传统 signal mask。新 activation 的 nonce/`run_main`
  状态与被中断调用分开，signal handler 内的 catch 或 nested guest 调用不能冒领外层真实 main
  catcher。handler 若展开则在 signal 的普通 C 边界终止，不会穿回内核 signal frame。若 A
  不活动而 B 正运行，A 的进程信号只进 A inbox，B 不执行它；A 下次进入安全点才派送。
- **`raise` 保持同步嵌套**：guest `HostRaise` 先在普通 VM 状态重新核对当前内核 disposition，
  对仍属 guest 的 top registration 直接取得在途计数并同步派送；不同 signal 的嵌套 handler
  在内层 `raise` 返回前完成，同 signal 因默认 mask 暂存，外层 handler 返回后、原 `raise`
  返回前继续派送。SIG_DFL/SIG_IGN 或真实 native handler 仍交给 libc。自产 native archive
  链接时只 interpose `signal`/`sigaction`/`raise` 三个符号，经隐藏 owner 槽进入同一路径；
  这不声称能看见任意第三方动态库内部的同名调用。
- **guest 视图与 kernel 视图分开**：进程 registry 为每个 signal 保存安装前原生基线及按安装
  次序排列的 Engine 节点；节点同时保存 guest-visible action 与实际 kernel stub action。
  `signal` 返回值、`sigaction` 查询和 `oldact` 因而返回 guest 原 handler/flags/mask，不泄漏
  stub。安装、同步 `raise` 和关闭都在 registry 锁内重读 kernel action；若宿主或第三方绕过
  MIRVM 改了 disposition，旧链失去所有权，后续 close 不会覆盖这次外部更改。
- **非 LIFO close 与在途 frame 有确定顺序**：关闭 Engine 会先停用它的全部 registration，
  从每条 disposition 链移除该 owner；只有被移除节点仍是当前 kernel top 时，才恢复存活的
  上一层或原生基线。随后等待所有已经通过 active 门的 frame/同步调用退出，再在 Closing 的
  普通 activation 排空已登记事件。handler 在 drain 中重新注册也会进入下一轮“停用、等待、
  drain”，直到该 Engine 不再拥有节点或 pending，才清 inbox 并进入 Finalizing。这使 A/B
  以任意顺序关闭都不会串 owner、丢掉已经登记的事件或提前释放 Shared。
- **验收与边界**：解释器和强制同步 JIT 的嵌入回归覆盖 guest 地址 `oldact`/query、A/B
  非 LIFO close 恢复原生 action、A 不活动/B 活动时只给 A 排队，以及 close/JIT 锁持有期间
  frame 只登记、解锁后的安全点才允许 handler 嵌套 guest。native 差分 probe 锁定同步嵌套
  顺序和 SIG_IGN，archive 单测锁定三符号 owner 桥；最终数字只记实际整体验收，不在此预填。
  同步故障 signal guest handler 仍按 R1 拒绝；realtime、高级 `sigaction` flags、跨线程定向
  投递与安全点延迟边界精确登记在 R21。更低延迟只能增加普通执行安全点，不能退回信号帧跑
  guest。

### 7.55 2026-08-13：线程定向 signal 进入目标 pthread；关闭与线程退出共同收口

- **推翻的旧边界**：§7.54 曾拒绝 `pthread_kill` 等线程定向信号，理由是不能在内核 signal
  frame 内进入任意时刻的 guest，又不能把“发给目标线程”改成 owner Engine 随便找一条线程
  执行。真实嵌入立即需要 `raise`、阻塞等待、跨线程 `pthread_kill` 和并发 close 共同成立，
  因此继续拒绝不是可接受的产品边界。解决办法仍然是不在 signal frame 跑 guest，但登记位置
  必须从“只有 owner inbox”扩成“进程事件归 owner、线程事件归目标 pthread”。
- **保留内核可观察语义**：受控 `HostRaise`（MIRVM 承接的 `raise`）调用真实 libc `raise`，
  不再伪造自定义排队标记。
  Linux 由此给出 `SI_TKILL`，也就是“发给当前/指定 pthread”的 `siginfo` 来源码。信号被阻塞
  时继续留在内核待决集合，`sigwaitinfo`/`rt_sigtimedwait` 可以消费并看到真实来源；不能
  为了方便识别而改成可被 guest 观察到的 `SI_QUEUE`。未阻塞时，受控 `HostRaise` 在普通执行
  状态排空本线程事件，handler 完成后才返回，包括 handler 内再次 `raise` 的嵌套顺序。
- **目标线程有自己的稳定槽**：每条已接入的 pthread 都有一个进程期 inbox；每次成功安装
  handler 都是一个 registration generation（一次注册代际）。系统在普通状态预先为
  `(pthread, 注册代际)` 链入固定 cell（槽），再允许内核看见登记桩或发布线程 inbox。内核从
  桩跳入的固定登记函数只读当前线程的 local-exec TLS（编译器固定偏移的线程局部存储）和这些
  预建槽，并做原子操作；不取锁、不分配、不调用 libffi/guest。传统信号在同一槽内按 POSIX
  语义合并，但第一次、第二次、第三次替换各有自己的槽，不会把旧 handler 的事件算到新
  handler。进程定向事件仍进入 callback owner 的 `EngineControl` inbox；`SI_TKILL` 只进入
  目标 pthread 对应的槽。
- **派送线程不能替换**：原生 `pthread_kill` 返回只表示内核接受投递，handler 在目标 pthread
  下一普通安全点运行；若目标线程正在退出，则由它自己的退出收口运行。close 会停止新登记，
  恢复正确的内核 disposition，并等待已经通过门的 frame、owner inbox 和所有目标线程 cell；
  执行关闭的线程或别的活跃线程不得代跑目标事件。这样既不在异步 frame 内进入 guest，
  也不丢掉 pthread 身份。
- **线程退出顺序**：glibc 的四轮上限是整条 pthread key 表的四次扫描，不是每个 key 各有
  四轮。`Ctx` 在第三轮结束前登记“下一轮是全局末轮”；第四轮到达 `Ctx` key 后，以原始
  pthread key 号为单调游标，只补跑尚未被 glibc 扫过的高号受管值一次。游标已经越过的低号
  key 即使被 signal handler 重设，也只清值并释放生命周期持有，不能凭空制造第五轮。收口在
  原线程 mask 下交替排空目标线程 signal cell 和仍合资格的 TSD 值；最终再由内核阻塞所有可
  捕获的传统信号，复查两边为空，关闭 inbox 并等待在途 frame，最后放弃本轮已无资格再调用
  的残值。退出前不恢复物理 mask，避免“刚判空又进一帧”；handler 仍看到原来的逻辑 mask。
- **等待不能卡住事件所属线程**：`wait_closed` 若发现当前 pthread 自己仍持有目标 Engine 的
  thread-directed cell，返回 `ActiveOnCurrentThread`，让宿主先离开等待并到下一安全点或线程
  退出收口执行 handler。检查与睡眠之间仍可能刚好到达 signal frame，因此等待使用有界超时
  反复检查，而不是只在阻塞前看一次；Engine 已 Closed 时始终优先返回成功。
- **进程期地址与陈旧桩**：固定 22 字节 stub、`SignalRegistration` 和线程 cell 都可能被
  内核或原生代码留下裸地址，因此不释放、不复用，保留到进程结束。这不是让关闭后的 owner
  继续可调用：若原生代码绕过受控入口，在 owner 已关闭后回装旧 stub，下一次裸内核投递只能
  `_exit(70)`；若同一状态由受控 `HostRaise` 发现，则在普通状态报告 `EngineFault(70)`。两条
  路都快速、明确地失败，不能重试挂住，也不能误投给后来 Engine。
- **验收与仍拒绝的面**：真实内核回归覆盖未阻塞/阻塞 `raise`、`sigwaitinfo` 来源、跨线程
  `pthread_kill` 的目标身份、registration 替换与同代合并、并发 close、TSD 最后一帧及陈旧
  stub 失败；解释器和强制同步 JIT 共用同一合同。同步故障 guest handler、realtime 逐事件
  队列、三参数 `SA_SIGINFO`、替代栈 `SA_ONSTACK`、`SA_NODEFER` 和 `SA_RESETHAND` 仍按
  R1/R21 响亮拒绝。进程定向外部事件仍只承诺 owner Engine 下一普通安全点可见，不外推原生
  handler 级即时延迟。

### 7.56 2026-08-13：日志/profile 旧稿退回审计，未批准新架构

- **旧状态**：§7.32 把“日志 v1 在外施工”和“v2 ring + 消费者线程”当作既有路线，
  并预先裁定与 cache write-behind 共用服务线程；D16 又沿用了“v1 已过 TSan”的表述。
- **触发证据**：当前仓库只有 `mirvm_log!` 宏。文本臂同步锁标准流，等级臂转发进程全局
  `log` facade；没有 mirvm 自有 backend、`MIRVM_LOG` 读取或 `trace-log` feature。真实热
  路径 `MIRVM_SYSCALL_TRACE` 每次重新查环境、格式化并同步写 `stderr`。旧设计所列
  `2 ns`/`100 ns`/`200 ns`、ring 容量和 v1/v2 状态均无产品实测支持。
- **安全性反证**：旧稿让致命 signal handler flush 普通 ring；但当前 fixed adapter 的
  现行不变量是只访问进程期稳定内存、原子和 local-exec TLS。`write(2)` 可在 signal
  handler 使用，并不使遍历 ring、等待未提交槽或读取普通 TLS 安全。现有
  `MIRVM_SEGV_DUMP` 在 signal frame 中格式化、读 `/proc` 和写文件，只能视作待替换的开发
  排障债务，不能作为新底座。
- **本次选择**：旧稿退回为“审计讨论稿”，不批准施工；诊断文本、结构化事件、聚合指标、
  采样 profile、时间线 trace 和 emergency 崩溃记录先分开建模。它们可以共享标识、decoder
  或经实测共享 writer，但不再按日志等级选传输路径，也不靠解析人类文本做 profile。
  ring 形态、丢失/阻塞政策、文件格式、后台线程归属和性能门槛仍是 open，须按讨论稿
  §12 逐项由用户裁决。
- **首个取证切片**：若基础合同批准，以 MIRVM 自己生成、解释或能安全双物化的 syscall 点
  做第一条结构化事件纵切，只交付足以验证和导出的 `inspect`/`export`。旧
  `MIRVM_SYSCALL_TRACE` 只覆盖被改写的汇编点，降为性能债务基线，不能冒充全进程 syscall
  流。完整 syscall 流另接 Linux raw-syscall tracepoint；profile 先接外部 `perf`，并在 JIT
  发布点提供真实机器码范围。现有 backtrace token ELF 不是 JIT profile 地址映射。
- **被替代范围**：§7.32 的共享服务线程和日志 v1/v2 路线降为历史候选；§7.48/D16 的性能
  入口仍有效，但不得把旧日志状态或估算当作基线。
- **第一项用户裁决（2026-08-13）**：确认诊断日志、结构化事件、聚合指标、采样 profile、
  时间线 trace 和崩溃记录是不同的数据合同。它们可以共享 session/进程/线程/Engine 标识、
  decoder，以及经实测证明合适的部分落盘设施；不得因此默认共享容量、背压或丢失语义。
  其余候选架构继续保持 open，按设计稿 §12 逐项讨论。
- **第二项用户裁决（2026-08-13）**：运行正确性和前进性高于遥测完整性。Engine 执行、
  constructor、fini、延后 handler、TSD 回调和 close 期间，任何诊断/事件/指标/trace 都不得
  等待输出端；容量耗尽可丢，但须以计数和 sequence 缺口如实暴露。真实错误由 `Result`、
  `RunOutcome` 或退出码交付，不能依赖日志落盘。CLI 只有在回到控制边界后才可同步显示
  最终诊断；嵌入执行路径不得直接调用可能锁住或重入的宿主全局 logger。
- **第三项用户裁决（2026-08-13）**：采集会话是进程级概念，不随单个 Engine 开关。进程
  启动时只自动准备极小固定的内存 emergency 槽，不创建文件或线程；普通诊断文件、事件、
  指标、trace 和 profile 默认关闭，由 CLI capture/profile 命令或进程级嵌入 API 明确开启。
  开启后当前/后续线程与 Engine 自动纳入，fork 子进程自动切新 generation 和独立文件，
  不要求调用者报名线程、Engine 或 handler。环境变量只作兼容排障入口；在真实证据证明
  emergency 槽不足前，不建立默认常驻普通事件黑匣子。
- **第四项用户裁决（2026-08-13）**：机器读取的权威原始记录使用二进制；JSONL、CSV、
  可读文本和 Perfetto 都是离线派生格式，人类诊断文本不是脚本合同。每个进程/fork 代际
  独占原始文件，不并发追加同一文件。本项不冻结 magic、header、chunk、checksum、扩展名
  或字段宽度，具体字节布局继续讨论。
- **第五项用户裁决（2026-08-13）**：原始文件采用块级恢复。只有 header、payload、commit
  footer/checksum 完整且校验通过的 chunk 才可信；普通进程崩溃后恢复此前所有可信块并舍弃
  尾部半块，signal handler 不扫描或 flush 普通 ring。正常 `End` 才精确汇总 produced、
  written、dropped 等总账；异常结束只报告可证实的 sequence 缺口和未知尾部。恢复工具不改
  原活动文件，而另存可信前缀。不为每块 `fsync`，所以不承诺内存 ring、在途块或断电时尚未
  持久化的数据；checksum 算法、chunk 大小、扩展名和刷盘周期仍未冻结。
- **讨论顺序纠偏（2026-08-13）**：用户明确否决在系统尚未实现时继续优先讨论 schema
  兼容，要求先把最高效实现细化到时间戳指令、指针传递、guest 停顿、cache line、commit、
  writer 和 profile 插桩。兼容问题延后到首个真实文件出现后；这不是兼容策略裁决。异步
  “传指针”只允许传预分配 recorder/page/slot 的稳定地址，不能把 guest/栈 payload 借给
  writer。当前按设计稿 §5.2.1 的热路径依赖树继续逐项讨论。
- **第六项用户裁决（2026-08-13，热路径第一项）**：时间戳按含义付费，而非统一读取。
  聚合计数不读时间；高频普通事件在自动 TSC 资格检查通过时使用裸 `RDTSC`，以线程 sequence
  作为权威顺序、TSC 只作近似定位；严格 timeline/span 边界使用有序 TSC 读取。生产者只存
  原始计数，不逐事件换算纳秒；控制线程采集 TSC 与 `CLOCK_MONOTONIC_RAW` 的分段校准锚点，
  离线转换。CPU/内核时钟源/跨核同步不满足资格时自动退到 vDSO
  `clock_gettime(CLOCK_MONOTONIC_RAW)`，调用者不能强制开启错误快路。当前 KVM 开发机就是
  退化案例。具体 fence 语义、`TSC_AUX`/clock epoch 和本机窄 probe 见设计稿 §5.2.2。
- **第七项用户裁决（2026-08-13，热路径第二项）**：采用普通/trace 两套执行代码，而不是
  在每个事件点动态检查 enabled。普通 JIT 保持现有纯 guest ABI，新增采集指令、TLS load、
  分支、隐藏参数和保留寄存器均为零；默认 perf profile 也不切换代码。timeline 开启后才进入
  独立 trace JIT 域，在 x86-64 用 `r15` 保存稳定的每线程 `ProducerHot*`，事件内联写页；
  普通/trace 调用槽各自闭合，边界和异常展开负责 save/set/restore，native 回调从 TLS/
  activation 重建而不信入口寄存器。解释器同样分 plain/trace 循环。尚未物化 trace JIT
  的函数先走 trace interpreter，不等待编译。详见设计稿 §5.2.3。
- **对第七项的实现复核（同日）**：撤销“线程可在下一块安全点直接切换代码域”的错误
  表述。现有块安全点没有 OSR/deopt 状态映射，不能把带活寄存器/栈槽的 plain JIT 帧迁成
  trace 帧。第七项只确认关闭态零插桩和独立代码域；动态 start/stop 对已经运行的 activation
  在哪个可重建边界生效，仍需下一项裁决，不能用一次普通安全点握手假装已经解决。
- **第八项用户裁决（2026-08-13，热路径第三项）**：普通结构化事件使用 per-pthread
  SPSC 页环。当前 pthread 独占写 current page，writer 只读 release 发布后的整页；
  producer 热状态、published tail 和 returned head 分占 cache line。页内 cursor 是线程
  私有完成标志，不做逐事件 `FREE/WRITING/COMMITTED` 原子提交；只有页发布/归还使用
  release/acquire。无空页时不等、不转、不分配、不覆盖，只推进 sequence 和本地 drop
  计数。内核 signal frame 使用独立 emergency/sampler 存储，绝不打断并复用普通页。
  进程 MPSC 不进入逐事件热路；若需要全局发布页通知，其共享成本只能在页边界摊销。页
  大小、每线程页数和低流量半页封存政策仍待后续裁决。详见设计稿 §5.3。
- **第九项用户裁决（2026-08-13，代码域生效边界）**：第一版只在最外层 guest activation
  入口选择 plain/trace，并让整条调用链（含 native 同步回调 guest）继承该域。`start()`
  先 armed，新进入者才 trace；已经运行的 plain 帧不迁移。`stop()` 只阻止新 trace root，
  已有 trace 调用链记录到返回。请求与逐线程 effective/end 分开记录，工具不得冒充完整覆盖。
  用户同时要求把“长期 guest 运行中动态开启 timeline”记为明确需求触发器：真实 workload
  一旦证明长期不返回宿主且必须中途开启/停止，就重开 OSR/可重建代码域迁移；不得以每块
  轮询作绕行。外部 sampling profile 不受此限制。
- **第十项用户裁决（2026-08-13，热页发布策略）**：hot timeline 的 active page 不设周期
  watermark 或时间 deadline，也不读取 writer flush request。只在页满、最外层 trace
  activation 返回、pthread 正常退出、stop 后 producer 已静止或显式冷控制边界封页并 release
  发布。nested activation/native 回调/deferred handler 在同一调用链内继续写当前 pthread 页。
  低事件率半页晚到是已选择的可见延迟；崩溃前未发布页仍是未知尾部。若真实长期 trace
  workload 提出最大可见延迟硬指标，再实测 K-watermark/deadline，不能预先给每事件加检查。
  该触发器与长期 plain activation 动态迁入 trace 的 OSR 触发器分立。
- **第十一项用户裁决（2026-08-13，writer 唤醒；2026-08-14 修正内存顺序）**：writer 空闲
  时使用进程级状态字和 futex 睡眠。后续审计否决了“writer 普通 store 后复查、producer
  条件 CAS”的初稿：两个 CPU 可能各自读到旧值，留下有页却睡死的 StoreLoad 竞态。正确
  基线是 writer 用 AcqRel swap 置 `SLEEPING` 后复查，producer 在整页 release 发布后用
  AcqRel swap 置 `AWAKE`；只有读到旧值 `SLEEPING` 的 producer 执行一次 `futex_wake(1)`。
  因此成本如实记为每封页一次 locked RMW，而不是普通 load；逐事件路径仍不读 writer 状态、
  不轮询、不调用 syscall。首版 writer 扫描稳定 producer 列表，只有真实大量休眠线程证明
  扫描成为瓶颈时，才评估页级 ready MPSC 队列。
- **第十二项用户裁决（2026-08-14，writer 归属）**：普通 capture 关闭时没有 writer；开启后
  每个 process generation 恰好一个独立 `mirvm-capture` writer，供全部 Engine 使用，但不与
  cache、JIT、惰性解码或 close worker 共线程。仓库当前并不存在 cache write-behind 服务；
  复用只会新造混合职责，并让不可抢占的序列化/阻塞 I/O 阻止 trace 页归还，甚至让等解码的
  guest 直接被日志拖住。writer 只排空 sealed pages、组 chunk、校验、写盘和归还页，不在线
  格式化、符号化、排序或压缩；使用普通调度且空闲 futex 睡眠。Engine close 不停止它，正常
  capture stop 才在 producer 静止后 drain/End/join。永久 sink 错误后继续消费并归还页，同时
  记 sink loss。实现必须让现有 fork 守卫自动识别 MIRVM 自己创建的服务线程，并在 child 中
  作废父代 writer/producer/fd、普通边界自动新建 generation；不得把 fork 负担转给调用者。
  具体 I/O API仍待下一项裁决。
- **第十三项用户裁决（2026-08-14，writer I/O）**：第一版由独立 writer 直接以 buffered
  `pwritev` 写 sealed producer pages，不先复制到 staging，不用 `io_uring`、文件 `mmap` 或
  在线压缩。每个 process generation 独占文件并由单 writer 维护显式 offset；writer 用固定
  `iovec[]` 机会式批量当前可见页，不等待凑批，checksum 后按 header、page 有效区、footer
  顺序写入。实现必须正确处理 `EINTR`、正数短写和 `IOV_MAX`；某页全部字节被内核接受后才
  可归还，完整 footer 被接受后 chunk 才计入 written。成功不等于 fsync，维持既定断电边界。
  staging 只有在相同总内存下确证 write 长尾导致页环耗尽时才与增加 per-thread pages 对拍；
  `io_uring` 只有单 outstanding write 吃不满仍有带宽的存储且造成 drop 时重开；`mmap` 只有
  writer 复制/syscall 已成主要扰动时重开；在线压缩只在 sink 长期字节吞吐或文件预算成为
  blocker 时评估。持续输入率超过 sink 吞吐时，上述机制都只能延迟而不能消除丢失。
- **第十四项用户裁决（2026-08-14，页容量所有权）**：否决每 pthread 固定相同页数；每种
  数据合同拥有独立的进程级硬页池，pthread 在首次 trace root 冷边界自动懒建自己的 SPSC
  ring，至少双页起步，额外页由 writer 根据实际发布率和 page-return gap 自动放入该 producer
  的 free-page queue。全局池只在控制/writer 路径管理，逐事件仍不访问；页满且无 free page
  时立即 drop，后续有页后自动恢复，不要求用户报名或救援。预算用
  `N_i >= q_i + 1 + ceil(A_i(L)/U)` 计算，并在进程硬字节预算下让各活跃 producer 覆盖尽量
  一致的 writer 停顿窗口，而非先到先占。descriptor 可保持稳定，但退出线程的 page 必须在
  TSD/signal 收口及 writer drain 后归池，不能让历史线程耗尽预算。绝对内存、页数和批次扣页
  数必须由真实 MIRVM-owned syscall event 的事件率、归还长尾和同预算 drop 曲线裁决；
  memory cap 只改变 retention/loss，不是正确性开关。
- **第十五项用户裁决（2026-08-14，logical page 等级）**：logical page 总长包含首个 64B
  header cache line。新 producer 至少以两张 4 KiB starter 开始（4032B payload）；writer 根据
  页级编码速率、真实 return gap 和硬页池预算自动补 64 KiB hot pages（65472B payload），
  producer 只在页满慢路径换 pointer，逐事件不识别等级。不能因低率线程最终填满 starter 就
  升级，`ROOT_RETURN` 半页也不触发；静止且无在途页时可自动收回 hot page。当时说明性的
  固定 64B record 容量已由第20项真实 64B/24B syscall pair 取代，page class 机制不变。16 KiB
  必须在相同总内存下做挑战基准，证据成立才加入第三等级；2 MiB 不作 logical publish page，
  DTLB 若成瓶颈只可用2 MiB backing slab承载仍为64 KiB的logical pages。page由少量普通匿名
  mmap slab切分、只需64B对齐、不依赖hugepage；record不跨页，sealed page一个iovec。NUMA
  first-touch/prefault归属仍待真实 minor-fault与远端访问对拍。
- **第十六项用户裁决（2026-08-14，syscall 采集边界）**：第一条内部 SPSC 纵切定义为
  MIRVM-owned syscall event，只覆盖 `Builtin::HostSyscall`、可双物化 inline-asm wrapper 等
  MIRVM 自己掌握的 syscall 点。plain 版本保持原 syscall 指令及 raw/libc 各自的返回与 errno
  语义，且不读 recorder/TLS；trace 版本在站点内联固定整数记录后执行同一 syscall。旧路径
  只作债务基线：本机 release 窄基准约为裸 syscall 332 cycles、slot 间接 call 至理想
  `syscall; ret` leaf 384、旧
  TRACE-off 588、TRACE-on 且 `stderr=/dev/null` 16,828；其栈/red-zone、YMM/ZMM 上半部和 raw
  errno 语义也不能作为新底座。stateful global/native、opaque archive 与 libc 内部等全进程
  syscall 只能在内核边界完整观察，另由 Linux raw-syscall tracepoint 写独立 stream；能力或
  权限不可用时响亮报告，不把局部内部事件冒充完整流。两份流各有容量、sequence 和 loss
  账本，离线合并。
- **第十七项用户裁决（2026-08-15，syscall 返回记录）**：所有正常返回的 MIRVM-owned
  syscall 都分别提交调用前的 `SyscallEnter` 和返回后的独立 `SyscallExit`；禁止预留一条记录
  并跨 syscall 保持未完成。`exec`、`exit`、崩溃等不返回情形只有 Enter 合法；任一侧出现
  sequence 缺口时，工具只能报告不完整 span，不得猜返回值或耗时。raw Exit 保存原始 `RAX`；
  libc 语义 Exit 在任何记录操作前保存返回值和 `errno`，且不得改变 guest 最终观察到的
  `errno`。Enter-only 只作为 benchmark 对照，用来单独计量 Exit 的额外时间读取、容量判断、
  stores 和页发布成本，不是正式采集模式。
- **第十八项用户裁决（2026-08-15，syscall 配对）**：Enter/Exit 不写显式 `call_id`。
  decoder 按 `(process generation, thread generation)` 和 event sequence 维护未闭合 syscall
  栈；同线程嵌套时，Exit 关闭最近的 Enter。显式 ID 会占两条记录的字段，raw syscall 站点
  还必须跨调用保存它，而该路径没有免费寄存器。任一 sequence 缺口、fork generation 切换、
  session 结尾或异常未知尾部都立即截断当前栈；未闭合 Enter 与孤立 Exit 只报告
  `incomplete`，不得按 syscall 编号、参数或邻近时间推测配对。
- **第十九项用户裁决（2026-08-15，内部 syscall 时间）**：首版 MIRVM-owned
  `SyscallEnter`/`SyscallExit` 是无逐事件时间戳的因果/结果流，以 sequence、参数、result 和
  errno 语义为权威。严格 span 的两次有序 TSC 读取在当前窄 probe 中已约需 108–117 cycles；
  当前 KVM 又不满足 TSC 快路资格，而 vDSO 是普通 SysV 调用，插入任意 raw syscall 站点会
  触碰 guest 栈并要求保存 caller-saved GPR 和完整向量状态。不得为统一字段重建旧 trampoline。
  duration 只由独立 Linux raw-syscall stream 提供，并且只在两份流均无相关 loss、按 tid/
  顺序/syscall 身份可无歧义关联时生成；内核流不可用或关联失败时明确报告不可用，不猜测。
  将来若真实问题要求带 Engine 归因的 guest-observed latency，作为独立严格 timeline 合同
  重开，而不向这条结构化事件热路补 vDSO。
- **第二十项用户裁决（2026-08-15，syscall 记录宽度）**：使用 syscall 专用的两种定长
  编码：Enter 64B（8B control、8B number、六个 8B 参数），Exit 24B（8B control、8B
  result、8B errno/status），每个正常 pair 共 88B。control 自描述 kind/length/raw-libc 语义；
  sequence 由页头起点和记录次序推导，不逐条写。raw 站点不能用会改变 RFLAGS 的普通容量
  compare，因此冷路径设置 `pair_budget=floor(remaining/88)`；Enter 只用
  `mov/jrcxz/lea` 扣一个额度并提交完整 64B，Exit 借已保证的24B直接提交，不再比较。额度不
  创建半条 Exit；不返回只浪费容量。writer 只写 page header + `used_bytes`，页尾和旧内容不
  落盘。相对统一64B Exit，pair bytes与11个qword stores均减少31.25%；32B Exit只作同预算
  对拍，除非实测对齐收益胜过持续多写8B，否则不采用。
- **第二十一项用户裁决（2026-08-16，Engine 归因）**：`engine_id` 逻辑上属于每条事件，
  物理上不逐条重复。page header 快照该页第一条记录处的实际 Engine（`0`=无 Engine）；同一
  pthread 的实际 Engine 改变时，activation 冷边界写16B `EngineContext(control,id)`，A→B→A
  依次写B、A，同Engine递归不写。页尾不足先滚页，新header直接写实际id。marker和header均
  无法提交时进入 `context_unsynced`：sequence/context-loss继续计数、所有payload drop、
  `pair_budget`不可用，直到按runtime实际当前Engine成功写新header/marker才恢复；不得沿用旧
  id误记。fork child丢弃父归因状态，新generation重建。decoder遇sequence缺口时同时清空
  syscall配对栈与Engine归因，直到下一页header恢复。否决逐事件多写8B，也否决per-Engine
  ring导致的页膨胀和同pthread全序分裂。
- **第二十二项用户裁决（2026-08-16，errno 热路）**：每个 pthread 只在 capture producer
  attach 的冷边界调用一次 `__errno_location()`，把 `errno_ptr` 缓存在仅由该线程读取的
  producer cache line；未开启 capture 时没有该调用。`HostSyscall` 返回后先保留 result，只有
  `result == -1` 才从缓存指针读取一次 errno，并编码 `errno_valid=1`；成功 Exit 不读 errno，
  固定编码 invalid/0，raw Exit 则始终保存原始 `RAX` 且不碰 libc errno。producer 的逐事件、
  页满、drop、封页和唤醒路径只准使用普通内存、原子和不设置 errno 的 raw futex leaf，禁止
  libc helper，因此全路径 errno-transparent，不为每条 syscall 增加保存/恢复 store。不得每次
  调 `__errno_location()`，也不得硬编码 glibc 的 `FS:` 私有偏移。writer 不解引用该指针；
  fork child 废弃父 generation producer，并在 child 的新 producer 冷边界重新取得指针。
- **第二十三项用户裁决（2026-08-16，Enter drop 后的返回路径）**：Enter 在滚页后仍无法
  提交时，本次调用锁定为 `unrecorded-pair`。producer 先为未写出的 Enter 推进一次
  sequence/drop，再执行原 syscall；只有在同一 process generation 正常返回时，才为被抑制的
  Exit 再推进一次 sequence/drop，随后直接返回。返回点不得因阻塞期间已有 free page 而重新
  取页、封页、唤醒 writer 或写孤立 Exit；采集只从下一次 Enter 恢复。内核 signal frame 不写
  普通页，受管延后 handler 在 Exit 后才运行，因此首版 producer 不为不存在的 syscall 嵌套
  增加 `open_depth/tainted_page`。nonreturn 只有一次 Enter drop；失败 exec 等正常返回有第二次
  drop。fork 父按正常返回处理，child 先切换 process generation，绝不触碰复制来的父
  producer、sequence 或 Exit drop。raw 成功/失败使用两条生成式控制流，不跨 syscall 保存
  bool，计数与跳转必须保持 guest RFLAGS。专门的 `HostFork` 只走 fork 生命周期协议，
  `SYS_rt_sigreturn` plain bypass；二者不冒充首版 direct pair。
- **第二十四项用户裁决（2026-08-17，`ProducerFast` 热线）**：`r15` 指向稳定的
  `ProducerHot`，其首个独占64B cache line固定为 `ProducerFast { cursor:u64,
  pair_budget:u64, errno_ptr:u64, reserved:[u8;40] }`。健康 Enter只读写前两项，raw Exit只更新
  cursor，libc Exit只在失败时再读errno指针；剩余40B保留，不得塞入writer/session共享状态。
  control由代码点immediate给出，Engine从page header/16B marker继承；sequence、drop、page、
  end、fork、session和stop全在页头或producer冷线，writer在producer活跃时永不读fast/cold
  两线。marker只写在完整pair之间，成功后按`floor((payload_end-cursor)/88)`重算budget。封页
  设payload used为U、marker数为M，必须验证`U>=16M`及`(U-16M)%88==0`，再算
  `P=(U-16M)/88`、`record_count=2P+M`，由page first_sequence推进next_sequence；健康syscall
  不逐条改sequence/drop。marker也占sequence，无法提交且无法开新页时在冷账本记一次drop并
  进入context-unsynced；无页的正常返回pair在冷线一次增加两个sequence/drop。
- **第二十五项用户裁决（2026-08-18，剩余设计整体转施工）**：用户不再逐项问答，批准
  `mirvm_high_performance_log.md` §12.1 的首版默认包整体成为施工合同；§12.2 的页池数字、
  page class阈值、24/32B Exit、writer批量/checksum、NUMA、ready MPSC、I/O候选和正式性能门
  必须由首个真实纵切自动裁判，不许人工先填；§12.3 的schema长期兼容、完整kernel syscall
  流、一般timeline/Perfetto、OSR动态启停、signal解释栈采样、rotation/durability、未触发I/O
  优化和完整epoch回收明确延期。施工按1A `HostSyscall + recorder/writer/v0 decoder +
  inspect/export`、1B stateless inline-asm raw site分片；1A完成不得冒充全部覆盖。
- **第二十六项用户裁决（2026-08-18，日志与 profile 余项进入显式队列）**：1A 参考纵切
  落地后，用户要求全局硬页池/自动救援/retire、fork child 新代际、HostSyscall 直接热路、
  trace JIT `r15`、1B raw site、4 KiB→64 KiB 自适应和 perf-map/profile 全部提上日程，不能
  继续只写“后续”或“延期”。现按 L1 生命周期 → L2 fork → L3 HostSyscall 热路 → L4 raw
  site 排主线；P1 JIT 地址登记 → P2 perf capture 在 L1 后并行；最后以相同内存预算的数据
  裁决自动伸缩和 writer 参数。§12.3 仍不阻塞首版，但每项改绑明确进入条件，不作无限期
  搁置。该裁决不改变“性能数字只能由真实 workload 决定”和基建够用即冻结的纪律。
- **第二十七项用户裁决（2026-08-18，诊断来源分层但默认 fd2 不变）**：代码审计确认
  `run_compiler` 先输出 compiler/frontend/lower 诊断，runner 只窄删额外 warning-count
  summary，compiler 成功后 guest 才继承同一 fd2 运行；MIRVM control 也写该 fd。因此当前
  warning 与运行期 stderr 共用 fd2、按实际发生顺序出现，符合默认 `mirvm run` 的 Cargo
  语义，不应为“看起来混在一起”而改写顺序。新增 D0 `DiagnosticRouter` 当时待施（现已由
  §7.58 闭合）：内部区分
  compiler/lower、MIRVM control、guest stderr；capture/profile 自动把前两类逐字节 tee 到
  独立 diagnostics stream，guest fd2 绝不进入 router 或普通事件 ring。direct、cargoless、
  Cargo runner 必须一次覆盖，故本轮只排期，不半实现单一路径。

### 7.57 2026-08-19：日志 L1 页与线程生命周期闭合

- **旧状态**：1A 为每个 producer 固定分配双 4 KiB 页；预算耗尽时不能注册 page-less
  producer，retired descriptor 永久留在 writer 扫描表，writer 一轮会清空单个 producer 的
  全部已发布页。该形态只证明文件链正确，未兑现 §7.56 第二十六项的 L1 合同。
- **实现选择**：每个 capture session 建一个按字节硬封顶的进程页池，首版池元素固定为双
  4 KiB starter。拿不到页仍创建 producer；无 active page 的每次新 Enter 只做一次 acquire
  检查 writer offer，失败立即 drop。writer 排空 retired producer 后以单遍 retain 从活跃表
  摘除并归页，历史 descriptor 只留在终结账本；每轮每 producer 最多消费一页。会话结束在
  active root 归零、所有页已排空且所有 producer `has_active=false` 后回收全部页内存。
- **所有权证据**：新增零预算 attach、退线程归页后自动恢复、2,048 短命线程扫描表归零、
  双 producer `A/B/A/B` 页序和 writer offer/producer retire 同轮竞态回归。TSan 独立 crate
  新增真实 arm session 的 publish/return/retire/rescue case，零竞争告警；既有 Engine capture
  解释/JIT 纵切与 telemetry 27 项也通过。
- **未外推边界**：公开入口当前 64 MiB 只是首轮实现默认值；producer 仍固定双 4 KiB 页。
  4/16/64 KiB、自动晋升/回收、最终硬池数字与 writer 后续批量仍由 §12.2 数据裁决，L2 fork、
  L3 热路、L4 raw site 和 P1/P2 均未因 L1 完成而自动完成。

### 7.58 2026-08-19：P1 JIT 地址登记与 D0 诊断通道闭合

- **P1 最终机制**：`JitSymbolRange` 分开登记 fast body、guarded、packed、c2i，
  显示名带 Engine/Func/role。每个编译请求在局部累积 symbol batch；finalize 成功后
  先把整批登记到 per-Engine 视图和进程内存 registry，再 Release 发布调用 slot。
  失败请求丢弃局部批次，不能留下未发布范围。
- **P1 I/O 与会话截断**：JIT register 只做内存 push，不读写 perf-map。`install`
  在 registry mutex 外以 no-replace 创建空 map；显式 `stop` 在锁内先切成
  `Inactive` 并快照全部已登记范围，然后在锁外批量 write/flush。截断点之后才
  register 的范围归下一 session，慢或失败的 map I/O 不再拖住 JIT worker/teardown。
- **P1 闭合证据**：确定性失败注入证明失败 batch 不泄漏到下一 compile；JIT 定向
  15/15、check、Clippy `-D warnings` 与 embedding exact 全部通过。
- **P1 未外推边界**：当前只有地址登记与 perf-map 冷路 API，没有 `mirvm profile capture`、
  perf 权限/lost sample 裁判或脚本；这些仍属 P2。fork child 不能继承父 registry/file 状态，
  重置与新代际由 L2/P2 一起闭合。
- **D0 最终机制**：默认 run 不启用 router，compiler/frontend/lower、MIRVM control
  与 guest stderr 继续物理共用 fd2，原始字节、顺序、stdout 和退出码不变。
  capture 在 command boundary 就 arm `DiagnosticRouter`：rustc emitter 与 MIRVM control 同时
  写原 fd2 并逐字节 tee 到独立 `diagnostics.log`；child attached marker 让 runner
  继承已建立路由而不重复包装。guest 仍直接写 fd2，绝不进入 router 或普通
  事件 ring。正常路径由 atexit 收口后 no-replace 发布，异常结束保留 partial。
- **D0 闭合证据**：direct、cargoless 与 Cargo runner 三路分别对拍 plain/capture；
  compiler warning 和 control error 被收录，含 NUL、非 UTF-8、ANSI 和同文 warning 的
  guest 单次 write 被完整排除。表驱动合同另覆盖 stack/jit bad、unknown arg、
  missing input（含无换行 usage bytes）、invalid `MIRVM_DEPS`、单文件 `--bin`、
  missing source 与 forwarded runner fake-binary 解析错误。plain/capture 的 stdout、stderr、
  exit 与 diagnostics 均逐字节断言，`runtime.diagnostics` 31/31 通过。

### 7.59 2026-09-18：日志 L2 第一片——服务线程登记与 fork 基线自愈

- **本片目标**：L2（fork 子代独立代际）的实现前置。设计 §5.5/§6.3 明确要求「实现必须由
  MIRVM 的进程级服务生命周期自动登记自身线程，让 fork 守卫区分运行时服务线程和
  guest/宿主并发线程；不得要求调用者避开 fork 或手调基线」。本片只做这个前置，代际重建
  仍未实现。
- **旧状态的实测缺陷**：`guest_spawned_threads` 直接把 `/proc/self/task` 计数与
  guest main 启动时钉的基线比较。capture 会话在 guest 启动之后开启时，writer 线程会把
  计数抬高 1，于是此后**任何** fork 都被误判为「guest 已多线程」并响亮拒绝。这不是
  理论风险：basline 钉点（`interp/mod.rs:591/676`）与 session 建立点（`cli.rs` capture
  命令）没有固定先后。
- **本片机制**：新增 `ServiceThreadGuard`（`src/os/linux/thread.rs`）——register/drop
  维护一个进程级原子计数；capture writer 在 `writer_main` 首行注册。fork 守卫改用它：
  `guest_thread_count() = os_thread_count() - service_thread_count()`，用 `saturating_sub`
  保证账目出错时只少算、不回绕。注册发生在 `std::thread::spawn` 返回之前，所以守卫
  不可能观察到「线程已存在但未登记」；`pthread_create` 在持有 libc 全局锁时读
  `/proc/self/task`，也不会与创建竞争。
- **顺带修出的第二个缺陷**：fork 子进程继承父代的基线和 pid，但没有继承父代的服务线程。
  子进程里服务计数归零、基线偏高，导致**子进程再 fork 会被误拒**。修法是基线自愈：
  `Shared` 增加 `fork_baseline_pid`；`guest_thread_count_for(&Shared)` 发现 pid 与钉基线
  时不一致就重算并重钉。只在钉点和这里写这两个字段，因此读-比较-写落在单线程的子进程里。
  `after_fork_child()` 另外调用 `reset_service_threads_after_fork()` 把服务计数归零。
- **本片机制（二）代际身份**：`PROCESS_GENERATION` 是进程期稳定计数，`after_fork_child` 置
  `GENERATION_PENDING`；子进程的**第一个**会话把代际推进一位，之后的会话（同一进程）复用该值。
  文件头的 `process_generation` 不再硬编码 0，改由 `claim_process_generation()` 提供，因此父子
  并发写出的文件不能在离线侧被读成同一条流（header 里同时带各自 pid）。
- **闭合证据**：`cargo fmt --check` 干净；`cargo clippy --locked --all-features -D warnings`
  0 错；`cargo test --locked --all-features` **387/387**。新增两条单测：
  `service_threads_are_excluded_from_the_guest_fork_baseline`（只断言与进程实际线程数无关的
  不变量，因为并行测试的 capture writer 也会注册服务线程——第一版断言绝对值，在并行下确实
  被抓出 flake，已改）与 `forked_child_advances_the_process_generation_once`（真 fork：子进程
  第一个会话取下一号、第二个复用同号，且子进程的值不回流父进程）。后者顺带钉住一个事实：
  测试必须走产品 fork 路径（`host_syscall(SYS_fork)`），裸 `libc::fork()` 不经过 MIRVM 包装，
  因此不会触发 child hook。
- **本片机制（三）文件名与文件头用同一个代际**：CLI 之前把输出名硬编码成
  `events-<pid>-0.mlog`。现在代际在**命名之前**一次确定：CLI 用
  `capture::claim_process_generation()` 取名并把同一个值通过
  `CaptureOptions::with_process_generation` 交给会话，`StartOptions` 只在调用者
  不关心命名时才自行 claim。这样父进程 `events-<pid>-0.mlog`、子进程
  `events-<childpid>-1.mlog`，文件名与 header 不会不一致（实测：父 0，解码头
  `process_generation = "0"`）。
- **本片机制（四）fork 安全的重建配方**：子代要建自己的会话，但**不许**碰父代的 session core、
  页池、writer 线程或 fd，只能读派生内存里稳定不变的部分。因此父代把
  `RebuildRecipe { directory, page_budget_bytes }` 泄漏到进程结束，并把地址存进原子；
  fork 复制地址空间时该指针已在，子代读取它既不需要分配器也不需要锁。
  `publish_rebuild_recipe` 在会话可见之前发布（首个 Engine 一起来就 fork 也覆盖得到）；
  `request_stop` 撤回发布（owner 停会话后不再有新子代期待重建）。文件名是**推导**而非存储：
  子代在自己的 pid 与代际上生成 `events-<child pid>-<child generation>.mlog`，落在父代输出
  目录里。
- **闭合证据（配方片）**：`cargo fmt --check`、`clippy -D warnings` 干净；`cargo test` **389/389**
  连跑三次。两条新单测：配方可发布/读取/撤回；以及配方内存**经真实产品路径 fork**
  （`host_syscall(SYS_fork)`）后在子进程里仍读到父代的值。第一版把全局配方当作断言对象，
  被并行测试的 `request_stop` 清掉而 flake——已改为由父代在 fork 前取地址、传给孩子断言，
  测的是派生内存而不是全局指针。
- **本片未兑现（仍属 L2）**：重建的**消费者**还没接——子代尚未自动建立自己的文件、页池、
  writer、producer 和 errno pointer，`after_fork_child` 仍只把父代 producer 与缓存置空。
  已定下但**尚未实现**的收尾协议：子代没有 `finish()` 调用点，其孤儿会话应在进程退出时由
  进程级收尾停会话、唤醒 writer 并有界 join，写出正常 `End`（设计 §6.2 明确不依赖 TSD
  flush）。完成前子代仍是 drop-only，不得写成 L2 已闭合。P2 的 `mirvm profile capture`
  仍未实现。

### 7.60 2026-09-18：L2 复核——子代重建可用，缺口收窄到"子进程事件落盘"

- **推翻本会话两次错误结论，均因探针不合法**：
  ① 我曾判定"子代 fork 后重建不可达"——实际用仓库自己的 `demo/fork_exec_probe.rs` 跑 capture，
  子文件确实生成（`events-<child pid>-1.mlog.partial`，`process_generation`=1、pid=子进程，
  `inspect` 仅报 `SessionEnd is absent`，即设计对 `.partial` 的定义）。
  ② 我又曾判定"capture 对真实程序零事件"——实际并不成立：带 `libc::syscall(...)` 的探针得到
  `pages: 1 / records: 4`。真实边界是：**只有变参 `syscall` 符号被注册为记录型 builtin**
  （`lower/builtins.rs:49` → `HostSyscall`），`std::fs`/`Command` 走各自的 builtin
  （HostWrite/HostFork）**本就不属于这条事件流**。设计 §2.3 把"拦截一切 syscall"拆成多形态，
  完整覆盖是后续工作，不是当前缺陷。
- **本轮真正的修复**：`host_syscall` 只在 `TLS_ACTIVE_PRODUCER` 非空时记录，而该 producer 只在
  `activation_enter`（进 Engine 时）创建；fork 子进程在**已进入的 Engine 内**继续跑，永远不再
  进 Engine，于是永远没有 producer，子进程每个 syscall 都走"空 producer 直通"路径。
  `rebuild_on_boundary` 现在在重建子会话后**当场挂上属于新会话的 producer**（复用
  `activation_enter` 的同款序列：`producer_for_session` → `current_engine` → `open_page` →
  TLS 存储）。判据 trace 证实修复生效：子进程出现 `attach ... producer_null=false` 及随后的
  `record`/`recording`，修复前一条都没有。
- **收口（同轮完成）**：子进程的页落盘缺在退出收尾——`drain_lingering_writer` 只停会话和唤醒
  writer，**从不封存子进程 producer 的活动页**，因此没有任何 page 交给 writer，文件停在
  `chunks: 0`。修法：收尾里先对 `active_producers` 逐个 `seal_page`（它自身就完成发布 + 唤醒），
  再切 `PHASE_STOPPING` 并做有界等待。**实测收口**：父 `events-<pid>-0.mlog`（generation 0、
  2 条记录、`status: clean`）与子 `events-<child pid>-1.mlog`（generation 1、4 条记录、
  `status: clean`）并存；`_exit` 路径仍如实保留可恢复的 `.partial`。
- **gate 工件**：新增 `runtime.telemetry` 套件（`tests/suites/runtime/telemetry.sh` + 探针
  `tests/fixtures/telemetry_fork_child.rs`），已注册进 `fast` 与 `gate`；断言 8 条：父/子各有
  自己的文件、父 generation 0 / 子 generation 1、子文件带子 pid、两侧各有自己的 committed
  记录。`harness.truth` 的假入口白名单同步更新，自检保持 16/16。
- **L2 状态**：**闭合**。服务线程登记、fork 基线自愈、代际身份、命名一致、fork 安全配方、
  裸 `SYS_fork` 覆盖、builtin 边界 hook、子进程 producer 挂载、退出封页发布共九片全部落地，
  证据为上述套件与 389 条单测。
- **仍属后续（非 L2）**：采集目前只记录变参 `libc::syscall` 形态；`std::fs`/`Command` 走各自
  builtin，不在该事件流内，完整 syscall 覆盖是设计 §2.3 的后续工作。

## 8. 尚未兑现或需要重新验证的架构承诺

> **2026-07-22 收束**：本清单多条已被后续兑现或推翻——方法级 JIT
> （M5.3–M5.5 已落地，§7.19/§7.20/§7.21）、constructor 分治放行（§7.8）、
> RTLD_DEFAULT 重名归档句柄优先（`fb0b204`）——维持原文备查，当前结论
> 以 §7.21 与各专项条目为准。

- ~~P7 设想独立 `src/os/` 物理层~~（**2026-07-18/19 已兑现**：`src/os/` + `src/arch/` 双 leaf 建成，E21 闭合，见 §7.16）。
- ~~“engine 是 library”目前只是 crate 结构~~（**§7.51-§7.53 已兑现真实嵌入**）：
  Package v4、结构化 main 结果、每 Engine 地址隔离与 close/wait 生命周期均已存在。公开安全面
  只有 package 校验和既有 Engine 操作；instantiate/raw export 的 native ABI 信任仍为
  `unsafe`。进程期函数地址与 safe typed binding 的剩余边界精确归 E22。
- `.mirvm` mode B 与方法级 JIT 已实现；当前包格式 v4 仍未冻结且精确绑定 build_id/target。
  v4 保留逐函数惰性驻留和热序，但为 safe `Package::load` 使用 owned snapshot，仍须逐函数
  临时解码完成语义验证。档案直接借用验证、fat target artifact 与 checked 模式仍只是设计
  或余项，不是已完成能力。alloca 局部已由 §7.49 撤销为必做承诺。
- static `.a`→`.so` 的受约束 Linux/ELF 切片已实现；每实例 `.init_array`/`.fini_array`
  生命周期和 archive-first 符号解析已经支持。非 PIC、跨 archive 依赖/顺序或重名、thin、
  export-symbols 与裸 `.init`/`.fini` 仍是明确拒绝面，需要新 link plan，不能从现有单 archive
  路径外推通用。

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
