# S3′b-A2 施工设计：纯化聚合 deps-image

> 状态：**已施工（2026-07-15，A2-1/A2-2/A2-3 三片全绿收官）**。gate5 50→51（新增
> a2_deps_image 行）；eco 冷 924→热 66ms；S3′c 同 workspace 跨 bin 共享。施工日志与
> 账本 [m6-log.md](m6-log.md) 片7/8/9；施工偏离与意外（closure 护栏误伤底座命中、
> S4 补建条目的 image 域变体、phase_cargo 相对路径嵌套）记于 m6-log。
> 裁定依据：[decision-history.md §7.5](../decision-history.md)；岔路现场：
> [s3b-design-fork.md §8](s3b-design-fork.md)。
>
> 批准状态（2026-07-15，用户裁定 Q1–Q5）：Q1 = --extern 解析；Q2 = v1 要求
> 底座在场；Q3 = 域样条 k=0；Q4 = A2-3 默认开（旁路旋钮在）；Q5 = S3′c 验证顺手做
> （A2-3 加 eco 变体 bin 冒烟）。

## 1. 一句话

把 bin runner 会话的 mono 闭包按 purity 切成两层：**bin 无关实例（pure）→ deps-image**
（跨 bin 编辑稳定、跨项目可共享的磁盘缓存，键 = build_id + 底座键 + 依赖工件盖戳），
**bin 附着物（LOCAL_CRATE + tainted）→ delta**（每编辑重降，eco 实测 2.9ms）。栈 =
`[std 底座, deps-image, delta]`，S3′a ImageStack 直接承载；无链、无屏障、无 relocation、
无膨胀，DAG 问题结构性地不存在。

## 2. 目标与验收锚点

- **主锚点**：eco 编辑 bin 重跑**加载相总账**（deps-image 装载 + delta 降低）≤300ms
  （fork 文档原锚点"lower ≤300ms"的诚实化——装载成本必须入账，见 §5 账本预估）。
- diff_cargo 5/5（含 warm 维度）；deps-image 在场/旁路（`MIRVM_NO_DEPS_IMAGE=1`）
  双态差分一致；gate5 全绿 + 新增双态冒烟行。
- 命中率与首次冷跑开销**如实记账**（S2 同款诚实修正纪律）。
- 非目标：S3′c 跨项目共享的验证（Q5）；零拷贝装载（rkyv，与 mode B 同题，M5.3 后）。

## 3. 键与生命周期（含 L2 键链的完整论证）

### 3.1 deps-image 键

```
key = fnv( MIRVM_BUILD_ID , 底座键 , 排序后的 --extern 工件 (path,size,mtime_ns) 盖戳 )
```

- **--extern 清单来自 runner 的 rustc_args**（不依赖 tcx）——这是硬约束：L2 热路径在
  编译会话**之前**（命中即跳过整个 rustc 会话），deps-image 必须能在 pre-compiler
  装载，否则 L2 键链断、bin 未改的热跑从 33ms 退化成 100ms+（**回归**，不可接受）。
  cargo 把全部直接+传递依赖的 rlib 路径经 `--extern` 传给最终 bin 的 rustc 调用，
  与 L2 清单口径（`used_crate_source`）同源同保真度（(size, mtime_ns)，ircache 既有
  标准，"cargo 指纹同保真度"；mtime 粒度风险沿用 distribution-design §6 的既有记账）。
  sysroot 工件不经 --extern，由底座键兜住（底座键含 sysroot stamp）。
- **不含 bin 源、不含项目身份** ⇒ bin 编辑必命中；同 lockfile + 同工具链的项目共享
  （S3′c 的复活点）。
- **降低指纹（ub/overflow/contract checks）不入 pre-key**：pre-compiler 无法从 args
  可靠重推会话默认。其安全性由既有分工覆盖（§3.3）。

### 3.2 生命周期

- **首次冷跑（image miss）**：runner after_analysis（L2 已 miss）→ split lower
  （§4）→ 产出 deps-image 模块 + delta 模块 → image 原子写盘（内容寻址 = 键即文件名，
  临时名写全再 rename，既有模式）→ L2 入账 delta（键链 = 栈键）→ absorb 运行。
  代价 = split 分类 + 一次序列化写盘（记账）。
- **编辑重跑（主收益）**：run_driver 起手 ensure() 装 `[底座, deps-image]`（pre-key
  命中）→ L2 miss（bin 源变）→ 编译会话只 lower delta（sym 并集路由自动把 pure
  挡在队外——S4 底座命中同机制，**热路径零分类成本**）→ 运行。
- **bin 未改重跑**：L2 命中（键链含 deps-image 键）→ 跳过整个 rustc 会话（33ms 级，
  与今天一致）。
- **依赖变更**：盖戳变 → pre-key 变 → miss → 回到首次冷跑路径，旧 image 按内容寻址
  自然淘汰。
- **任何校验不合 = 不装载**（全量降低自愈）：文件缺失/损坏、build_id 不符、底座键
  不符、冻结区恢复失败（域被占）、required .so 缺失。旁路：`MIRVM_NO_DEPS_IMAGE=1`。

### 3.3 降低指纹（fp）安全性论证（沿用 base/L2 的既有分工）

deps-image 文件记录构建会话 fp；`ImageStack::from_images` 的 fp 前缀截断 +
after_analysis `fp_matches` 复核（既有机制）覆盖**冷路径**（截断 → 全量降低自愈）。
**L2 热路径**跳过 after_analysis，但 L2 键 = fnv(完整 rustc_args)：fp 相关旗标
（-C/-Z、MIRVM_ENCODED_RUSTFLAGS_APPEND 追加）都落在 args 里——fp 不同的两个会话
args 必不同 ⇒ L2 必 miss ⇒ 回到受 fp 复核的冷路径。bin 编辑不改变 fp 旗标（profile
旋钮不变），故正常编辑-重跑链上 image fp == 会话 fp 恒成立。结论：无新增 fp 缺口
（与今天 base+L2 的不变量完全相同）。

### 3.4 v1 约束

- **要求底座在场**（正常配置恒真）：deps-image 的 below = [底座]，键含底座键；无底座
  （`MIRVM_NO_BASE_IMAGE`）时走今天的全量路径，不产/不用 deps-image。
- **单个 deps-image 层**（域 = 样条 k=0，0x6A00）；多层（tainted 层、S3′c 合并）留后续。
- **split 失败自愈**：image 样条域被占（MAP_FIXED_NOREPLACE 回退动态基址）→ 本次不产
  image（delta 照常运行，序列化判据①不满足即不写盘——FrozenArena serde 既有契约）。

## 4. split lower 机制（核心新机件）

### 4.1 为什么不是"降完再切"

绝对 FuncId/TlsId/AsmStubId 与冻结区地址在降低时**逐条烤进字节码**——单序列降完无法
后切（S4 §7.1 拒绝地址 relocation 的同因）。必须在降低过程中按类分轨：

### 4.2 双队列 + 标签 id（FuncId/TlsId/AsmStubId 同构）

- 分类器（探针产品化）：`classify_purity(inst) = Local | Tainted | Pure`；
  Local+Tainted 统称 **delta 类**，Pure = **image 类**。
- `func_id` 首见即分类：image 类 id = `TAG | j`（j = image_funcs 位序），delta 类
  id = `k`（k = delta_funcs 位序）——**两个独立稠密空间**，TAG = FuncId 高位
  （u32 高位的临时标签，rebase 前绝不进执行相；2^31 实例不可能）。
- 降低驱动：image 队列与 delta 队列**不动点轮替**（image 体只发现 image 类——
  purity 向下封闭；delta 体两类都发现），两队列俱空即止。
- **rebase（收尾一次）**：image id `TAG|j` → `栈total + j`；delta id `k` →
  `栈total + image_count + k`。触及的有界类型化字段清单（**编译期穷尽**——op enum
  全分支 match，新增携带 id 的变体 = 编译错误，防静默错值的机械门禁）：
  ① 全部 FuncBody 中携带 id 的 op（施工第 0 步盘点 ir.rs：Call.callee 等）；
  ② module.exports 值；③ module.fn_addrs 值；④ entry plan；⑤ TlsRef/AsmStub 同构
  字段。fn-entry cell 的 **内容**（FuncId 值）按 fork 文档发现是调试用（运行期派发走
  fn_addrs 按地址反查）——施工第 0 步在 interp.rs 实证此断言；若证伪，fn_entries 表
  留有 (instance → addr, 按类知域) 可补写冻结字节。

### 4.3 双冻结区路由（materialization 时刻按类定域）

Linker 持双 arena：image 区（`new_image(0)`，0x6A00）+ delta 区（`DELTA_FIXED_ADDR`），
**当前降低实例的类**决定路由：

- **Memory 常量**（含 vtable 分配）：image 类上下文 → image 区；delta 类上下文 →
  delta 区。去重表两张：image 表 / delta 表；image 上下文只查 image 表（不在则物化，
  已在 delta 表 = **提升**：字节+重定位在 image 区重物化一份——常量只读，双份安全；
  地址去重身份属 unspecified 行为，四级定义度内自由）。delta 上下文先查 delta 表、
  再查 image 表（delta 引用 image 域地址 = 稳定，合法）。
- **GlobalAlloc::Static / TLS**：按 **def_id.krate** 定域（非本地 → image 区，本地 →
  delta 区），**不按当前类**——单一地址身份（static mut/内部可变性双份 = 精神分裂，
  S4 红线；`&static` 相等性是 well-defined）。
- **fn 条目（fn_entry_addr）**：按 instance 的类定域——单类 ⇒ 单域 ⇒ 运行期地址
  比较身份唯一（S4 单一身份契约）。fn_addrs 反查表统一一张（值为标签 id，rebase 收）。
- **asm stub**：按当前类入 image_sites / delta_sites 两张配方表；stub id 同构标签化
  （wrapper 名按 (类, 位序) 生成，与最终 id 无关——名字只需唯一）。
- **栈并集去重优先**：以上全部先查 ImageStack 并集表（底座命中复用，S3′a 既有）。

### 4.4 热路径（image 在场）零分类

image 装载后，纯实例在 `func_id` 的栈并集查找中命中（v0 symbol_name，S4/S3′a 既有），
根本不入队——**不需要分类器**。分类器只在 image 构建（首次冷跑）运行。

## 5. 正确性论证与静默错值面清单

1. **purity 向下封闭** ⇒ image 字节码只引用 image 类（无吊引用到 delta）；delta 可
   自由引用 image/底座域（地址向下稳定）。两个方向都安全。
2. **image 不含 bin 派生数据** ⇒ bin 编辑不可能使它腐坏；键无需实例化指纹（A1 的
   死结）。错配装载 = 全盘错值的红线在结构上不存在（2 元素固定栈 + 无 bin 派生键）。
3. **唯一新增静默错值面 = 分类器漏判 local**（bin 派生实例混入 image → bin 编辑后
   stale 装载）。四重缓解：① 分类保守化（不确定 → delta 类，只伤命中率不伤正确性）；
   ② 写盘前全量自检（image 每 instance 复查 args/shim 携带类型不含 LOCAL_CRATE，
   违例 = 不写盘并响亮日志）；③ 双态差分 gate（编辑 bin 后 mirvm vs native 输出一致）；
   ④ rebase/路由的编译期穷尽 match（§4.2）。
4. **账本预估（eco）**：image 内容 10593 inst；装载 ≈ 按 S4 底座 3000 inst/33ms 线性
   外推 ~110ms（postcard 解码为主，decision-history §7.3 已记）；delta lower ~3ms；
   加载相总账 ~115ms ≪ 300ms 锚点。首次冷跑额外 = 分类 + rebase + 写盘（记账，预估
   <5% lower）。
5. **降级方向只有"少装"**：image 缺实例 → worklist 正常入队降进 delta；多装 = 死重。

## 6. 施工切片（每片全绿 gate + 独立 commit）

| 片 | 内容 | Gate |
|---|---|---|
| **A2-1 分类分轨机件**（默认 OFF） | 施工第 0 步实证（ir.rs op 盘点、interp fn_addrs 反查、asm wrapper 命名）；分类器产品化；双队列/标签 id/双 arena 路由/rebase；单测（分类器口径、rebase 阈值与穷尽、arena 路由、提升双份） | 默认路径零行为变化：gate5 全绿；env 开启下 split-lower 自检日志与探针口径一致（eco 10593/83 复现） |
| **A2-2 image 写盘/装载 + L2 键链** | DepsFile 格式（BaseFile 同构 + deps 键字段）；pre-key（--extern 解析+盖戳）；ensure() 装 `[底座, image]`；after_analysis split 接线；L2 键链贯通 | env 开启端到端：eco 冷写/热读双跑输出与 native 一致；diff_cargo 5/5；`MIRVM_NO_DEPS_IMAGE=1` 旁路双态一致；L2 热跑 33ms 级无回归 |
| **A2-3 默认开启 + gate + 账本** | 默认开（旁路旋钮在）；gate5 新增 deps-image 双态冒烟行；加载相性能门防回归 | eco 编辑重跑加载相总账 ≤300ms（记账）；gate5 全绿（总数 +1）；账本入 m6-log；current-status/本设计标施工态 |

## 7. 风险表

| 风险 | 缓解 |
|---|---|
| 分类器漏判 local（唯一静默错值面） | 保守化 + 写盘全量自检 + 双态差分 + 穷尽 match（§5.3） |
| rebase 字段遗漏 | op enum 编译期穷尽；第 0 步盘点留档；双态差分 |
| fn-entry cell 内容实为运行期语义（fork 文档判断证伪） | 第 0 步实证；备用 = fn_entries 表按类知域补写冻结字节 |
| image 装载 postcard 成本（~110ms）吃进收益 | 记账；零拷贝（rkyv）与 mode B 同题留后续 |
| --extern 解析随 cargo 传参形态漂移 | runner 真实 args 为准；stamp 失败 = 不缓存自愈；diff_cargo 覆盖 |
| 并发会话同写 image 文件 | 内容寻址（同名 = 同内容，写幂等）+ 原子 rename（既有模式） |
| 标签 id 泄进执行相 | rebase 后断言无 TAG 残留（debug_assert + 单测） |

## 8. 开题问题（请裁）

- **Q1**：deps 清单 = runner rustc_args 的 `--extern` 解析（**建议采纳**：可 pre-compiler
  计算，保 L2 热路径；与 L2 同保真度）——还是接受 L2 热路径退化、改用
  `used_crate_source`（需 tcx）？
- **Q2**：v1 要求底座在场（**建议采纳**：below 恒 = [底座]，键自然分级；无底座走今路径）？
- **Q3**：image 域 = 样条 k=0 固定（**建议采纳**：v1 单层）？
- **Q4**：默认开启时机 = A2-3（**建议采纳**，`MIRVM_NO_DEPS_IMAGE=1` 旁路）——还是
  长期 env 门控？
- **Q5**：S3′c 跨项目共享验证 = 顺手做（A2-3 加"第二个 eco 变体 bin 同 lockfile 白拿"
  冒烟）——还是独立后续片？

## 9. 施工第 0 步实证清单（动工先核，凭记忆写 rustc/引擎 API 必翻车）

1. `src/vm/engine/ir.rs`：全部 op 变体中携带 FuncId/TlsId/AsmStubId 的字段盘点
   （rebase 穷尽 match 的输入）。
2. `src/vm/engine/interp.rs`：间接调用派发实证只经 `fn_addrs` 按地址反查（fn-entry
   cell 内容确为调试用）。
3. `src/lower/asm.rs`：wrapper 名生成与 `materialize` 的命名耦合点（stub 标签化后
   配方仍幂等）。
4. `cargo_shim.rs parse_runner_invocation`：`--extern` 在 rustc_args 中的实际形态
   （路径/名值对），确认 pre-key 解析面。

### 第 0 步实证结果（2026-07-15，全部核毕）

1. **ir.rs id 字段盘点（rebase 全表面 = 6 处）**：`Terminator::Call.callee`（FuncId，
   op 中唯一）、`Rvalue::TlsRef`（TlsId，op 中唯一）、`Terminator::InlineAsm.stub`
   （AsmStubId，op 中唯一）、`Module.exports` 值、`Module.fn_addrs` 值、
   `EntryPlan.lang_start`。CallBuiltin/CallForeign/CallIndirect 均不携带三者。
2. **fn-entry cell 内容 = 调试用，坐实**：interp 的全部间接派发（CallIndirect、
   thunks、signal handler、atexit、_Unwind_DeleteException）只经 `fn_addrs.get(&addr)`
   按地址反查 FuncId（interp.rs:1979/2064/2105/2372/2434/2640），cell 内容从不被
   读——rebase 不碰冻结区字节成立。
3. **asm wrapper 命名耦合**：`materialize` 按 Vec 位序生成 `mirvm_asm_{i}` 并 dlsym，
   而 wrapper 文本在 lower 期已把名字烤进 `.globl/.type/.size`（自引用）——S4 的不变量
   是"预留 id == 最终位序"。split 模式下最终位序收尾才知 ⇒ **asm_sites 改为携带符号名
   的 `(name, text)` 对**（materialize 按名 dlsym，与位序解耦）；非 split 路径沿用
   今日的位序名（行为零变），split 路径用类前缀名（`mirvm_asm_xi{j}`/`xd{k}`）。
4. **--extern 形态与传递闭包覆盖论证**：cargo 传 `--extern name=path`（两个 argv 项），
   且**只有直接依赖**（eco 4 个：rand/regex/serde/serde_json）——传递闭包（22 crate）
   不在 --extern 里。键的传递覆盖依赖 **cargo 重建传播**：任何传递 crate 变更 ⇒ 其全部
   反向依赖（含 ≥1 个直接依赖）被 cargo 重编译 ⇒ 直接 rlib 的 (size, mtime) 盖戳变 ⇒
   键变（每条传递路径都有到直接依赖的反向链，否则它不在闭包里）。残余风险 = mtime
   粒度（ircache 已在 distribution-design §6 记账的同类）。bin 的 `.d` 只列本地源
   （不含依赖工件），不可用。`--extern` 无显式路径的形态 ⇒ v1 不产/不用 image（自愈）。

