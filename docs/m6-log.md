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
