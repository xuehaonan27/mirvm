# S3′b 依赖成像：设计岔路与裁定（A2 纯化聚合 deps-image）

> 状态：**已裁定（2026-07-15）= A2 纯化聚合 deps-image**。裁定证据与替代关系见
> [decision-history.md §7.5](decision-history.md)；本文 §0–§7 保留岔路现场与四条出路的
> 原始记录（其中"方案 A"为 A1 口径，已被 A2 替代其切分口径与键构成，见 §8）。
> 下一步：A2 施工设计过审后动工。
>
> 历史状态：S3′b 暂停（2026-07-15）。S3′a（多源查找 + 地址样条，行为等价重构）已落地
> 全绿（commit b4ed691）；S3′b（依赖成像）施工中撞上一个**线性链无法表达非线性依赖
> DAG** 的固有难题，实测命中率过低（eco 4/19 依赖），不达设计目标。这是个真正的设计
> 岔路，需裁定方向后再续。本文详解问题、已探索的 chain 方案为何不行、四条出路的取舍。
> 已探索代码存 `docs/parked/s3b-chain-wip.patch`（可 `git apply` 接续）。
>
> 前置：[m5.3-design.md](m5.3-design.md) §3.3（S3′ 设计）、[s4-base-image-design.md](s4-base-image-design.md)（底座机制）、
> [coldstart-research.md](coldstart-research.md)（冷启动账本）。

## 8. 裁定（2026-07-15）：A2 纯化聚合 deps-image

**裁定 = A2**（A 家族内部改良；用户裁定）。与 §4 方案 A（下称 A1）的差异在切分口径与键：

- **切分**：deps-image 只装 **bin 无关实例**（定义在非本地 crate，且泛型参数/shim 携带
  类型不含 LOCAL_CRATE 类型——"pure"）；bin 自身代码与 tainted 实例（bin 泛型在依赖里的
  实例化）进 delta。purity 向下封闭 ⇒ image 无吊引用到 delta。
- **键**：std 底座键 + 各依赖 rlib 指纹 + 降低指纹。**不含 bin 派生数据**——会话开始
  即知，bin 编辑必命中，结构上不可能腐坏；A1 的"实际实例化指纹"（需先做单态化收集
  才能算，正是要跳过的成本）被证伪为不值得。
- **跨项目共享（S3′c）自动复活**：键无项目身份，同 lockfile + 同工具链的项目共享
  同一 image（A1 曾明确放弃此项）。缺实例退 delta，优雅退化。
- **账本（eco，`MIRVM_PURITY_STATS=1` 探针实测，探针留 src/lower/mod.rs）**：
  tainted 72 inst/1.9ms（0.7%）；A2 每编辑重降 = local+tainted = 83 inst/2.9ms；
  pure 10593 inst/1042.7ms 可缓存。A1 相对 A2 只多买 1.9ms。
- **B/C/D/E 不采纳**的理由维持 §4–§6 原判（屏障活性、膨胀、relocation 风险、≈没做），
  本账本下不再有反转理由。
- **重估触发器**与完整证据：[decision-history.md §7.5](decision-history.md)。

## 0. 一分钟速览（岔路现场，历史）

- **目标**：把 ecosystem 类项目的 registry 依赖 lower（冷 runner 相 ~988ms）在依赖自己
  的构建会话里预成像，编辑 bin 重跑时只 lower delta（bin 增量），目标 988→100-300ms。
- **已建**：S3′a 把单底座泛化成 image 栈（`[std 底座, dep₁, dep₂ …]`），行为等价、全绿。
- **卡点**：S3′b 让每个依赖在自己的 cargo 会话内成像，装载时按 below_key 拼成线性链。
  **正确性airtight**（image 只在 below_key 精确等于当前前缀键时装载，错配退 delta，绝不
  腐坏），但**线性链无法表达非线性依赖 DAG**：cargo 并行构建下多数依赖记 below_key=[std
  底座]，装完首个后前缀就移过 [std]，其余互斥 → 实测 eco 只装 4/19 依赖，lower 几乎不降。
- **岔路**：四条出路（详见 §4），各有取舍。当前推荐 **方案 A（单 deps-image，跨运行）**——
  最简、风险最低、直接兑现 edit-rerun 的 988→~150ms，代价是放弃跨项目共享（S3′c）。

## 1. 为什么要做依赖成像（S3′ 的动机）

冷启动调研（coldstart-research §2/§3）实测：ecosystem（rand+regex+serde_json 及传递
闭包，22 crate）冷 runner 会话 = frontend 72ms + **lower 988ms** + engine 1400ms。
lower 的 988ms 就是把 22 个依赖的 MIR 闭包逐一降低成引擎字节码——**每次冷跑重付**。

S4 底座已把程序无关的 std 闭包一次预降低（脚本 lower 300→31ms，9.6×）。同一机制对
registry 依赖的推广就是 S3′：依赖的 lower 产物（native 世界里叫 rlib 的目标码，我们叫
image）应在依赖构建时一次产出、跨运行/跨项目复用，而非 runner 每次重降。

**主要收益场景 = 编辑-重跑循环**：改 bin 的 `main.rs` 后重跑，L2 delta 条目失效（源变），
但依赖未变。无 S3′：runner 全量 lower 988ms；有 S3′：装依赖 image + 只 lower bin delta。

## 2. 已建的地基（S3′a，已落地全绿）

commit b4ed691。把 S4 的单底座泛化成 **image 栈**，行为等价（无 image/单底座两态字节等价）：

- `frozen.rs`：地址样条 `IMAGE_SPLINE`（0x6A00 + k·2^34，16 GiB 步距，上界 1300 不触
  mmap 顶带）+ `is_valid_home` 白名单（底座 0x6800 / delta 0x6900 / 对齐样条；伪造快照
  防线）+ `new_image(k)`。
- `ir.rs`：`Module.base_frozen`（Option）→ `image_frozens`（Vec）——多域冻结区都要保活。
- `baseimage.rs`：`ImageStack{ images + 并集查找 fn/entry/static/tls_by_sym + 累积偏移
  total_fns/tls/asm + 键链 + 降低指纹 }`；`absorb_stack` 把 `[栈…][delta]` 拼单表。
- `lower`：`Linker::new(&ImageStack, frozen)`（base-maps=并集，delta 偏移=Σ栈）；
  `lower_for_image_build(tcx, stack, k)`（不排除 LOCAL_CRATE——依赖 crate 本身正是要成像的）。

**这层是任何 S3′b 方案的共同地基**（多域、并集查找、偏移合并），四条出路都用得上。

## 3. S3′b（依赖成像）撞的墙：线性链 vs 非线性 DAG

### 3.1 已探索的 chain 机制（docs/parked/s3b-chain-wip.patch）

在每个 target 依赖的 cargo 会话（S2 的 DepCallbacks，`-Zno-codegen` + in-process）里，
`collect_and_partition_mono_items` 之后就地成像：该 crate mono 集减栈下 → image 文件。
装载时按 below_key 贪心/定点拼成线性栈 `[std, depA, depB, …]`，delta 在其上。

**正确性核心（airtight，绝不腐坏）**：每个 image 的字节码内嵌**绝对** FuncId 偏移与跨域
地址，这些量只在"装载时栈下 == 构建时栈下"时才正确。故每个 image 记 `below_key`（构建
时栈下键），装载时**只在 below_key 精确等于当前前缀键时才装**——命中即扩前缀，失配即
跳过（该依赖退 delta 现降）。最坏是不装载，绝不错值。（施工中还踩了一个 no-codegen 根因：
cargo 流水线 metadata-only 趟 `should_codegen()`=false ⇒ `reachable_non_generics` 空
⇒ lib 自身非泛型函数不成 mono root ⇒ 空 image；门控在 should_codegen()=true 的 link 趟
成像即修复。见 patch 与 symbol_export.rs:53。）

### 3.2 为什么命中率必然低（实测 eco 4/19）

**根因：cargo 并行构建下，多数依赖记 `below_key=[std 底座]`**。memchr、serde_core、
regex_syntax、zerocopy… 这些依赖构建时它们的上游 image 还没就绪（并行），于是各自建于
`[std 底座]`（below_key = 底座键）。

装载时（bin runner 会话）贪心拼链：
```
前缀=[std]        → 装 memchr（below=[std] ✓）→ 前缀=[std, memchr]
前缀=[std,memchr] → serde_core.below=[std] ✗（≠[std,memchr]）→ 跳过
                  → zerocopy.below=[std]  ✗ → 跳过 …
```
装完**第一个** `[std]`-built 依赖后，前缀就从 `[std]` 变成 `[std, X]`，其余所有
`[std]`-built 依赖的 below_key 全部失配 → 全跳过。定点装载（反复扫描）也只能顺着构建
DAG 装出**一条线性链**（std → memchr → aho_corasick → regex_automata …），实测 4 个依赖。

**这是线性数据结构（链）表达非线性结构（依赖 DAG）的固有信息损失**，不是实现 bug。
设计文 m5.3-design §3.3 已预警此为"最尖风险"，并留了"实测命中率，不达标再议 barrier"。
实测结论：**不达标**（4/19，lower 988→~850ms，基本没兑现价值）。

## 4. 四条出路（详细取舍）

### 方案 A：单 deps-image（跨运行；**当前推荐**）

**机制**：放弃 per-crate 链，改在 **bin runner 会话**内把整个依赖闭包切成**一张** deps-image
缓存（键 = 依赖集 = 各依赖 rlib 指纹 + 底座键，**不含 bin 源**）。bin 首次冷跑照常全量
lower（988ms），随后按 crate 归属**切分**：非本地 crate 函数 → deps-image（落盘缓存）、
本地 crate（bin 自身）→ delta。bin 编辑重跑：装 deps-image + 只 lower delta。

```
bin 首次冷跑 lower 全量 988ms → 切分
  DefId.krate ≠ LOCAL_CRATE 的函数 → deps-image（键=依赖集）
  LOCAL_CRATE（bin 自身）        → delta
bin 编辑重跑：装 deps-image（1 张，无 DAG）+ 只 lower delta ≈ 150ms
```

- **无 DAG 问题**：一张 image、一个 below（just [std 底座]），装载零链一致性问题，
  近 100% 命中。
- **正确性简单**：deps-image 建于 `[std 底座]`，delta 建于 `[std, deps-image]`——2 元
  素栈，S3′a 地基直接够用（无需链/below_key/定点装载）。
- **代价 = 只跨运行，不跨项目**：deps-image 含**本项目** bin 泛型在依赖里的实例化
  （如 `serde_json::to_string::<本地Struct>`，DefId.krate=serde_json 属"依赖"，但符号名
  带本地类型路径），故键必须含"本项目依赖集实际实例化"，另一项目即便同 lockfile 也
  不能复用。放弃 S3′c 的跨项目共享。
- **首次冷跑仍全量**（deps-image 在首个冷跑里产出）——但那本来就要付 lower。收益从**第
  二次**冷跑（编辑重跑）起兑现。
- **工程量**：中。bin 会话切分 + deps-image 键设计 + 2 元素栈装载。风险低（无链、无
  relocation、正确性直接）。

### 方案 B：chain + 构建屏障（保留跨项目；忠于原设计）

**机制**：保留 per-crate 链，加**构建屏障**使 below_key 一致：依赖成像按规范序**串行**——
依赖 X 成像前，等所有规范序更小的依赖 image 就绪（轮询/超时），再建于完整规范前缀。
如此各依赖 below_key 一致 → 装载高命中，且保留跨项目共享（每 crate 自己的 image 项目无关）。

- **保留跨项目共享**（S3′c 可做）：每个依赖的 image 只含它自己的函数（项目无关符号名），
  同版本依赖跨项目白拿。
- **无重复**：链式（各依赖建于其下依赖之上）只含自己的新函数，不重复上游闭包。
- **代价 = fiddly + 风险**：屏障要轮询等待上游 image 文件、带超时（防死锁）；cargo 拓扑
  序 ≠ 规范序（crate_id 序）时依赖可能互等；串行化成像给依赖构建相加延迟（虽然成像只是
  构建的一小部分）。cargo 并行度下屏障的正确性/活性需仔细设计。
- **工程量**：大。屏障协议 + 超时 + 与 cargo 并行的交互测试。

### 方案 C：FuncId relocation，每依赖建于 [std] only（跨项目，但有膨胀）

**机制**：每依赖只建于 `[std 底座]`（below_key 恒=底座键），装载时把各依赖 relocate 到
紧凑 FuncId 块。**关键发现**：冻结区 fn-entry cell 的内容（FuncId）是**调试用**，运行期
派发走 `fn_addrs` 反查（地址→FuncId），故 relocation 只需改 3 处**类型化字段**：
`Call.callee`、`fn_addrs` 值、`exports` 值（≥N0 的加块偏移；N0=底座函数数，阈值干净分离
底座引用 <N0 与自身引用 ≥N0），**不碰冻结区字节**（不同于 S4 §7.1 拒绝的地址 relocation）。

- **近 100% 命中**：各依赖独立装载 relocate，无链一致性问题。
- **致命代价 = 膨胀**：每依赖建于 `[std]` only ⇒ 其 image 含**完整传递闭包减底座**
  （regex 的 image 会重含 aho_corasick、memchr、regex_syntax…）。跨依赖共享的泛型
  （std 容器/迭代器 + 传递依赖）被**每个依赖各存一份**，合并模块可能 2-3× 膨胀。内存与
  funcs 向量都胀。
- 冻结区仍每依赖一域（地址不 relocate），域 k 须按 crate_id 稳定分配（碰撞→跳过）。
- **工程量**：中；风险中（relocation 逻辑错=静默腐坏，但字段有界）。膨胀是硬伤。

### 方案 D：chain + relocation（无膨胀 + 高命中；最复杂）

**机制**：依赖建于其真实上游链（无膨胀），装载时按**实际装载前缀大小**动态 relocate，
并处理跨依赖引用。兼得方案 B 的无膨胀与方案 C 的高命中。

- **代价 = 最复杂**：relocation 偏移随实际前缀变动，且要正确处理"依赖 X 引用依赖 Y 的
  函数"（Y 在链中的位置随装载集变）——跨依赖引用的 relocation 分类远比 C 的"底座 vs 自身"
  二分复杂。风险最高。
- **工程量**：大；风险高。收益最全（无膨胀 + 高命中 + 跨项目）。

### 方案 E：接受低命中 v1（记账收口）

chain 保持现状（正确但 4/19、lower 基本不降），文档记 DAG 限制与后续方案，收口。
风险最低但基本没兑现 S3′ 价值。**不推荐**（等于没做）。

## 5. 取舍矩阵

| 方案 | 命中/价值 | 跨项目 | 膨胀 | 复杂度/风险 | 兑现 988→? |
|---|---|---|---|---|---|
| A 单 deps-image | 高（跨运行） | ✗ | 无 | **低** | ~150ms（编辑重跑） |
| B chain+屏障 | 高 | ✓ | 无 | 大/中 | ~150ms |
| C reloc（[std]only） | 高 | ✓ | **大** | 中/中 | ~150ms 但内存胀 |
| D chain+reloc | 高 | ✓ | 无 | **大/高** | ~150ms |
| E 接受低命中 | **低** | 部分 | 无 | 低 | ~850ms（≈没做） |

## 6. 推荐与理由

**推荐方案 A（单 deps-image，跨运行）**，理由：
1. **主收益场景是编辑-重跑循环**，A 直接、干净地兑现（988→~150ms）。
2. **跨项目共享（S3′c）是锦上添花**，不是核心；放弃它换来最低风险、最简实现。
3. 复用 S3′a 地基（2 元素栈够用），无链、无 relocation、无屏障、无膨胀——**防静默错值
   面最小**。
4. B/C/D 都是"为了跨项目共享"付出大幅复杂度/风险/膨胀；在实测 chain 命中率证伪后，
   应先用 A 兑现主价值，跨项目留作独立后续（若真有多项目同 lockfile 的诉求再上 B/D）。

**次选**：若跨项目共享是硬需求，走 **B（屏障）**（比 D 简单、无膨胀）。

## 7. 交接要点（后续 session）

- **别信"chain 只是没调好"**：4/19 是线性链表达 DAG 的**固有**信息损失，不是 bug。
  定点装载已是链方案的最优，仍 4/19。要高命中必须换结构（A/C/D）或串行化构建（B）。
- **正确性红线**：任何方案，image 的绝对 FuncId 偏移/跨域地址只在"装载栈下 == 构建栈下"
  时有效。A 天然满足（2 元素固定栈）；C/D 靠 relocation；B 靠屏障使前缀一致。**错配装载
  = 全盘错值**，必须像 chain 那样"精确匹配才装，否则退 delta"。
- **no-codegen 根因已解**：成像必须门控 `tcx.sess.opts.output_types.should_codegen()`
  =true（link 趟），否则 `reachable_non_generics` 空、lib 自身函数不成 mono root、空 image。
- **fn-entry cell 内容是调试用**：运行期派发走 `fn_addrs` 反查，故 relocation（C/D）不必
  改冻结区字节，只改 `Call.callee`/`fn_addrs` 值/`exports` 值 3 处类型化字段。
- **已探索代码**：`docs/parked/s3b-chain-wip.patch`（719 行 diff，含 chain 完整实现 +
  no-codegen 门控 + 定点装载 + 命中率日志）。`git apply` 可接续 chain 方案（若选 B）。
  A/C/D 是不同结构，patch 仅作参考（地基 S3′a 已在主干）。
- **验收锚点**：eco 编辑 bin 重跑 lower ≤300ms（目标）；diff_cargo 5/5（含 warm 维度）+
  底座/依赖 image 在场/旁路双态；gate5 全绿。**必须实测装载命中率并如实记账**（像 S2 那样
  诚实修正预估）。
