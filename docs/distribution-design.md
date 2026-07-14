# 轨 C：分发与产品面设计 —— 负载摄入、缓存分层、打包与发行（D9）

> 状态：**方向已批（2026-07-14，D9a–D9f）；施工未立项**。里程碑编号待立项时定
> （候选 M6），不占用 M5.3–5.5 JIT 编号；施工顺序 §5 中 ①–③ 可与 JIT 期穿插，
> ④ 明确排在 M5.3 之后。
> 本文细化 DESIGN.md 的 mode B 与 m4-plan.md 尾部".mirvm 化 sysroot"雏形；
> 与 current-status.md"分发与产品面"行互为索引。

## 0. 结论先行（用户设想 vs 现状）

| 设想 | 现状（2026-07-14 亲核） |
|---|---|
| raw cargo 项目直接 `mirvm run` | ✅ 已实现（`src/cargo_shim.rs` 三阶段，cargo-miri 机制移植） |
| Rust 脚本带依赖单文件分发 | ✅ 已实现（cargo `-Zscript` 同款 `---` frontmatter，物化到 `~/.cache/mirvm/scripts/<hash>` 走同一 cargo 通道） |
| 统一入口 vs 独立 mirvmc | ✅ 事实定型：`mirvm` 单二进制三形态（run / RUSTC_WRAPPER / runner，busybox 式）；D9a 确认此方向 |
| mirvmc 预降打包分发 | ❌ 未动 = DESIGN.md **mode B**；D9b 定路线：先 L2 缓存，包 = 缓存可移植化 |
| 本地缓存 | 部分：五个内容哈希缓存 + 每项目 `target/mirvm` 已有；**缺 L2 单态 engine-IR 缓存（最高价值）** |

## 1. 现状：已实现的摄入链路（事实盘点）

### 1.1 三种运行形态（`src/cli.rs`）

1. `mirvm run <file.rs>`（纯单文件）：零 cargo 快路径，进程内 rustc 前端直编直跑。
2. `mirvm run <file.rs>`（带 frontmatter）：RFC 3503 语法（shebang 容忍、`---`/`---cargo`
   围栏、正文行号保持），物化为缓存 cargo 项目后走 1.2 通道。
3. `mirvm run <dir | Cargo.toml>`：直接走 1.2 通道。

### 1.2 cargo 三阶段（`src/cargo_shim.rs`，机制移植自 cargo-miri）

- **phase_cargo**：驱动真 `cargo run`，注入 `RUSTC_WRAPPER=mirvm` +
  `target.runner=["mirvm","runner"]` + 独立 `target/mirvm`；强制 `--target <host>`
  作为 host/target crate 区分开关。依赖解析/下载/失败即停 = cargo 原生行为
  （`Cargo.lock`、`~/.cargo` registry 缓存全部白嫖）。
- **phase_wrapper**：host crate（build.rs、proc-macro）→ 真 rustc 正常编译**真执行**；
  target 依赖 → 真 rustc + MIR sysroot + `-Zalways-encode-mir`（产出带全量 MIR 的
  rlib）；最终 bin → **不编译**，写 JSON 假二进制 + stub `.d` 防重建。
- **phase_runner**：cargo"运行"假二进制时回到 mirvm，以 cargo 原始 rustc 参数在
  进程内起前端，单态化 + lower + 解释。

**分工要点**：依赖的 **MIR 化**由 cargo 前置、并行（crate DAG + `-j` + pipelining：
依赖 rmeta 一出下游即开编）、增量（指纹）；依赖的**字节码化（lower）**在每次运行的
加载相做，由单态化按需驱动（天然"用到才降"）。guest 运行时不编译任何依赖。

### 1.3 sysroot 与 toolchain 锁定

- `src/sysroot.rs`：rustc-build-sysroot（Miri 同款）从 rust-src 以
  `-Zalways-encode-mir` 重建 std，缓存于 `~/.cache/mirvm/sysroot-<target>`，
  内容哈希判新。
- `build.rs`：构建期烘焙 `MIRVM_DEFAULT_SYSROOT` + rpath 指向该 sysroot 的
  `librustc_driver.so` ⇒ **mirvm 二进制与构建它的 nightly toolchain ABI 锁死**。
  wrapper 阶段无视 cargo 传来的 rustc 名字、一律用 pinned rustc（proc-macro dylib
  与 rmeta 必须和解释会话同编译器版本）。

### 1.4 既有缓存盘点

| 位置 | 内容 | 键 |
|---|---|---|
| `~/.cargo` | registry/git 源码（跨项目） | cargo 原生 |
| `<project>/target/mirvm` | 依赖 MIR-rlib + 指纹（每项目） | cargo 指纹 |
| `~/.cache/mirvm/sysroot-<target>` | MIR-rich std | 内容哈希（rustc-build-sysroot） |
| `~/.cache/mirvm/native-archives` | `.a`→`.so` 产物 | 内容哈希 |
| `~/.cache/mirvm/asm-stubs`、`global-asm` | asm 工厂 `.so` | 内容哈希 |
| `~/.cache/mirvm/scripts` | frontmatter 脚本物化项目 | 路径+内容哈希 |

## 2. 数字地基（2026-07-14 实测，EPYC 7773X，release，热缓存）

| 负载 | 墙钟 | 含义 |
|---|---|---|
| `demo/args_env.rs`（std-only 快路径） | **0.40s** | 前端+单态化+lower+跑，std-only 图 |
| `demo/ecosystem.rs`（serde+serde_json+rand+regex） | **3.07s**（稳定复现） | 依赖 rmeta 全在缓存，此 3s 为**每跑必付**：叶前端 + 拉依赖 MIR + 全图单态化 + lower + 跑 |

结论：冷启动痛点**不在依赖解析**（一次性）也不主要在依赖编译（一次性、并行、增量），
而在每跑必付的加载相。ripgrep 档 3.7–4.7s（real-projects 证据）同理。
相位内部分割（前端/metadata/mono/lower/guest 各占多少）**未测**——是施工 ① 的任务，
缓存设计前先有账本（调研先行纪律）。

## 3. 决策（D9，全批 2026-07-14）

- **D9a 统一入口**：保持 `mirvm` 单二进制 + 子命令（现有 `run`，未来 `pack` 等）。
  `mirvmc` 至多为同一二进制的别名硬链；**不出独立二进制**（rustc_private 链接体积
  巨大，双份纯浪费）。
- **D9b 包格式自定，两层路线**：拒绝"StableMIR 当格式"——StableMIR（rustc_smir，
  上游向 rustc_public 演进）是**进程内 API**，无 on-disk 稳定性保证。格式层次：
  多态层（≈rmeta 已是）与单态层（post-mono engine IR）。路线 = **先做 L2 单态
  engine-IR 缓存（本机），mode B 的 `.mirvm` 包 = 该缓存的可移植化**（版本头/
  校验/重定位段）。对外格式冻结推迟到 M5.3 JIT 期 IR 稳定后（M5.2 刚给 ir.rs
  加约 20 种语句，churn 期勿冻结对外承诺）。
- **D9c 缓存分层账本**：见 §4 表。L2 key = **mirvm build id + sysroot hash +
  crate 图内容哈希**；失配即整体重建（无部分复用，AppCDS 类比）。toolchain 自 pin、
  格式自有 ⇒ 重建总可行，脆性无害。
- **D9d 依赖构建剪 codegen**：target 依赖改走 check 形态（只出 metadata，cargo
  原生档位），剪掉白编的 rlib 机器码。落地对照 cargo-miri phases.rs 实证 cargo
  对产物存在性检查的容忍度（miri 源码未随 rustc-src vendor，实现时另取）。
  **省首建（一次性），不省每跑**——优先级低于 L2。
- **D9e toolchain 模型 = 自带编译器**：运行时发现本地 toolchain **架构上不可能**
  （rustc_private 仅 nightly；二进制↔toolchain ABI 锁死；rmeta 跨版本不稳定）。
  正确类比是 JDK **自带** javac 而非发现 javac。发行两阶段（已批）：
  **先 miri 式**——按 toolchain 出对应构建（将来可做 rustup 组件），"换 toolchain"
  = 换对应构建的 mirvm；**成熟后 JDK 式**——自包含 tarball 捆 toolchain 必要子集
  （rustc + rust-src + cargo），用户零 rustup 依赖。`--sysroot` 旋钮语义边界不变：
  管"guest std 从哪来"，不管"编译器是谁"。guest 代码不受 nightly 限制（stable
  语法照跑）。
- **D9f 施工顺序**（立项后执行）：① 相位计时（挂 `--vm-stats` 现有仪器加时间维度，
  不开新 harness 机制）→ ② L2 engine-IR 缓存 → ③ 依赖剪 codegen → ④ mode B
  `.mirvm` 包 + `mirvm pack` 子命令（**排 M5.3 后**）→ ⑤ 发行形态与命名收尾。
  **命名**：MRsDK 否决（混排难念）；"kit"命名推迟到 mode B 出实物；届时候选
  MDK / "mirvm toolkit"。

## 4. 缓存分层账本（D9c）

| 层 | 内容 | 状态 |
|---|---|---|
| L0 | registry/git 源码（`~/.cargo`，跨项目） | ✅ cargo 白嫖 |
| L1 | 依赖 MIR-rlib + 指纹（`target/mirvm`，每项目） | ✅ 已有；跨项目共享登记不做（低优先） |
| L1.5 | sysroot / native `.so` / asm-stubs / 脚本物化 | ✅ 已有（内容哈希） |
| **L2** | **post-mono engine-IR 整包缓存** | ❌ **最高价值缺口**：把每跑必付 ~3s 压成"反序列化+跑" |
| L3 | JIT code cache | ❌ M5.3–5.5 定型（CFI/PLT/重定位）前禁做 |

**L2 设计要点**（施工 ② 的前置调研清单）：
- 可序列化性来自 M4 立身性质：执行相 tcx-free、字节码自包含。
- **活体 relink 节**：dlopen 出的 `.so` 句柄、FnPtr 表、字符串/常量池宿主指针、
  thunk 工厂产物——不可序列化，载入时重建；需要独立重定位小节。
- 防静默错值：载入后 FnPtr/句柄若失配宁 Trap（校验段），不允许悬垂旧值。
- 验证通道 = 现有 gate：同一负载冷/热双跑，产出**逐字节一致**（diff 通道），
  防"缓存返回旧语义"假绿。不新增 harness 机制（AGENTS.md 预算纪律）。

## 5. 硬边界（防误设想）

1. **proc-macro / build.rs 永远真编译、真执行**（host crate）。任何"全部预降"设想
   到此为止：mode B 打包期它们照样跑一遍（javac 注解处理器类比）。
2. StableMIR 不是序列化格式（D9b）。
3. 运行时 toolchain 发现不可能（D9e 三条硬约束）。
4. L3 JIT code cache 禁做，直到 M5.3–5.5 把 CFI/PLT/重定位定型。
5. 跨项目共享 L1 target dir：登记不做（低优先，cargo 语义坑多）。

## 6. 风险与重估触发器

| 风险 | 处置 | 触发器 |
|---|---|---|
| ir.rs churn 使 L2 缓存频繁失效 | key 含 build id，天然免疫（只是命中率低）；对外格式不冻结 | M5.3 收官后重估格式冻结 |
| cargo 对剪 emit 后产物存在性检查不容忍 | 落地实证（对照 cargo-miri）；不行则保持现状 | 施工 ③；pinned toolchain 升级时重测 |
| relink 节 FnPtr/句柄悬垂 → 静默错值 | 校验段 + 宁 Trap | 施工 ② 设计审 |
| 缓存命中路径语义漂移（假绿） | 冷/热双跑逐字节 diff 进 gate | 施工 ② 验收判据 |

## 7. JVM 对映速查

| JVM | mirvm |
|---|---|
| jar | 源 + rmeta 包（多态层） |
| CDS / AppCDS | L2 engine-IR 缓存 |
| AOT / JIT code cache | L3（M5.3+ 之后） |
| javac 注解处理器 | proc-macro / build.rs（永远真执行） |
| JDK 自带 javac | mirvm 绑定编译器发行（miri 式 → 自包含 tarball） |
