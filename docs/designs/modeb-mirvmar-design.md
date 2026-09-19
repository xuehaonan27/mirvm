# mode B：`.mirvm` 包格式 v4 + pack/run/MC 机器码节/多 Engine

> 状态：**已实现；2026-08-13 完成可重复实例化与真实嵌入闭环**。母案：
> [distribution-design.md](distribution-design.md)
> D9b（包 = L2 engine-IR 缓存的可移植化：版本头/校验/重定位段）；
> 新增输入：git 历史（机器码节 = 第三类
> 内容物的干净归宿）与 §7.23（C4 片①数据源——dep global_asm 清单 .so 已入
> required_native_libs 链）。**包必须 honestly 自包含**：除 FFI 真外国库
> （glibc 类）外，运行期不读任何 `$HOME/.mirvm` 与 rustc/cargo 痕迹。
>
> **格式稳定性声明（2026-07-23 用户裁定）**：本格式**当前不定死**——
> 随 mode B 后续开发（片③机器码节、D15、C12 评审）可以变动；fmt_ver
> 只作同代区分，不承诺跨版本兼容。对外冻结是 D4 的独立评审，届时另行
> 定稿并写明迁移规则。

> v2 更正：v1 只把 global_asm 字节放入 MC，static archive 物化出的 `.so` 仍靠
> 原缓存路径，且默认校验源码时间戳和编译环境，不能诚实称为可分发单文件。v2 将
> 所有自产库字节纳入 NATIVELIBS，运行时按内容哈希自动物化；STAMPS/envs 只作来源记录。
>
> v3 更正：v2 用 `fs::read` 复制整包并一次 postcard 解码整个 Module，所有函数体会常驻。
> v3 改为只读 mmap，MODULE 只保存非函数元数据，FUNCS 保存逐函数索引和独立函数体。
> 运行时按实际访问惰性驻留并记录热序。E20 完整语义验证目前仍在首次装载时逐函数临时
> 解码一次；这部分对象立即释放，后续执行再从 mmap 按需解码。
>
> v4 更正：固定地址只保留为 artifact 内的逻辑地址（`LinkAddr`），不再作为运行地址。
> `Package::load` 一次复制并校验不可变字节快照；每次 `unsafe Package::instantiate`
> 独立映射 frozen/TLS、物化独有 P1（交给原生代码调用的 guest 函数入口）closure，并用
> `LoadMap` 与 `FrozenReloc`
> 统一修补字节码、静态指针、入口、GOT 和 native bridge。一个 `Package` 可并发创建
> 多个 Engine；源文件在 load 后被改写或删除不影响已加载对象。

## 1. 目标 / 非目标

**目标**：
- 单文件包格式 v4 + `mirvm pack`（cargo 项目/脚本 → `.mirvm`）+
  `mirvm run x.mirvm`（校验、装载、执行）。
- 三维验收：pack+run 与 `mirvm run` 直接跑逐字节一致（eco / faer /
  wasmtime 三负载），拒绝探针（陈旧/缺库/基址冲突/版本错配）响亮。

**非目标（另案登记，不预支）**：
- C12 跨 mirvm 版本的字节码兼容（当前仍要求 build_id 精确相等）；
- fat artifact 多 target（节表已预留多 MODULE 位）;
- static archive `.so` 的无文件进程内装载（当前从包内自动物化后 dlopen）；
- L3 JIT 机器码缓存（D5 禁令面，与包无关）。

## 2. 包格式 v4（容器）

```
offset 0   magic        8B   "MIRVMAR\0"
           fmt_ver      u32  = 4
           build_id     u32 长度 + UTF-8（MIRVM_BUILD_ID，精确匹配）
           section_cnt  u32
节表 ×N    tag u32 | off u64 | len u64 | fnv1a-128（节内容哈希）
尾         whole_hash   u128 fnv1a（除本字段外全文件）
```

节（未知 tag → 跳过（前向兼容）；缺必需 tag → 拒绝）：

| tag | 必需 | 内容 |
|---|---|---|
| META | ✓ | postcard：`{args, envs, base_key, target_triple, 是否含 BASE}`（= L2 Header 元信息） |
| STAMPS | ✓ | postcard：`Vec<(path,size,mtime_ns)>`，只作构建来源记录，不参与运行许可 |
| BASE | 保留 | 当前写入器不产出，装载器拒绝 BASE/delta 形态；不能把预留 tag 当成已支持 |
| MODULE | ✓ | postcard 模块元数据：exports、frozen snapshot、`link_fn_addrs`、`FrozenReloc`、TLS、asm/GOT/P1 entry 配方等；不含运行地址与函数体 |
| NATIVELIBS | ✓ | postcard：`Vec<{path, role, fnv128, bytes}>`；所有自产 `.so` 字节都在包内，旧 path 只用于与 MODULE 互证及诊断 |
| RELOC | ✓ | postcard：`{requires_fixed_base: bool, entry: Box<str>}`（固定基要求 + 入口符号；argv 经 run 转发） |
| MC | 可选 | global_asm/dep_asm 的完整 ELF 字节；由进程内 mcload 解析、重定位和注册符号 |
| FUNCS | ✓ | `count u32`；固定索引项 `offset u64 | len u64 | fnv1a-128`；随后连续保存各函数的 postcard `FuncBody` |

校验语义（refuse-loud，**绝不静默重建**——包是分发物不是缓存）：
- fmt_ver / build_id 不匹配 → 拒绝并指出重打工具链；
- whole_hash / 节 hash 不符 → 拒绝（损坏）;
- build target 与当前平台不符 → 拒绝；STAMPS/envs 不参与运行校验；
- NATIVELIBS 与 MODULE 的路径顺序必须完全互证，role 合法，内嵌 bytes 的 fnv128
  必须匹配；MC 还要与 role=global_asm 的条目互证；
- 节数量必须能被剩余节表容纳；所有 offset/len 做 checked 范围换算，节不得越界、
  重叠或复用 tag；每个节都校验哈希，包括未知 tag；
- FUNCS 的数量、表长度、每个函数范围和重叠关系做 checked 校验；装载时和每次需求解码
  都重算逐函数哈希，损坏不得进入执行；
- v4 若仍声明 `requires_fixed_base` → 拒绝；运行实例必须动态映射；
- 严格地址表中，每个不落在 frozen 范围的 guest fn 地址必须恰有一个同 FuncId 的
  P1 entry 配方；缺失、重复、与 frozen 地址重叠均在 safe load 阶段拒绝。

## 3. pack 流程（`mirvm pack <target> [-o out.mirvm]`）

与 `mirvm run` **同一条管线**到 Module 为止，岔口只改"执行 → 落盘"：

1. **cargo 项目**：走 cargo_shim 全量构建（dep 照常 metadata-only + C4 清单），
   lower 时**强制全量冷降低**（旁路 deps-image/底座分层，保证单模块自包含；
   pack 是构建动作，秒级冷降低可付）。当前不写 BASE，MODULE 保存完整模块元数据，
   FUNCS 独立保存所有函数体。
2. **脚本/单文件**：同 1 的单文件变体（frontmatter 依赖同 cargo 路径）。
3. 先对内存 Module 做 E20 全量验证；再收集 META/STAMPS/NATIVELIBS（从
   module.required_native_libs 展开、读取每个
   自产库字节、现场计算 fnv128、判别 global_asm 与 static_archive）、RELOC；
   global_asm 默认同时进入 MC，并逐函数编码 FUNCS。最后以临时名 + rename 原子发布。

## 4. run 流程（`mirvm run x.mirvm [-- args]`）

1. magic 嗅探（前 8 字节）→ 非包走既有路径；读取一次并取得进程自有的不可变字节快照；
2. 在该快照上做全文件 whole_hash + 节 hash 校验；后续惰性解码不再读取源 inode；
3. META：build_id / fmt_ver / target 校验；STAMPS/envs 只反序列化验证格式；
4. 从 MODULE 恢复非函数元数据，解析 FUNCS 固定索引。为保持 E20，在任何 MC/native
   物化前逐函数临时解码并完成语义验证，随后释放临时对象；执行期的 FuncTable 仍绑定
   owned snapshot，函数第一次被访问时才解码并常驻。每次实例化独立恢复动态 frozen，
   先建立全部 P1 `LinkAddr → closure` 映射，再应用 frozen 指针重定位；自产 archive
   为每个 Engine 建独立 `.so` 映像，MC 也每实例独立装载。GOT、TLS 模板、Static、
   AddrImm 和 entry 都通过同一 LoadMap 解析；
5. RELOC.entry 启动（main 启动链；argv 转发;`--vm-call` 语义不随包）。

函数访问会自动记入本次真实顺序。Module 释放时以 FUNCS 节内容哈希为键，原子写到
`$MIRVM_HOME/package-heat/<hash>.order`；下次装载由一个 `mirvm-decode` worker 按该顺序
后台预取。需求任务永远先于预测任务；若预测中的函数突然被需求访问，会被提升到需求队列。
等待上界是当前正在解码的一项加该需求自身，预测错误只影响速度，不改变语义。

## 5. MC（已实现）

- 内容：每个 global_asm/dep_asm ELF 一条 `{fnv128, bytes}`，与 NATIVELIBS
  role=1 条目互证；相同内容只入一次。
- 装载契约：mcload 在进程内解析 ELF、映射/重定位、注册符号与 eh_frame；
  FFI 真外国库（glibc 类）仍按系统 ABI 查找。
- global_asm 与 C2 archive 内对 guest fn 的调用不再烤固定 P1 地址。P1 的白话含义是
  “交给原生代码调用的 guest 函数入口”：机器码只跳到
  RIP 相对的隐藏 8 字节槽，实例化时写入本 Engine 的 closure。自产 archive 用
  `-Bsymbolic` 固定内部绑定，并为每个 Engine 复制成唯一映像，旧映像与 closure
  都不回收复用，因此关闭后的旧指针不会因地址重新分配而误指向新 Engine。
- MC 缺省或 `MIRVM_PACK_NO_MC=1` 时，同一内嵌 bytes 自动物化到
  `$MIRVM_HOME/package-native/<hash>.so` 再 dlopen；这只是装载策略，不是对预存缓存的依赖。

## 6. 嵌入面与 Engine 生命周期

公开嵌入面为 `Package::load(path) -> Result<Package, String>` 与
`unsafe Package::instantiate() -> Result<Engine, String>`。load 是 safe 的容器/字节码
验证与 owned snapshot；instantiate 的 `unsafe` 表示调用方仍须信任包内 native 库、
foreign ABI 声明和宿主符号契约。同一 `Package` 支持并发、重复实例化。手工构造 Module
只能走 `unsafe Engine::from_module_unchecked`；无类型的导出调用位于
`vm::engine::raw::run_export_raw`，同样是 `unsafe` 并返回两个机器字。普通调用者看不到
`Shared`，因此这不是一套已经完成的 safe typed export API。

Engine 的公开状态是 `Running -> Closing -> Closed`。实现内部还有 `Finalizing`，意思是
所有普通调用和延迟回调已经退出，正在做最后清理：

1. 每次公开调用、native 回调和启动/析构过程先取得执行租约；租约是“这次调用仍在使用
   Engine”的计数凭据。`close` 原子地进入 Closing，此后拒绝新的普通入口，但允许原调用链
   和已经登记的延迟回调收尾。
2. `DeferredHold`（延迟持有）覆盖 pthread 已收下回调到实际启动/撤销之间的空窗，以及
   pthread 线程私有数据析构器仍可能运行的时期。close 会同步清理当前线程的析构值，并等
   其他存活线程退出或删除 key；不会在回调尚可发生时提前释放 Shared。
3. signal 使用进程级 disposition owner 链、每 Engine inbox（待处理信号箱）和每 pthread
   稳定 cell（槽）。每次 guest handler 安装都得到固定 22 字节 RX 桩；内核 frame 只做固定
   TLS 读取和原子登记，不进入 guest。进程定向事件进 owner inbox；`pthread_kill`/真实 libc
   `raise` 的 `SI_TKILL` 进目标 pthread 按本次 handler 安装建立的 cell。close 先停用本
   Engine 的 registration（一次 handler 安装的登记对象），非 LIFO 地从链中摘除 owner，按当前 kernel disposition 恢复存活
   上一层或原生基线，再等在途 frame、owner inbox 和已接收的目标线程 cell；目标事件只能由
   目标 pthread 在安全点或退出收口执行。pthread 退出按 glibc 全局末轮的原始 key 号游标
   交替收口受管 TSD 与自己的 cell，最后才阻塞可捕获信号、复查并关闭 inbox。当前线程自己
   尚有目标事件时，`wait_closed` 返回 `ActiveOnCurrentThread`，不睡住等待自己。查询和
   `oldact` 始终返回 guest handler 地址，不泄漏 kernel stub。
4. 每个宿主线程的 `Ctx` 是该线程进入 Engine 时使用的执行上下文。Engine 用弱引用登记
   `CtxSlot`（上下文槽）；所有租约退出后，finalizer 会清空各槽，所以长期不退出的宿主线程
   不会继续持有 guest TLS、1 GiB 虚拟帧区或整份 Shared，只留下一个空槽。
5. 原生映像先完成“重定位 -> P1/GOT/bridge 填槽 -> 构造函数”，只有全部构造函数完成的
   实例才在 close 时按逆序运行一次析构函数。构造函数中的受控 MIRVM 异常会分类成
   `Result` 失败并启动关闭；析构函数则是不可展开的拆除边界。它在 Closing 中可以正常
   回调本 Engine 或创建有明确完成事件的 pthread 工作；随后再做一轮等待，才进入
   Finalizing。但任何 MIRVM、foreign 或宿主 Rust 异常一旦逃出析构函数，必须固定诊断
   `native finalizer unwound during Engine teardown` 后 `abort`；不得让它续传并把 Engine 卡在
   Closing。
6. `Engine::close` 只发起关闭；`wait_closed` 等到最终清理完成。如果当前宿主线程仍在该
   Engine 的调用链里，等待会明确返回 `ActiveOnCurrentThread`，让外层调用退出后再等，
   而不是死锁。

关闭并不等于所有可执行地址立即解除映射。原生代码可以长期保存函数指针，而通用 FFI
没有“撤销所有副本”的协议，因此下列地址有意保留到进程结束：已发布的普通/P1 libffi
closure、JIT 机器码与系统展开器使用的 `.eh_frame`、已提交的 MC 映像和自产动态库映像。
关闭后的 closure 只保留小型 Engine 身份墓碑，不保留 Module/Shared；C-unwind 入口稳定
报告 `EngineClosed`，普通 C 入口按其不得展开的 ABI 终止。创建新 Engine 不会复用旧 P1
地址，避免旧指针先失效、又碰巧指向新对象的 ABA 问题。在允许构造函数运行之前发生的
实例化失败会回收尚未发布的 closure、MC 映像和动态库句柄；一旦进入构造阶段，地址可能
已经逃逸，即使构造失败也只执行关闭协议，不回收已发布代码。

## 7. CLI 面

CLI 仍使用同一 `Package`/Engine 装载路径：

```
mirvm pack <proj-dir|script.rs> [-o <out.mirvm>]   # 默认 <名>.mirvm
mirvm run <x.mirvm> [-- <guest args>]
```

## 8. 验证计划

- **三维逐字节**：eco（cargo 大项目）/ c_faer_lu（dep global_asm + pulp
  LD_ST）/ c_wasmtime_wat（大依赖闭包 + fiber sym 跳过分支）——
  `mirvm run` 直接跑 vs `pack + run`（冷/热/包三跑）输出逐字节一致;
- **拒绝探针**：fmt_ver、whole/节 hash、重复 tag、节表截断、offset 溢出、
  节重叠、FUNCS 表截断/越界/重叠/逐体哈希、NATIVELIBS/MC 互证失败均返回错误，
  P1 地址缺配方/重复/与 frozen 重叠均返回错误，不 panic；
- **自包含酸试**：关闭 MC 打包，移走原 native/global-asm 缓存，换全新
  MIRVM_HOME，包仍按内容哈希物化并逐字节运行；
- **热序合同**：首次运行产生且只产生一个非空 `.order`；复用同一 MIRVM_HOME 的第二次
  运行输出不变，证明预测通道不改变结果；
- **真实嵌入合同**：load 后改写并删除源包；同一对象并发建 A/B 两 Engine；static/TLS
  隔离，fn-ptr 地址不同，global_asm 与 C2 bridge 各自回到所属 Engine。关闭 A 后旧
  C-unwind 指针稳定返回 A 的 `EngineClosed`，B 继续运行，创建 C 不复用 A/B 地址；
- **signal 嵌入合同**：查询/`oldact` 保留 guest 地址；A/B 覆盖同一 signal 后以非 LIFO
  顺序关闭仍恢复正确上一层与原生 action；进程信号在 owner A 不活动而 B 活动时只进入 A
  inbox；`pthread_kill` 只在目标 pthread 安全点执行，registration 替换不串代，同代传统
  信号按内核语义合并；阻塞的 `raise` 可由 `sigwaitinfo` 以真实 `SI_TKILL` 消费。close/JIT
  锁期间的 frame 只登记，close 等目标线程自行收口；guest 与自产 native archive 的未阻塞
  `raise` 在返回前完成嵌套 handler。线程退出与关闭竞态、陈旧 stub 状态 70 失败也由真实
  内核回归锁定；
- **gate5 全量** + cargo test + diff 双态（SYNC）维持绿。

## 9. 边界与触发器（如实）

- C12 跨版本字节码：当前 build_id 精确匹配；**对外格式冻结**待 D4 触发器
  （mode B 立项已响，冻结评审在片③后）;
- v4 包运行期不占 artifact 固定基址；每个 Engine 使用独立匿名映射；
- 当前只提供安全的包读取与结构化 `run_main`，没有按导出 Rust 类型自动生成的 safe
  宿主绑定；可信包的 native/FFI 契约仍由 `unsafe instantiate` 的调用者承担；
- 任意第三方库若无限期保存回调且不给出完成或撤销事件，引擎无法知道何时可释放该地址；
  当前以进程期 closure + 关闭墓碑解决陈旧调用，不让这种未知期限反过来使
  `wait_closed` 永久等待；
- signal 当前支持传统、无 guest 高级 `sigaction` flag 的进程定向与 `SI_TKILL` 线程定向
  handler。进程事件等 owner Engine 下一普通安全点；线程事件等目标 pthread 下一安全点或
  退出收口。close 可以等待但不能换线程代跑。固定 stub、registration 和线程 cell 保留到
  进程结束；owner 关闭后回装旧 stub 时，裸内核投递 `_exit(70)`，经 MIRVM 桥调用的 `raise`
  报 `EngineFault(70)`。同步故障、realtime、
  `SA_SIGINFO/SA_ONSTACK/SA_NODEFER/SA_RESETHAND` 仍响亮拒绝，进程定向外部事件也不承诺
  原生 handler 级即时延迟；
- proc-macro/build.rs 只在 **pack 期**真执行（D9 §5 硬边界原样）;
- D15 后续施工已完成这里预留的替换：pack 期缺省使用 cargoless 自有依赖驱动，
  `MIRVM_DEPS=cargo` 显式保留 Cargo 回退；包格式与 run 路径不因此改变;
- D3 的档案直接验证余项仍需后续不稳定格式：把实际执行的函数体改为偏移式只读归档表示，
  所有长度、偏移和枚举载荷先做边界检查，E20 再通过借用视图遍历同一份字节；运行时仅在
  函数首次使用时从这份已验证表示恢复 `FuncBody`。并列摘要不能证明另一份 postcard
  执行字节安全，因此不采用“双份表示 + 哈希相等”捷径；
- fat artifact 多 target：节表 tag 预留（`MODULE@<triple>` 形态），D4 后评。

## 10. 实现位置

- `src/pack.rs`：容器读写、检查式解析、自产库内嵌与内容寻址物化；
- `src/vm/engine/mcload.rs`：MC ELF 进程内装载；
- `src/vm/engine/native_instance.rs`：每 Engine native 映像隔离与 P1 隐藏槽重填；
- `src/vm/engine/ctx.rs` / `deferred.rs`：关闭状态、执行租约、pthread 延迟回调与
  长寿命宿主线程 `CtxSlot` 清理；
- `src/vm/engine/signal.rs`：进程级 disposition owner 链、固定登记桩、每 Engine inbox、
  同步 `raise` 与 close-time 停用/等待/排空；自产 archive 的三符号 owner 槽由
  `native_instance.rs` 填入；
- `src/cli.rs`：pack 子命令和 run 的 magic 分流；
- 后续档案直接验证与格式冻结计划见 open-issues D3/D4。
