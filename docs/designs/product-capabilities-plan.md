# 产品能力补全计划

> 状态：**P1 主链已完成；P2 的 resolver 3 / rust-version、Git 依赖、替代
> registry/Cargo 配置、source replacement/patch/replace 与 pack self 五批及 full
> corpus 总验收均已完成（2026-08-10，138 PASS / 1 SKIP / 0 FAIL）；D17、D19 与
> D3 mmap/逐函数惰性装载核心已于 2026-08-11 完成**。
> 本文把当前明确缺失的产品
> 能力合并成一条可执行路线；现状仍以 [current-status.md](../current-status.md) 为准，
> 单项债务仍以 [open-issues.md](../open-issues.md) 为唯一登记入口。

## 1. 排序原则

1. 先补日常 Rust 开发的主流程，再扩服务形态和平台数量。
2. “能运行受信任代码”与“能安全运行不受信任代码”分开声明。正式沙箱完成前，
   后者一律不宣称支持。
3. 每阶段用真实项目的 stdout、stderr、退出码和资源边界验收，不以“命令存在”代替完成。
4. `.mirvm` 对外格式冻结必须晚于 D3 零拷贝布局；否则刚冻结就必须破坏兼容。

## 2. 建议施工顺序

### P1：`mirvm test`，补齐开发主循环

**进展（2026-08-11）**：D17 完成。单包、resolver 1/2/3 workspace、bench、根
proc-macro 测试、复杂成员 glob、workspace lints 和完整路径 package spec 都由
[cargoless 合同](mirvm-test-cargoless-contract.md) 约束；成员发现、继承、统一 lock、
package 选择和特性统一均在依赖机制内实现，没有命令层特判。D19 后续也已用 rustdoc
前端接入 doctest，没有把代码块伪装成普通 test。

**用户结果**：项目可以直接运行单元测试，不必退回 cargo 才能完成“改代码、跑测试”。

- 解析 lib/bin/test target，按 `cfg(test)` 重编本包；复用现有 `test` sysroot crate。
- 接入 libtest harness，透传过滤器、`--nocapture`、`--ignored`、线程数和退出码。
- 覆盖 workspace 根、默认/当前成员、指定 package 和 workspace 特性；doctest 由固定
  rustdoc 提取、裁判，临时 crate 交给 VM 编译执行。
- 验收：至少一个 lib、bin、integration test、失败测试和 panic 测试与
  `cargo test` 的可观察结果一致；默认依赖驱动全程不启动 cargo。

对应欠账：D17。

### P2：依赖语义 P5，消除常见项目拒绝

**用户结果**：Git 依赖、替代 registry 和 source replacement 可以直接解析、锁定、
离线复跑，不要求用户改写项目；resolver 3 与 rust-version-aware 选择也能正确工作。

- resolver 1/2/3 常见 workspace、多包图、精确/版本/路径 package spec、复杂成员 glob、
  workspace lints 与 rust-version-aware 候选选择已完成；嵌套 workspace 和同名成员按
  Cargo 自身规则拒绝，不把 Cargo 的错误形态误写成 mirvm 缺口。
- Git 源以提交哈希入锁和内容存储；网络获取与离线消费分开，禁止浮动 HEAD 偷换内容。
- 替代 registry 复用 Cargo 凭据的只读面；实现 index/source replacement 的来源映射。
- 让 `mirvm pack` 改走自有依赖驱动。
- 验收：现有 corpus 的 P5 项清零；每种来源都有首次获取、`--locked`、`--offline`、
  内容篡改拒绝和双版本共存用例。

对应欠账：D15 P5、D14、D18 的获取/环境部分。

#### P2 施工批次（2026-08-10 定稿）

以下批次按顺序闭合；前一批的语义和回归未通过前，不把后一批接入默认路径。
“对齐 Cargo”采用版本化合同：固定版本 Cargo 是当前裁判，新 Cargo 版本只做差异
观察；只有差异被解释、验收并记入决策记录后，才有意升级裁判，不能随工具链更新
静默改变 mirvm 行为。沿用现有合同脚本，只有当前 RED 无法复现或判断时才增加最小
夹具，不另建通用 harness。

1. **resolver 3 与 rust-version-aware 选择（已完成，2026-08-10）**
   - 支持显式 `resolver = "3"`，以及 edition 2024 package 的隐式 resolver 3；
     resolver 1 后续已在 D17 余项中补齐显式和默认推导及 feature 统一。
   - 读取包、workspace 继承和 registry index 中的 `rust-version`。按照 Cargo 的
     `resolver.incompatible-rust-versions = fallback/allow` 策略选择候选版本，不能把
     “当前 rustc 编不过”误报成普通版本冲突。
   - fresh lock、既有 lock、`--locked`、`--offline` 和 resolver 2 回归一起验收。
   - 完成条件：Cargo 与 self 的选中版本、feature 图一致；双方生成的 lock 都能被
     对方在 `--locked --offline` 下接受。
   - 完成结果：resolver 3 显式/edition 2024 隐式规则、workspace 最低 Rust 版本、
     `fallback/allow` 配置层级、`--ignore-rust-version` 和编译前版本诊断均已落地；
     Rust 1.82 及以前的项目写 lock v3，1.83 起写 v4。固定 Cargo 与 self 的真实
     `home = "0.5"` 探针都选择 0.5.9，self lock 已被固定 Cargo 以
     `--locked --offline` 接受。空目录 fast 门禁同时暴露并补齐了 package `[lints]`
     到 rustc 参数和编译指纹的传播，当前 rust-src sysroot 可以从零构建。
2. **Git 依赖（已完成，2026-08-10）**
   - 支持默认分支、`branch`、`tag`、`rev`、仓库内 workspace/package 和 feature；
     私有仓库复用 Git 既有凭据机制，不新增 mirvm 报名表。
   - 网络获取只负责把可变引用解析到不可变 commit；lock 记录精确 commit，本地内容
     存储按仓库身份与 commit 隔离。locked/offline 路径不得重新解释浮动分支。
   - 完成条件：首次获取、暖缓存离线、冷缓存离线失败、分支移动后 locked 不变、
     内容篡改拒绝、同仓库双 commit 共存，并证明 self 不启动 Cargo。
   - 完成结果：默认分支、`branch`、`tag`、`rev`、仓库内 workspace/package、feature
     与 path 边已接通；系统 Git 复用用户凭据和 submodule 机制。lock 精确记录 commit，
     构建指纹包含完整 Git 来源；同名同版本的两个 commit 在解析图、编译缓存和 lock
     依赖行中保持分立。`cargoless_git_contract` **9/9**，self 单/双 commit lock 均被
     固定 Cargo `--locked` 接受，execve 审计证明 self 不启动 Cargo。
3. **替代 registry 与 Cargo 配置合并（已完成，2026-08-10）**
   - 为依赖获取实现项目祖先目录和 `CARGO_HOME` 的 `.cargo/config.toml` 合并，覆盖
     `[registries]`、稀疏/Git index、认证读取；不借机实现无关 Cargo 配置键。
   - 来源身份包含 registry，保证不同 registry 的同名同版本包不碰撞；继续执行
     checksum、locked、offline 和篡改拒绝。
   - 完成条件：公共/私有替代 registry 的首次获取与离线复跑对齐 Cargo，配置缺失、
     认证失败和来源冲突均响亮报错。
   - 完成结果：实现 `$CARGO_HOME` 到项目祖先目录的配置层叠、`include`、环境覆盖、
     extensionless `config` 优先，以及依赖来源所需的 registries/registry/source/
     credential-provider 子集。替代 sparse registry 支持 token、`cargo:token`、
     `cargo:token-from-stdout` 与 Cargo credential protocol v1；registry 身份进入解析图、
     lock 和本地 store。固定 Cargo 1.98 nightly 与 self 的认证 sparse 真实合同已覆盖
     fresh、逐字节同 lock、Cargo `--locked` 接受、离线热缓存与 execve 零 Cargo。
4. **source replacement、`[patch]` 与 `[replace]`（已完成，2026-08-10）**
   - source replacement 按来源映射链工作，覆盖 registry 镜像、local registry 和
     directory vendor，并拒绝替换环。
   - `[patch]` 的 path/Git/registry 包必须作为版本候选参与求解，不能在求解后替换
     源码；旧式 `[replace]` 按精确 package ID 处理。
   - 完成条件：直接/传递依赖、多版本、未使用 patch、替换链及 lock 来源均与 Cargo
     一致。
   - 完成结果：registry 镜像、local registry、Cargo vendor directory 三类替换共用
     来源映射链，替换环响亮拒绝；directory 每文件 checksum 篡改会被拒绝。registry
     目标上的 path/Git/registry `[patch]` 在求解候选阶段生效，未选版本写入
     `[[patch.unused]]`；`[replace]` 保留 Cargo 的原 package 行与 `replace` 指针。
     标准来源合同 **30/30**，所有 self fresh lock 与固定 Cargo 逐字节相同并被其
     `--locked --offline` 接受。Git source replacement 和以 Git URL 为目标的 patch
     尚未纳入本批合同，继续作为明确边界。
5. **`mirvm pack` 翻到 self 依赖驱动（已完成，2026-08-10）**
   - `run`、`test`、`pack` 共用同一依赖图、build.rs、proc-macro、native library 和
     lock 规则；PATH 哨兵与进程审计证明默认路径不启动 Cargo。
   - Cargo compat 长期保留：用户可显式回退，也是 Cargo 行为对拍和差异定位的裁判；
     cargoless 继续作为默认路径。两条路径都必须保持完整，不能把 compat 降成无人维护的
     临时救援开关。
   - 完成结果：项目和 frontmatter 脚本缺省复用 cargoless 的解析、build.rs、
     proc-macro、native library 与最终 rustc 会话；`MIRVM_DEPS=cargo` 仍走原 Cargo
     runner。pack 合同 **6/6**：默认路径生成包、execve 零 Cargo、全新
     `MIRVM_HOME` 自包含运行、显式 Cargo 回退真实启动固定 Cargo 且其包同样可独立
     运行，非法模式值响亮拒绝。
6. **依赖阶段总验收**
   - 清零现有 corpus 中标作 D15 P5 的项目；每种来源都覆盖版本图、feature、fresh/
     locked/offline、篡改拒绝和双版本共存。
   - 只建设解析、获取、离线复跑所需的最小内容存储；完整 env/GC 等到真实数据产生
     无法由现有 purge 处理的问题时再立项。
   - 完成结果（2026-08-10）：第一轮 7 个 RED 逐项分诊并修复同源多版本 lock 边覆盖；
     外部超时/瞬态错误定点复核，`miden_prove` 迁到非 yanked 的 0.25.8 并摘除 P5。
     第二轮 full corpus **138 pass / 1 skip / 0 expected-fail / 0 fail**；唯一 SKIP 是
     本机缺 `libffi.so` 开发链接名。最终非性能 `fast` **12/12**。

**D17/D19 完成（2026-08-11）**：根 proc-macro 测试、bench、workspace lints、复杂
package ID/成员 glob 和旧项目 resolver 1 已落地；doctest 由固定 rustdoc 处理代码块、
临时 crate、行号和 compile-fail，MIRVM 只接管临时 crate 的 MIR 编译与执行。

### P3：OS 级沙箱与资源治理

**进展（2026-08-10）**：VM 内第一层防护已补上操作数区双端 guard page 和 JIT
入帧前栈检查。这只把越界与栈耗尽变成较清楚的失败，不能阻止任意 syscall、native
FFI 或宿主进程级故障，因此不改变本阶段尚未完成的判断。

**用户结果**：执行不受信任代码时，有默认安全边界和明确的 CPU、内存、进程、文件、
网络限额；超限由宿主报告，不拖垮调用进程。

- 以独立 worker 进程作为边界，监督进程只负责构建、策略和结果收集。
- Linux 首版组合 namespace、seccomp、cgroup v2/rlimit 和超时终止；文件系统采用
  明确的只读输入与可写工作目录模型。
- syscall 虚拟化接现有统一 dispatch，但安全性由内核边界兜底，不把 VM 检查冒充沙箱。
- 提供安全默认策略；crate 不需要报名或加入特判名单。确需能力时按资源类别授权，
  并在诊断中说明被拒的操作。
- 验收：死循环、内存耗尽、进程风暴、越界文件写、禁网连接和 syscall 绕过探针均被
  限制；正常线程、FFI、panic 和包运行保持一致。

对应欠账：D10、E23；不推翻已冻结的“VM 内虚拟地址隔离不是正式沙箱”结论。

### P4：稳定嵌入 API 与常驻服务

**进展（2026-08-10）**：多 Engine 的内部身份和核心所有权已落地。Engine 各自持有
Shared/JIT worker，线程执行态按 Engine id 隔离并支持嵌套恢复；引擎故障通过
`RunError` 返回，guest TLS 与退出回调残留会清理，rustc 编译会话也已串行保护。
仍缺公开稳定 API、native 回调撤销、JIT 活动码回收、长寿命宿主线程 TSD 回收及
进程级故障隔离，所以不能把内部基础完成写成 P4 完成。

**用户结果**：调用方可以在一个进程或服务中反复创建、运行和销毁 Engine，错误通过
返回值传播，不直接退出宿主进程。

- 先收敛所有权：Shared、JIT worker、thunk、TLS key、动态库句柄、展开注册和诊断槽
  都归 Engine/Session 生命周期管理。
- 去掉库路径里的 `exit` 和进程全局单槽；加入两个 Engine 并发、创建销毁循环和失败隔离测试。
- 触发 vmctx T/R 复评；只有多 Engine 基础成立后再加 daemon/agent 协议和请求取消。
- 验收：同进程并发 Engine 不串状态；连续创建销毁资源有界；一个 guest 失败不终止服务。

对应欠账：E22、D10、D11。

### P5：包布局定型与可移植分发

**进展（2026-08-11）**：D3 核心布局已完成。格式 v3 以只读 mmap 打开容器，将函数体
拆成独立索引项，按访问惰性驻留，并用上一轮真实访问顺序后台预取；需求解码优先于预测。
E20 完整语义验证仍要求首次装载逐函数临时解码一次，因此 D3 还剩“直接在档案表示上验证”
这一项，完成前不启动 D4 格式冻结。该余项可做，但需要不稳定格式 v4：实际执行字节本身
改为可边界检查、可借用遍历的偏移式归档表示，E20 直接验证同一表示；不能靠并列摘要证明
另一份 postcard 函数体安全。

**用户结果**：`.mirvm` 有明确兼容期和迁移规则，加载大型包不必整包复制解码，并能按
目标平台选择内容。

- 先完成 D3：mmap 读取、逐函数惰性解码、按实际热序预取和需求优先调度。
- 给字节码、机器码和本地库清单分别设版本与能力位；错误必须指出不兼容的具体层。
- 再评审 D4：冻结容器布局、兼容窗口、升级工具和损坏/签名策略。
- 最后加入 fat artifact 的多 target 索引；仍只嵌入自产库，系统 ABI 库按目标契约解析。
- 验收：旧兼容样本、未知可跳过节、不可兼容拒绝、大包内存上限、源码与缓存全缺失运行。

对应欠账：D3 直接验证余项、D4、D2；当前格式 v3 仍明确不稳定。

### P6：跨平台

**用户结果**：支持范围从 Linux/ELF/x86_64 扩展，并且每个平台有同等级验收，而非仅能编译。

- 先把 os/arch/loader/unwind/native-link 契约列成平台接口，清除隐藏的 GNU/x86 假设。
- 第一目标在“Linux AArch64”和“macOS AArch64”间按真实用户负载与 CI 可得性选择；
  前者复用 ELF/进程模型较多，后者更早暴露 Mach-O/dyld 差异。
- 每个平台建立解释、JIT、unwind、线程、TLS、FFI、包、自有依赖驱动的同级矩阵；
  平台特有 inline asm 可响亮拒绝，但基础 Rust 语义不能靠跳过测试宣称完成。

对应欠账：E26、D2。

## 3. 里程碑关系

- P1、P2 五个施工批次和 P2 总 corpus 验收均已完成；D17 余项也已闭合。
- P3 是任何“不受信任代码执行”产品声明的硬前置。
- P4 核心内部基础已开始，但稳定公开 API 与剩余生命周期边界仍是 daemon、agent API
  和 REPL 的硬前置。
- P5 必须遵守“D3 先于 D4”；P6 的 fat artifact 依赖 P5 的 target 索引。
- D16 性能战役可穿插，但不得用性能工作代替上述产品完成条件。

每一阶段开工前，应把本节拆成可独立验收的 issue；远程 GitHub 操作恢复前只在本地文档维护。

## 4. 当前施工队列与禁止提前项（2026-08-11）

当前产品主队列是：

`D17/D19（完成） → D3 mmap/逐函数惰性装载核心（完成） → D3 v4 档案直接语义验证 → D4 格式冻结评审`

OS 沙箱按维护者本轮裁定暂缓，不进入这条施工链；这不改变“当前只能运行受信任代码”的
产品边界。D16 性能线和性能基线也按维护者要求另期统一恢复，本轮不从未测数据推导收益。

以下事项明确不提前：

- 不为未来 Cargo 形态增加通用 harness schema、inventory、provenance 元数据或远程 gate。
- 不删除 Cargo compat；默认 self 与显式 `MIRVM_DEPS=cargo` 是长期双轨合同。
- 不把 doctest 伪装成普通 test target，也不为凑覆盖静默跳过 rustdoc 语义。
- 不在 D3 mmap/惰性解码布局完成前冻结 `.mirvm` 格式。
- 不在 OS 级沙箱完成前宣称可安全执行不受信任代码。
- 不在多 Engine 所有权和失败隔离完成前建设 daemon、agent API 或 REPL。
