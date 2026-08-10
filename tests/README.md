# mirvm 测试套件

本文是现行测试入口、套件用途和新增测试规则的唯一说明。历史设计文档可以记录
旧文件名，但用户、CI 和现行文档只能通过 `tests/run.sh` 运行测试。

## 运行入口

```bash
./tests/run.sh fast
./tests/run.sh smoke
./tests/run.sh gate
./tests/run.sh list
./tests/run.sh suite <suite-id> [套件参数...]
```

- `fast`：日常提交检查。
- `smoke`：在 `fast` 的义务上增加小规模真实负载和运行时检查。
- `gate`：收尾门禁，覆盖 `fast`、`smoke` 的全部义务，再增加完整 corpus、
  性能、依赖镜像和额外运行模式。
- `list`：列出所有活动套件及用途。
- `suite`：只运行一个套件。例：
  `./tests/run.sh suite corpus.run --tier smoke`。

入口会先切换到仓库根目录，所以可以从任意工作目录调用。未设置 `MIRVM` 时，
需要产品二进制的套件会先执行 `cargo build --release --locked`；显式设置 `MIRVM`
时使用指定二进制。固定 Rust 工具链来自 `rust-toolchain.toml`。

默认 Cargo home 不可写时，入口使用 `target/test-state/cargo-home`。默认 mirvm home
不可写时，入口使用 `${TMPDIR:-/tmp}/mirvm-contract-home`。可以用 `CARGO_HOME`、
`MIRVM_HOME`、`MIRVM_CONTRACT_HOME` 显式覆盖。入口负责建立可写目录，不要求用户
为每个套件手工准备环境。

## 档位内容

| 套件组 | `fast` | `smoke` | `gate` |
|---|:---:|:---:|:---:|
| Rust 格式、clippy、单元测试 | 是 | 是 | 是 |
| 程序、Cargo、cargoless 三类差分 | 是 | 是 | 是 |
| cargoless test/workspace/Git 合同 | 是 | 是 | 是 |
| build.rs 增量合同 | 是 | 是 | 是 |
| 测试框架防假绿回归 | 是 | 是 | 是 |
| corpus smoke 探索跑批 | 否 | 是 | 由严格 corpus 覆盖 |
| x86 与运行时语义 | 否 | 是 | 是 |
| 完整严格 corpus | 否 | 否 | 是 |
| 依赖镜像、JIT 统计、性能上限 | 否 | 否 | 是 |

`gate` 不会为了形式重复运行相同套件，但它检查的行为范围必须是前两档的完整
上级。默认产品路径是 cargoless；`differential.cargo` 始终保留 Cargo 回退路径，
`differential.cargoless` 始终比较两条路径。完整 Cargo 兼容轨使用：

```bash
MIRVM_DEPS=cargo ./tests/run.sh gate
```

## 套件目录

| 套件 ID | 作用和行为权威 | 主要夹具 |
|---|---|---|
| `quality.rust` | 格式、clippy、Rust 单元测试 | `src/` |
| `differential.programs` | `demo/*.rs` 的 native stdout、stderr、退出码是权威 | `demo/` |
| `differential.cargo` | 固定 Cargo 的脚本和项目行为是权威 | `demo/`、`tests/fixtures/` |
| `differential.cargoless` | Cargo 路径与 cargoless 路径逐字节一致 | `tests/fixtures/cless_*` |
| `contracts.cargoless-test` | 固定 Cargo 的 `cargo test` 选择、输出和退出码是权威 | `cless_test_contract/` |
| `contracts.cargoless-workspace` | 固定 Cargo 的工作区、包、feature 和失败传播是权威 | `cless_workspace_contract/` |
| `contracts.cargoless-git` | 固定 Cargo lock 格式加本地 Git 仓库的提交内容是权威 | 运行时生成 |
| `contracts.build-script-rerun` | build.rs 的输入变化和 Cargo 指令决定是否重跑 | `cless_br/`、`cless_libc.rs` |
| `contracts.deps-image` | 固定输出、缓存文件数量和既定时间上限 | `a2_ws/` 的临时副本 |
| `corpus.run` | 真实依赖探索跑批；检查退出码，XFAIL 还锁定诊断 | `cases.manifest`、`corpus/` |
| `corpus.deps-pair` | 每个 corpus 条目的 Cargo/cargoless 三维一致 | `cases.manifest`、`corpus/` |
| `corpus.contract` | 按 manifest 的 exit、oracle、diff、xfail 严格判定 | `cases.manifest`、`oracles/` |
| `runtime.semantics` | 数学常量或同源 native 结果是运行时语义权威 | `demo/m4/`、`tsan/` |
| `runtime.x86-features` | 当前宿主 native 结果是各 x86 子能力权威 | `tests/fixtures/m51_*.rs` |
| `runtime.tsan` | TSan 退出码为零且无数据竞争警告 | `tsan/` |
| `runtime.jit-stats` | JIT 退出统计必须存在且关键桶非零 | `demo/jit_unwind_probe.rs` |
| `performance.limits` | 已有加载、rayon、fib 时间上限和缓存预算 | `corpus/c_rayon.rs`、`demo/m4/pure.rs` |
| `harness.truth` | 确定性假程序证明框架不会假绿或吞失败 | `tests/fixtures/gate_truth/` |

`tests/support/harness.sh` 是共享实现，不是测试。它负责根目录定位、PASS/FAIL/
SKIP/XFAIL 计数、统一汇总、corpus manifest 解析、临时 sysroot 准备、计时和磁盘
保护。`tests/parked/` 是停放材料，不属于活动套件，不能从 `run.sh list` 到达。

## 判定规则

每个测试项只能使用以下状态：

| 状态 | 含义 | 是否让套件失败 |
|---|---|:---:|
| `PASS` | 要求确实执行并通过 | 否 |
| `FAIL` | 产品行为、行为权威或测试框架不符合要求 | 是 |
| `SKIP` | 宿主确实不具备该项能力，并写明原因 | 否 |
| `XFAIL` | 已登记的产品缺口以精确退出码和诊断失败 | 否 |
| `XPASS` | 已登记缺口意外转绿，必须更新合同 | 是 |

缺少 `strace`、固定 Cargo、Rust 工具链或测试 sysroot 属于环境错误，不能写成
SKIP。原来的 `P5` 记账不再作为测试状态；当前明确未实现的边界必须写成带精确
原因的 XFAIL。

套件退出码：

- `0`：没有意外失败。
- `1`：存在 FAIL 或 XPASS。
- `64`：命令参数错误。
- `69`：必要工具或测试环境不可用。
- `77`：整个套件只能因宿主能力缺失而跳过。

每个叶套件必须用 `suite_summary <suite-id>` 输出统一尾行。总入口依据退出码判断
套件结果，不通过搜索任意说明文字猜测成功。套件输出在该套件结束时完整打印，
失败信息不会只保留最后几行。

## 编写要求

1. 新脚本放在 `tests/suites/<类别>/`，文件名说明行为，不使用里程碑代号。
2. 套件 ID 由路径自动产生：例如 `contracts/cargoless_git.sh` 对应
   `contracts.cargoless-git`。脚本第二行必须是单行用途说明；不依赖产品二进制的套件
   再声明 `# product: no`。不得另建人工注册表。
3. 脚本必须 source `tests/support/harness.sh`，随后调用 `test_enter_repo`。不得假定
   调用者的当前目录。
4. 文件头必须说明测试什么、为什么需要、谁是行为权威、哪些输入会被比较。
5. 原生 Rust 或固定 Cargo 作为权威时，权威侧必须先达到明示的预期退出码。
   双方同样构建失败或运行失败不能算通过。
6. 对拍默认比较 stdout、stderr、退出码三项。只能过滤时间、线程号等确实不稳定的
   字段，每条过滤规则都要在脚本中说明原因。
7. 已提交夹具只读。需要改文件时，先复制到 `mktemp -d` 创建的目录，并用 trap
   清理。测试结束后 `git status --short` 不得出现测试产生的修改。
8. 套件必须可单独运行，不能依赖前一个套件留下 lock、sysroot、环境变量或文件。
   共享缓存可以加速，但缓存缺失不能改变判定标准。
9. 聚合型套件不要使用 `set -e`，因为预期非零退出码本身可能是合同。每个外部命令
   必须显式捕获并判断退出码。
10. SKIP 只用于宿主能力差异。必要工具缺失、fixture 丢失、manifest 非法必须响亮
   失败。
11. 默认串行运行。性能、缓存和部分系统能力共享全局状态，当前没有可信的并行合同。

## 新增套件步骤

1. 先写能够因目标缺陷失败的断言，并验证失败原因正确。
2. 在 `tests/suites/` 下添加叶脚本，复用共享 harness。
3. 确认 `./tests/run.sh list` 已按路径自动发现它，无需人工报名。
4. 在本文件的套件表中写明用途、行为权威和夹具。
5. 明确加入 `fast`、`smoke`、`gate` 的验收政策，或说明它为什么只能手工运行。
6. 若修改入口、状态传播或汇总，必须先扩充 `harness.truth`。
7. 依次运行 `bash -n`、目标单套件、`fast`，按影响范围再运行 `smoke` 或 `gate`。

套件数量目前很小，不增加第二份机器清单或结果数据库。自动发现只依赖目录和文件名，
档位仍在 `tests/run.sh` 中明示验收内容，避免测试基础设施成为新的产品工程。
