# 与 rust-lang 主线的一致性：现状盘点与建议

> 状态：**建议文档（2026-09-19），未立项、未裁定、未施工**。本文回答一个问题：
> mirvm 在「嵌 rustc + 自研 cargoless」架构下，如何持续与 rust-lang 主线保持
> 行为一致。§2–§3 是现状与定位盘点（事实以
> [current-status.md](../current-status.md) 为准），§4–§6 是三项建议（B1–B3），
> §7 是明确不建议做的事。若采纳任何建议，按惯例在
> [open-issues.md](../open-issues.md) 立项、decision-history 记裁定，并更新本状态行；
> 本文自身不承担登记职责，也不得被引用为「已批准/已实现」。
>
> **2026-09-19 复核修订**：B1 改为「`./tests/run.sh gate` + gate 未覆盖的四项」，
> 不再另立并列清单；B2 补上 native 预过滤准入与文件导入形态；§2/§4 的事实引用按
> 代码现状更正（`schedule/args.rs`、sysroot 已不用 `rustc-build-sysroot`、stamp 名与
> 所在章节、toolchain 注释里的 commit）。

## 0. 一句话

一致性不靠「追着上游重新实现」，靠三层既有机制：语义承载体嵌 rustc
（**按构造一致**）、调度承载体自研但真 Cargo 永任裁判（**可证一致**）、
两者对主线的跟随统一收敛到「pinned nightly + 带完整回归 gate 的定期 bump」
一个节奏点。本文建议：把 bump 从注释约定升级为制度化必跑集（B1）、
补一路上游测试套件 oracle（B2）、零成本跟踪 rustc_public（B3）。

## 1. 问题定义与结论速览

产品形态是「预先 install 依赖，然后直接解释运行 + 分层 JIT」。由此派生两问：

1. **是否要自己写/改 Cargo 与 rustc 前端？**
   - rustc 前端：**不写、不 fork、不打语义补丁**。以 `rustc_private` 把 rustc
     当库嵌入（现状即如此），语言语义由主线 rustc 计算，一致性按构造成立。
   - Cargo：**自研其解析/调度子集**（cargoless，D15，动机见
     [d15-cargoless-design.md §1](d15-cargoless-design.md)），但真 Cargo 以
     compat 轨长期留任行为裁判（decision-history §7.37），一致性靠对拍证明。
2. **如何与主线保持行为一致？** 见 §2 的三层机制；唯一持续性工作是 §4 的
   toolchain bump 节奏。

## 2. 现状盘点：已成立的三层一致性机制

| 层 | 机制 | 一致性来源 | 现状证据 |
|---|---|---|---|
| 语言/编译器语义 | rustc as library：`rustc_driver::run_compiler` + `after_analysis` callback 内 lower；sysroot 自 rust-src 自建（D15 P4 起走自有 cargoless 调度：伪根 + `PackageManifest` + `compile_plan`；**不再用 `rustc-build-sysroot`**），dep 走 `-Zalways-encode-mir` + metadata-only rlib | **按构造一致**：parsing/宏/typeck/borrowck/单态化/layout/ABI 全由 pinned rustc 计算 | `src/baseimage.rs`（`run_compiler`/`after_analysis`）、`src/sysroot.rs`（`build_sysroot`）、`src/cargoless/schedule/args.rs`（`-Zalways-encode-mir`） |
| Cargo 语义 | 双轨：compat 轨占 Cargo 的 `RUSTC` 槽（机制移植自 cargo-miri，`src/cargo_shim.rs` 三相），按构造一致；self 轨 = `src/cargoless/` 自研解析/调度 | **可证一致**：fresh lock 与固定 Cargo 逐字节相同且被 `cargo --locked --offline` 接受（audit 链）；corpus full 双腿逐字节对拍；每条语义规则有 probe 实证（d15 §8）；未实现角落响亮拒绝，不静默近似 | current-status D15 P1–P5、D17 行；[mirvm-test-cargoless-contract.md](mirvm-test-cargoless-contract.md) |
| 运行时语义 | native 差分：真 rustc+LLVM 编出 native 当 oracle，stdout/stderr/exit 逐字节；corpus 三维（mirvm 默认 / native / 逢调即编 JIT） | 漂移以差分 RED 形式暴露 | `tests/` 的 differential/corpus 套件 |

三层的**基准全部同源于一个 pin**：`rust-toolchain.toml` 钉 dated nightly，
rustc、cargo（裁判）、rustdoc（doctest 前端）、rust-src（sysroot）随 pin 整体
升级，不存在「裁判和被测者版本错开」的缝隙。混用工具链已被识别为危险面
（D15 P3 曾显式钉 `RUSTC` 防 rustup 按 cwd 解析出混合工具链）。

## 3. 生态参照系与路线定位

- **in-tree 模式**（Miri / Clippy / cg_clif）：住在 rust-lang/rust 树内经
  subtree 双向同步，rustc 自身 CI 持续测它们，破坏性 PR 由上游负责修——
  同步最强，但要求进树治理与持续跟主线的人力。对 mirvm 现阶段不现实。
- **out-of-tree pin 模式**（Kani、Charon 等 MIR 消费工具的公开做法）：钉一个
  dated nightly，周期性 bump，每次 bump 是一个带完整回归的变更。
- **定位结论（建议维持）**：mirvm 属于 out-of-tree pin 模式，这是当前规模下的
  合理默认。in-tree 化不立项；重开条件 = 项目公开成熟且上游协作关系成立。

以上生态描述是截至本文撰写的公开状态，未随本仓验证，不作为合同依据。

## 4. 建议 B1：toolchain bump 制度化（优先级最高）

现状：`rust-toolchain.toml` 注释约定「每月 bump」；open-issues 触发器速查挂着
「pinned toolchain 升级 → D9e emit 剪枝重测」；2026-07 那次 pin 撞过 LLVM 22
release 误编译（decision-history §7.33 的 `debug=2` + `strip` workaround）。
即 bump 本身是高危事件，但目前没有成文的必跑集。

**建议形态：以现有 gate 为主体，只补它没覆盖的部分。** bump 必跑 =
`./tests/run.sh gate`（它已经覆盖全部静态面与语义面对拍：`quality.rust`
（fmt/clippy/test）、Cargo 三条合同、`differential.{programs,cargo,cargoless}`、
corpus 双腿 `corpus.contract`、runtime 各套件（TSan 在 `runtime.semantics` 内）、
`performance.limits`、`harness.truth`），**再加下列 gate 不覆盖的四项**：

1. **空 `MIRVM_HOME` 冷建 sysroot 一次**：P5 第一批就是在空目录冷建时撞出 rust-src
   `[lints.rust]` 传播缺口，此步有前科价值。随后确认 stamp/键随 toolchain 换代失效
   （stamp = `<sysroot>/lib/rustlib/<host>/.mirvm-sysroot-hash`，逃生门 = 删
   stamp/sysroot；该条在 open-issues §H 表内，不在 §G）。
2. **触发器清账**：逐条走 open-issues 触发器速查表（当前含 D9e emit 剪枝重测）；
   并按 §7.33 记的移除条件复评 `debug=2` + `strip="debuginfo"` 在新 LLVM 上是否仍需
   要——判据是升级后先 `debug=0` A/B 复跑，不是猜。
3. **`rust-toolchain.toml` 注释里的 rustc commit 必须与新 pin 一致**：当前文件写
   `c397dae80`，pin 实际解析到 `4c9d2bfe4`（本轮已改正）。bump 时以
   `rustc +<channel> --version` 的输出为准更新，别让注释里留一个过期的版本号。
4. **分诊与收口**：每个失败三分——①上游行为变化（跟进实现/合同整体前移）、
   ②上游 bug（钉版 + 上游 issue 记档；先例 = D15 P1 的六枚上游破洞钉版）、
   ③本仓潜伏债（修复并补回归）。全绿后单 commit 收口
   （`chore: bump toolchain`），必跑集结果随 commit 留档。

验收（采纳后）：下一次真实 bump 按本清单走完并留档一次。

## 5. 建议 B2：上游测试套件作为额外差分 oracle

动机：现有 corpus 是自建程序 + 真实 crate，覆盖「想到要测的」和「真实世界
用到的」；上游测试套件覆盖「语言角落」，且其内容随 rev 前移，语义变化自动
进入 oracle。这是当前对拍体系里唯一缺的 oracle 来源。

- **首选源：Miri 的 pass 测试集**。单文件、无依赖；按 pinned toolchain 对应的
  rev 钉住导入。
- **准入必须先过 native（本轮补的关键一条）**：Miri 是 UB 检查器，它 pass 用例的
  判据是「Miri 不报 UB」，**不是**「输出与 native 一致」；其中相当一部分依赖
  `-Zmiri-*`（isolation / provenance / tree-borrows）与 Miri 自己的内存模型，而
  native 侧没有对应物。因此准入过滤 = **该用例在 native 下 exit 0 且输出确定**，
  满足才允许进 mirvm 差分，不满足的直接出局。加了这道过滤，这批用例的价值才定位
  清楚：**语言角落的覆盖面（mirvm 能不能把它跑起来）**，而不是一致性证明。
- **导入形态（本轮补，须先裁定）**：本仓的既有约束是离线自包含（sysroot 自建、
  运行期零 cargo、包里不留源码痕迹），所以**钉 rev 把测试文件拷进仓库**是与现状
  相容的形态；「执行时去上游拉」与之冲突，不采用，仓库体积增大是可接受代价。
  上游测试文本是 MIT/Apache-2.0，导入时在文件头留出处（先例：`src/cargo_shim.rs`
  抄 cargo-miri 的归属写法）。
- **次选源：rustc `tests/ui` 的 run-pass 子集**。量大但 directive 驳杂
  （`ignore-*`/`needs-*`/unstable feature/aux-build 等），需要 harvester +
  准入过滤器，成本明显更高，放第二阶段。
- **差分口径**：沿用 corpus 三维（native / mirvm 默认 / 逢调即编）；输出不
  确定的用例（地址、时间、线程序）按上面的准入规则剔除；Linux/x86_64 之外的平台
  directive 直接过滤（实现现状即 Linux/ELF/x86_64 优先）。
- **制度**：沿用 corpus 的 P5 单列纪律——「归 P5」响亮拒绝单列、不计失败、
  不静默跳过；nightly feature 用例如实准入（本仓本就 pin nightly）。**不为上游
  套件另搭一套 runner**：接进现有 corpus 机制（同一份 manifest、同一套判定口径）。
- **规模控制**：先 spike 100–300 条验证信噪比（预告：首批分诊工作量不小，
  std 内部行为/平台依赖/feature gate 都会撞），再裁定常驻 gate 还是只进
  B1 的 bump 必跑集。

验收（采纳后）：首批导入双腿全绿或逐条分诊记档；spike 结论（常驻 or
bump-only or 放弃）写回本节。

## 6. 建议 B3：rustc_public（StableMIR）零成本跟踪

上游为 MIR 消费工具提供稳定接口的项目。截至本文撰写的认知：其对
layout/ABI/单态化/const-eval 的查询覆盖不足以支撑解释器 + JIT，Miri 自身未
迁移。故：

- **不迁移、不立项施工**；在 B1 的 bump 清单末尾附一行「看一眼 rustc_public
  进展」，有实质变化才记档。
- **重评触发条件**：① 稳定面覆盖 layout/ABI 查询；② Miri 或 cg_clif 量级的
  工具开始实际消费。
- **收益边界（诚实）**：即便可迁，收益是降低 `rustc_private` 的月度 API 追赶
  成本，**不是**脱离 nightly——`-Zalways-encode-mir`、rust-src sysroot 等仍是
  nightly 事实。

## 7. 明确不建议做的事（拒绝面）

1. **不 fork / 不 patch rustc 语义**。上游 bug 走「钉版 + 上游 issue 记档」
   （既有先例见 D15 P1）；本地私改语义等于自毁「按构造一致」的地基。
2. **不自写前端**（parser/typeck/borrowck/宏展开）。一致性会变成无底洞。
3. **不删 compat 轨**。decision-history §7.37 已裁定长期保留；其「裁判」职能
   是 B1/B2 的地基，此处只是重申，不是新决定。
4. **不追 stable channel、不做运行时多版本 rustc 适配**。每个 mirvm 版本绑定
   一个明确 toolchain（mirvm-test-cargoless-contract §4 尾段既有口径）；
   nightly pin 是结构性事实，不是待还的债。
5. **不为上游测试套件另搭一套 runner**（若采纳 B2）。接进现有 corpus 机制即可；
   两套执行面会各自漂移，而且第二套不会有人持续维护。

## 8. 采纳路径

| 建议 | 成本 | 建议节奏 |
|---|---|---|
| B1 bump 制度化 | 纯流程化，近零代码 | 下一次 bump 前定稿并首跑 |
| B2 上游套件 oracle | 测试文件导入 + spike 的分诊人力（须先定导入形态） | 先做 Miri pass 小批 spike，再裁定去留 |
| B3 rustc_public 跟踪 | 每次 bump 一眼 | 随 B1 清单生效 |

采纳即在 open-issues.md 登记（G 维护态或新 D 项），裁定进 decision-history，
并更新本文状态行；「已批准」「已实现」「测试通过」三断言按仓库纪律分开陈述。
