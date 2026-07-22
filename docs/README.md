# mirvm 文档导航与权威规则

> 本文只回答两个问题：**哪份文档管什么**、**冲突时信谁**。
> 最后重整：2026-07-18（文档目录大精简：历史文档归档 `history/`、四个过时文档删除、
> 新增 [open-issues.md](open-issues.md) 债务登记册与 [agents/onboarding.md](agents/onboarding.md)）；
> 最后同步：2026-07-22（新增 HEAD `cda7421` 的一次性开发状况审查快照索引；不改变
> current-status/open-issues/decision-history 的既有权威顺序）。

## 1. 权威顺序（冲突时从高到低取信）

1. **当前代码与可复现测试**——实现事实的最终依据。
2. **[current-status.md](current-status.md)**——跨阶段状态的唯一汇总入口。
3. **[open-issues.md](open-issues.md)**——未解决债务/开放问题/拒绝边界的唯一登记入口。
4. **[decision-history.md](decision-history.md)**——关键可逆决策、被否方案、重开条件。
5. **[designs/](designs/)**——仍有效的语义契约与已批准设计（含未完成片的蓝图）。
6. **[history/](history/)**——施工日志与已完成设计（只读；事实权威让位给 2–4，
   论证过程价值不被取代）。

「已批准」「已实现」「测试通过」是三个不同断言，禁止混写。发现冲突时在 current-status
登记、在 decision-history 写明推翻证据，不静默改旧文档。

## 2. 新接手阅读顺序

1. [current-status.md](current-status.md)：阶段、执行链、可信边界、下一步。
2. [open-issues.md](open-issues.md)：欠了什么、为什么、转正需要什么。
3. [../DESIGN.md](../DESIGN.md) + [designs/ram-spec.md](designs/ram-spec.md)：长期心智模型与语义契约。
4. [decision-history.md](decision-history.md)：决策演变。
5. 按题进 [designs/](designs/) 与 [history/](history/)；动手前看一眼
   [agents/onboarding.md](agents/onboarding.md) 的环境铁律与验收食谱。

## 3. 目录与职责

### 仓库根

| 文档 | 定位 |
|---|---|
| `README.md` | 门面：项目一句话、快速开始、权威指向 |
| `DESIGN.md` | **契约**：RAM 命题、P0–P7、C0–C13 账本、里程碑（tier-0 与 M6=REPL 段为历史） |
| `AGENTS.md` | agent 治理：issue tracker 归所、triage 标签、基建预算纪律 |

### docs/ 顶层（活文档）

| 文档 | 定位 |
|---|---|
| `current-status.md` | 跨阶段事实唯一入口（阶段表 / 执行路径 / 已验证边界 / 缺口表 / 开发顺序） |
| `open-issues.md` | 未解决债务登记册：T 待施 / C corpus 实锤 / E 引擎架构 / D 分发 / R 拒绝边界 / G 维护态 + 触发器速查 + 定型否决 |
| `decision-history.md` | append-only 决策索引（ADR-lite）；推翻旧决策在此追加，不删旧节 |
| `corpus.md` | 真实 crate 三维差分扩编台账（§5，批1–10）+ 候选池（§7）；§0–§4 为 tier-0 时代票据归档 |
| `real-projects.md` | `tests/real_projects*.sh` 严格 harness 的合同说明（四层身份/schema-3/4/隔离边界） |

### docs/agents/（agent 操作规约）

| 文档 | 定位 |
|---|---|
| `domain.md` | 单上下文文档域的阅读纪律 |
| `onboarding.md` | 新会话入职速查：环境事实、铁律、三维验收食谱、自伤纪律 |
| `issue-tracker.md` | issue tracker 选型（GitHub，当前全面暂停） |
| `triage-labels.md` | workflow 标签定义 |

### docs/designs/（契约与已批准设计）

| 文档 | 定位 |
|---|---|
| `ram-spec.md` | **语义契约**：RAM 五组成、定义度四级、UB 立场、差分对拍合法性 |
| `concurrency-arch.md` | 并发架构原则（状态三分、真线程）；checked 模式设计储备 |
| `frame-stack-models.md` | 帧模型 A/B 论证（选 A 的唯一完整证据链） |
| `frame-abi-bytecode.md` | M4 帧/ABI 设计基线；alloca 迁移承诺（open-issues E12）在此 |
| `vmctx-passing.md` | vmctx P/T/R 三案论证 + §7 终裁（T 定稿 + 双触发器）；R 复测要回来读 |
| `async-stackless.md` | async 无栈状态机调研（C11 证据） |
| `m5-design.md` | M5 总案（M5.0–M5.5 全完成）：D5 T/R 分层终裁、§7 gate6 判据——M5.5 原案之本 |
| `m5.4-design.md` | M5.4a–d 施工蓝图（ABI 泛化 + LSDA 版式参数 + SIMD 覆盖矩阵；片 a–d 全落地） |
| `c1-ffi-agg-design.md` | C1 FFI 按值聚合封送设计（FfiAgg 冻结 + libffi struct 编组；R17 边界之母） |
| `c2-rlib-symbols-design.md` | C2 native-archive「符号在 rlib」救援链设计（elfsym 枚举 + P1 跳板重链） |
| `distribution-design.md` | 轨 C 分发 D9a–D9f 全文；未立项 ④⑤ 的唯一设计规范 |

> designs/ 与 history/ 的分界不是「未完成/已完成」：designs/ 保存**仍具规范性或
> 重开时必须遵守的契约**（已完成战役的现行规范也在此），history/ 保存非规范性的
> 施工证据、被替代方案与当时记录。

### docs/history/（只读归档）

施工日志（m4/m5/m6-log）+ 已完成设计（m4.1/m4.4/m5.1/m5.2/m5.3/s3b×2/s4）+
调研档案（coldstart-research）+ spike1–5 冻结证据 + 带日期的一次性审查快照。索引与
内容定位见
[history/README.md](history/README.md)。已删除文档（m4-debt-map、m4-plan、m4.5-plan、
AGENT-HANDOFF）须查 git 历史；其债务与速查精华已分别迁入 open-issues.md 与
agents/onboarding.md。

### docs/parked/

| 文档 | 定位 |
|---|---|
| `s3b-chain-wip.patch` | chain 成像方案的已探索代码存档（跨项目硬需求时的备选 B，重开条件见 history/s3b-design-fork.md §6） |
| `c3-resume-spike.md` | C3 inline-asm resume/longjmp 转移形的冻结 spike 证据（E32 hazard 的实锤现场） |

## 4. 维护纪律

- 每次阶段完成，同时留下：代码、可复现 gate、对应日志（或 decision-history 条目）、
  current-status 状态变化四件套。
- 新债务/新边界登记到 open-issues.md 对应分区；关闭条目连同行移出并在
  decision-history 留证据与重开条件。
- 设计被推翻：旧文档保留，decision-history 追加时间、证据、新结论、重开条件。
- 绿色必须核可观察输出或不变式；预期红必须锁定失败原因。
- 路线图不写入现状段；当前代码没有的目录、CLI 模式和 API 必须标「计划」。
- history/ 内文档只读，不再更新（勘误除外，且保持「当时记录」原样）。
- **阶段关账清单**（D-08，2026-07-22 起——只核关键词的复扫曾被外审证伪）：
  关闭任何阶段前，沿权威链交叉核对——① README（首段/快照/快速开始）；
  ② CLI `--help`；③ current-status 的头注/阶段表/边界表/开发顺序四块互洽；
  ④ open-issues 只含未解决项（闭合行已移出）；⑤ decision-history 当前摘要
  （§6/§8 等汇总位）与新条目不冲突；⑥ 被引设计档头部状态行；⑦ 纯文本
  路径锚（file:line 与 docs/ 相对路径）现场重验。
