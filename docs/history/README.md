# docs/history/ — 施工日志、历史文档与审查快照

> 本目录于 2026-07-18 文档重整时建立，原有历史文档从 `docs/` 顶层迁入。**本目录内文档
> 一律只读**：它们是
> 「当时实际交付了什么/为什么这么设计」的一手记录，事实权威让位给
> [`../current-status.md`](../current-status.md)，论证的过程价值不被取代。
> 既有施工/设计档中的未解决开放项已上收至
> [`../open-issues.md`](../open-issues.md)。审查快照若发现新的候选债务，以报告末尾的
> 候选迁移清单为线索；写入 canonical 文档前不得把它视为已正式登记或据此排期。

## 施工日志（事实层）

| 文档 | 覆盖 |
|---|---|
| `m4-log.md` | M4.0–M4.5 自研字节码解释器（2026-07-07~10）；含全库唯一的 nightly MIR API 漂移实录与总验收移交清单 |
| `m5-log.md` | M5.0–M5.4b（~2026-07-15）；miscompile 级根因实录、LSDA probe 三发现、CLIF/glib 漂移 |
| `m6-log.md` | M6 冷启动与缓存（2026-07-14~15）；毫秒级实测账本、事故刑侦（environ SIGSEGV、fn_addrs 负对照） |

## 已完成设计（论证层）

| 文档 | 内容 |
|---|---|
| `m4.1-design.md` | M4.1 值与内存：191-instance Trap 普查原始数据 + F1–F7 调研证据链 |
| `m4.4-design.md` | M4.4 真线程：thunk 工厂三约束推导、TLS dtor 顺序裁定、signal A/B |
| `m5.1-design.md` | M5.1 轨 A 收口：llvm.x86 差集处置、静态归档 D2 拒绝面全文、D7b 重开触发器 |
| `m5.2-design.md` | M5.2 非 JIT 语义补全 D8a–D8l：14 项差分探针原表、intrinsic 差集明细 |
| `m5.3-design.md` | M5.3 JIT×S3 联合设计：J2「懒降低不建」三重反对完整论证、chain 原始设计 |
| `s3b-design-fork.md` | S3′b 岔路现场：chain 4/19 证伪机制级证据、A–E 取舍矩阵 |
| `s3b-a2-design.md` | A2 纯化聚合 deps-image 施工设计：split-lower 机件、rebase 清单、键安全论证 |
| `s4-base-image-design.md` | S4 std 预降底座：方案 A/B 取舍、symbol_name 键调研、两项施工偏离的原案 |

## 调研档案

| 文档 | 内容 |
|---|---|
| `coldstart-research.md` | M6 前置调研：30-demo 相位账本、perf 归因桶、rustc-src file:line 先例解剖；V5 并行 lower / V6 零拷贝两个未立项杠杆（→ open-issues D7/D3） |

## 审查快照

| 文档 | 内容 |
|---|---|
| [development-status-audit-2026-07-22.md](development-status-audit-2026-07-22.md) | HEAD `cda7421` 的一次性开发状况审查：实跑矩阵、JIT/FFI 正确性发现、证据盲区、文档权威失真、成熟度和稳定化顺序；非当前状态或开放债务权威 |

## spike（冻结证据）

| 文档 | 内容 |
|---|---|
| `spike1-model-a-skeleton.md` | 模型 A 骨架；slaved ByteRegion 与借用解耦教训 |
| `spike2-interp-compiled-adapters.md` | i2c/c2i 混合栈赌注；裸指针 vmctx 借用纪律 |
| `spike3-mixed-stack-unwind.md` | 混合栈 unwind；exception class 开放问题的唯一登记（→ open-issues E13） |
| `spike4-concurrency-tsan.md` | 真线程 + TSan 收官；TSan harness 独立化工程论证 |
| `spike5-cranelift-adapters.md` | 真 Cranelift接入；唯一 vmctx P/R 实测数据与 eh_frame 注册管线细节 |

## 已删除、须查 git 历史的文档

- `docs/m4-debt-map.md` —— §0–§5 普查精髓已并入本目录（m4.1-design/m4-log），
  §6–§10 债务已迁 `../open-issues.md`（附旧编号对照）。
- `docs/m4-plan.md`、`docs/m4.5-plan.md` —— 计划全部兑现，事实层归 m4-log。
- `docs/AGENT-HANDOFF.md` —— 交接速查精华已迁 `../agents/onboarding.md`。
