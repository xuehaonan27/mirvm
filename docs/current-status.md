# mirvm 当前开发状态

> 状态日期：2026-08-19（2026-07-22 外部审核
> [history/development-status-audit-2026-07-22.md](history/development-status-audit-2026-07-22.md)
> 之后的 D15 P1-P5 来源批次、稳定化、`mirvm test` 工作区合同，以及日志采集 1A/L1、
> profile P1 与诊断 D0 的集成闭合均已纳入，见 decision-history §7.21-§7.58）。
> 本文是当前状态的唯一汇总入口；
> 若与早期计划、README 或交接文档冲突，以当前代码、可复现测试结果和本文为准。文档权威
> 规则见 [README.md](README.md)。
> **未解决债务/开放问题/拒绝边界的唯一登记入口 = [open-issues.md](open-issues.md)**，
> 本文 §4 只保留最有代表性的边界快照。

## 1. 阶段结论

| 阶段 | 状态 | 当前含义 |
|---|---|---|
| M0–M2.5 | **历史完成** | 基于 rustc `InterpCx` 的 bootstrap；tier-0 源码已于 2026-07-09 删除 |
| M4.0–M4.5 | **完成** | 自研 typed bytecode、tcx-free tree-walking interpreter、FFI、unwind、真线程、TLS 与回调 |
| M5.0 | **完成并复审** | x86_64 inline-asm stub 工厂：GAS wrapper → `.so` → `dlopen`/`dlsym` |
| M5.1 | **完成（2026-07-12）** | numbigint、xgetbv、sha2、blake3、ecosystem、diff_cargo 3/3 与六个 release tracer 全绿 |
| 真实项目 TDD | **继续扩面（2026-07-15）** | workspace-local ripgrep/tokei 驱动四项通用语义修复；corpus 升级为三维逐字节差分（mirvm 默认 / native / 逢调即编 JIT）并两批扩编 27 个真实 crate（24 绿入 gate5 corpus 段 57 程序、2 expected-red 锁定、1 手工批；[corpus.md §5](corpus.md)）：撞出并已修两个产品 bug（fn_addrs 地址域分拆 `718dac5`、extern fn-ptr foreign 通道 `cb09b5b` 解锁 ring），另钉死 M5.x intrinsic 内建欠账队列（psad.bw/pclmulqdq/aesni/avx512ifma/avx2-gather） |
| M5.2 | **完成（2026-07-14）** | 非 JIT 语义补全（轨 A 完备）：标量/simd intrinsic 差集、真栈深度、f16/f128、atomic 序、backtrace/signal/fork/atexit/global_asm/naked、128 位残余、嵌套 DST 全清；两个历史 XFAIL 转绿（[m5.2-design.md](history/m5.2-design.md) D8a–D8l，施工日志见 [m5-log.md](history/m5-log.md)） |
| M6 ①② | **完成（2026-07-14）** | 轨 C 分发（[distribution-design.md](designs/distribution-design.md) D9f）：① 相位计时（MIRVM_TIMING 账本）+ ② L2 post-mono engine-IR 缓存（冻结区固定基址整包序列化；热跑加载相 11–15×，std-only 程序 374ms→33ms；告警程序诚实不缓存）；③④⑤ 未立项；施工日志 [m6-log.md](history/m6-log.md) |
| M6 片3 前置调研 | **完成（2026-07-14）** | 冷启动/lower 全解剖与杠杆清单（[coldstart-research.md](history/coldstart-research.md)）：lower 是近常数 std 税（~0.10ms/instance；执行集仅占降低集 7–29%）、lower 相 75% 耗在 rustc 查询/解码机器、依赖构建 codegen 白烧实证、三个缓存盲区（P1 runner 环境化石化 / P2 空 stub .d / P3 diff_cargo 无 warm 维度）；**施工顺序已裁定（decision-history §7.2）：S1 小件包 → S2 依赖剪枝 → S4 底座（设计过审）→ S3 懒降低并入 M5.3 JIT 联合设计** |
| M6 片4（S1 小件包） | **完成（2026-07-14）** | 四个语义单元逐 commit 全绿（m6-log 片4）：S1a sysroot 仪式 stamp 化（warm fib 墙钟 85→55ms；加载相性能门 443→364ms）；S1b runner 不回放 MIRVM_\*（P1 化石化修复）；S1c 假二进制 dep-info 真实化（P2；源码编辑触发重录，runner 回写路线实测证伪后改 wrapper 侧 --emit=dep-info）；S1d diff_cargo 补 L2 warm 复跑维度（P3；runner 缓存路径首次有 gate 覆盖） |
| M6 片5（S2 / D9d 依赖剪枝） | **完成（2026-07-14）** | target 依赖 in-process + `-Zno-codegen`（树内现成空转；metadata-only rlib 由默认 link 路径照常产出）；post-mono const-eval 错误面由显式 mono 收集补齐（探针：native/mirvm 同一条 E0080）。账本诚实修正：wall ≈0（128 核噪声内，调研预估证伪）、CPU −12%、磁盘 −60%、少 22 次 exec（m6-log 片5） |
| M6 片6（S4 std 预降低底座） | **完成（2026-07-15）** | 空 main 底座（0x6800 域）+ delta（0x6900 域）偏移合并（施工偏离 §7.3：取代域位双表，解释器零改动）；v0 symbol_name 复用（fn/static/TLS 去重）；降低指纹会话内验证；L2 分层 base_key。**脚本纯冷 385→104ms（lower 9.6×）**，ecosystem runner lower 1272→988ms（std 份额）；底座构建幂等且字节确定（验收抓获 HashMap 随机序缺陷）；gate5 46→47（新增旁路冒烟）全绿（[s4-base-image-design.md](history/s4-base-image-design.md)，m6-log 片6） |
| **M5.3 JIT 骨架** | **完成（2026-07-15）** | 方法级 Cranelift JIT 三片全落地（[m5.3-design.md](history/m5.3-design.md) Q1-Q4 全批；m5-log M5.3 节）：J1 分层基座（call_guest 单一派发点 + PLT/计数，S4 合并 FuncId 空间）+ 翻译器标量子集（语义 = 与解释器逐位一致；调用点两路分治——热路 PLT 间接/冷路 c2i）+ CFI（spike5 管线产品化）。**fib(32)：解释 engine 922.6→19.3ms（47.8×）= 2.9× native；硬门 ≤80ms 达成（69ms 含加载）**。oracle：逢调即编（阈值=1）diff 30/30 + JIT-off 对齐 + gate5 三新行（硬门/逢调即编全量/off 冒烟），gate 总数 47→50 |
| S3′a（image 栈重构） | **完成（2026-07-15，commit b4ed691）** | 多源查找 + 地址样条（行为等价）：单底座泛化成 image 栈（`[std 底座, dep…]`）；frozen 样条域 + is_valid_home 白名单；ImageStack 并集查找/累积偏移/键链；Linker 收 &ImageStack。无 image/单底座两态字节等价，gate5 50/0/0/0 |
| **S3′b（依赖成像）** | **完成（2026-07-15）= A2 纯化聚合 deps-image** | chain 方案撞"线性链无法表达非线性依赖 DAG"固有难题（4/19 证伪）后，purity 探针实测 eco 账本（tainted 72 inst/1.9ms）裁定 A2（[decision-history.md §7.5](decision-history.md)）；[s3b-a2-design.md](history/s3b-a2-design.md) 过审后三片全落地：A2-1 split lower 机件（双队列/标签 id/双 arena 路由/编译期穷尽 rebase）、A2-2 写盘/装载/L2 键链（**eco 冷 924→热 66ms**，自愈矩阵验证）、A2-3 默认开启 + gate 双态冒烟 + S3′c 同 workspace 跨 bin 共享（gate5 50→51；[m6-log.md](history/m6-log.md) 片7/8/9） |
| M5.4（翻译器全覆盖） | **a/b/c/d 全完成（2026-07-21 收口）** | a = 帧模型 v2 + 内存操作数；b = 标量全集 + 128 位族 + atomics（[m5-log.md](history/m5-log.md) M5.4a/b 节）；**c = ABI 泛化（CalleeAbi 全形态）+ 五调用助手 + unwind 产品化（try_call/Resume/Terminate + 双 CIE 全覆 LSDA，`83e168d`）；d = 准入放开——stmt/rvalue/terminator 三表穷尽**（SIMD 15 族 + Sat128 + rvalue 三件经统一助手调 interp 共享本体零漂移，`5252a89`/`5a1bcbe`）。伴生修出既有 bug 四件（call_foreign 签名、bitcast 旗、Bin128 旗标槽 SSA 误提升、frame mask/ptr 区间欠覆盖）。oracle：gate2 9/9、cargo test 76、diff 双态 45/45、diff_cargo 5/5、gate5 复绿（decision-history §7.19，T1/E1 闭合） |
| M5.5（vmctx 终裁 + gate6 收口） | **完成（2026-07-21）= M5 战役全收** | 按原案收口（用户裁定不融合 E6）：`MIRVM_JIT_STATS=1` 十二桶助手频度统计（`05021d2`）+ 计量基线（fib 全空桶；rayon alloc=0；corpus 129/0 格③ 候选 alloc 7.82M/tls_ref 0.38M）+ vmctx-passing §7 落笔（**T 骨架生产定稿**——三表穷尽下编译码 ctx 站点仍零；**复测双触发器：E6 进场 或 多 Engine 嵌入立项**，先到先裁；T→R 单开关路径在案）+ `tests/m5_gate6.sh` 全绿（`aae9aec`）。T3 闭合（decision-history §7.20） |
| C4（dep crate global_asm 物化） | **片①完成（2026-07-23）** | 定稿方向一次落地（decision-history §7.22/§7.23，`176ae20`）：dep 编译期自 HIR 抽取 global_asm/naked 落 rlib 旁挂文本清单，bin 加载相按 crate 图序经 assemble 通道物化装载——不绕行（无 env 名单/无救援链）。验收：c_faer_lu 复原 faer 默认特性，native/默认/SYNC 三维逐字节一致。伴生：JIT 帧 >16 对齐潜伏错值修复（cranelift 栈基 16 上限 + 槽内余量代码级抬基）、C6 补面六件。片② = mode B 机器码节（D1），dep `sym` 指向 guest fn 残余转 R16 |
| mode B（`.mirvm` 包 + pack/run + MC 机器码节） | **已实现；2026-08-13 升为可重复实例化 v4** | v4 的 `Package` 是已校验程序映像，不是单次 Engine：`Package::load` 复制源文件为进程自有的不可变快照，源文件随后改写/删除不影响对象；同一对象可并发重复实例化。artifact 固定地址改成 `LinkAddr` 逻辑地址，每个 Engine 独立映射 frozen/TLS、native/MC 映像和 P1（交给原生代码调用的 guest 函数入口）closure，再由 `LoadMap`/`FrozenReloc` 修补字节码、静态指针、入口、GOT 与 native bridge。函数仍按 FUNCS 索引惰性驻留并记录热序；为保持 E20，load 时仍逐函数临时解码做完整语义验证。格式继续不冻结，档案直接验证仍是后续格式工程，不再把“v4”这个编号预留给它 |
| **2026-08-07 现状复核与稳定化** | **完成** | 修复 JIT 多帧 unwind：同一 FrameTable 的完整 `.eh_frame` 一次注册，旧逐 FDE 做法在两层 JIT 栈会使 panic 无法启动；五个 unwind probe 与两个产品差分用例复绿。TSan 独立 crate 补入日志宏同源依赖并实跑零竞争。`threads_sync` 在 SYNC+阈值1+热缓存下连续 100 次通过，未复现独立崩溃，未做猜测性修补。另定位 pinned LLVM 22 的 release 清理链误编译，固定 `debug=2` 参与代码生成并在链接后剥离调试段；成品仍约 14.4 MiB，默认与 SYNC+阈值1 差分均 45/45。产品能力缺口与建议顺序见 [product-capabilities-plan.md](designs/product-capabilities-plan.md) |
| **标准测试套件（2026-08-10；取代散落脚本）** | **完成** | `tests/run.sh` 是统一入口，提供 `fast`/`smoke`/`gate`、`suite <id>`、`list`；套件按 `quality`、`differential`、`contracts`、`corpus`、`runtime`、`performance`、`harness` 分目录。`tests/support/harness.sh` 统一路径、PASS/FAIL/SKIP/XFAIL、汇总、manifest 执行、cache 与磁盘保护；corpus 只保留 `tests/suites/corpus/cases.manifest` 一份条目清单。旧脚本已迁移或删除，`tests/README.md` 逐项说明用途和新增测试规则，`harness.truth` 锁住注册表覆盖、失败传播与 XFAIL 真实性。默认依赖轨仍为 cargoless self，Cargo compat 与独立 Cargo 裁判同时保留以便持续对拍。2026-07-23 整顿及其 179/0/0/0 是历史证据，不再是当前路径说明 |
| **高性能日志/采集 1A + L1（2026-08-19）** | **参考纵切与页/线程生命周期已落地；性能终态施工中** | 1A 的 capture session、双 4 KiB 页、writer、v0 文件/decoder/inspect/export 基础上，L1 已加入进程硬页池、零页 attach、后续 Enter 自动救援、retire 归页并移出活跃扫描表，以及 writer `q=1` 公平基线。零预算、退线程恢复、2,048 短命线程、双 producer 顺序、offer/retire 竞态与真实 TSan arm session 均有回归。公开入口 64 MiB 仍是待实测默认值；fork child 新代际、trace JIT `r15` 热路、1B raw site、4→64 KiB 自适应和 P2 profile 尚未完成，现行队列见 [日志设计 §11](designs/mirvm_high_performance_log.md) |
| **profile P1：JIT 地址范围/perf-map（2026-08-19）** | **完成** | `JitSymbolRange` 覆盖 fast body、guarded、packed、c2i。编译请求成功后才把请求局部批次登记到内存 registry，再 Release 发布入口；失败批次直接丢弃。install 在锁外 no-replace 创建空 map，显式 stop 在锁内先切 `Inactive` 并快照，再在锁外批量 write/flush；JIT worker 零 map I/O。JIT 15/15 及失败注入回归已过；P2 命令与 fork child registry/map 重置仍归 P2/L2 |
| **诊断通道 D0（2026-08-19）** | **完成** | 默认 `mirvm run` 仍让 compiler/frontend/lower、MIRVM control 与 guest stderr 物理共用 fd2，字节和顺序不变。capture 从 command boundary 建立 `DiagnosticRouter`，只把 compiler/control 逐字节 tee 到 `diagnostics.log`；child attached marker 避免重复路由，guest fd2 不进 router/ring。direct、cargoless、Cargo runner 与早期命令错误合同 **31/31** 逐字节通过；正常 atexit/no-replace，异常保留 partial |
| **D15 P1（砍 cargo 之解析地基，2026-07-27，decision-history §7.27/§7.28）** | **完成** | `src/cargoless/` 五件：manifest（Cargo.toml 模型 + cfg 平台求值 + frontmatter 伪包）/ lockfile（v1–v4 读写 + canonical v3/v4）/ registry（自有 store + 读穿 cargo 缓存只读 + sparse index + .crate sha256 自实现校验 + 解包防护）/ resolve（lock/pubgrub 双模式 + feature 统一 + 单元装配）/ audit（`mirvm deps audit`：项目等值对账 + 脚本 cargo `--locked --offline` 验收链 + manifest needs/env 联动）。**cargo 解析语义全实证定稿**（resolve 图全平台并集 ∪ build 图 host 过滤、lazy-bucket 多版本 fork（hashbrown 0.14/0.15 类）、optional 门按（父包,父版本,依赖键）、?/ 弱引用级联（rust_decimal→borsh→bytes 实锤）、pre 精确规则、exact 钉兼容 build 元数据、lock canonical 尾逗号、同名多 req 条目分立、rename 双路匹配）；上游破洞六枚钉版（均验证 cargo 自家 fresh 同撞）。29 单测绿 + corpus 全量 audit 166 目标绿。P2/P3 已收（见下行）；rust-version-aware 偏好与 Git 来源分别在 2026-08-10 的 §7.36/§7.38 补齐 |
| **D15 P2（砍 cargo 之编译调度，2026-07-27，decision-history §7.29/§7.42）** | **完成** | cargoless 新增 schedule/buildrs/driver：`MIRVM_DEPS=self` 的 `mirvm run` 全程零 cargo——自排拓扑、自算每 crate rustc 参数（内容指纹含传递传播不变量）、proc-macro 闭包 ∪ build-deps 闭包真 rustc host 真 codegen（proc-macro 五钉/host rlib 形态全探针实锤）、build.rs 编译→执行→指令传播全生命周期（`-l` 只进本包、`-L` 传递、metadata 只给直接依赖者、无自动 DEP_*_ROOT、无自动 check-cfg 补钉——全按 probe_link 实证）。**闭合验收：corpus smoke 24 双腿（cargo vs self）stdout/stderr/exit 逐字节 24/24**；diff_cless 六夹具（PATH 只含 mirvm + 离线实证零 cargo 进程）。对拍暴露四枚修复当日落地（含 cargo 腿既有 bug：phase_wrapper 劫持 rustix 1.1.4 RUSTC_WRAPPER 探针竞态 EPIPE）。E36 后续也已关闭：compat runner 通过内部协议恢复调用者 cwd，编译仍保持 Cargo 项目目录；异目录 `--manifest-path` 回归已进入标准差分套件。P3 已收（见下行） |
| **D15 P3（迁移全量 corpus 与双轨 gate，2026-07-28，decision-history §7.30）** | **完成** | 四切落地：① **rustflags 子集**（新 `rustflags.rs`：CARGO_ENCODED_RUSTFLAGS > RUSTFLAGS > config 三键，优先级/发现按 cargo；实证 13 条 rustc 行逐类核对定稿——rustflags 只落 target 单元，host 侧签名级不吃；进全 unit 指纹；伴生修 proc_macro 下划线双拼写、根包 [lib]+[[bin]] 根 lib 编译两枚缺口）。② **build.rs rerun-if 精细增量**（cargo 同语义重跑判定：默认面 registry 源不可变永不重跑 / path 树快照、rerun-if-changed 按 (len,mtime_ns)、env-changed 按值、links 直接依赖传递、存档缺席损坏自愈；存档 = build/<pkg>-<fp>/{output.txt,rerun.txt}，跳过执行则原始 stdout 重解析回放零失真，warning 同门控回放；libgit2 第二腿 20s→0s、tree_sitter 8s→0s）。③ **编译调度并行化**（`run_scheduler` Kahn 就绪队列 + std-only worker 池；完成表只归主线程、派发时捎依赖侧输入进 WorkMsg 零锁；jobs=1 与 Kahn FIFO 逐位一致的对拍锚；FpLocks 互斥同 fp 的 Normal/Build 双 unit；冷跑 wasmtime_wat 152s→79s、libgit2 19s→11s）。④ **full 层迁移**（137 条目双腿对拍 120/19 起 → 分诊修复四枚：extern 命名无 rename 时按 dep 包 lib target 名（new_debug_unreachable 实锤）、StrongDep 强形在有同名显式 feature 定义时被 dep: 遮蔽也置旗（zerotrie litemap 三态定稿）、build script env 补 CARGO_MANIFEST_LINKS（ring 实锤）、bin 会话 --remap-path-prefix + 脚本正文物化 src/main.rs（file!() 路径形态逐字节））。**P5 单列制度化**（对拍轴与 gate 的 corpus 段遇「归 P5」响亮拒绝单列 p5 不计失败；miden_prove 归列）；**双轨接线**（diff_cargo 恒钉 cargo 轨、gate DEPS 轴、a2 恒钉 cargo 轨——跨轨 image 共享原理性不可能、native 基线显式钉 RUSTC 防 rustup 代理按 cwd 解析的混合工具链）。**闭合验收：corpus_deps_pair --tier full 138 pass 1 p5 0 fail；cargo test 153；run.sh fast 9/9；gate DEPS=self（SKIP_TSAN=1）177 pass 1 p5 1 fail**（唯一 fail = 纯度门禁 mirvm-tsan 被工作树内并行重构卡住，与 D15 无关如实记）。P4（sysroot 自管 + MIRVM_DEPS 默认翻 self + `--bin`/`--package` 多目标选择 + compat 评审）待施 |
| **D15 P4（sysroot 自管与默认翻转，2026-07-29，decision-history §7.31/§7.37）** | **完成** | ① **sysroot 自管**（`5c2e9bf`）：MIR sysroot 构建换 cargoless 调度——新 `vendor.rs`（VendorDir 通用 vendored-dir PkgSource，后续已复用于 source replacement）+ 伪根（std/test/proc_macro）+ library/Cargo.lock 增广 lock 模式 + compile_plan 纯抽取复用 + tmp 原子发布 + cargo 轨 dep 缓存换代连坐 purge + 顶层哨兵盖戳；**意外 crates.io 依赖根除**（.d 引用 ~/.cargo 数 = 0）；冷建 27.2s（metadata-only 无对象码，双轨 gate 全消费面实证无炸）25 crate 集对齐、PATH strip 零 cargo 实证。② **--bin 多目标选择**（`271425b`）：cargo run --bin 语义，错名响亮列名单；compat 轨直通。③ **长期双轨**：`MIRVM_DEPS` 缺省 = self，`=cargo` 是长期保留的用户回退与 Cargo 行为裁判；两条路径各自完整并持续对拍，不再安排 compat 删除评审。**当时边界**含 workspace 多包图；resolver 2/3、Git、替代 registry、常见 source replacement/patch/replace 与 pack self 已在后续 §7.35-§7.41 补齐；resolver 1 等仍开放 |
| **D17 `mirvm test` Cargo 合同（2026-08-11）** | **完成** | Cargo compat 与默认 self 双轨均已接入。除 lib/bin/test/example、Normal/Dev 边、libtest/custom harness、工作区选择/继承/统一 lock/feature 外，现支持 bench、根 proc-macro、resolver 1、复杂成员 glob、workspace lints 和完整 package ID spec。D19 又以 rustdoc 专项接入 doctest，没有伪装成普通 test。固定 Cargo/compat/self 的单包/test/bench/doctest 合同 **34/34**、工作区合同 **31/31**；PATH 哨兵 + execve 审计证明 self 零 Cargo。规范见 [mirvm-test-cargoless-contract.md](designs/mirvm-test-cargoless-contract.md) |
| **D19 doctest / rustdoc 前端（2026-08-11）** | **`mirvm test` 范围完成** | 固定 rustdoc 继续负责 Markdown 代码块提取、源行号、edition/cfg、`no_run`、`ignore`、`compile_fail` 和 `should_panic` 裁判；MIRVM test builder 把它生成的临时库编成带 MIR 的 metadata-only rlib，把可运行临时 crate 发布成 VM 启动器。默认 self 与 Cargo compat 均使用同一 MIR sysroot，支持默认/`--doc`、Dev 依赖、build.rs 输出、过滤与失败码；合同 **34/34**，self execve 零 Cargo。独立 `mirvm doc`/HTML 生成未由此宣称 |
| **D15 P5 第一批：resolver 3 / rust-version（2026-08-10，decision-history §7.36）** | **完成** | 显式 resolver 3 与 edition 2024 隐式规则、workspace 最低 Rust 版本、registry index 的 `rust_version`/`rust_version2`、Cargo 配置 `resolver.incompatible-rust-versions=fallback/allow`、`--ignore-rust-version` 和编译前诊断已接通。fresh lock 按最低 Rust 版本写 v3/v4；固定 Cargo 与 self 的真实 `home=0.5` 探针同选 0.5.9，self lock 被 Cargo `--locked --offline` 接受。工作区合同改为 resolver 3 后仍 **27/27**。从空 `MIRVM_HOME` 跑 fast 时发现当前 rust-src 的 `proc_macro` 使用 `[lints.rust]`，已按 Cargo 的 priority 顺序传播 level/check-cfg 到全部 rustc target 并进入指纹；空目录 sysroot 冷建成功。Rust 测试 **186/186**，隔离可写 Cargo home/target 的 release fast **11/11** |
| **D15 P5 第二批：Git 依赖（2026-08-10，decision-history §7.38）** | **完成** | 支持默认分支、branch/tag/rev、仓库内 workspace/package、feature 与 path 依赖；系统 Git 复用 credential helper/SSH agent/known_hosts，并初始化 submodule。可变引用只在 fresh 解析，lock 固定 commit；暖缓存离线复跑、冷缓存离线失败、缓存 origin 校验、checkout 篡改拒绝均落地。包身份和编译指纹包含精确 Git 来源，同名同版本双 commit 可在一个依赖图和锁文件中分立；self 单/双 commit lock 均被固定 Cargo `--locked` 接受。`cargoless_git_contract` **9/9**，execve 审计 self 零 Cargo；cargoless Rust 测试 **105/105**，全量 Rust 测试 **193/193**，隔离可写 Cargo home 的 release fast **12/12** |
| **D15 P5 第三至五批及 P2 总验收（2026-08-10，decision-history §7.39-§7.43）** | **完成** | Cargo config 按 `$CARGO_HOME`、项目祖先、include 与环境覆盖合并依赖来源子集；替代 sparse/Git registry 有独立身份，认证复用 Cargo token/provider。source replacement 覆盖 registry 镜像、local registry 与 vendor directory，拒绝替换环和 checksum 篡改；registry 目标上的 path/Git/registry `[patch]` 参与版本求解，`[replace]` 保留 Cargo 锁语义。项目/frontmatter 的 `mirvm pack` 缺省走同一 cargoless 图，`MIRVM_DEPS=cargo` 显式回退长期保留。来源合同 **30/30**，每个 self fresh lock 与固定 Cargo 逐字节相同并被其 `--locked --offline` 接受；P2 full corpus 最终 **138 pass / 1 skip / 0 fail**，旧 `miden_prove` P5/XFAIL 已摘。D17 后续已补 resolver 1、复杂成员 glob/package spec 和 workspace lints。Git source replacement、以 Git URL 为目标的 patch、`paths`/HOST_RUSTFLAGS 等完整 Cargo config 仍是明确边界 |
| **E 引擎与架构第一轮（2026-08-10，decision-history §7.42）** | **当时核心完成；E22 生命周期已由第四轮接续** | 多 Engine 触发器命中后采用线程局部当前 Engine 身份：Shared/JIT worker/fork 基线/退出回调按 Engine 隔离，线程表按 Engine id 保存 Ctx，嵌套进入可恢复，guest TLS、builtin 字符串和遗留 atexit 项会回收；MC 符号只在当前 Module 内解析。引擎故障经 `RunError` 返回，不再从库路径直接退出宿主；rustc compiler 会话有全局 guard。新增穷尽字节码验证器，接入 pack、镜像、L2 和最终执行入口。操作数区双端 `PROT_NONE` guard 与 JIT 入帧前栈检查已落地，小栈深递归返回诊断而不是裸 SIGSEGV。该轮记下的并发关闭、pthread 回调与长寿命宿主线程 Ctx 问题已在 2026-08-13 闭合；checked 指针来源仍归 E23，当前仍不宣称沙箱 |
| **E 引擎与架构第二轮（2026-08-12，decision-history §7.47）** | **E3/E8/E24/E28/E31 完成** | 解释帧与真实 JIT unwind 帧按 CFA 合并；Engine 自动物化最小 ELF，让标准 Rust backtrace 解析客体函数名。IR/deps image 键加入文件内容 BLAKE3，并拒绝读取期 inode/ctime/path 身份漂移。Cargo compat 改占 `RUSTC` 槽，普通/workspace wrapper 的环境变量、config、顺序和适用范围继续由固定 Cargo 原样决定。固定 rustc 的 Global alloc error shim 已由 native 差分证实自然闭合，libc abort 不再多打印 MIRVM 私有诊断。同期修正 JIT cleanup 在 `try_call` 终结前切块的非法 Cranelift 构造。开放账中已实现项与明确接受的优化选择完成迁移 |
| **E 引擎与架构第三轮（2026-08-12，decision-history §7.48-§7.52）** | **E9/E12/E13/R18 收口** | `dl_iterate_phdr` 已用真实动态库装载/枚举探针验证 native、解释器和同步 JIT 一致。解释态局部正式保留带 guard page 的 slaved ByteRegion，alloca 降为真实性能证据触发的候选。跨语言异常按源 `C/System { unwind }` 分治：普通 C 终止，C-unwind 保留 C++ 异常或 Rust panic 身份并执行 cleanup；非 C/System ABI 明确拒绝。E13 经真实嵌入 RED 重裁：guest panic 与 `EngineFault` 使用 MIRVM 自有异常类和原始分类器，但每帧 cleanup 继续复用现有 Rust personality/LSDA。每个解释器 raw catch 和 JIT landing pad 都按当前异常指针分类；只有当前对象是 `EngineFault` 才跳过 guest cleanup。线程内带 nonce 的 LIFO token 栈只记录 owner 与消费顺序，因此 native catch 暂停外层故障后，同线程重入的 guest panic 仍会 cleanup，嵌套 `EngineFault` 也能独立入栈和消费。未捕获 guest payload 交回 guest std 清理。lower 还会精确标出真实 `lang_start` 的 `MainPanicBoundary`，每次 `run_main` 的状态栈把 main panic 与正常 `Termination` 返回 101 分开；解释器、JIT、pack 和 verifier 共守这项合同。C++ typed exception 可原样穿出整个 Engine。合同与修复前后矩阵见 [designs/c-unwind-contract.md](designs/c-unwind-contract.md) |
| **E 引擎与架构第四轮（2026-08-13，decision-history §7.53）** | **Package v4 与真实关闭协议完成** | `Package::load` 是 safe 的不可变映像校验，`unsafe instantiate` 每次建立独立 Engine；P1 是交给原生代码调用的 guest 入口，每 Engine 地址不同且关闭后永不复用，global_asm/C2 通过隐藏槽回到正确 owner。Engine 用 Running/Closing/Finalizing/Closed 状态和执行租约处理并发 close；`DeferredHold`（延迟持有）覆盖 pthread 已收下但未开始/撤销的 start 与线程私有数据（TSD）析构器，也覆盖被 native catch 暂停的 MIRVM 异常。close 等这些工作退出，在 Closing 中运行逐实例 native fini，再做第二轮等待；constructor 的受控异常转成实例化 `Result` 失败，fini 则是不可展开的拆除边界，任何 MIRVM/foreign/宿主 Rust 异常逃出都固定诊断后 `abort`。同线程调用链内 `wait_closed` 明确报错而不死锁。`CtxSlot`（上下文槽）让 finalizer 清空长期宿主线程留下的 guest TLS、ByteRegion 和 Shared。已发布 closure、JIT 码/展开表与 committed MC/native 映像因外部裸函数指针无法普遍撤销而保留到进程结束；关闭墓碑不再持有 Module。公开面不是全 safe typed API：包实例化、手工 IR 和 raw 两机器字 export 仍是 `unsafe` |
| 地址模型 P2（GOT 间接） | **完成（2026-07-17）** | §7.5b 手术单定场（真实地址模型保留）→ §7.5c 零 IR 变更 GOT 机制（槽 = 冻结区普通格 + 启动相重填；extern static/fn 值不再烤宿主地址，字节码复用 `Mem{Static(槽)}`/`SubImm` 通道，JIT/interp 零改动）→ §7.5d 拒缓存三判据全退役 + 纯 std 会话 want_split 修正（先存 A2 沉默债：L2 对纯 std 程序永 miss）。外来符号用例冷→热全通（c_process 463→30ms），gate5 117/0/0。JIT 间接调用准入记债（[open-issues.md E1](open-issues.md)） |
| 地址模型 P1（fn 条目可执行化） | **完成；2026-08-13 扩成每 Engine 实例地址** | §7.6 的第一版用固定地址域给 FFI 可派生条目建立可执行 closure，根治结构体内嵌 fn-ptr 盲区。v4 不再把固定 P1 地址当运行身份：包只存 `EntryStubSite` 配方和 `LinkAddr`，每个 Engine 物化独有 libffi closure 并重填所有引用。旧 closure 与小型 owner 墓碑保留到进程结束且地址不复用，所以陈旧指针不会因地址重新分配而误指向新 Engine。签名不可派生条目（Rust ABI/聚合/变参）仍保持数据槽，不能由此宣称任意 Rust ABI callback 可用 |
| corpus 批7（激进 24 三波） | **完成（2026-07-17）** | 23/24 全绿可用（corpus.md §5 批7）；**修出两只产品 bug 当日修复**：native-archive 链接行收 crate 图动态库（`867b3de`，libgit2 红转绿）+ custom `#[global_allocator]` 运行时统一路由 `__rust_*`（§7.7，c_mimalloc 三维绿、跨堆 SIGSEGV 根治）。c_tree_sitter 按值聚合 FFI 记档（open-issues C1，**2026-07-18 C1 闭合后转正入 gate**）。gate5 128→**139**；corpus 实测真实 crate 总账 123 |
| corpus 批8（重型 10+ 三波） | **完成（2026-07-17，总收口 `2f9efef`）** | 波1 重 FFI/C 5/5 绿（ladder：lddqu 补面 + constructor 分治 + `\x01` 前缀剥除 + GOT 键名盲点，§7.8）；波2 VM/大物 4/5 绿（JIT analyze_frame 帧末 ZST 实锤修复 `dc6e30c`；c_wasmtime_wat 双层欠账记档 [open-issues.md C2/C3](open-issues.md)：符号在 rlib + asm noreturn）。gate5 139→**148**；corpus 实测真实 crate 总账 ≈133 |
| 第 0 步收口（E27/G4/C7） | **完成（2026-07-18）** | 按 open-issues 闭合性总判排单（decision-history §7.9）：① E27 修复「weak extern static 恒 0 判空 cell」隐藏缺陷 → GOT 启动相真解析（引擎接管符号强制缺席），std 弱探测（statx 等）自此走主径；② G4 双钉回摘（exr 摘 half 钉、flate2 gz/zlib 原生 API 回归）；③ C7 global_asm/naked `sym` 指向解释态 guest fn 主面闭合（trampoline 导出 + P1 条目预算；不可派生签名与 SymStatic→static 转 R16 如实拒绝）。三单各配三维探针；`cargo test` 67/67、`diff.sh` 33/33、gate5 全量复绿 |
| corpus 批9（偏门小中型 8） | **完成（2026-07-18）** | 8/8 全绿（corpus.md §5 批9）：zune_jpeg/candle_mlp/pest/scraper_dom/arrow_rs/fontdue/unicode_rs/rustpython_mini——数据/图像/ML/语法/DOM/字体/unicode/VM-in-VM 七类面又压了一轮。修出并当日修复 JIT analyze_frame **0 字节帧 ZST 取址**（`68829ad`：FrameMap force 档 + 1 字节物化；批8 starlark 帧末 ZST 同族第三形态）；zune-jpeg 0.4.21 上游破洞与 fontdue simd 官方开关两处在案。gate5 148→**156** |
| C1 FFI 按值聚合封送 | **完成（2026-07-18，首单闭合性总判大工程）** | 旧 debt §9 转正闭合（[designs/c1-ffi-agg-design.md](designs/c1-ffi-agg-design.md)，decision-history §7.10）：FfiAgg 冻结布局 + libffi struct 全聚合编组（出向零拷贝/返回 Indirect dst memcpy；入向 `call_guest_ffi` 按 callee ParamAbi 展开 + sret 直传/小档重打包）；伴生 = 聚合签名 guest fn 入 P1 条目候选。**c_tree_sitter 原样三维转绿**入 gate（corpus 段 +1）；合成矩阵探针 `demo/ffi_agg_probe.rs` 三维绿。残余五形态如实拒绝（R17）。gate5 →**157**，diff.sh 35/35 |
| C2 native-archive 闭包「符号在 rlib」 | **完成（2026-07-18）** | 旧 debt §10① 评审定案即施工（[designs/c2-rlib-symbols-design.md](designs/c2-rlib-symbols-design.md)，decision-history §7.11）：首链失败救援链 = elfsym 二进制级 `SHN_UNDEF` 枚举 ∩ `exported_defs`（native final link 集符集同源）⇒ `fn_entry_addr` 预算 ⇒ `.hidden` P1 跳板重链（首链路径逐字节不变）。MRE 转绿、`c_bzip2_csys`（bzip2 vendored C 后端）三维绿入 gate、**c_wasmtime_wat 换面**（层①消除，锁层② asm noreturn/70 = C3 入口）。施工实雷一只（GNU ar `/N` 字符串表引用被误判元数据，单测反咬修复）。数据符号如实拒绝另立。gate5 →**158 pass + 1 expected-red**，cargo test 68/68 |
| C3 inline asm `noreturn` 两面孔 | **完成（2026-07-18，含一次自我反转）** | 合成 spike 先判「不可开」（v2 内联 setjmp/resume：native 绿/mirvm 撞死，落点 `Channel::send` 内部——解释帧复用捕获帧宿主栈内存）；**真实 workload 复验反转**：c_wasmtime_wat 冷缓存全 trap 面三维确定性绿（trap 链在存活链内推进，捕获帧恢复时仍有效）。终判：两面孔物化保留（decision-history §7.12）；ud2 面完全闭合（demo/noreturn_ud2 入 diff.sh）；resume 面真实 workload 支持物化 + 复用 hazard 如实记 E32（消除 = JIT 真帧身份）。**c_wasmtime_wat 由 expected-red 转绿**（VM-in-VM 旗舰全程）。gate5 →**159 pass / 0 expected-red**，diff.sh 36/36 |
| corpus 批10 波1（重/大物 4） | **完成（2026-07-18）** | 1 直接绿（c_miden_prove；gate 接线随波2 总收口）/ 3 红当日同修转正（[corpus.md §5 批10 波1](corpus.md)）：c_risc0_run 撞**跨归档 weak/COMDAT 碰撞**（`reject_symbol_ambiguity` 纳入 nm posix 类型字母：W/w/V/v/u 首件胜出、恰一 strong 放行、双 strong 仍拒）；c_typst_pdf 撞 **asm-stub xmm 16B 槽**（`__m128i` 按值过 asm ins/outs → AsmIoVal/AsmIoDst 向量字节通道，lower/interp/jit analyze_frame 四面同步）并双供养 C5；c_datafusion_sql 撞 C5（下 row）。gate5 159→**162**，cargo test 69/69 |
| C5 dyn 上溯 vtable 变换 | **完成（2026-07-18，M4.2 欠账闭合）** | decision-history §7.13：`dyn_unsize_tails` 统一递归判据（builtin_deref 直达 / **解引用落点 lockstep 再判** / Pat 壳剥除 / Adt 唯一非 ZST 字段递归——Arc→NonNull→Pat→*const ArcInner→data 全链）+ PC::Unsize 臂 `supertrait_vtable_slot` chase（目标 vtable = `*(源 vtable + slot×8)`，cg_ssa `unsized_info` 同构）。探针 demo/dyn_upcast_probe.rs（Arc 包装链 + & 直达两形）入 diff.sh 37/37；c_datafusion_sql 三维 95 行逐字节；c_typst_pdf fnv 锚点无回归 |
| corpus 批10 波2（中轻 5） | **完成（2026-07-18）** | 4 直接绿（ethers_evm/polars_lazy/xlsxwriter_rw/ugrep_bin，[corpus.md §5 批10 波2](corpus.md)）/ 1 RED-bug 当日修复转绿：c_resvg_svg C 维撞 **JIT signed 窄宽 Div/Rem 漏 sext**（槽不变量零扩到宽，负值被当大正数除；tiny-skia `fast_div` 供养出 hairline_aa slope 断言）——`int_bin` signed Div/Rem 先 sext 双操作数 + MIN/-1 特判 `ineg` + Bin128 同族并修；探针 demo/signed_div_probe.rs 入 diff.sh 37→38。c_miden_prove gate 接线落地（tmo=400）。gate5 →**167 pass / 0 expected-red** |
| 缓存重审与根迁 | **完成（2026-07-18）** | 缓存胀大实证解剖（主项 = 每 frontmatter 程序两套 cargo target，shim 一套 + B 维 native 一套；次项 = build_id 换代无 GC）后裁定（decision-history §7.14）：根迁 `$HOME/.mirvm`（`MIRVM_HOME` 改址，不再读 XDG）；必要性审计结论 = 全组件必要、缺管理面——补 `mirvm cache status|purge`（默认清陈代，build_id 首字段 varint peek 判代，条目扩展名过滤免咬构建副产）。tests 三脚本跟迁。cargo test 73/73，gate5 167/0/0 |
| 统一依赖存储近期片（D14） | **完成（2026-07-18）** | 用户裁定 cache 机器级统一（decision-history §7.15）：mirvm 构建 `--target-dir` 由 per-project 改 `$MIRVM_HOME/target/mirvm` 共享（cargo fingerprint 即编译键内容寻址；`MIRVM_TARGET_DIR` 改址）；产物定位零改动（runner 协议供路径）；`[[bin]]` 名带哈希短缀消 `debug/<binname>` 碰撞；B 维经物化 `.cargo/config.toml` 进 `target/native`。附带收益 = deps image 跨脚本/项目共享。实测 ethers 二跑 0.66s、两树并集 525M、script dir 缩至 KB 级；`purge --target` 配套。终态原生 store 登记 [open-issues.md D14](open-issues.md) 与 mode B 合并评审。cargo test 73/73，gate5 167/0/0 |
| **结构重构战役（E21 闭合）** | **完成（2026-07-18/19，六片全绿）** | 对标 OpenJDK 分层（decision-history §7.16，用户裁定纯搬移零行为变化）：`src/os/`（mod 契约 + linux/{mem,thread,signal,dll,process}）与 `src/arch/`（x86_64/{intrinsics,asmstub}）双 leaf 建成，**非 os 域 `libc::` 与非 arch 域 x86 触点 grep 机械清零**；`vm/engine/addrlayout.rs` 固定基址三方共享。巨型文件治理：func.rs→lower/func/ 八件、interp.rs→engine/interp/ 七件、jit_compile.rs→engine/jit/ 七件（并 J1 基座）、lower/mod.rs→linker 五件 + builtins/ffi_sig/purity/rebase 四件，lower/mod ⇄ native_archive 文件级环解。25 个拆分文件补 `//!` 契约头；build 零警告、cargo test 74/74、diff 双态 38/38、diff_cargo 5/5、tsan 过、gate5 167/0/0。记档接受面：lower/asm.rs 与 llvm.x86 名表 rustc 耦合留 lower 域；臂级再拆不做 |
| M5.5 | **完成（2026-07-21）** | vmctx 终裁计量 + gate6 收口（decision-history §7.20）：T 骨架生产定稿 + 复测双触发器（E6/多 Engine）；`tests/m5_gate6.sh` 落位，CI 接线（2026-07-22）。M5 战役（M5.0–M5.5）全收 |

产品执行引擎 = M4 解释器 + 方法级 JIT（cranelift 为默认 feature；JIT 默认开启，
`--jit off`/`MIRVM_JIT=off` 回退纯解释；tsan harness 不开 cranelift）。M5.2 把解释器
语义面补全，为 JIT 期交付语义面干净的基线；M5.4a–d 把 JIT 翻译器推进到
**stmt/rvalue/terminator 三表穷尽**（ABI 全形态、unwind 产品化双 CIE 全覆 LSDA、
SIMD 族经 interp 共享本体助手），解释器继续作为差分 oracle 与回退语义源；
2026-07-22 稳定化战役（§7.21）补齐验证强度——`MIRVM_JIT_SYNC` 同步发布 +
可准入编译失败响亮 RED，threshold=1 差分自此证明编译码真被执行。

## 2. 当前实现路径

```text
.rs / Cargo 项目
  → cli / cargo wrapper + runner
  → rustc_driver::after_analysis（加载相仍可访问 tcx；callback 只 lower）
  → lower_program
      collector 种子 + call-site worklist 求单态化闭包
      冻结 layout / ABI / place / statics / vtable / TLS / FFI 签名
      物化可支持的 asm stubs 与受约束 Static native archives；未知构造降为带诊断的 Trap
  → tcx-free ir::Module（留在 callback）
  → Cargo runner 仅在 lower 后安装窄 TRACK_DIAGNOSTIC filter
  → run_compiler 完整执行 tcx.finish / diagnostics / compiler drop，然后恢复 filter
  → 仅 compiler success 时执行 Module
  → 每 Engine Shared + 每个宿主线程一个 CtxSlot（上下文槽；关闭时清空重资源）
  → interp_frame / run_blocks
      guest 调用：宿主递归
      guest → native：dlsym + libffi（按源 ABI 选择 C / C-unwind）
      native → guest：libffi closure thunk + TLS attach（callback/P1 同样按 ABI 分治）
      inline asm：调用已物化的 fn(*mut u8) stub
      guest panic / EngineFault：MIRVM 自有异常类 + 原始指针分类
          guest panic：按所属 Engine 捕获；未捕获 payload 交回 guest std 清理
          每帧按实际异常对象分类：只有当前 EngineFault 跳过 guest cleanup
          TLS nonce token 栈：只核对 owner 与 LIFO 消费顺序，不参与 cleanup 决策
      real lang_start main：MainPanicBoundary + 每次 run_main 状态栈
      Engine 出口：Returned / GuestPanic / RunErrorKind；正常 Termination 101 不冒充 panic
      C++ foreign：Engine 不消费、不改写，继续按原类型展开
  → close：执行租约/延迟回调归零 → native fini（异常逃出 = 诊断 + abort） → Ctx/Shared 回收
      已发布 closure/JIT/MC/native 地址只留进程期关闭墓碑
```

代码热点集中在 `src/lower/func/`（调用/调用约定降低）、`src/vm/engine/interp/`
（解释器主循环）与 `src/vm/engine/jit/translate.rs`（JIT 翻译器，E34 记治理）。
实现目前实质上是 Linux/ELF/x86_64 优先：依赖 pthread、dlopen、GNU 链接行为和
x86 asm wrapper。

## 3. 已验证边界

- **当前增量验证（2026-08-12 实跑）**：`cargo fmt --all -- --check`、Clippy
  `-D warnings`、`cargo test --locked --all-features` **220/220** 均通过。最终 release
  产物的 `runtime.c-unwind` **13/13**，`runtime.semantics unwind` **13/13**；程序差分默认与强制同步 JIT 各
  **47 PASS / 2 SKIP / 0 FAIL**。本轮没有重跑完整 `fast`/`gate` 或性能基线。
  最近一次其余标准套件分项结果为：Cargo 裁判 **12/12**、
  cargoless **7/7**、单包 test/bench/doctest 合同
  **34/34**、工作区合同 **31/31**、Git 来源 **9/9**、来源合同 **30/30**、
  pack 合同 **8/8**、build.rs **21/21**、
  harness 自检 **16/16**。标准化前后受改动影响的 corpus 条目、Git 来源、运行时语义
  另行定点复跑均通过。`miden_prove` 已迁到同发布线非 yanked 的 0.25.8，删除不再
  需要的 Git patch 后 Cargo/cargoless 证明输出逐字节一致，旧 P5/XFAIL 已摘；
  `rustpython_mini` 因本机缺 `libffi.so` 开发链接名为 SKIP。
  P2 full corpus 总验收 **138 PASS / 1 SKIP / 0 FAIL**；`runtime.semantics` 四组也在
  最终 release 上通过，其中 threads **12/12** 含 TSan。
  按维护者裁定，本轮未运行 `performance.limits`、OS 沙箱或完整 `gate`。2026-08-10 的完整
  `gate` 尝试曾暴露、推动修复 resolver 兼容版本统一、Git rust-version
  校验误访问 crates.io、JIT 退出期与 Rayon 并发线程生命周期、以及数个 fixture 漂移；
  修复后受影响路径均已定点复绿，但未再花约 40 分钟整轮复跑。当前仍有一个真实 RED：
  `performance.limits` 的 `fib(32)` 最快 **97ms > 80ms**；输出正确且 JIT 有效，不放宽门槛，
  已并入 [open-issues.md D16](open-issues.md) 的进行中性能战役。因此当前不得宣称完整
  `gate` 全绿。
  **上一轮全矩阵（2026-08-07 实跑，以下路径和数字只作历史证据）**：
  `tests/diff.sh` **45/45**（默认与阈值=1 双态）、**+MIRVM_JIT_SYNC 同步发布
  45/45**（可准入函数首调同步编译并真跑机器码，编译失败 RED——audit F-05 起
  threshold=1 的证明力升级）、`tests/diff_cargo.sh` 5/5、
  `tests/gate_truth_regression.sh` 12/12、`tests/run.sh fast` 9/9、
  `tests/gate.sh` **179/0/0/0**（corpus 全防线 163 条目 = 161 脚本（smoke+full
  两层）+ hexyl/tokei 项目三维对拍；fib(32) JIT 62ms ≤80ms 硬门在列）、
  TSan 零警告。该轮沿用 2026-07-23 测试管线；现行入口与规则见
  [tests/README.md](../tests/README.md)。
  CI（`.github/workflows/ci.yml`）同构命令全绿（GitHub 侧操作暂停，本地同构为准）。
  **以下为各阶段 dated 历史记录**（保留备查，非本轮复跑）：
- debug/release 构建可通过；执行必须优先使用 release 版本。当前 pinned LLVM 22 下，
  release 必须保留 Cargo.toml 的 `debug=2` + `strip="debuginfo"` 组合，原因和移除条件见
  decision-history §7.33。
- M4 的纯函数、值/内存、unwind、FFI、真线程和 TSan gate 已有端到端覆盖。
- `tests/diff.sh` 对当前 demo 做 native 差分，M5.0 的 asm probe 已进入回归。
- 2026-07-13 当前 Rust tests 为 **35/35 passed**：asm stub、12 个 native archive、
  2 个 FFI 必需/可选库、Width、9 个 volatile（含低对齐、padding、宽值和重叠快照）、
  direct dyn 尾动态对齐、2 个 x86 helper、6 个 Cargo shim/rustflags/wrapper 测试，
  以及 1 个 runner warning-summary 结构化过滤测试；Rust 层覆盖仍小，
  shell gate 仍是主要安全网。
- `tests/gate_truth_regression.sh` 最终 **12/12**，除了双方失败假绿、XFAIL 原因、
  XPASS、corpus 状态传播和 gate0/1/2，还锁住 TSan/性能/CPU 特性 SKIP 不得冒充
  PASS、`diff.sh` 单侧 stderr 不得被忽略，x86 vector 的 pshufb/SHA 子能力必须分别记账。
- `tests/m51_addcarry.sh` 已把 addcarry/subborrow 边界 checksum 与 native 差分，c_numbigint 转绿；
  `tests/m51_xgetbv.sh` 在 CPUID guard 下与 native 输出一致；x86 vectors 与 `simd_insert`
  /extract、SIMD shifts、vzeroupper 也有 feature-gated native differential probes。六个 M5.1
  release tracers 全部通过。
- M5.1 收官时的 full release gate5（含 gate0/1/2/4、TSan、六个 M5.1 tracer 脚本/七个子断言）为
  **40 PASS / 2 XFAIL / 0 SKIP / 0 FAIL**；XFAIL 分别是 signal guest handler 和
  guest backtrace/frame-IP 映射。
- 2026-07-13 本轮产品补丁后的最终 full gate5 为
  **40 PASS / 2 XFAIL / 0 SKIP / 0 FAIL**，`diff.sh` 为 **20/20**、`diff_cargo.sh` 为
  **5/5**；新增的 `cargo_warning_return` 锁住 Cargo runner 不得在 guest 正常返回后泄漏额外
  rustc warning summary，同时保留 guest 自己输出的同文 stderr。同轮 fmt、35/35 Rust tests、
  Clippy `-D warnings` 与 release build 均最终通过；最终复跑的 load/rayon 子项分别为 443 ms / 927 ms。
- `tests/real_projects_regression.sh` 最终 **65/65**，覆盖严格 case schema、repo/rev/lock provenance、
  native-first oracle、exit/stdout/stderr 精确比较、canonical XFAIL/XPASS、断网与写隔离、
  toolchain/host target、干净 env/EOF stdin、私有 runtime cache、source mutation，以及 benchmark
  correctness 准入、warmup/交替采样、每轮 oracle、基础设施状态、完整 summary provenance、
  namespace-private 稳定逻辑路径、项目 `.cargo/config.toml` rustflags 保留，以及会被私有 `/run`
  tmpfs 隐藏的 workspace/suite/toolchain/cache 宿主路径在启动时 fail-fast。新增 identity/evidence
  tracer 还锁住同名不同语义、Case/Check/Bench/Evidence 分层、Git/controller 与 sysroot 内容身份、
  exact check 依赖及发布前复验、同名与 suite-cache 并发锁、controller/grouping 子 shell 的锁 FD
  隔离、两层 hard-crash staging 与临时 symlink 回收、prepare 恢复、后台后代隔离、consumer 身份重推、
  PASS/XFAIL oracle 语义复算、exact check execution provenance、额外 sidecar 拒绝、
  samples/median/p95 复算、pre-rename 只读封存、current symlink 原子发布和 failed-check 撤销 current。
  无外部 workload tool 的当前发布格式仍是 schema 3；显式声明外部工具的 case 使用 schema 4
  记录其身份。consumer 仍按独立 schema-2/3 路径验证既有历史对象，不把新字段静默回写成旧
  schema 当时已具备的事实。
- Cargo compat 把 MIRVM 放在 Cargo 的 `RUSTC` 编译器槽，普通
  `RUSTC_WRAPPER` 与 workspace wrapper 的选择、配置合并和嵌套顺序仍由固定 Cargo
  决定。环境变量和 `build.rustc-wrapper` / `build.rustc-workspace-wrapper` 两种配置
  都已与固定 Cargo 对拍；普通 wrapper 覆盖依赖与根包，workspace wrapper 只覆盖成员。
- Cargo runner 的 callback 现在只 lower `Module`；lower 后才安装 `TRACK_DIAGNOSTIC` 结构化
  filter。它仅抑制无 lint/code/span/children/suggestions 的 `ForceWarning` `N warnings emitted`，
  并继续委托原 hook；guest 自己写出的同文 stderr 不经该路径。`run_compiler` 完整执行
  tcx/diagnostics/compiler 收尾并恢复 hook，只在 compiler success 后才执行 VM。直接
  `process::exit` 是已被取代的中间方案；纯单文件仍正常收尾。
- 当前工作集是 Git-ignored 的 `artifacts/real-projects/` 下固定 revision/lock 的 ripgrep 与 tokei。
  本轮完成 direct dyn 尾 alignment、u128 `SwitchInt`、`track_caller` Reify shim 和宽 volatile
  修复及最小 native 差分。最终使用 namespace-private `/run` 逻辑根 + RUSTC proxy harness 复跑：
  ripgrep 与 tokei 的原始 JSON 切片、稳定 compact languages 切片均 PASS，warmup 1、samples 3，且
  共同使用 release mirvm sha256
  `7b064b3f8861e39cfe07dd7df67583d73c0ec32bcd6108e12fb8e217c89da16e`。ripgrep native/mirvm
  median 分别为 52.10 ms/3.687 s；原始 tokei JSON 为 129.80 ms/3.796 s；覆盖 `tests/data`
  206 个 tracked fixtures 的 `tokei_languages` 为 148.10 ms/4.663 s，205 行 stdout 两侧
  SHA-256 均为 `0f5058901e0042629c9ff02714134206ca417235ebf6f892aeccec03a34d64c4`。
  这些 case manifest 与结果仍被 Git ignore，仓库尚未提交远程项目 case，
  因此这些结果仍不是持续 gate，也不能外推为“任意 Rust”。完整合同、revision 和证据层级见
  [real-projects.md](real-projects.md)。
- 本轮最终 `cargo fmt --all -- --check`、35/35 Rust tests、Clippy `-D warnings` 与 release build
  均通过。GitHub Actions workflow 已建立，执行 fmt、Clippy、Rust tests、gate harness、
  release build、独立可见的 TSan 与 runtime gates。
- 测试脚本必须以**可观察输出或不变式**为 oracle，不能把“双方都失败”或“仅退出码相同”当成功。

历史日志中的 “gate5 31/31” 只表示旧脚本口径，不再等同于 31 个语义正确断言。特别是：

- `c_signal` 曾在 handler 未执行时仍以退出码 0 被判绿；
- `diff_cargo.sh` 曾可能把 native 与 mirvm 同时失败判为 PASS；
- `corpus.sh` 曾可能在存在失败时仍以状态 0 结束。

这些发现促成了 M5.1 退出标准重写。最终结果已记录在 m5-log；不能把新 oracle 回写成旧日志
当时已经具备的事实。

## 4. 当前缺口与诚实边界

| 类别 | 当前状态 |
|---|---|
| signal / `sigaction`（M5.2 D8d，2026-08-13 重构） | **传统异步 signal 的多 Engine、真线程合同已闭合**：每次 guest handler 注册生成固定 22 字节 RX 桩；内核 frame 只做固定 TLS 读取和原子登记，不加锁、不分配、不运行 libffi/guest/展开。进程定向事件进入 owner `EngineControl` inbox（所属 Engine 的待处理信号箱）；`pthread_kill` 与真实 libc `raise` 产生的 `SI_TKILL`（内核的线程定向来源码）进入目标 pthread 按 registration generation 建立的稳定 cell，也就是“这条线程、这次 handler 安装”专用的槽，只能在目标线程安全点或退出收口时执行。阻塞的 `HostRaise`（MIRVM 承接的 `raise`）保留在内核，`sigwaitinfo` 仍看到真实 `SI_TKILL`；未阻塞的 `HostRaise` 返回前完成 handler。close 先关闭登记门、恢复正确 disposition，再等在途 frame、owner inbox 和已接收的目标线程事件，不能换线程代跑；若当前 pthread 自己持有待执行事件，`wait_closed` 返回 `ActiveOnCurrentThread` 而不自锁。pthread 退出把 glibc 全局第四轮 TSD 扫描和 signal cell 一起收口：按原始 key 号游标只补跑尚未扫描的值，最终阻塞可捕获信号、复查、关 inbox 并释放不能再获第五轮的残值。查询/`oldact` 返回 guest 地址，A/B 非 LIFO close 只摘本 owner。固定桩、registration 和 cell 保留到进程结束；owner 关闭后若原生代码回装旧桩，裸内核投递固定 `_exit(70)`，`HostRaise` 报 `EngineFault(70)`，不会挂起或误投。**仍拒绝**同步故障（SEGV/BUS/FPE/ILL/TRAP）guest handler、realtime 与 `SA_SIGINFO/SA_ONSTACK/SA_NODEFER/SA_RESETHAND`；进程定向外部信号只承诺在 owner Engine 下一安全点派送（open-issues R1/R21，decision-history §7.54-§7.55） |
| backtrace（M5.2 D8e） | **解释/JIT 混合帧与真符号均已支持**：解释帧记录合成 CFA，宿主 unwinder 收集已发布 JIT 码段的真 IP/CFA，两者按栈位置合并后交给 guest 回调。加载 Engine 时自动物化最小 ELF 符号镜像，标准 Rust backtrace 可把函数地址解析为原 rustc 符号并解码名称；`c_backtrace` 同时检查深度和 `c_backtrace::deep`。其余 `_Unwind_Set/GetGR/Resume/CFA-外` context 家族维持 `Unsupported`。`atexit` 已 builtin 化（引擎 LIFO + libc trampoline）；`dl_iterate_phdr` 仍走 native FFI |
| fork / exec（M5.2 D8f） | **exec 族直通**（进程替换语义正确）；**fork 仅 guest 单线程时放行**（守卫用 `/proc/self/task` 对 guest-main 基线判定，避 Ctx 计数 TOCTOU）——解锁 `Command::pre_exec`。多线程 fork、vfork/clone/setjmp 系维持响亮拒绝 |
| volatile | 独立 volatile IR 使用 alignment=1 的 opaque `MaybeUninit` 字节载体，不把 padding 解释成宿主整数。1/2/4/8/16-byte 保持单个后端 volatile 事件；更宽 memory-repr 值先快照，再按 16/8/4/2/1-byte 块分解，不承诺原子性。该结论于 2026-07-13 推翻旧“其他宽度 Trap/不得拆”选择，演变见 decision-history |
| direct dyn 尾字段 | sized prefix 后的 direct `dyn` 尾不能一律使用 lower 期静态 offset；当前从 vtable 读取运行期 alignment，并考虑 `repr(packed)` 上限后向上取整。slice/str 仍走静态公式，其他嵌套 DST 继续显式拒绝 |
| 128-bit `SwitchInt` | targets 与 discriminator 现都保留完整 128 位；i128/u128 discriminator 由 `SwitchDiscr::Wide` 从 place 读取，不再截成 u64。这不等于所有 128-bit ABI 形态都已标量化 |
| `track_caller` fn pointer | `ReifyFnPointer` 使用 rustc `resolve_for_fn_ptr`；需要 caller location 时由 Reify shim 以普通 fn-pointer ABI 接参并补 Location。ClosureFnPointer 等其他 adjustment 尚不能由此外推 |
| M5.1 收口 | 六个 release native-differential tracer 脚本通过，x86_vectors 内 pshufb/SHA 分别记账；numbigint、xgetbv、sha2、blake3、ecosystem 全部转绿。M5.1 旧前沿 expected-red 已删除，diff_cargo 3/3；signal/backtrace 两个历史 XFAIL 已由 M5.2 转绿（见本表前两行） |
| x86 向量 helper | pshufb128/256 与 SHA256 msg1/msg2/rnds2 已通过 tcx-free stdarch target-feature helpers 接入；m51_x86_vectors native 差分与 c_sha2 两个标准 SHA256 输出通过 |
| guest 静态归档 | Linux/ELF 受约束路径已接产品：收集 rustc `Static NativeLib`、内容寻址 `.a→.so`，作为 required library 在任何 dlsym 前以 `RTLD_NOW` 加载；失败保留 `dlerror` 并立即终止。constructor/destructor 已分治（§7.8：`.init_array/.fini_array/ctors/dtors` 段经 DT_INIT 与 native 同构放行；裸 `.init/.fini` 仍拒）；RTLD_DEFAULT 同名碰撞已改归档句柄优先（`fb0b204`，native 链接期绑定语义）。其余拒绝面仍在：非 PIC、thin、跨 archive 依赖/顺序/重名导出、export-symbols——多 archive link plan 未立项（[open-issues.md R6](open-issues.md)），不是通用链接器 |
| JIT | **生产已落地**：方法级 Cranelift JIT = M5.3 骨架 + M5.4a–d（标量/内存/128 位/原子/ABI 全形态/五调用助手/unwind 产品化双 CIE 全覆 LSDA/三表准入穷尽）+ M5.5 vmctx 终裁（T 骨架定稿），默认开启（`--jit off`/`MIRVM_JIT=off` 回退纯解释）。验证强度（2026-07-22 起）：`MIRVM_JIT_SYNC` 同步发布 + 可准入失败 RED。当前接受的边界是无 OSR/deopt/生产 tiering；Engine close 会停止并 join 编译 worker、释放 Shared，但已发布 JIT 机器码和系统展开器持有的 `.eh_frame` 保留到进程结束。纯性能优化候选统一归进行中的 D16，不再当作独立引擎缺陷 |
| 日志、事件与 profile | 1A、L1 与 P1 已完成：内部 HostSyscall 成对事件能写盘/离线检查，页池生命周期闭合；JIT 地址范围先成批进内存 registry 再发布入口，perf-map 只在显式 stop 边界于锁外输出，失败编译不泄漏未发布范围。当前 JIT 仍走通用 helper，获页 producer 固定双 4 KiB，子进程不会自动建立独立采集/map 代际，也没有 1B raw site 或 profile 命令。L2–L4、P2 与数据裁决仍待施，具体依赖见 [日志设计 §11](designs/mirvm_high_performance_log.md) |
| 生命周期/嵌入 | `Package::load` 安全地复制并验证不可变 v4 映像；每次 `unsafe instantiate` 创建独立 Engine。Engine 可 clone，显式 `close` 或最后一个 handle drop 发起 Running→Closing；执行租约与 `DeferredHold` 延迟持有保证活动调用、pthread start/TSD destructor 和暂停异常退出后才 Finalizing。逐实例 native ctor/fini、关闭中重入、同线程等待拒绝、长寿命宿主线程 `CtxSlot` 清空均已接通；ctor 受控异常转成 `Result` 失败，fini 逃出任何异常都固定诊断后 `abort`。`RunOutcome`/`RunErrorKind` 继续区分正常 101、main panic、关闭和引擎错误。**公开信任边界**：`instantiate` 仍须信任 native/FFI ABI；手工 Module 和 raw 两机器字 export 是 `unsafe`，`Shared` 不公开，尚无生成 safe typed export 绑定。任意第三方库保存的裸 callback 无普遍撤销事件，已发布 closure/JIT/MC/native 代码因此保留到进程结束；close 后只剩稳定 owner 墓碑，不持有整份 Shared。正式进程级故障隔离仍不在本项内 |
| C/C-unwind 异常 | **R18/E13 已闭合**：direct foreign、native fn pointer、callback/P1 入口都保全并消费源 ABI；普通 C 终止，C-unwind 保留 C++ 异常类型或 Rust panic payload，并在解释/JIT 帧执行 Drop；非 C/System ABI 响亮拒绝。guest panic 用 MIRVM 自有异常类标明身份和 owner，但内层 payload 仍由 guest std 捕获或清理，不另写 personality；解释器 raw catch 与 JIT landing pad 按当前异常指针分类，只有 `EngineFault` 对象跳过 guest cleanup。lower 精确标出的 `MainPanicBoundary` 及运行状态栈把真实 main panic 与正常 101 分开，并由解释器、JIT、pack 和 verifier 共同校验。标准 `runtime.c-unwind` **13/13**、`runtime.semantics unwind` **13/13** 覆盖两种执行模式；前者还包括 C++ typed exception 原样穿出整个 Engine，以及 C++ exception 到达 guest `catch_unwind` 时终止。详见 [designs/c-unwind-contract.md](designs/c-unwind-contract.md) |
| 分发与产品面 | `.mirvm` mode B 已实现到不稳定格式 v4：owned snapshot、逐函数索引/惰性驻留、真实热序预取、逻辑链接地址和可重复实例化已落地；包运行不依赖源码或预存自产库缓存。为满足 E20，首次 load 仍逐函数临时解码验证，档案直接借用验证尚未完成；跨 build_id/target 兼容、fat artifact 与格式冻结也未做。daemon、REPL、safe typed export、checked 模式、资源治理和正式沙箱仍未实现，当前只运行受信任代码。合并计划见 [product-capabilities-plan.md](designs/product-capabilities-plan.md) |
| 平台 | 当前仅应宣称 Linux/ELF/x86_64 开发基线 |
| Cargo wrapper / runner | MIRVM 占 Cargo 的 `RUSTC` 槽，用户普通/workspace wrapper 仍由 Cargo 按原生顺序从环境变量或 config 组合，MIRVM 在最内层捕获两层修改后的 rustc 参数。runner 在 lower 后用窄结构化 filter 去除额外 warning-count summary，但保留完整 driver 收尾，只在 compiler success 后执行 VM |
| 真实项目隔离 | build/check unshare network，所有 guest sandbox unshare PID，但不 unshare IPC；仅适用于受信任、固定 provenance 的 source，不是对抗性安全边界。构建期会安装记录的 rustc 环境，执行 guest 前再恢复调用者的 cwd 与完整运行环境。`MIRVM_ENCODED_RUSTFLAGS_APPEND` 按内容分 target store，暖缓存改值不会复用旧产物。正式资源隔离仍归 P3 OS worker |
| 任意 Rust / 真实项目 | 尚不支持任意 Rust；已有严格 real-project harness、两个项目的十一个 correctness PASS workload，其中三个完成 benchmark，以及通用修复的最小差分回归。case 仍是 Git-ignored workspace evidence，远程固定 case 尚未入库 |

架构目标不等于当前资格。尤其“RAM 运行参考实现”是长期语义契约；在上述已知缺口和有限 corpus
仍存在时，不应把它描述成已经覆盖完整 Rust 语义的成品。

## 5. 当前开发顺序

1. **产品能力顺序**（2026-08-13 汇总）：D17/D19 测试合同、D3 逐函数惰性驻留与
   Package v4 可重复实例化已经完成。后续档案直接语义验证需要新的偏移式只读表示，不能
   再沿用“预留 v4”旧称；完成后再评审 D4 格式冻结。OS 级沙箱按
   维护者本轮裁定暂缓，不进入当前施工链。其余阶段边界和验收见
   [product-capabilities-plan.md](designs/product-capabilities-plan.md)。
2. **日志采集主线**：L1 固定 4 KiB 硬页池、自动救援和 retire 已完成；**L2 已闭合
   （2026-09-18，§7.59/§7.60）**——MIRVM 服务线程自动登记（fork 守卫不再把 capture writer
   误判成 guest pthread）、fork 子代基线自愈、子代独立 `process_generation`（同时进入文件头与
   文件名）、fork 安全的 `RebuildRecipe`、裸 `SYS_fork` 覆盖、builtin 边界 hook、子进程
   producer 挂载、退出封页发布。端到端证据：`runtime.telemetry` 8/8——父
   `events-<pid>-0.mlog`（generation 0）与子 `events-<child pid>-1.mlog`（generation 1）各有
   自己的 committed 记录，`_exit` 路径如实保留可恢复 `.partial`。下一步是 L3 HostSyscall 直接
   热路与 trace JIT `r15`，随后 L4 stateless inline-asm raw site；L4 完成前不宣称首个内部
   syscall 纵切完成。注：当前只记录变参 `libc::syscall` 形态，`std::fs`/`Command` 走各自
   builtin，完整 syscall 覆盖属后续工作。

3. **profile 并行线**：P1 JIT 地址范围/perf-map 已完成；register 只改内存，
   显式 stop 先在锁内切 `Inactive` 并快照，再在锁外 write/flush。P2 Linux perf capture
   现可与 L2–L4 并行，必须报告权限、lost samples 和缺映射，且不切 trace 代码域；
   fork child 的 registry/map 重置与 L2 一起闭合。
4. **数据裁决与 D16 后续**：L1–L4/P2 完成后，在同一内存预算下裁定 4/16/64 KiB、硬池
   数字、writer 批量和 checksum，再实现 4→64 KiB 自动伸缩。随后用 profile 和相位账本
   裁定 JIT 码持久化、档案直接验证/装载、后台服务线程及 `-Cincremental`；已知
   `fib(32)` 约 97ms > 80ms 的 RED 不得靠放宽门槛关闭。三维逐字节差分铁律不动摇。
5. **诊断通道 D0（已完成）**：默认 `mirvm run` 的 fd2 合流与原始顺序不变；
   capture 从 command boundary 起把 compiler/frontend/lower 和 MIRVM control 逐字节 tee 到
   独立 diagnostics stream，child attached marker 避免重复路由，guest fd2 不进 router 或普通
   事件 ring。direct/cargoless/runner 及早期错误的逐字节合同 31/31 通过；P2
   复用这条边界。
6. **基建预算纪律**（根 AGENTS.md）：harness 只在当前产品 RED 无法复现/判定正确时
   做最小修改；不为未来加固。
7. **文档纪律**：完成阶段 = 代码 + 可复现 gate + 施工记录 + 本文更新四件套；
   新债入 open-issues.md，推翻入 decision-history.md，history/ 只读不再更新。
8. **远程项目与 GitHub Issues 暂停**至维护者明确恢复（open-issues G1）。

## 6. 完成一个阶段时如何更新

阶段完成必须同时留下四类证据：代码、可复现 gate、对应施工记录（history/ 日志或
decision-history 条目）、本文的状态变化。若结果推翻既有设计，还须在
[decision-history.md](decision-history.md) 记录旧选项为何曾合理、什么新证据触发了
改变，以及未来何时应重新评估；新产生的未解决债务登记到
[open-issues.md](open-issues.md) 对应分区。
