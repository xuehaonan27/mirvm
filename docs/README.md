# mirvm 文档导航与权威规则

> 最后整理：2026-07-13。本文只管理“应该相信哪份文档”；实际阶段、验证结果和已知缺口见
> [current-status.md](current-status.md)。历史方案不会因被替代而删除，决策演变集中记录在
> [decision-history.md](decision-history.md)。

## 1. 如何读这些文档

同一问题出现冲突时，按下列顺序取信：

1. **当前代码与可复现的测试结果**——实现事实的最终依据。
2. **当前状态页与已完成施工日志**——状态页负责跨阶段汇总；`m4-log.md`、`m5-log.md`
   负责记录当时实际交付。
3. **仍有效的语义契约和已批准设计**——规定目标与约束，不等于已经实现。
4. **待审提案**——只表示准备采用的方案，不能写成现状。
5. **早期计划、RFC、调研与 spike**——保存问题空间、备选模型和证据；后来的结论可以替代它们。

“较新”本身并不保证正确：施工日志可能比同日设计稿更接近事实，当前代码又可能暴露日志中的
误判。发现冲突时，应在当前状态页登记，并在决策历史中写明“由什么证据推翻了什么结论”，不要
静默改掉旧文档。

## 2. 当前应先读什么

新接手开发时建议按此顺序：

1. [current-status.md](current-status.md)：当前阶段、真实执行链、可信测试边界、下一步。
2. [real-projects.md](real-projects.md)：真实 Cargo 项目 oracle、benchmark 合同与当前证据。
3. [../DESIGN.md](../DESIGN.md)：长期心智模型与仍有效的架构契约。
4. [designs/ram-spec.md](designs/ram-spec.md)：RAM 语义目标。
5. [decision-history.md](decision-history.md)：frame、vmctx 等关键决策的备选项和演变。
6. [m4-log.md](m4-log.md) 与 [m5-log.md](m5-log.md)：已经实现了什么。
7. [m5.1-design.md](m5.1-design.md)：已完成 M5.1 的最终设计、被替代方案和受约束能力边界。

[AGENT-HANDOFF.md](AGENT-HANDOFF.md) 是面向接手者的操作速查；若其状态与
`current-status.md` 冲突，以后者为准。

## 3. 文档状态表

状态含义：**契约** = 目标层长期约束；**当前** = 随代码同步；**已批准** = 设计有效但未必完成；
**已完成日志** = 当期施工事实；**历史** = 保留证据与备选，不直接描述现状；**待审** = 尚未批准。

| 文档 | 状态 / 权威 | 作用与替代关系 |
|---|---|---|
| `current-status.md` | **当前** | 唯一跨阶段状态入口；替代 README、HANDOFF 中旧的阶段快照 |
| `decision-history.md` | **当前** | 关键决策索引；保留被替代方案和重新开启决策的触发条件 |
| `real-projects.md` | **当前** | 真实项目 case、correctness/benchmark 合同、workspace-local artifacts、证据层级与限制 |
| `DESIGN.md` | **契约** | 长期心智模型；其中明确标为历史的 tier-0/M0–M2 章节不描述现实现 |
| `designs/ram-spec.md` | **契约** | 语义目标；实现差距由 current-status 登记 |
| `m5-design.md` | **已批准** | M5 总体双轨设计；M5.0/M5.1 已实现；M5.2=非 JIT 语义补全已完成（2026-07-14）；M5.3+ JIT 仍属路线图 |
| `m5.2-design.md` | **已完成设计/施工记录** | 非 JIT 语义补全 D8a–D8l；实施结果看 m5-log M5.2 节；施工偏离记 decision-history §6 |
| `distribution-design.md` | **已批准（方向，未立项）** | 轨 C 分发与产品面 D9a–D9f：缓存分层、mode B=缓存可移植化、toolchain 自带、发行先 miri 式 |
| `coldstart-research.md` | **调研完成（2026-07-14）** | 冷启动/lower 全解剖（M6 片3 前置）：std 税线性模型、执行集≪降低集、依赖 codegen 白烧实证、P1–P3 缓存盲区、杠杆 V1–V6 与施工顺序 S1–S4 待裁定；M5.3 Pending 的依据文 |
| `m5-log.md` | **已完成日志** | M5.0/M5.1 实际结果；优先于 M5 设计稿原先的预期 |
| `m5.1-design.md` | **已完成设计/施工记录** | numbigint/xgetbv/sha2/blake3/ecosystem、diff_cargo 3/3 与六 tracer 脚本已绿；signal/backtrace 独立遗留 |
| `m4-log.md` | **已完成日志** | M4.0–M4.5 实际施工记录；M4 状态的历史权威 |
| `m4-plan.md` | **历史（已完成计划）** | M4 原计划；实际差异由 m4-log 与 m4-debt-map 替代 |
| `m4.1-design.md`、`m4.4-design.md`、`m4.5-plan.md` | **历史（已完成设计）** | 保留当时方案；实际结果看 m4-log 对应章节 |
| `m4-debt-map.md` | **历史快照** | collector/worklist、panic 边界等修正证据，结论已吸收进 M4 实现 |
| `designs/frame-stack-models.md` | **历史论证 + 当前决策依据** | 完整保留 A/B 比较；当前选择与实现差异看 decision-history |
| `designs/frame-abi-bytecode.md` | **历史设计基线** | M4 的 frame/ABI 设计来源；已实现部分看代码/m4-log，JIT 部分仍是未来设计 |
| `designs/vmctx-passing.md` | **历史论证 + 当前决策依据** | 保留显式参数/TLS/固定寄存器三案；最新分层结论看 m5-design D5 与 decision-history |
| `designs/concurrency-arch.md` | **历史 RFC、核心原则仍有效** | tcx-free 执行相、状态三分和真线程已落地；mode B/TLAB 等含未来内容 |
| `designs/async-stackless.md` | **调研** | async 状态机与 OS I/O 边界的背景；不证明 signal 已实现 |
| `corpus.md` | **历史调研快照** | 2026-07-05 的边界发现；当前 corpus 结果看 current-status 与测试脚本 |
| `spike1-*` … `spike5-*` | **冻结证据** | 证明候选机制可行，不代表生产路径已采用全部机制 |
| `AGENT-HANDOFF.md` | **当前速查 + 历史机制导读** | 操作入口；阶段事实仍以 current-status 为准 |

## 4. 时间线与主要替代点

| 日期 | 文档阶段 | 阅读含义 |
|---|---|---|
| 2026-07-05 | RAM、frame A/B、并发、async、corpus | 问题空间和第一轮架构选择；包含后来被 M4 推翻的 tier-0 假设 |
| 2026-07-07 | 五个 spike、M4 计划、vmctx 初判 | 可行性证据；P/R 微基准和候选机制不等于生产终裁 |
| 2026-07-08–10 | M4.1–M4.5 设计与 m4-log | 实现逐期替代计划预期；worklist、panic、TLS dtor、spread_arg 等以日志/代码为准 |
| 2026-07-11 | M5 总设计、M5.0 日志、vmctx D5 | M5 总路线获批；只有 M5.0 已实现；vmctx 改为 T 骨架 + R 触发式缓存层 |
| 2026-07-12 | 全项目审计、M5.1 设计与施工 | 先发现测试假阳性并重建 oracle；随后完成 M5.1，又用最终复审修正 volatile UB、unwinder 伪回溯、required archive 装载和 SKIP 冒充 PASS；signal/backtrace 两个独立 XFAIL；本索引与状态页建立 |
| 2026-07-13 | 真实项目 TDD 与文档重整 | 建立严格 real-project oracle/benchmark harness；ripgrep/tokei 移入 Git-ignored workspace working set，并依次推动 direct dyn 尾 alignment、u128 SwitchInt、track_caller Reify shim、宽 volatile、稳定逻辑路径/rustflags 保留与 Cargo runner warning-summary 收口。最终 runner 先完成 rustc finalization，再执行 VM，仅结构化过滤 count-summary。两项目的三个 workload 已在 `/run` + RUSTC proxy harness 下 correctness PASS 并完成 3 样本 benchmark；广覆盖 tokei workload 选择稳定 compact aggregate，不用 normalizer 掩盖并行 JSON 次序。随后单 case 的 CaseID→CheckID/BenchID→EvidenceID、不可变对象、原子 current view 与并发锁落地；最终 schema-3 consumer 进一步重算 PASS/XFAIL 语义、绑定 exact-check provenance 与 benchmark statistics，发布改为两层 staging 下先封存再同父目录 rename，并保留 schema-2 历史 dispatch。suite inventory/集合身份仍开放；历史 frame/vmctx、旧 identity 与旧 volatile 模型完整保留 |

典型替代关系：`m4-plan → m4-log`、`M5 设计预期 → m5-log 实测`、
`vmctx-passing §7 旧 P/R 开放项 → m5-design D5`、`M5.1 原退出标准 → 2026-07-12
可信 oracle 前置`、`静态 direct-dyn field offset → vtable runtime alignment`、
`直接 track_caller item entry → Reify shim`、`宽 volatile 不得拆 → memory-repr 后端分块`、
`runner callback 内执行/提前退出 → compiler 完整收尾后执行 VM + 结构化 summary filter`。
完整理由和重开条件见 decision-history。

## 5. 文档维护纪律

- 每次阶段完成，同时更新 `current-status.md`、对应施工日志和本索引中的状态。
- 设计被推翻时，旧文档保留；在标题下加状态说明，并在 `decision-history.md` 追加时间、证据、
  新结论和回退条件。
- “已批准”“已实现”“测试通过”是三个不同断言，禁止混写。
- 预期失败必须校验**失败原因**；绿色用例必须校验可观察结果，不能只看退出码。
- 路线图不写入现状段；当前代码没有的目录、CLI 模式和 API 必须明确标为“计划”。
