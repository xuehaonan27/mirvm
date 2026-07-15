# M6 施工日志 —— 轨 C 分发与产品面（D9）

> 设计与决策：[distribution-design.md](distribution-design.md)（D9a–D9f，方向批准 2026-07-14）。
> 本期立项 2026-07-14：用户指令"先做分发轨①②，M5.3 之后再说"——即 D9f 施工顺序的
> ① 相位计时 + ② L2 post-mono engine-IR 缓存；③④⑤ 未立项。
> 纪律同前：逐片提交、全量 gate5 绿、偏离记 decision-history、不新增 harness 机制。

## 片 1：相位计时（D9f①）

**改动**：`src/cli.rs` —— `MirvmCallbacks` 挂 `PhaseTiming`；frontend =
run_driver 进入→`after_analysis` 到达（含依赖 metadata 加载），lower =
`lower_program` 全程（mono 收集+降低+冻结物化），engine = `run_vm_engine`
guest 执行段。输出 = stderr 单行 `mirvm-timing: frontend=… lower=… engine=…
total=…`，仅 `MIRVM_TIMING=1` 或 `--vm-stats` 时输出（stderr 参与 native 差分
逐字节比对，默认必须零噪声）。`--vm-stats` 分支不跑 guest，engine 段不报。

**实测账本（EPYC 7773X，release，热 sysroot/热依赖）**：

| 负载 | frontend | lower | engine | total |
|---|---|---|---|---|
| `demo/args_env.rs`（std-only） | 17.0ms | 322.0ms | 1.8ms | 356.1ms |
| `demo/ecosystem.rs`（serde+serde_json+rand+regex） | 76.2ms | 1295.5ms | 1399.1ms | 2822.9ms |

**修正 distribution-design §2 的推断**：ecosystem 的 ~3s 并非全是加载相——
**近半（1.4s）是解释执行本身**（30–70× native 的解释开销，那是 M5.3 JIT 的地盘）。
L2 缓存的可吃部分 = frontend+lower ≈ 1.37s（ecosystem）/ ≈339ms（args_env，占 95%）。
结论仍成立但期望校准：L2 对 std-only 小程序是 ~20× 启动改善，对重执行负载是
"砍一半"；另一半等 JIT。**调研先行再次自证**：没有这张账本，片 2 的验收指标会定错。

**验收**：gate5 全量绿（46 PASS / 0 XFAIL / 0 FAIL 口径不变——本片零语义改动，
仅默认关闭的仪器输出）。

## 片 2：L2 post-mono engine-IR 缓存（D9f②）

**机制**（distribution-design §4 落地）：

- **冻结区固定基址**（`frozen.rs`）：`FROZEN_FIXED_BASE = 0x6800_0000_0000` +
  `MAP_FIXED_NOREPLACE`。冻结区内嵌绝对地址（fn 条目、statics 互指、字节码 const、
  fn_addrs 键）跨进程稳定 ⇒ 整包可序列化——**JVM CDS 同思路（映射偏好地址），
  替代设计稿"载入时重建指针"的重定位路线**（偏离已记 decision-history §7.1）。
  基址被占（并发单测/罕见 ASLR 冲突）→ 响亮回退动态基址，本进程照常运行仅不缓存；
  恢复失败 = miss，绝不在错基址重放快照。
- **argv 出快照**（`Module::finalize_entry_argv`，从 lower 迁出）：argv 是运行期
  输入，冷/热每次运行在快照语义之后追加分配并回填 EntryPlan——单一路径防冷热漂移。
- **ir.rs 全类型 serde 化**（41 个 derive + `Builtin::Unsupported` 改
  `StaticStr` newtype 手动 serde——裸 `&'static str` 会让 serde 给容器推导
  `'de: 'static` 借用约束）；`Module.asm_sites` 新增 = asm-stub 物化配方，
  warm 路径幂等重物化覆写陈旧真地址（.so 内容哈希缓存命中则仅 dlopen+dlsym，
  被清则重 cc 自愈）。序列化格式 = postcard（serde 二进制，新依赖）。
- **键与输入清单**（`src/ircache.rs`）：键 = fnv(MIRVM_BUILD_ID, rustc_args)，
  条目头存完整 args 回比（碰撞免疫）。MIRVM_BUILD_ID = build.rs 对 src 树 +
  Cargo.lock 内容哈希（任何引擎源变更 ⇒ 全部失配重建）。清单口径 = **rustc 自身
  dep-info 同构**（rustc_interface::passes）：本地源文件（source_map 非 imported）
  + include! 追踪（sess.file_depinfo）+ 全部上游 crate 工件（used_crate_source，
  含 sysroot std rlib ⇒ sysroot 变更天然失配）+ `env!` 依赖（sess.env_depinfo，
  精确到变量与编译时值）。文件校验 = (size, mtime_ns)，cargo 指纹同保真度。
- **告警不入缓存**（施工中发现的真语义问题）：warm 路径跳过 rustc 会话，无法重演
  编译诊断；静默吞告警违反 run-from-source 语义（native 差分口径 = 每次新鲜编译
  必发告警）。会话级告警计数钩经 `psess_created` 安装（rustc_interface 的
  setup_callbacks 会覆写 TRACK_DIAGNOSTIC 喂增量系统，psess_created 在其后、
  首次解析前触发，链式委派前钩）——**有告警/错误的会话不 store**：告警程序每跑
  冷路径重演，零告警程序才享受缓存。诚实边界，不做诊断回放（升级路径留 M6 后续）。
- **environ 类宿主地址直嵌不入缓存**（gate 抓获的真 SIGSEGV，第二个不可缓存类）：
  非 weak extern static（`environ` 等数据符号）在 lower 期 dlsym，**宿主 libc 真
  地址**直接烤进字节码 const 与冻结区重定位——`&environ` 语义要求就是 libc 变量
  本体地址，固定基址救不了它。ASLR 下热回放上个进程的地址 = 野指针：gate corpus
  的 c_process（`std::process::Command` 读 environ）冷跑过、热跑 SIGSEGV（cargo
  `--quiet` 还把信号死亡吞成静默 exit=1，strace 定位）。修复 = lower 登记
  `Module.foreign_static_syms`，**非空即拒 store**——含此类符号的程序每跑冷路径，
  语义精确无损。升级路径 = GOT 式 Operand 间接（IR 设计变更，M6 后续）。
  覆盖注记：`std::env::var()` 走 getenv（函数 FFI，安全可缓存）；`env::vars()`
  迭代与 Command 环境捕获走 environ static（触发不缓存）。
- **幂等脚本物化**（顺带修复）：materialize_script 原每跑无条件重写
  Cargo.toml/main.rs ⇒ mtime 漂移使清单必失配（也扰动 cargo 指纹）；改
  write_if_changed。
- **失效自愈**：required .so 缺失 / 清单失配 / 基址被占 / build id 变 → miss →
  冷路径重建覆写。`MIRVM_NO_IR_CACHE=1` 全程旁路。条目无逐出（v1 登记：
  ~/.cache/mirvm/ir 手动清理）。

**账本（EPYC 7773X，release）**：

| 负载 | 冷（含 store） | 热 | 加载相收益 |
|---|---|---|---|
| args_env（std-only） | 373.9ms（store 13.8ms） | **32.8ms**（load 31.4ms + engine 1.5ms） | 339ms → 31ms，**11×** |
| ecosystem（4 依赖） | 2967ms（store 46.7ms） | **1471ms**（load 91.5ms + engine 1379.6ms） | 1396ms → 92ms，**15×**；总时长砍半，余下是解释执行（M5.3 JIT 的地盘） |

条目尺寸：args_env ≈2.2MB，ecosystem ≈8.0MB。

**验收**：cargo test 39/39（新增 frozen 定基往返、ircache 盖戳/env 匹配 3 测）；
diff.sh 30/30 **连跑两遍全绿**（新增 L2 warm 复跑校验：第二次运行命中缓存后
stdout/退出码/stderr 仍须与 native 三项一致——防"缓存回放旧语义"假绿；ptr_int
的 dead-code 告警曾使首版假 hit 丢告警被本通道当场抓获，即"告警不入缓存"的来源）；
diff_cargo 5/5（warning_return 每跑重演告警 ✓）；gate_truth_regression 12/12；
fmt/clippy -D warnings 零告警。

## M6 ①② 验收（2026-07-14）

**gate5 = 46 pass / 0 expected-red / 0 skip / 0 fail**（全量：corpus 30 + diff.sh
30（含 warm 复跑维度）+ diff_cargo 5 + 六 m51 tracer + 加载相/rayon 性能门 +
gate0–4）。两个不可缓存类（告警、environ 类宿主地址直嵌）均由既有 gate 通道当场
抓获并以"诚实不缓存"落契约——防静默错值纪律在缓存层的直接兑现。轨 C ③④⑤
（依赖剪 codegen、mode B 打包、发行）未立项；下一阶段默认 = M5.3 方法级 JIT
（用户拍板）。

## 片3 前置调研：冷启动/lower 解剖（2026-07-14，只调研不施工）

用户裁定：**M5.3 Pending**——上一节"下一阶段默认 = M5.3"作废；冷启动（lower 相）是
当前一等问题，先调研后施工。全文（方法、数据、杠杆清单与建议顺序 S1–S4）见
[coldstart-research.md](coldstart-research.md)。要点：

- 单文件冷启动 ~430ms 墙钟：**lower ~310ms 近常数**（std 税；T_lower ≈ 0.10ms ×
  instance 数，fib 3027 个与 ecosystem 13209 个完全线性），frontend 仅 15–37ms，
  启动仪式 ~55ms（ensure_sysroot 每跑 spawn 两个 rustc 子进程——**warm 也在付**）。
- perf 归因：lower 相 ~75% 耗在 rustc 机器（查询/interning 27.5% + rmeta 解码 10.5% +
  其内存代谢 kernel 35% + malloc 10%），**我方发射代码仅 6.2%**——优化方向只能是
  "少碰 rustc"（懒降低/预降低底座）或"并行碰"（-Zthreads）。
- **执行集 ≪ 降低集**（临时探针实测）：fib 230/3027（7.6%）、threads_channel
  493/3467、ecosystem 3840/13209（29%）——急切降低做了 3–13× 多余工作。
- cargo 形态全冷 10.6s = **7.7s 依赖构建 + 2.9s runner**；target 依赖今天跑完整
  codegen+link（`ar t` 实证 `.rcgu.o`），mirvm 只消费 rmeta MIR——D9d 剪枝标的实证。
- **L2 账本复核有效**（ecosystem 干净环境 store 45ms / warm load 97ms，与片2 账本
  一致）。调研顺带抓到三个缓存盲区（在案待修，见调研文 §5）：**P1** runner 把构建期
  环境整份化石化进假二进制（MIRVM_* 引擎旋钮被回放重设——实证 MIRVM_NO_IR_CACHE
  被化石化后 L2 永久旁路且无迹象）；**P2** 空 stub `.d` 使源码编辑永不触发假 bin
  重录（化石洗不掉）；**P3** diff_cargo 无 L2 warm 复跑维度（runner 缓存路径零 gate
  覆盖，本次污染任何 gate 都抓不到）。
- 测量探针（interp 执行集计数、store 拒因）均已回滚未提交；复原后 diff_cargo 5/5。

## 片4（S1 小件包）：sysroot stamp + 三个缓存盲区修缮（2026-07-14）

施工顺序裁定见 decision-history §7.2（S1→S2→S4→S3+JIT 联合设计）。本片四个语义单元
逐 commit，每个 commit 全量 gate5 = 46 pass / 0 expected-red / 0 skip / 0 fail：

- **S1a（V1）sysroot 仪式 stamp 化**：ensure_sysroot 原每跑 spawn 两个 rustc 子进程
  （--print sysroot 15ms + -vV 13ms）+ builder 递归 stat 整棵 rust-src 判新，合计
  ~40-55ms 且 warm 也付。stamp = (MIRVM_BUILD_ID, rustc 二进制 len+mtime_ns, builder
  hash 文件内容)，失效轴对照 rustc-build-sysroot 0.5.13 sysroot_compute_hash 逐项
  覆盖；任何缺失/失配/读写失败回退全仪式自愈。**warm fib 墙钟 85→55ms**（wall−total
  只剩 ~16ms exec/dyld）；gate5 加载相性能门 443→364ms。不在防护面：同 toolchain 内
  手改 rust-src（逃生门 = 删 stamp/sysroot 目录）。
- **S1b（P1）runner 不回放 MIRVM_\***：录制环境回放把构建期引擎旋钮化石化进假二进制
  （调研实证 MIRVM_NO_IR_CACHE 化石化 ⇒ L2 永久旁路无迹象）。修复 = 回放跳过 MIRVM_
  前缀；编译语义变量（env!/CARGO_*）维持录制优先。实证：化石 JSON 项目干净环境
  run1 入账 46ms / run2 命中 87ms。
- **S1c（P2）假二进制 dep-info 真实化**：先试"runner 会话后回写真 .d"路线——实测
  **证伪**（cargo 在 rustc 调用结束即把 dep-info 快照进 .fingerprint，事后补写不可见），
  改为 wrapper 写假产物时用真 rustc 只发 --emit=dep-info（清单精确含 mod/include!/
  env!；stderr 静默保持 native"单次告警"口径；失败退回 crate 根单行清单）。实证：
  编辑源码 → 假二进制重录 ✓；无编辑复跑指纹稳定 ✓。
- **S1d（P3）diff_cargo 补 L2 warm 复跑维度**：与 diff.sh M6 片2 通道对位——每 green
  case 第二跑（应命中缓存；告警类=冷重演）stdout/stderr/退出码须与首跑逐字节一致。
  此前 runner 缓存路径零 gate 覆盖（化石化事故任何 gate 抓不到），本维度上锁。
  gate_truth_regression 12/12 维持。

## 片5（S2 / D9d）：依赖 codegen 剪枝（2026-07-14）

**着陆形态**（三条候选路线的终选，超出调研稿预设）：不是 cargo-miri 的自制空 backend、
也不是 --emit 手术，而是**树内现成的 `-Zno-codegen`**——rustc_interface::start_codegen
对它有原生空转（空 CompiledModules；rmeta 编码在 backend 之外不受影响；Linker::link
照走默认 link_binary 产出 **metadata-only rlib**，cargo 与下游 --extern 全无感）。
wrapper 的 target 依赖分支从 exec 真 rustc 改为 **in-process 驱动**（进程本就链着
librustc_driver，顺带省 22 次 exec）+ 追加 `-Zno-codegen`。

**post-mono 错误面保真**（本片的语义风险点）：native 构建在 codegen 期做 post-mono
const-eval，裸 -Zno-codegen 会漏掉依赖 crate 的构建期 const 错误。DepCallbacks 在
after_analysis 显式 `collect_and_partition_mono_items`（cargo-miri dummy backend 同款）。
探针实证：死代码 `const { assert!(…) }` 依赖，native 与 mirvm 在依赖构建期报**同一条
E0080 同一文案**，mirvm 非零退出。

**账本（诚实修正调研稿 §6 V2 的预估）**：wall ≈0（A/B 各两跑在噪声内——128 核上
依赖 codegen 本就在关键路径外并行，调研稿 −40~60% 的预估被证伪，"调研先行再被实测
校正"第三例）；真实收益 = **CPU −12%**（27.3→24.0 CPU 秒/全冷）、**磁盘 −60%**
（target rlib 100→40MB，全 rlib 只剩 lib.rmeta）、少 22 次 rustc exec；低核/CI 机器
wall 收益按 CPU 差推算。

## 片6（S4）：std 预降低底座（2026-07-15，简报过审后施工）

三个子片逐 commit 全绿（6a 双域地基 / 6b 底座主体 / 6c 收官）；两处施工偏离记
decision-history §7.3（偏移合并取代 FuncId 域位——解释器热路径零改动；COW 定基映射
按实测裁剪——冻结区 memcpy 仅 0.2ms 非成本大头）。

**机制**：底座 = 空 main 合成会话的完整降低产物（~3000 instance，冻结区 0x6800 域，
delta 迁 0x6900 域，跨域绝对地址互指两侧皆稳定），子进程构建（本进程 rustc 会话唯一），
文件字节确定（验收抓获 HashMap 随机迭代序缺陷 → 排序 Vec 落盘）。delta 按 v0
symbol_name 复用底座：函数不入队、static/TLS 地址与身份去重（双份 = static mut/
线程局部精神分裂）、fn 条目单一地址身份；fn/TLS/asm id 从底座计数起编，absorb =
base++delta 拼单表 + asm 配方合并重物化。降低指纹（ub/overflow/contract checks）
会话内验证；L2 条目携 base_key 精确相等（含 None 侧）。一切失配/被占/构建失败 =
静默弃底座走全量冷降低自愈（逃生门 MIRVM_NO_BASE_IMAGE=1，gate5 新增旁路冒烟行，
gate 总数 46→47）。

**账本（EPYC 7773X，release）**：

| 负载 | S4 前纯冷 | S4 后纯冷 | lower 相 |
|---|---|---|---|
| fib | 385ms（wall） | **104ms** | 299→31ms（**9.6×**） |
| args_env | 420ms | **111ms** | 311→35ms |
| hashmap | 441ms | **150ms** | 329→70ms |
| threads_channel | 475ms | **164ms** | 354→76ms |
| ecosystem（runner） | — | — | 1272→**988ms**（std 份额；registry 依赖 = mode B 线） |

简报验收 ≤120ms（fib 类）达成；底座构建一次性 ~450ms（键 = build_id+sysroot stamp，
debug/release 共享）；底座文件 2.19MB；delta L2 条目从整包 2.2MB 缩至 ~0.1MB
（store 0.9ms）。warm 路径不变仍 ~50ms wall（cache-load 33ms 大头 = FuncBody
postcard 解码，零拷贝布局留 mode B）。

**验收**：diff.sh 底座在场/旁路双态 30/30（含 warm 复跑维度）；diff_cargo 5/5；
底座构建幂等 cmp 逐位一致；gate_truth_regression 12/12（冒烟判据用汇总式——
fixture bash 垫片回放罐头输出，逐例 grep 不成立，当场踩中修正）；cargo test 42/42；
终局 gate5 = **47 pass / 0 expected-red / 0 skip / 0 fail**。
