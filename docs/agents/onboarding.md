# 新会话入职速查（onboarding）

> 一页 = 环境事实 + 铁律 + 阅读顺序 + 验收食谱。权威状态永远以
> [../current-status.md](../current-status.md) 为准；债务问 [../open-issues.md](../open-issues.md)。
> 本文替代并删除了旧 `docs/AGENT-HANDOFF.md`（2026-07-07 为一次 context 事故所建，
> 524 行大半已被状态页/日志吸收；修订史在 git）。

## 环境事实

- 机器：Linux x86_64。构建期工具 cmake/g++/perl/make 在场；**nasm/clang/go 缺席**
  （决定 corpus 候选可判性：rocksdb=bindgen 无 libclang、ravif=nasm 均判不可）。
- 工具链：rustc 1.98.0-nightly（`nightly-2026-07-02`，rust-toolchain.toml 锁定）。
  rustc-src 在 `~/.rustup/toolchains/nightly-2026-07-02-*/lib/rustlib/rustc-src/rust/compiler`。
  月度 bump 由人发起，bump 前先在 m4-log/m5-log 查同类漂移先例。
- 构建：`cargo build --release --locked`（**锁文件铁律**）；执行与性能一律 release。
- Cranelift 0.133.1：同一 `FrameTable` 生成的 `.eh_frame` 含共享 CIE，必须把完整、
  零结尾的节一次交给 `__register_frame`；逐 FDE 注册会在多层 JIT unwind 时失败。
  Cranelift 原子无弱序（JIT 统一 SeqCst，记账在案）。
- MIR API 逐期漂移实录：`../history/m4-log.md` 与 `../history/m5-log.md` 是唯一档案。

## 三条铁律

1. **Miri 是代码参考，不是心智模型来源**（DESIGN.md P1）。心智模型 = RAM + JVM 式
   VM 作者视角；遇抉择问「一台 JVM 类系统软件会怎么做」。
2. **绿必须核可观察输出/不变式**；预期红锁定失败原因；「已批准/已实现/测试通过」
   是三种断言，不可混写。都失败 ≠ PASS。
3. 里程碑完成 = **代码 + 可复现 gate + 施工日志 + current-status 更新**四件套；
   推翻旧设计时旧文档保留，decision-history 追加时间与证据，不静默改历史。

## 阅读顺序

1. [../current-status.md](../current-status.md)（阶段事实唯一入口）
2. [../open-issues.md](../open-issues.md)（未解决债务唯一入口）
3. [../../DESIGN.md](../../DESIGN.md) + [../designs/ram-spec.md](../designs/ram-spec.md)（契约）
4. [../decision-history.md](../decision-history.md)（可逆决策与重开条件）
5. 按题进 [../designs/](../designs/) 与 [../history/](../history/)，再 git log。

## 三维验收食谱（新建/修改 corpus driver 必过）

```bash
M=$PWD/target/release/mirvm
# A: mirvm 默认维
$M run corpus/c_X.rs
# B: native 维
D=$(grep -l 'name = "c_X"' ~/.mirvm/scripts/*/Cargo.toml | head -1 | xargs -r dirname)
CARGO=$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo
(cd $D && RUSTC=$(dirname $CARGO)/rustc $CARGO run -q)
# C: 逢调即编 JIT 维
MIRVM_JIT_THRESHOLD=1 $M run corpus/c_X.rs
```

stdout/stderr/exit 三维全部逐字节一致才算绿。主 gate：`bash tests/gate.sh`
（corpus 全防线；日常节奏：`bash tests/run.sh fast` 逢提交、`bash tests/run.sh smoke`
批次级）。corpus 条目唯一真源 = `tests/corpus.manifest`（tier/mode/timeout/env 全在此），
新增 driver 必须先登记。

## 工作流纪律（撞过的坑）

- **corpus 基建冻结**：campaign 期间 driver 只新建不改旧；回归/gate 在跑时不编辑其
  脚本、不 rebuild（bash 流式读脚本 + 二进制原子里换 = 假象红，两例在案）。
- 重负载下 `MIRVM_JIT_THRESHOLD` 与 fib 硬门可 flake：判非语义，安静期复跑为准。
- 自伤纪律：新增 FuncId 字段必过 rebase 消费面五处清单（exports/fn_addrs/ids/
  entry_stub_sites/custom_alloc_shims；dc6e30c 前车之鉴）。
- 债务登记在 `../open-issues.md`；决策推翻在 `../decision-history.md`；
  阶段账目进 `../history/` 对应日志。
- `MIRVM_SEGV_DUMP` / `MIRVM_JIT_DEBUG` 是刻意的诊断旋钮，勿删。
- GitHub/远程全面暂停（AGENTS.md）；commit 纪律与基建预算纪律同见 AGENTS.md。
