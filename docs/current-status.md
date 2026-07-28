# mirvm 当前开发状态

> 状态日期：2026-07-22（事实截至 HEAD `51cff03`+波3/波4 收尾；2026-07-22 外部审核
> （[history/development-status-audit-2026-07-22.md](history/development-status-audit-2026-07-22.md)）
> 驱动的稳定化战役已四波落地，decision-history §7.21）。本文是当前状态的唯一汇总入口；
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
| mode B（`.mirvm` 包 + pack/run + MC 机器码节） | **片②③完成（2026-07-23）** | 片②（`254692c`，§7.24）：包格式 v0 五节 + `mirvm pack`（强制全量冷路径单模块自包含）+ `mirvm run x.mirvm`（零新执行路径），五负载逐字节一致。**片③（`1697c82`，§7.25）：MC 机器码节 + 进程内 ELF 装载器**（自解析/自重定位/eh_frame/符号表，系统链接器零依赖）——**真自包含酸试通过**（删 global-asm 缓存后 faer 包仍逐字节跑通）；运行期对自产码零 cc/ELF/.so/缓存依赖，dlopen 仅剩 FFI 真外国库。格式声明不定死（冻结归 D4）；余量 = fat artifact、C12、MC 预消化形态 |
| **测试管线整顿（2026-07-23，decision-history §7.26）** | **完成** | `tests/corpus.manifest` **唯一真源**（tier/timeout/mode/env/needs/xfail 六列；164 脚本驱动全接线 = 137 full + 24 smoke + 3 manual——**双名单漂移实锤：13 个批6 条目两边都没接线**）+ `tests/run.sh` 分层入口（fast 逢提交 / smoke 批次级 / gate 收尾级）+ 时间/空间/cache 三维计量与磁盘护栏（lib.sh disk_guard + target 预算闸）+ 代号名退役（m4_/m5_/m51_ → gate.sh/runtime_gates.sh/probes.sh/perf.sh）+ real_projects 重型 harness 挪 tests/parked/。**corpus/projects/ 真 cargo 项目形态**：hexyl 0.17.0 + tokei 14.0.0 vendor（.crate 全量解包 + sha256 钉），manifest `mode=diff` = mirvm warm 三维 == native 三维 + warm stdout == cold stdout；两机制实锤（cargo 回放缓存告警 → 双侧 `--cap-lints allow` 同帽；mirvm 项目模式 guest cwd=项目目录 → `{ROOT}` 占位绕行，产品侧对齐待裁定 = [open-issues.md E36](open-issues.md)）。验收：gate_truth 12/12、run.sh fast 7/7、gate.sh 排练 17/0/1/0、**gate.sh 全量 179/0/0/0**（jiff_time 钉版后复绿，corpus.md 批11） |
| **D15 P1（砍 cargo 之解析地基，2026-07-27，decision-history §7.27/§7.28）** | **完成** | `src/cargoless/` 五件：manifest（Cargo.toml 模型 + cfg 平台求值 + frontmatter 伪包）/ lockfile（v1–v4 读写 + canonical v4）/ registry（自有 store + 读穿 cargo 缓存只读 + sparse index + .crate sha256 自实现校验 + 解包防护）/ resolve（lock/pubgrub 双模式 + feature 统一 + 单元装配）/ audit（`mirvm deps audit`：项目等值对账 + 脚本 cargo `--locked --offline` 验收链 + manifest needs/env 联动）。**cargo 解析语义全实证定稿**（resolve 图全平台并集 ∪ build 图 host 过滤、lazy-bucket 多版本 fork（hashbrown 0.14/0.15 类）、optional 门按（父包,父版本,依赖键）、?/ 弱引用级联（rust_decimal→borsh→bytes 实锤）、pre 精确规则、exact 钉兼容 build 元数据、lock canonical 尾逗号、同名多 req 条目分立、rename 双路匹配）；上游破洞六枚钉版（均验证 cargo 自家 fresh 同撞）。29 单测绿 + corpus 全量 audit 166 目标绿（wincode git 源 = P5 合法响亮拒绝）。P2（调度 + build.rs + proc-macro）待施；rust-version-aware 偏好记边界 |
| **D15 P2（砍 cargo 之编译调度，2026-07-27，decision-history §7.29）** | **完成** | cargoless 新增 schedule/buildrs/driver：`MIRVM_DEPS=self` 的 `mirvm run` 全程零 cargo——自排拓扑、自算每 crate rustc 参数（内容指纹含传递传播不变量）、proc-macro 闭包 ∪ build-deps 闭包真 rustc host 真 codegen（proc-macro 五钉/host rlib 形态全探针实锤）、build.rs 编译→执行→指令传播全生命周期（`-l` 只进本包、`-L` 传递、metadata 只给直接依赖者、无自动 DEP_*_ROOT、无自动 check-cfg 补钉——全按 probe_link 实证）。**闭合验收：corpus smoke 24 双腿（cargo vs self）stdout/stderr/exit 逐字节 24/24**；diff_cless 六夹具（PATH 只含 mirvm + 离线实证零 cargo 进程）；E36 以构造闭合（guest cwd = 调用者 cwd）。对拍暴露四枚修复当日落地（含 cargo 腿既有 bug：phase_wrapper 劫持 rustix 1.1.4 RUSTC_WRAPPER 探针竞态 EPIPE）。P3（rerun-if 精细增量 + full 层迁移 + 双轨 gate + 并行调度）待施；RUSTFLAGS/config rustflags 子集已由切⑤a 落地（见下行） |
| **D15 P3 切⑤a（rustflags 子集接入 self 调度，2026-07-28）** | **完成** | cargoless 新增 rustflags 模块：`CARGO_ENCODED_RUSTFLAGS`（\x1f 分）> `RUSTFLAGS`（空白分）> config（`target.<triple>.rustflags` > `target.'cfg(all())'.rustflags` > `build.rustflags`，字符串/数组都收；项目根逐级向上 + $HOME 兜底、最近者胜不合并）——实证定稿（probe_buildrs + `--target`，13 条 rustc 调用行逐类核对）：**rustflags 只落 target 单元**（dep/bin 参数串末尾追加，后旗压前旗），host 侧（build.rs 编译/proc-macro/host dep）一律不吃（三个参数函数签名不含 rustflags，编译期保证）；指纹全 unit 统一吃（host 侧跟随失效无害，v1 从简）。验收连带修出两枚 full 层迁移缺口当日落地：① crates.io 归一化 `proc_macro` 下划线拼写（derive_arbitrary 1.3.2 实锤，新归一化产物）被误当 target dep 编——manifest/resolve 双侧收两种拼写；② 根包 `[lib]`+`[[bin]]` 双 target（hexyl 实锤：bin 隐式依赖同名 lib）——root_lib_rustc_args 经 __cless-dep 编 target rlib、bin 会话补 --extern（cargo build -v 实证形态：根 lib --extern 指 .rmeta、bin 侧指 .rlib、path 包无内置 --cap-lints）。**闭合验收：corpus full 层 hexyl/tokei 双腿（`env=RUSTFLAGS=--cap-lints allow`）stdout/stderr/exit 逐字节 2/2**；cargo test 136/136、diff_cless 6/6、run.sh fast 8/8。边界记档：HOST_RUSTFLAGS 不实现；config 多文件合并不做、无扩展名 `.cargo/config` 旧形态不读、`cfg(all())` 外的 cfg 表达式不求值；根 proc-macro lib + bin 组合响亮拒绝（P5）。P3 余量：rerun-if 精细增量（切⑤b 已落地，见下行）、full 层迁移其余面、双轨 gate、并行调度（切⑤c 已落地，见下行） |
| **D15 P3 切⑤b（build.rs rerun-if 精细增量，2026-07-28）** | **完成** | build.rs 重跑判定对齐 cargo（buildrs::should_rerun + 存档 output.txt/rerun.txt 落 `build/<pkg>-<fp>/`）：默认面（未发 rerun-if-changed）registry 包**永不重跑**（源按 cksum 不可变，最大收益面）、path/根包按包树折叠快照（source_stamp 同款，排除 target/.git）；changed 面只盯发出的 PATH 的 (len,mtime_ns)（缺席按变化计）；rerun-if-env-changed 比现在进程值；links 传递 = 直接依赖中带 links 的包本次重跑 ⇒ 本包也跑（DEP_* 只给直接依赖者）；fp 变 ⇒ 新 fp 目录存档天然缺席 ⇒ 重跑（「源/旗/依赖变 ⇒ 重跑」由指纹先行免费覆盖，rerun 存档主战场 = fp 不变时的 skip/run 决策）；存档缺一/损坏 ⇒ 重跑自愈。未触发 ⇒ 跳过执行，从 output.txt 重新 parse_instructions 回放 BuildOutput（指令流零序列化失真：DEP_*/OUT_DIR/rustc-env/warning 与重跑逐字节一致——夹具 BR_TOGGLE 面实证输出变与不变两态）。观测：`MIRVM_DEBUG_BLDRS=1` 打 `bldrs run|skip <pkg> <原因>`。**验收：判定矩阵单测 9 条全绿（cargo test 145/145）、tests/bldrs_rerun.sh 21/21（全 run→全 skip→touch 触发→env 变/不变五态 + registry libc skip）、diff_cless 6/6、corpus smoke 24 双腿逐字节 24/24**；registry build.rs 第二腿全 skip 实测提速（deps/ir 清、target store 热的第二腿墙钟：libgit2 20s→0s、tree_sitter 8s→0s、blake3 2s→1s——build.rs 内 cc 重编译全省）。P3 余量：full 层迁移其余面、双轨 gate、并行调度（切⑤c 已落地，见下行） |
| **D15 P3 切⑤c（编译调度并行化，2026-07-28）** | **完成** | schedule 新增 run_scheduler（Kahn 就绪队列 + N worker，std-only）：unit 的全部依赖「完成」（build.rs 生命周期 + host/target 编译按集合归属全结束）即就绪，worker 把领到的 unit 的完整流水线跑完；`MIRVM_CLESS_JOBS` 覆盖并发度（缺省 available_parallelism；**=1 与旧串行 topo 序逐位一致——对拍调试锚**）。完成表（BuildOutputs/re_ran）只归主线程——worker 开工所需的依赖侧输入（DEP_* env、传递 -L 汇集、links 重跑名单）由主线程在派发时算好捎进 WorkMsg（此刻依赖必已完成，与串行在 unit 开头算的逐位相等），零锁；同 fp 重复 unit（同包同版本同 feature 的 Normal/Build 双 unit——fp 不含 class 会撞产物名/build 目录）由 FpLocks 把整个流水线互斥，后到者全命中跳过，与串行同效。失败语义：记第一枚错误、停发新活、在飞汇合后响亮点名（文案与串行同形）；根包阶段（根 build.rs、根 lib、bin 会话）照旧汇合后主线程。**验收：调度器单测 4 条（依赖序保持/全完成/失败首枚保留/jobs=1==topo 序）+ cargo test 149/149 + clippy/fmt 净 + diff_cless 6/6（默认并行与 jobs=1 双态）+ bldrs_rerun 21/21 + corpus smoke 24 双腿逐字节 24/24 + run.sh fast 9/9**；8 核冷跑（purge --target+--deps --ir）墙钟：libgit2 19s→11s、wasmtime_wat（178 crate 闭包）152s→79s，且 jobs=1 vs 默认 stdout/stderr 逐字节同。P3 余量：full 层迁移其余面、双轨 gate |
| **D15 P3 切⑤d（full 层迁移 + 双轨 gate，2026-07-28，decision-history §7.30）** | **完成** | corpus full 层 137 条目双腿对拍（`tests/corpus_deps_pair.sh`）120/19 起，分诊修复四枚当日落地：① **extern 命名无 rename 时按 dep 包 lib target 名**（tendril→new_debug_unreachable（[lib] name="debug_unreachable"）等 9 条目，cargo -v 实证行）；② **StrongDep 强形在有同名显式 feature 定义时被 dep: 遮蔽也置旗**（zerotrie 0.2.4 的 serde=[dep:litemap, litemap/serde] 配显式 litemap 定义，合成探针三态定稿：dep: 永不置、x/y 有显式定义置、无定义未遮蔽走隐式旗）；③ **build script env 补 CARGO_MANIFEST_LINKS**（ring 0.17.14 build.rs:287 unwrap 实锤，cargo 文档「the manifest links value」）；④ **bin 会话 --remap-path-prefix=<根包目录>/= + 脚本正文物化改 <cache>/src/main.rs**（file!()/panic Location 路径形态与 cargo 逐字节；from_frontmatter_at 解耦 root 与 body）。**P5 单列制度化**：对拍轴遇「归 P5」响亮拒绝且 cargo 腿通过时单列 p5 计数（mirvm deps audit 同款先例；miden_prove 的 wincode git 源归列）。**双轨接线**：diff_cargo.sh 恒钉 MIRVM_DEPS=cargo（compat 轨不缺席）、gate.sh 头注 DEPS 轴。**闭合验收：corpus_deps_pair --tier full 138 pass 1 p5 0 fail**；cargo test 153/153、run.sh fast 9/9、gate DEPS=self（SKIP_TSAN=1）**177 pass 1 p5 1 fail**——唯一 fail = runtime_gates 纯度门禁（harness 编译 mirvm-tsan，被工作树内并行的 mirvm_log 重构卡住，与 D15 无关，重构收尾后回启复验）；hexyl/tokei native 基线修复 = gate.sh 显式钉 RUSTC（rustup 代理按 cwd 解析落仓外 default stable 的混合工具链实锤）。P3 整期收口，P4（sysroot 自管 + MIRVM_DEPS 默认翻 self + compat 评审）待施 |
| 地址模型 P2（GOT 间接） | **完成（2026-07-17）** | §7.5b 手术单定场（真实地址模型保留）→ §7.5c 零 IR 变更 GOT 机制（槽 = 冻结区普通格 + 启动相重填；extern static/fn 值不再烤宿主地址，字节码复用 `Mem{Static(槽)}`/`SubImm` 通道，JIT/interp 零改动）→ §7.5d 拒缓存三判据全退役 + 纯 std 会话 want_split 修正（先存 A2 沉默债：L2 对纯 std 程序永 miss）。外来符号用例冷→热全通（c_process 463→30ms），gate5 117/0/0。JIT 间接调用准入记债（[open-issues.md E1](open-issues.md)） |
| 地址模型 P1（fn 条目可执行化） | **完成（2026-07-17，commit `4202317`）** | §7.6：FFI 可派生条目值 = 可执行 stub 码址（新第三固定地址域族 0x6C00/0x6D00/0x6E00+k + libffi closure 蹦床 + 配方随模块、启动相重建封存 RX）——thunk 盲区结构性根治（旧 debt §6 关闭，对照见 [open-issues.md](open-issues.md)；负对照 flate2 C-libz 结构体内嵌回调往返，三维+L2 热一致）。残余边界 = 签名不可派生条目（Rust ABI/聚合/变参）保持数据槽，无实质盲区；SIGSEGV 诊断化可选后补（open-issues T4）。gate5 117/0/0 |
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
  → Shared（进程期）+ 每个宿主线程一个 Ctx
  → interp_frame / run_blocks
      guest 调用：宿主递归
      guest → native：dlsym + libffi
      native → guest：libffi closure thunk + TLS attach
      inline asm：调用已物化的 fn(*mut u8) stub
```

代码热点集中在 `src/lower/func/`（调用/调用约定降低）、`src/vm/engine/interp/`
（解释器主循环）与 `src/vm/engine/jit/translate.rs`（JIT 翻译器，E34 记治理）。
实现目前实质上是 Linux/ELF/x86_64 优先：依赖 pthread、dlopen、GNU 链接行为和
x86 asm wrapper。

## 3. 已验证边界

- **当前验证矩阵（2026-07-23 实跑）**：`cargo fmt --all -- --check` 绿、
  Clippy `-D warnings` 零诊断、`cargo test --locked` **76/76**、
  `tests/diff.sh` **45/45**（默认与阈值=1 双态）、**+MIRVM_JIT_SYNC 同步发布
  45/45**（可准入函数首调同步编译并真跑机器码，编译失败 RED——audit F-05 起
  threshold=1 的证明力升级）、`tests/diff_cargo.sh` 5/5、
  `tests/gate_truth_regression.sh` 12/12、`tests/run.sh fast` 7/7、
  `tests/gate.sh` **179/0/0/0**（corpus 全防线 163 条目 = 161 脚本（smoke+full
  两层）+ hexyl/tokei 项目三维对拍；fib(32) JIT 62ms ≤80ms 硬门在列）、
  TSan 零警告。测试管线 2026-07-23 整顿（decision-history §7.26）：套件入口
  `tests/run.sh fast|smoke|gate`，corpus 唯一真源 `tests/corpus.manifest`。
  CI（`.github/workflows/ci.yml`）同构命令全绿（GitHub 侧操作暂停，本地同构为准）。
  **以下为各阶段 dated 历史记录**（保留备查，非本轮复跑）：
- debug/release 构建可通过；执行必须优先使用 release 版本。
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
- 产品 Cargo shim 会在接管 Cargo 前检查非空 `RUSTC_WRAPPER` / `RUSTC_WORKSPACE_WRAPPER`，以及
  有效的 `build.rustc-wrapper` / `build.rustc-workspace-wrapper`；发现任一自定义 wrapper 就
  fail-closed。当前明确**不支持 wrapper composition**，不能把拒绝误写成项目 wrapper 已兼容。
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
| signal / `sigaction`（M5.2 D8d） | **async 信号 guest handler 已支持**：经 AS-trampoline（复用 M4.4 thunk 工厂，`(i32)->void`）真执行，signal() 与 sigaction() 两条注册路，重入（handler 内嵌套 raise）已 native 差分。SIG_DFL/IGN 直通。**sync 故障信号（SEGV/BUS/FPE/ILL/TRAP）guest handler 仍响亮拒绝**（宿主/guest 故障不可分辨）。c_signal 转绿 |
| backtrace（M5.2 D8e） | **guest 影子帧栈已支持**：Ctx 维护每帧合成 IP，`_Unwind_Backtrace/GetIP(Info)/FindEnclosingFunction/GetCFA` 由影子帧诚实回答。合成 IP 不经 dladdr 符号化（诚实 `<unknown>`，不伪造宿主符号）→ backtrace 文本非 well-defined，oracle 是影子帧不变式（捕获/非空/深度反映）。c_backtrace 转绿。其余 `_Unwind_Set/GetGR/Resume/CFA-外` context 家族维持 `Unsupported`。`atexit` 已 builtin 化（引擎 LIFO + libc trampoline）；`dl_iterate_phdr` 仍走 native FFI |
| fork / exec（M5.2 D8f） | **exec 族直通**（进程替换语义正确）；**fork 仅 guest 单线程时放行**（守卫用 `/proc/self/task` 对 guest-main 基线判定，避 Ctx 计数 TOCTOU）——解锁 `Command::pre_exec`。多线程 fork、vfork/clone/setjmp 系维持响亮拒绝 |
| volatile | 独立 volatile IR 使用 alignment=1 的 opaque `MaybeUninit` 字节载体，不把 padding 解释成宿主整数。1/2/4/8/16-byte 保持单个后端 volatile 事件；更宽 memory-repr 值先快照，再按 16/8/4/2/1-byte 块分解，不承诺原子性。该结论于 2026-07-13 推翻旧“其他宽度 Trap/不得拆”选择，演变见 decision-history |
| direct dyn 尾字段 | sized prefix 后的 direct `dyn` 尾不能一律使用 lower 期静态 offset；当前从 vtable 读取运行期 alignment，并考虑 `repr(packed)` 上限后向上取整。slice/str 仍走静态公式，其他嵌套 DST 继续显式拒绝 |
| 128-bit `SwitchInt` | targets 与 discriminator 现都保留完整 128 位；i128/u128 discriminator 由 `SwitchDiscr::Wide` 从 place 读取，不再截成 u64。这不等于所有 128-bit ABI 形态都已标量化 |
| `track_caller` fn pointer | `ReifyFnPointer` 使用 rustc `resolve_for_fn_ptr`；需要 caller location 时由 Reify shim 以普通 fn-pointer ABI 接参并补 Location。ClosureFnPointer 等其他 adjustment 尚不能由此外推 |
| M5.1 收口 | 六个 release native-differential tracer 脚本通过，x86_vectors 内 pshufb/SHA 分别记账；numbigint、xgetbv、sha2、blake3、ecosystem 全部转绿。M5.1 旧前沿 expected-red 已删除，diff_cargo 3/3；signal/backtrace 两个历史 XFAIL 已由 M5.2 转绿（见本表前两行） |
| x86 向量 helper | pshufb128/256 与 SHA256 msg1/msg2/rnds2 已通过 tcx-free stdarch target-feature helpers 接入；m51_x86_vectors native 差分与 c_sha2 两个标准 SHA256 输出通过 |
| guest 静态归档 | Linux/ELF 受约束路径已接产品：收集 rustc `Static NativeLib`、内容寻址 `.a→.so`，作为 required library 在任何 dlsym 前以 `RTLD_NOW` 加载；失败保留 `dlerror` 并立即终止。constructor/destructor 已分治（§7.8：`.init_array/.fini_array/ctors/dtors` 段经 DT_INIT 与 native 同构放行；裸 `.init/.fini` 仍拒）；RTLD_DEFAULT 同名碰撞已改归档句柄优先（`fb0b204`，native 链接期绑定语义）。其余拒绝面仍在：非 PIC、thin、跨 archive 依赖/顺序/重名导出、export-symbols——多 archive link plan 未立项（[open-issues.md R6](open-issues.md)），不是通用链接器 |
| JIT | **生产已落地**：方法级 Cranelift JIT = M5.3 骨架 + M5.4a–d（标量/内存/128 位/原子/ABI 全形态/五调用助手/unwind 产品化双 CIE 全覆 LSDA/三表准入穷尽）+ M5.5 vmctx 终裁（T 骨架定稿），默认开启（`--jit off`/`MIRVM_JIT=off` 回退纯解释）。验证强度（2026-07-22 起）：`MIRVM_JIT_SYNC` 同步发布 + 可准入失败 RED。未实现：OSR/deopt/生产 tiering（E2）、JIT 码常驻（E5）、优化项池（E7：SIMD CLIF 向量内联/PLT try_call 快路/内联缓存等） |
| 生命周期/嵌入 | `Shared`、thunk、asm handle、部分 TLS 存储按进程期保存；错误路径可退出进程；runner 诊断 filter 依赖进程全局 `TRACK_DIAGNOSTIC`，目前只在单 compiler CLI 模型下安全。daemon/嵌入/并发 compiler 需先加 guard 或替代接口；尚非稳定多 Engine API |
| 分发与产品面 | `.mirvm` mode B、daemon、REPL、checked 模式、正式沙箱均未实现；分发轨方向已批未立项（D9，2026-07-14，[distribution-design.md](designs/distribution-design.md)）：先 L2 engine-IR 缓存，mode B=缓存可移植化（M5.3 后），发行先 miri 式 |
| 平台 | 当前仅应宣称 Linux/ELF/x86_64 开发基线 |
| Cargo wrapper / runner | 非空 `RUSTC_WRAPPER` / `RUSTC_WORKSPACE_WRAPPER` 或有效的 `build.rustc-wrapper` / `build.rustc-workspace-wrapper` 当前都会在 MIR capture 前 fail-closed；尚无 wrapper composition。runner 在 lower 后用窄结构化 filter 去除额外 warning-count summary，但保留完整 driver 收尾，只在 compiler success 后执行 VM |
| 真实项目隔离 | build/check unshare network，所有 guest sandbox unshare PID，但不 unshare IPC；仅适用于受信任、固定 provenance 的 source，不是对抗性安全边界。mirvm build env 与 guest runtime env 尚未彻底分离。`MIRVM_ENCODED_RUSTFLAGS_APPEND` 在 Cargo 完成 fingerprint 计算后才追加，尚未进入 Cargo fingerprint |
| 任意 Rust / 真实项目 | 尚不支持任意 Rust；已有严格 real-project harness、两个项目的十一个 correctness PASS workload，其中三个完成 benchmark，以及通用修复的最小差分回归。case 仍是 Git-ignored workspace evidence，远程固定 case 尚未入库 |

架构目标不等于当前资格。尤其“RAM 运行参考实现”是长期语义契约；在上述已知缺口和有限 corpus
仍存在时，不应把它描述成已经覆盖完整 Rust 语义的成品。

## 5. 当前开发顺序

1. **corpus 扩编继续**：三维逐字节差分铁律不动摇（mirvm 默认 / native / 逢调即编，
   全部逐字节一致才算绿）；新候选见 [corpus.md §7](corpus.md)；撞出的实锤债务登记到
   [open-issues.md](open-issues.md) 再按优先级转正。
2. **当前战役**：mode B 片②③已落地（§7.24/§7.25——包格式 + pack/run
   + MC 机器码节进程内装载，真自包含酸试通过）。**下一战役候选**：
   **砍 cargo（D15，已纳入日程，之后要做）**；D4 对外格式冻结评审
   （mode B 三片后触发器已响）；E6 性能轴继续待固定 workload 收益证据。
3. **基建预算纪律**（根 AGENTS.md）：harness 只在当前产品 RED 无法复现/判定正确时
   做最小修改；不为未来加固。
4. **文档纪律**：完成阶段 = 代码 + 可复现 gate + 施工记录 + 本文更新四件套；
   新债入 open-issues.md，推翻入 decision-history.md，history/ 只读不再更新。
5. **远程项目与 GitHub Issues 暂停**至维护者明确恢复（open-issues G1）。

## 6. 完成一个阶段时如何更新

阶段完成必须同时留下四类证据：代码、可复现 gate、对应施工记录（history/ 日志或
decision-history 条目）、本文的状态变化。若结果推翻既有设计，还须在
[decision-history.md](decision-history.md) 记录旧选项为何曾合理、什么新证据触发了
改变，以及未来何时应重新评估；新产生的未解决债务登记到
[open-issues.md](open-issues.md) 对应分区。
