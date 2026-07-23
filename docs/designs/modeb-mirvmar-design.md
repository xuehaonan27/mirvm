# mode B 片②设计：`.mirvm` 包格式 v0 + pack/run

> 状态：**待审（2026-07-23）**。母案：[distribution-design.md](distribution-design.md)
> D9b（包 = L2 engine-IR 缓存的可移植化：版本头/校验/重定位段）；
> 新增输入：[decision-history §7.22](../decision-history.md)（机器码节 = 第三类
> 内容物的干净归宿）与 §7.23（C4 片①数据源——dep global_asm 清单 .so 已入
> required_native_libs 链）。**包必须 honestly 自包含**：除 FFI 真外国库
> （glibc 类）外，运行期不读任何 `$HOME/.mirvm` 与 rustc/cargo 痕迹。
>
> **格式稳定性声明（2026-07-23 用户裁定）**：本格式**当前不定死**——
> 随 mode B 后续开发（片③机器码节、D15、C12 评审）可以变动；fmt_ver
> 只作同代区分，不承诺跨版本兼容。对外冻结是 D4 的独立评审，届时另行
> 定稿并写明迁移规则。

## 1. 目标 / 非目标

**目标**：
- 单文件包格式 v0 + `mirvm pack`（cargo 项目/脚本 → `.mirvm`）+
  `mirvm run x.mirvm`（校验、装载、执行）。
- 三维验收：pack+run 与 `mirvm run` 直接跑逐字节一致（eco / faer /
  wasmtime 三负载），拒绝探针（陈旧/缺库/基址冲突/版本错配）响亮。

**非目标（另案登记，不预支）**：
- C12 跨 mirvm 版本的字节码兼容（v0 要求 build_id 精确相等）；
- fat artifact 多 target（节表已预留多 MODULE 位）;
- 机器码节 MC 的进程内装载（片③，本文只定格式与装载契约）;
- L3 JIT 机器码缓存（D5 禁令面，与包无关）。

## 2. 包格式 v0（容器）

```
offset 0   magic        8B   "MIRVMAR\0"
           fmt_ver      u32  = 1
           build_id     u32 长度 + UTF-8（MIRVM_BUILD_ID，v0 精确匹配）
           section_cnt  u32
节表 ×N    tag u32 | off u64 | len u64 | fnv1a-128（节内容哈希）
尾         whole_hash   u128 fnv1a（除本字段外全文件）
```

节（未知 tag → 跳过（前向兼容）；缺必需 tag → 拒绝）：

| tag | 必需 | 内容 |
|---|---|---|
| META | ✓ | postcard：`{args, envs, base_key, target_triple, 是否含 BASE}`（= L2 Header 元信息） |
| STAMPS | ✓ | postcard：`Vec<(path,size,mtime_ns)>` 输入盖戳（= L2 同构口径；本地失效判据） |
| BASE | 可选 | **S4 BaseFile 原样嵌入**（std 底座；键与 META.base_key 互证） |
| MODULE | ✓ | postcard `ir::Module`（delta-on-base（有 BASE 时）或全量；编码 = L2 条目本体） |
| NATIVELIBS | ✓ | postcard：`Vec<{path, role, fnv128}>`，role ∈ {static_archive, global_asm, dep_asm}——装载校验；**片③ 起字节改由 MC 内嵌，本 manifest 退化为索引** |
| RELOC | ✓ | postcard：`{requires_fixed_base: bool, entry: Box<str>}`（固定基要求 + 入口符号；argv 经 run 转发） |
| MC | 预留 | 片③：自产机器码 blob（asm stub/global_asm 的 .text+符号表+eh_frames） |

校验语义（refuse-loud，**绝不静默重建**——包是分发物不是缓存）：
- fmt_ver / build_id 不匹配 → 拒绝并指出重打工具链；
- whole_hash / 节 hash 不符 → 拒绝（损坏）;
- STAMPS 与本地文件不符 → 拒绝并指出 `mirvm pack` 重打（本机失效判据，
  与 L2 同口径但**不**回退冷重建——无源可用时必须能跑：分布到别处时
  STAMPS 校验应可旁路（`MIRVM_PACK_NOSTAMP=1`，默认仍校验）;
- NATIVELIBS 每个 `path` 存在且 fnv128 匹配 → 缺一即拒绝;
- `requires_fixed_base` 而固定基不可用（被占/ASLR 冲突）→ 拒绝。

## 3. pack 流程（`mirvm pack <target> [-o out.mirvm]`）

与 `mirvm run` **同一条管线**到 Module 为止，岔口只改"执行 → 落盘"：

1. **cargo 项目**：走 cargo_shim 全量构建（dep 照常 metadata-only + C4 清单），
   lower 时**强制全量冷降低**（旁路 deps-image/底座分层——保证单模块自包含；
   pack 是构建动作，秒级冷降低可付）;S4 底座存在时将其 BaseFile 字节嵌入
   BASE 节（META.base_key 互证），MODULE 落 delta 形态。
   无底座负载（纯 std 程序）→ 无 BASE 节，MODULE 全量。
2. **脚本/单文件**：同 1 的单文件变体（frontmatter 依赖同 cargo 路径）。
3. 收集 META/STAMPS/NATIVELIBS（从 module.required_native_libs 展开 +
   fnv128 现场计算 + role 判别（global_asm/dep_asm 由 `.mirasm.s` 清单来源
   标记，余为 static_archive））、RELOC（frozen.at_fixed_base + 入口符号）、
   原子发布（临时名 + rename，全仓同款）。

## 4. run 流程（`mirvm run x.mirvm [-- args]`）

1. magic 嗅探（前 8 字节）→ 非包走既有路径;
2. 全文件 whole_hash + 节 hash 校验;
3. META：build_id / fmt_ver 校验;STAMPS 校验（可旁路）;
4. 装载：**有 BASE → 复用 baseimage 装载（字节直接喂 BaseFile 解析，不读
   `$HOME/.mirvm/base`）组成 image 栈 `[BASE, MODULE]`；无 BASE → 单模块**
   ——与 warm 路径同一装载语义（frozen 固定基恢复、required_native_libs
   按 NATIVELIBS 逐一 fnv128 复核后 dlopen、asm_sites 幂等重物化、GOT
   启动相重填、entry_stub_sites 启动相重建）——**全部机件已在仓，无新
   执行路径**;
5. RELOC.entry 启动（main 启动链；argv 转发;`--vm-call` 语义不随包）。

## 5. MC（片③预留，本节只定契约）

- 内容：每个自产 .so（global_asm/asm-stub/dep_asm）一条 `{role, code: Vec<u8>,
  symbols: Vec<(name, off)>, eh_frame: Vec<u8>}`；与 NATIVELIBS 按 fnv128 互证。
- 装载契约：进程内 mmap RX 拷 code + 符号表注册（当前 archive_fallbacks/
  dlsym 全域同一解析序）+ `__register_frame`（JIT 先例）——运行期对自产码
  **不再需要 cc/ELF/.so**；FFI 真外国库（glibc 类）照走 dlopen。
- v0 兼容：MC 缺省 = NATIVELIBS 文件引用（本机缓存内有效）；片③ 后
  pack 默认改 MC 内嵌（`--no-mc` 保留文件引用模式）。

## 6. CLI 面

```
mirvm pack <proj-dir|script.rs> [-o <out.mirvm>]   # 默认 <名>.mirvm
mirvm run <x.mirvm> [-- <guest args>]
```

## 7. 验证计划

- **三维逐字节**：eco（cargo 大项目）/ c_faer_lu（dep global_asm + pulp
  LD_ST）/ c_wasmtime_wat（大依赖闭包 + fiber sym 跳过分支）——
  `mirvm run` 直接跑 vs `pack + run`（冷/热/包三跑）输出逐字节一致;
- **拒绝探针**：改源后跑包（STAMPS 拒绝）;purge 后跑文件引用包
  （NATIVELIBS 拒绝）;fmt_ver 篡改拒绝;`MIRVM_PACK_NOSTAMP=1` 旁路放行;
- **gate5 全量** + cargo test + diff 双态（SYNC）维持绿。

## 8. 边界与触发器（如实）

- C12 跨版本字节码：v0 build_id 精确匹配；**对外格式冻结**待 D4 触发器
  （mode B 立项已响，冻结评审在片③后）;
- 包运行期仍要求**固定基址可用**（与 L2 同契约；被占 = 拒绝非降级）;
- proc-macro/build.rs 只在 **pack 期**真执行（D9 §5 硬边界原样）;
- D15（砍 cargo）与本片正交：pack 期仍用 cargo 驱动 dep 构建，运行期
  不需要——D15 到来时换的是 pack 期的构建驱动，包格式与 run 不变;
- fat artifact 多 target：节表 tag 预留（`MODULE@<triple>` 形态），v1 评。

## 9. 工程估算（片② 本体，约一周量）

- `src/pack.rs`（~300 行）：格式读写（postcard）+ pack 命令驱动 + run-package
  装载路径（复用 baseimage/ircache/native_archive/global_asm 四件现成装载
  语义，**无新执行路径**）;
- `src/cli.rs` 接线（~50 行）：pack 子命令 + run 的 magic 嗅探岔路;
- 探针与 gate：三维脚本 + 拒绝探针 + gate5 复绿;
- 落档：decision-history（片②实录）、open-issues D1 推进、current-status、
  README 快速开始补 pack/run 一段。
