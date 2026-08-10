# `mirvm test` 的 cargoless 合同

> 生效日期：2026-08-10。这里的 “cargoless” 指 mirvm 自己解析清单、安排编译，
> 运行过程中不启动 Cargo。实现事实以代码和
> `tests/cargoless_test_contract.sh`、`tests/cargoless_workspace_contract.sh` 为准。

## 1. 谁决定正确行为

当前仓库锁定的 `nightly-2026-07-02` Cargo 是权威。mirvm 不复制 Cargo 的内部
实现，也不把自己的旧输出当标准；遇到不确定行为时，先让这个 Cargo 对最小项目执行
`cargo test -vv --no-run`，再按它实际传给 rustc 的参数和环境实现。

合同只承诺下面列出的单包与 resolver 2/3 工作区范围。范围内必须对齐；范围外必须明确
报错，不能忽略参数后继续运行。当前明确不在范围内的是 resolver 1、`**`/`[]`/`?`
成员 glob、嵌套 workspace、同名成员与复杂 package ID spec、`[patch]`/`[replace]`、
workspace lints、Git/替代 registry、doctest、bench、交叉 target 和根 proc-macro 包测试。
edition 2024 隐含 resolver 3，直接进入本合同。doctest 需要 rustdoc 前端；复杂依赖
来源归 D15 P5。

## 2. 当前范围内的行为

- 读取 lib、bin、integration test 和 example 目标；遵守 `test`、`harness`、
  `required-features` 与自动发现规则。
- 普通根 lib/bin 只看普通依赖；测试单元额外看开发依赖。开发依赖进入锁文件，但
  `mirvm run` 不构建它们。
- 测试使用 `[profile.test]`；libtest 目标带 `--test`，`harness=false` 目标保留用户
  `main` 并设置 `cfg(test)`。
- 默认编译 examples 但不运行；显式 `--example` 按测试目标运行。存在 integration
  test 时额外编译普通 bins。
- integration test 获得 `CARGO_TARGET_TMPDIR` 和可执行的
  `CARGO_BIN_EXE_<name>`；后者启动的仍是 VM 内目标程序。
- 支持 lib/bin/test/example 选择、单个过滤串、`--no-run`、`--no-fail-fast`、
  `--quiet`、`--locked`、`--offline`，并逐字透传 `--` 后的 harness 参数。
- 每个测试 artifact 在独立 mirvm 子进程运行；失败退出码与 Cargo 一样为 101。
- 从工作区根或成员目录发现最近的工作区；支持 virtual/root-package manifest、
  `members`、`exclude`、`default-members`、`*` glob，以及工作区内 path 依赖自动入成员。
  从被 `exclude` 的包启动时按独立包处理。缺失成员和无匹配成员模式必须报错。
- 物化 `[workspace.package]`、`[workspace.dependencies]`、目标条件依赖和根 `[profile]`；
  工作区依赖的特性相加，路径始终以工作区根解释。
- 支持默认成员、当前成员、`--workspace`/`--all`、重复的精确包名 `-p`/`--package`
  和 `--exclude`。选中多个包时先完成全部编译，再按 Cargo 的 fail-fast 规则执行。
- 支持 `--features`/`-F`、`--all-features`、`--no-default-features`、
  `package/feature` 与直接依赖的 `dependency/feature`；resolver 2/3 下相同包、相同依赖
  类别的特性统一，build 依赖仍与 normal/dev 分开。
- 读取 package、workspace 继承和 registry index 中的 `rust-version`。resolver 3 默认
  优先选择与工作区最低 Rust 版本兼容的候选；Cargo 配置可显式设为 `allow`，
  `--ignore-rust-version` 同时关闭候选偏好和编译前版本拒绝。最低版本不高于 1.82 时
  fresh lock 写 v3，1.83 起写 v4。
- 所有成员共用工作区根 `Cargo.lock`。无锁时按 Cargo 规则，以全成员的全部特性可达依赖
  一次求解并原子写入统一 lock；实际编译仍只启用用户选择的特性。`--locked` 缺锁直接
  失败，生成结果还必须被固定 Cargo 的 `--locked --offline` 接受。

## 3. 怎么判定没有漂移

单包夹具 `tests/fixtures/cless_test_contract` 覆盖库测试、bin 测试、integration test、
普通 example、自定义 harness、build script、path 开发依赖、panic、ignored、失败传播、
bin 子进程和 test profile。工作区夹具 `tests/fixtures/cless_workspace_contract` 采用
resolver 3，并覆盖
virtual manifest、四成员、排除包、继承、目标条件 Dev 依赖、默认/显式/依赖特性、
跨根传递依赖汇合、特性激活的可选依赖、自动 integration test、统一构建和统一锁文件。

两个合同脚本只保留三层判定：

1. **结构层**：读取固定 Cargo 的 `-vv` rustc 行，钉住 `--test`、`cfg(test)`、依赖
   种类、example、`CARGO_BIN_EXE` 和临时目录的落点。
2. **结果层**：native Cargo、`MIRVM_DEPS=cargo` 和 `MIRVM_DEPS=self` 三腿比较
   stdout、stderr、退出码；只归一化线程号、耗时和 Cargo 自身的构建进度行。
3. **机制层**：self 腿把 PATH 中的 `cargo` 替换成必失败哨兵，并在 Linux 基线上用
   `strace` 审计全部 `execve`，同时抓绝对路径调用；热复跑检查 build script 的实际
   执行次数仍为 1，防止“结果相同但偷偷退回 Cargo”或增量失效。工作区合同还删除
   lock 后由 self 重建，再交给固定 Cargo 以 `--locked --offline` 复核。

参数形状的纯函数断言留在 `resolve.rs`、`schedule.rs` 单测。合同脚本能判断当前真实
工作负载后即冻结；不为未来 Cargo 字段增加清单、快照格式或来源元数据。

## 4. Cargo 升级规程

升级 `rust-toolchain.toml` 时不得直接刷新期望值：

1. 用旧、新两个 Cargo 分别对固定夹具跑 `test -vv --no-run` 和合同脚本的 native 腿。
2. 人工解释每一条 rustc 参数、环境、目标选择或输出差异，区分 Cargo 行为变化与纯
   诊断文字变化。
3. 行为变化先改 manifest/resolve/schedule/driver 和对应单测，再改合同断言；不允许
   仅放宽归一化规则让测试变绿。
4. 依次运行 `cargo test --locked`、两个合同、`tests/diff_cless.sh`，最后跑
   `tests/run.sh fast`。三轨全部通过后，新的 pinned Cargo 才成为权威；工作区合同的
   Cargo `-vv` 结构检查也必须重新人工核对。

Cargo 主线继续迭代不会自动改变已发布 mirvm 的行为：每个 mirvm 版本绑定一个明确
toolchain；升级通过上述审核把合同整体前移，而不是运行时猜测 Cargo 版本。
