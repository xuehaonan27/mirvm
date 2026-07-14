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
