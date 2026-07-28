# D15 设计：砍掉 cargo——自有依赖解析与编译调度

## 8. P1 施工实录：求解语义定稿（2026-07-27）

P1 的闭合过程把 cargo 的解析语义逐条实证出来（每条都有对拍实锤，
证据链在 decision-history §7.28）。定稿规则如下，即 `resolve.rs` 的
当前实现口径：

1. **resolve 图 vs build 图分裂**：Cargo.lock 的解析图是全平台并集
   （`cfg(any())` 永假边照进；windows-sys 在 Linux 机入锁），build 图按
   host `rustc --print cfg` 求值过滤。版本求解、feature 激活、lock 依赖行
   用 resolve 图；编译单元（units）用 build 图（`unify_features` 的
   `include_weak` 双态）。
2. **多版本 fork（lazy-bucket）**：同名 crate 允许 semver 不兼容的多版本
   并存（hashbrown 0.14/0.15、syn 1/2/3 同图）。包 id = (name, bucket)：
   dep 边到达时与既有 bucket 的累积区间有共同候选（index 有版本同满足）
   即并入，否则开新 bucket；pubgrub 按 bucket 独立回退。可达集过滤清掉
   pubgrub 回退留下的孤儿 bucket。
3. **optional 依赖的门**：进版本求解与 lock 依赖行当且仅当
   ① 被（父包, 依赖键）激活（强形：dep:/隐式/x/y 三形态），或
   ② 被**已启用 feature 以 ?/ 弱形引用**——引用即入图（resolve 图语义），
   特征照常下发（可与强激活级联：rust_decimal std → borsh?/std →
   borsh std → bytes?/std → bytes 入锁）。build 图仅 ①。
   **门按（父包, 依赖键）判定**——全局包名门会把 A 包激活的同名依赖
   误植到 B 包（zerovec 的 yoke → litemap 的 yoke ^0.8 实锤）。
4. **feature 统一**：resolver v2 的 normal/build 边分列（同 crate 两类
   feature 集不同 = 两个编译单元）；feature 引用必须指向 feature 或
   optional 依赖，否则响亮报错；边到达的 feature 旗标若无表项可展开，
   其本身若是非隐藏 optional 依赖键即激活该依赖（收尾清扫规则）。
5. **pre 精确规则**：pre 版仅当被该包某 req 中 major/minor/patch 全同
   且带 pre 的 comparator 点名时才可选（req_to_ranges 自写保留下界 pre；
   ark-ff-asm 0.5.0-alpha.0 误选实锤）。
6. **yanked**：lock 在照吃（cargo 同）；fresh 求解跳过（no-solution 响亮）。
7. **lock 形态**：canonical v4——依赖行每行尾逗号（cargo `--locked` 对
   非 canonical lock 一律判"需重写"而拒）；同名多版本时依赖行写
   `name version` 消歧 hint。
8. **已明说的 P1 边界（记账，不冒充闭合）**：
   - **rust-version-aware 版本偏好未实现**（cargo 1.84+ 默认
     `resolver.incompatible-rust-versions=fallback`：新版要求更高 rustc
     时 cargo 回退选旧兼容版）。影响面 = 与 cargo 的选择可能不同但
     lock 自洽可构建；钉版 nightly  bleeding-edge 下几乎不触发。
     与 D14 store 合并评审时补。
   - req 遇本仓钉版未知的 semver 新 op 时退回 `Ranges::from_req`
     （pre 会丢，代码内响亮记账）。
   - git 源 / alt registry / workspace 多包图 / source replacement
     （P5，响亮拒绝记名）。


> 状态：2026-07-23 调研定稿（四决策点当日裁定）；**P1 已收口（2026-07-27，
> §8 求解语义定稿，decision-history §7.28）；P2 已收口（2026-07-27，
> corpus smoke 24 双腿逐字节 24/24，decision-history §7.29）；P3 已收口
> （2026-07-28，corpus full 138 pass 1 p5 0 fail + gate DEPS=self 双轨绿，
> decision-history §7.30）**；P4（sysroot 自管 + 默认翻转）待施。
> 立项记录：[open-issues.md D15](../open-issues.md)；动机源头：decision-history §7.22
> （C4 两轮绕行被否——"吃 cargo 产物就得绕"的处境要制度性消除）。
> 本文遵循"闭合契约"纪律：每期写明闭合到哪条可观察边界；原理上不能闭合的
> 事先明说，不许绕行冒充闭合。

## 0. 一句话

mirvm 自己当每个 dep crate 的编译调度者：自解析 manifest、自解版本、自排
拓扑序、自跑 build.rs、自编 proc-macro，bin crate 仍走既有的
`MirvmCallbacks` 降低通道——`mirvm run` 全程零 cargo 进程。

## 1. 为什么砍（三句话讲清动机）

1. **运行期去工具链化**：`mirvm run` 目前隐含依赖完整 cargo + registry 在线，
   这与 mode B（`.mirvm` 包自包含分发）的世界观冲突——包格式三片已落，
   生产侧的 cargo 依赖是下一个要拔的桩。
2. **调度自主权**：cargo 的指纹/排程/产物命名是黑盒，mirvm 的 L2 cache、
   deps-image、D14 store 都在"读 cargo 的产物反推意图"，每一层反推都是
   潜在漂移面（批11 实锤的双名单漂移是同构教训）。
3. **制度性消除绕行**：C4 的判例——dep global_asm 需要 dep 编译期的
   介入点，挂 RUSTC_WRAPPER 旁路才拿到；凡"想要一个钩子就得寄生在别人
   的调度里"的设计，都是被 cargo 的形态牵着走。自己的调度器 = 钩子免费。

## 2. cargo 今天替 mirvm 干什么（调研实锤清单）

| 职责 | 今天谁算 | 证据 |
|---|---|---|
| manifest 解析（package/deps/features/targets/workspace/profile） | cargo | mirvm 零 TOML 解析调用 |
| 版本解析（semver req → 具体版本；lock 读写） | cargo（无 lock 时每次重解，G7 在案） | `MIRVM_CARGO_LOCKED` 是唯一钉版机制（cargo_shim.rs:107-115） |
| registry 获取（sparse index、.crate 下载/校验/解包） | cargo 全委托 | 源码零处读 registry |
| feature 统一（resolver v2：normal/build 边分离） | cargo | — |
| build.rs 全生命周期（host 编译→执行→指令解析→传播） | cargo | mirvm 零行读 OUT_DIR（§7.22 明令绕行禁区） |
| proc-macro host dylib 编译 | cargo（host 不带 --target，wrapper 透传） | cargo_shim.rs:198,207-212 |
| 每 crate rustc 参数（--extern 闭包/-L/-l/--cfg/edition/metadata 哈希/profile 旗） | cargo | bin 侧全量透传（agent-178 清单） |
| 指纹与增量（mtime/dep-info/flags 哈希） | cargo（D14 共享 target 依赖它） | cargo_shim.rs:123-131 |
| bin 产物定位与运行协议 | cargo runner + 假二进制 JSON | cargo_shim.rs:214-222,340-383 |

**mirvm 已有资产（"已有其半"，不重建）**：`run_dep_compiler`（in-process
rustc_driver，`-Zno-codegen` + MIR sysroot + DepCallbacks global_asm 抽取，
cli.rs:493-517）；`MirvmCallbacks` bin 降低通道；ircache L2（输入盖戳 =
sess.file_depinfo + used_crate_source，自算内容寻址）；baseimage/depsimage；
D14 统一 target store；sysroot 自产（rustc-build-sysroot，可自管化见 §6 P4）。

## 3. 自写清单（按依赖序）

1. **manifest 模型**：`Cargo.toml` 解析（package/lib/[[bin]]/[dependencies]/
   [build-dependencies]/[features]/[profile]/[workspace] 基本继承/
   target.'cfg()'.dependencies 的平台求值——mirvm target 恒 = host triple，
   cfg 求值面因此有限）；frontmatter 脚本物化沿用现有 parse_frontmatter
   （cli.rs:1039-1074）但改喂给自有模型，不再物化成 cargo 项目。
2. **lock 读写**：读（v3/v4 格式）优先；写（自解出图后落 lock，供复现）。
3. **版本解析**：lock 在 → 按 lock（闭合）；lock 不在 → semver 求解
   （决策点 ④，见 §7）。
4. **registry 访问层**：sparse index 读取（HTTP）+ .crate 下载/sha256 校验/
   解包 → 自有 store `~/.mirvm/registry/{cache,src}`（决策点 ①②）。
5. **feature 统一**：resolver v2 语义子集——normal deps vs build deps 边分离、
   optional/dep:/weak(?)、default-features；**dev-deps 整体不求**（mirvm
   永不跑 test，事先明说）。
6. **拓扑调度 + 每 crate rustc 参数**：edition/--cfg features/--extern 全闭包
   （含 proc-macro `.so`）/-L/-l/crate-name；产物命名哈希**自定方案**
   （cargo 的 -C metadata 算法不稳定不追，反正 cargo 已退场——内部一致即可）；
   参数直接喂既有 `run_dep_compiler`（deps）与 `MirvmCallbacks` 会话（bin），
   **假二进制与 runner 协议整体退役**（E36 cwd 语义顺带在新路径闭合：
   guest cwd = 调用者 cwd，与 cargo run 一致）。
7. **build.rs 全生命周期**（最大新增职责，151 个 crate 实锤普遍性）：
   host 真编译（codegen）→ 以 cargo 兼容 env 执行（CARGO_PKG_*/OUT_DIR/
   TARGET/HOST/PROFILE/CARGO_CFG_*）→ 解析 `cargo::rustc-link-lib/-search/
   -cfg/-flags/metadata` 指令 → 传播（-l/-L/--cfg 进依赖者 rustc 参数；
   OUT_DIR/CARGO_PKG_* 进 bin 会话 env；DEP_* 进下游 build.rs env）。
   build.rs 是任意代码——语义就是"执行它"，我们同样执行（cc/pkg-config
   等外部工具依赖照旧，与现状同口径）。
8. **proc-macro**：host 编译为 dylib（真 codegen），--extern 进依赖者；
   机制直白（host/target 二分判据 wrapper 已有雏形）。
9. **指纹**：自定粗粒度 v1 = hash(manifest 子树 + lock 版本集 + features +
   rustflags + build.rs  rerun-if 输出 + rustc 版本 + 源树 dep-info)；
   不需要 cargo 指纹兼容（cargo 已退场），但**语叉钉死**：profile 语义按
   cargo dev profile 复刻（debug-assertions=on、overflow-checks=on——
   jiff debug_assert 判例：这两枚旗进 MIR 语义，错配 = 对拍漂移）。
10. **配置子集**：`.cargo/config.toml` 的 build.rustflags/target.*.rustflags；
    source replacement/alt registry 归 P5 按实需。

## 4. 模块形态

新命名空间 `src/cargoless/`（直说：无 cargo 构建）：

```
src/cargoless/
  manifest.rs   # Cargo.toml 模型 + frontmatter 接入 + cfg 平台求值
  lockfile.rs   # Cargo.lock 读写
  registry.rs   # sparse index + .crate 下载/校验/解包 + 自有 store
  resolve.rs    # 版本求解 + feature 统一 → 编译单元图
  schedule.rs   # 拓扑排序 + 指纹 + 每 crate rustc 参数计算
  buildrs.rs    # build.rs 编译/执行/指令解析/传播
  proc_macro.rs # host dylib 调度
  driver.rs     # mirvm run 新路径（替代 phase_cargo；compat 路径保留）
```

新直接依赖：`toml`（锁内已有 1.1.2）、`semver`（锁内已有 1.0.28）；
HTTP/tar 见决策点 ①。

## 5. 分期与闭合契约

### P1 地基：解析库 + 审计工具（不接 run 路径）

- 范围：§3 的 1-5（manifest/lock/registry/resolve/feature 图）。
- 闭合契约：对仓内全部 corpus 条目（164 + projects）——lock 在场者，
  自解版本集 **== lock 版本集**（审计工具逐条对账）；lock 缺席者
  （frontmatter 脚本），自解落 lock 后 `cargo build --locked --offline`
  能原样接受（兼容性反证）。build.rs/proc-macro 不在本期。
- 验收：审计工具全绿；单测覆盖 manifest/feature 形态矩阵。

### P2 机制全：调度 + build.rs + proc-macro（粗指纹 v1）

- 范围：§3 的 6-10；指纹粗粒度（build.rs 每次重跑，rerun-if 精细化归 P3）。
- 闭合契约：**corpus smoke 层 24 条目全量**（含 blake3/crossbeam/mimalloc/
  libgit2/rusqlite/mlua/tree_sitter 等 build.rs 重灾户）以零 cargo 进程
  跑通，stdout/stderr/exit 与 cargo 路径逐字节一致（新增对拍轴：
  self 路径 vs cargo 路径自一致 + 原三维判绿照常）。子集外构造
  （workspace 复杂形态/git deps/alt registry）**响亮拒绝点名构造**，
  不静默回退 cargo。
- 验收：`MIRVM_DEPS=self bash tests/corpus.sh --tier smoke` 24/24。

### P3 迁移：指纹精细化 + corpus 全量 + 双轨 gate

- 范围：rerun-if 精细增量（build.rs 不重跑语义对齐）；corpus full 层
  全量迁移；gate 增 DEPS 轴（self 全量一轮 + cargo compat 路径保留冒烟）。
- 闭合契约：`MIRVM_DEPS=self bash tests/gate.sh` 179/0/0/0 同构绿；
  每条目 self 路径与 cargo 路径逐字节一致。
- 验收：gate DEPS=self 全绿；冷/热 L2 行为不变式照绿。

### P4 sysroot 自管 + cargo 退场（默认翻转）

- 范围：sysroot 构建改用 D15 自管调度（rust-src 全本地源码 + 固定
  ~27 crate 图；顺手砍掉意外 crates.io 依赖——agent-177 实锤 .d 引用
  ~/.cargo/registry，改走 rust-src `library/vendor/`）；`MIRVM_DEPS` 默认
  翻为 self，cargo 路径保留为显式 compat（`MIRVM_DEPS=cargo`），
  **删除条件**另行评审（compat 不是救援，是双轨：两条路径各自完整）。
- 闭合契约：sysroot 构建零 cargo 进程；`mirvm run`（项目/脚本）默认路径
  全程零 cargo；compat 路径 gate 冒烟保留。
- 验收：purge --sysroot 后冷建全绿；gate 双轨绿。

### P5 复杂语义按实需（不设闭合承诺的边界）

- workspace 多包继承全形态、[patch]/[replace]、git deps、alt registry、
  source replacement——**按 corpus 扩编实需逐项立项**；本期不承诺
  "cargo 全语义"（事先明说的不闭合面；遇到即响亮拒绝并登记，按实需扩）。

## 6. 风险与诚实边界

- **semver 求解**：lock 在 = 闭合（读锁）；lock 不在见决策点 ④。
- **feature resolver v2 边角**（同一 crate normal/build 边不同 feature 集
  的并集规则）：按 cargo book 语义实现 + corpus 全量实证背书；
   exotic 形态（weak dep features 链式激活）单测矩阵覆盖。
- **build.rs 任意性**：它能联网/写任意路径——与 cargo 同口径（不沙箱，
  语义即执行；沙箱化归 D10 产品面，不在 D15 掺和）。
- **profile 语叉**：debug-assertions/overflow-checks 进 MIR 语义
  （jiff 判例），P2 起硬钉 dev profile 等价旗；opt-level 对
  -Zno-codegen deps 无语义影响（照传不误判）。
- **cargo 新旧版本行为差**：compat 双轨只对 pinned toolchain 的 cargo
  背书（与现状同）。

## 7. 决策点（**2026-07-23 已裁定**）

1. **HTTP/解包** → **纯 Rust crate**（ureq + flate2(miniz_oxide) + tar；
   自包含优先于依赖树最小；TLS 后端以 ureq 默认 rustls 落地）。
2. **registry store** → **自有 `~/.mirvm/registry` + 读穿复用
   `~/.cargo/registry`**（只读不污染；读穿顺序 = 自有 src → 自有 cache →
   cargo src → cargo cache → HTTP）。
3. **分期轴** → 照 §5 的 P1→P5。
4. **lock 缺席求解器** → **`pubgrub` crate**（0.4；原理闭合 + 工程量可控）。
