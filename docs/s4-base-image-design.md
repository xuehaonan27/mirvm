# S4 设计简报：std 预降低底座（base image）

> 状态：**已施工（2026-07-15，M6 片6；用户 2026-07-15 批准动工）**。实施结果与账本
> 见 m6-log 片6；两处施工偏离（§3 域位→偏移合并、§2 COW 映射按实测裁剪）记
> decision-history §7.3——本文其余机制按写实施。验收：脚本纯冷 385→104ms
> （目标 ≤120 达成），gate5 47/0/0/0。
> 依据：coldstart-research §2/§6 V4——lower 是近常数 std 税（fib 3027 个 instance
> ~300ms，其中绝大多数是程序无关的 std 闭包）；native 的答案是"安装时付清 std 税"
> （预编译 rlib），本片是它在解释世界的对映（JVM CDS base archive 同构）。

## 0. 目标与非目标

- **目标**：把程序无关的 sysroot std 闭包一次降低成共享"底座"模块；此后每次冷跑
  （首跑/编辑后）只降低**用户增量**（用户 crate + 以用户类型实例化的 std 泛型）。
  预期：fib 类脚本全冷墙钟 385ms → **≤120ms**（frontend 17 + base 载入 ~25 +
  delta lower ~20 + 仪式/engine）。
- **非目标（v1）**：registry 依赖 crate 的底座化——ecosystem 的 13209 个 instance 里
  ~10000 来自依赖，v1 只吃 std 份额（cold runner lower 1272→约 1000ms，收益有限）。
  依赖 crate 逐 crate 成像 = mode B `.mirvm` 包的同一条设计线（D9f④，排 M5.3 后）。
  L2 整包缓存照旧存在：底座只改变"冷路径"的成本结构，warm 命中路径不受影响。

## 1. 跨会话稳定的 instance 键

L2 整包缓存不需要跨会话键（FuncId 全模块内自洽）；底座是跨程序共享的，delta 降低时
必须判定"这个 instance 是否已在底座"。键 = **rustc v0 symbol_name(instance)**：
- 单态化名含全部泛型实参与 crate disambiguator，同一 sysroot 构建下跨会话稳定
  （native 链接正确性的同一根基）；drop glue/shim 等 InstanceKind 均可命名
  （cg_ssa 给每个 mono item 命名走的就是它）。
- lower 已在算 symbol_name（exported_defs 表），调研 perf 显示其占 lower 相 2.6%——
  作为 delta 期的底座查找成本可承受，可再加 Instance→hit 会话内 memo。

## 2. 双固定基址 + 文件 COW 映射

- 冻结区分两域：**base @ 0x6800_0000_0000**（现值）、**delta @ 0x6900_0000_0000**
  （新增第二固定基址；两者都 MAP_FIXED_NOREPLACE，被占即响亮回退全量冷降低）。
- base 冻结区改 **file-backed MAP_PRIVATE 映射**：干净页跨进程共享 page cache
  （载入 ≈ 免费，惰性缺页），guest 写 static（static mut/原子/lazy init）触发 COW
  私有页——语义与今日"匿名内存 + memcpy 恢复"等价，成本更低。这是 JVM CDS 的
  本相。delta 维持现状（匿名 + 快照恢复，L2 整包路径不变）。
- 跨域指针天然成立：delta 的 vtable/常量可以直嵌 base 函数条目地址与 base 常量
  地址——两域都在固定基址（可缓存性三判据①对两域分别成立）。base 是 worklist
  闭包 ⇒ base→delta 引用不存在。

## 3. FuncId 分域与引擎

- FuncId（u32）最高位 = 域位：0=delta（含用户 crate），1=base。
- `Shared` 持两张函数表；解释器按域位取表——改动集中在 `funcs[func]` 取址一处。
- fn_addrs/影子帧合成 IP 等以 FuncId 为键的面：域位随 id 走，无需结构改动
  （合成 IP 公式 FUNC_IP_BASE+func×64 对高位置位的 id 仍唯一、非零、不可执行）。

## 4. 底座内容与构建

- **种子（方案 A，推荐 v1）**：合成"空 main"程序跑一次完整 lower，其闭包
  （lang_start 链、panic/fmt/alloc 机器、TLS/unwind 骨架，估 ~2900 instance）
  即底座。确定性内容、实现最简。构建触发 = 底座文件缺失/失配时，在下一次
  `mirvm run` 的会话之前跑一个内部合成会话（一次性 ~400ms，之后永免）。
- **方案 B（演进方向，v1 不做）**：从真实运行的 lower 结果导出"实参只含 sysroot
  类型"的子集并跨程序增量合并——自适应底座（JVM AppCDS dynamic archive 对映），
  但引入合并/追加复杂度，留到 mode B 期一并考虑。
- **三判据复核**：底座 store 沿用 L2 契约——foreign_static_syms 非空拒绝
  （空 main 闭包预期干净：fib 今日可入账，其闭包 ⊇ 空 main 闭包）；asm_sites
  照配方每跑重物化；诊断面不适用（底座构建会话无用户代码，告警即 bug）。
  （2026-07-17 注：foreign_static_syms 判据已随 P2 GOT 间接退役，decision-history
  §7.5d；本条余者仍有效。）

## 5. 键与失效

底座文件键 = (MIRVM_BUILD_ID, sysroot builder hash［V1 stamp 同源］,
**会话降低指纹**)。第三项 = lower 期读过的、会影响降低产物的会话旗标集合的值哈希
（ub_checks/debug_assertions/panic 策略等——施工首日枚举并在代码里集中登记；
mirvm run 目前不暴露任何改变这些默认值的旋钮，指纹实为常量，登记是防将来加旋钮
时静默错配）。任何一项失配 ⇒ 无视底座走全量冷降低并重建（自愈，L2 同款）。
delta 的 L2 条目头新增底座键引用，load 时双验证。

## 6. 与 S3（懒降低）/ M5.3（JIT）的组合边界

- S4 不依赖 S3：底座消灭的是"std 闭包每冷跑重降低"的频次；S3 消灭的是"delta 中
  未执行函数的急切降低"。二者正交叠加（fib delta ~100 instance，S3 后首跑只降
  实际执行的 ~30 个）。
- 对 S3+JIT 联合设计的输入：域位/双区/稳定键是三者共用的地基——JIT 期底座函数的
  AOT 机器码可以进同一 base 文件的新 section（mode B 轨迹），懒降低的 lower-stub
  只出现在 delta 域。S4 先行不会被联合设计推翻，只会被其复用（若联合设计发现
  底座强依赖懒降低机制，按 §7.2 触发器并入——目前分析不成立）。

## 7. 工程量与风险

| 项 | 改动 | 风险 |
|---|---|---|
| frozen.rs 双域 + file mmap | 中 | MAP_FIXED_NOREPLACE 第二地址被占（同现有回退契约） |
| lower 底座查找（symbol_name 键） | 中 | 键碰撞（v0 唯一性=链接器根基，低）；查找开销（memo 兜底） |
| 引擎双函数表 + 域位 | 小 | 取址热路径 +1 分支（JIT 期消失） |
| 底座构建合成会话 | 小 | 空 main 闭包含 foreign static（探针首日验证，违反即拒建） |
| ircache 分层头 | 小 | 双键验证漏项 ⇒ 自愈冷路径兜底 |

## 8. 验收标准（gate 维度，不新增机制）

- fib/args_env 全冷墙钟 ≤120ms（MIRVM_TIMING 账本入 m6-log）；底座命中时 lower
  账本单列 `base-hit=N delta=M`。
- diff.sh 30/30 + diff_cargo 5/5（含既有 warm 复跑维度）在"底座在场/缺失/失配"
  三态下全绿（三态由删底座文件/改 stamp 注入，走既有脚本通道）。
- 底座构建幂等：连续两次构建字节一致（写入走临时名+rename）。
- gate5 全量 46/0/0/0；cargo test 新增：域位编解码、底座键失配拒载、双域往返。
