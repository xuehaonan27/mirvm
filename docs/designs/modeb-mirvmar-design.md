# mode B：`.mirvm` 包格式 v2 + pack/run/MC

> 状态：**已实现，2026-08-07 完成自包含与解析边界复核**。母案：
> [distribution-design.md](distribution-design.md)
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

> v2 更正：v1 只把 global_asm 字节放入 MC，static archive 物化出的 `.so` 仍靠
> 原缓存路径，且默认校验源码时间戳和编译环境，不能诚实称为可分发单文件。v2 将
> 所有自产库字节纳入 NATIVELIBS，运行时按内容哈希自动物化；STAMPS/envs 只作来源记录。

## 1. 目标 / 非目标

**目标**：
- 单文件包格式 v2 + `mirvm pack`（cargo 项目/脚本 → `.mirvm`）+
  `mirvm run x.mirvm`（校验、装载、执行）。
- 三维验收：pack+run 与 `mirvm run` 直接跑逐字节一致（eco / faer /
  wasmtime 三负载），拒绝探针（陈旧/缺库/基址冲突/版本错配）响亮。

**非目标（另案登记，不预支）**：
- C12 跨 mirvm 版本的字节码兼容（当前仍要求 build_id 精确相等）；
- fat artifact 多 target（节表已预留多 MODULE 位）;
- static archive `.so` 的无文件进程内装载（当前从包内自动物化后 dlopen）；
- L3 JIT 机器码缓存（D5 禁令面，与包无关）。

## 2. 包格式 v2（容器）

```
offset 0   magic        8B   "MIRVMAR\0"
           fmt_ver      u32  = 2
           build_id     u32 长度 + UTF-8（MIRVM_BUILD_ID，精确匹配）
           section_cnt  u32
节表 ×N    tag u32 | off u64 | len u64 | fnv1a-128（节内容哈希）
尾         whole_hash   u128 fnv1a（除本字段外全文件）
```

节（未知 tag → 跳过（前向兼容）；缺必需 tag → 拒绝）：

| tag | 必需 | 内容 |
|---|---|---|
| META | ✓ | postcard：`{args, envs, base_key, target_triple, 是否含 BASE}`（= L2 Header 元信息） |
| STAMPS | ✓ | postcard：`Vec<(path,size,mtime_ns)>`，只作构建来源记录，不参与运行许可 |
| BASE | 可选 | **S4 BaseFile 原样嵌入**（std 底座；键与 META.base_key 互证） |
| MODULE | ✓ | postcard `ir::Module`（delta-on-base（有 BASE 时）或全量；编码 = L2 条目本体） |
| NATIVELIBS | ✓ | postcard：`Vec<{path, role, fnv128, bytes}>`；所有自产 `.so` 字节都在包内，旧 path 只用于与 MODULE 互证及诊断 |
| RELOC | ✓ | postcard：`{requires_fixed_base: bool, entry: Box<str>}`（固定基要求 + 入口符号；argv 经 run 转发） |
| MC | 可选 | global_asm/dep_asm 的完整 ELF 字节；由进程内 mcload 解析、重定位和注册符号 |

校验语义（refuse-loud，**绝不静默重建**——包是分发物不是缓存）：
- fmt_ver / build_id 不匹配 → 拒绝并指出重打工具链；
- whole_hash / 节 hash 不符 → 拒绝（损坏）;
- build target 与当前平台不符 → 拒绝；STAMPS/envs 不参与运行校验；
- NATIVELIBS 与 MODULE 的路径顺序必须完全互证，role 合法，内嵌 bytes 的 fnv128
  必须匹配；MC 还要与 role=global_asm 的条目互证；
- 节数量必须能被剩余节表容纳；所有 offset/len 做 checked 范围换算，节不得越界、
  重叠或复用 tag；每个节都校验哈希，包括未知 tag；
- `requires_fixed_base` 而固定基不可用（被占/ASLR 冲突）→ 拒绝。

## 3. pack 流程（`mirvm pack <target> [-o out.mirvm]`）

与 `mirvm run` **同一条管线**到 Module 为止，岔口只改"执行 → 落盘"：

1. **cargo 项目**：走 cargo_shim 全量构建（dep 照常 metadata-only + C4 清单），
   lower 时**强制全量冷降低**（旁路 deps-image/底座分层——保证单模块自包含；
   pack 是构建动作，秒级冷降低可付）;S4 底座存在时将其 BaseFile 字节嵌入
   BASE 节（META.base_key 互证），MODULE 落 delta 形态。
   无底座负载（纯 std 程序）→ 无 BASE 节，MODULE 全量。
2. **脚本/单文件**：同 1 的单文件变体（frontmatter 依赖同 cargo 路径）。
3. 收集 META/STAMPS/NATIVELIBS（从 module.required_native_libs 展开、读取每个
   自产库字节、现场计算 fnv128、判别 global_asm 与 static_archive）、RELOC；
   global_asm 默认同时进入 MC。最后以临时名 + rename 原子发布。

## 4. run 流程（`mirvm run x.mirvm [-- args]`）

1. magic 嗅探（前 8 字节）→ 非包走既有路径;
2. 全文件 whole_hash + 节 hash 校验;
3. META：build_id / fmt_ver / target 校验；STAMPS/envs 只反序列化验证格式；
4. 装载：**有 BASE → 复用 baseimage 装载（字节直接喂 BaseFile 解析，不读
   `$HOME/.mirvm/base`）组成 image 栈 `[BASE, MODULE]`；无 BASE → 单模块**
   ——与 warm 路径同一装载语义（frozen 固定基恢复、required_native_libs
   从 NATIVELIBS 内嵌字节按哈希物化后 dlopen、asm_sites 幂等重物化、GOT
   启动相重填、entry_stub_sites 启动相重建）——**全部机件已在仓，无新
   执行路径**;
5. RELOC.entry 启动（main 启动链；argv 转发;`--vm-call` 语义不随包）。

## 5. MC（已实现）

- 内容：每个 global_asm/dep_asm ELF 一条 `{fnv128, bytes}`，与 NATIVELIBS
  role=1 条目互证；相同内容只入一次。
- 装载契约：mcload 在进程内解析 ELF、映射/重定位、注册符号与 eh_frame；
  FFI 真外国库（glibc 类）仍按系统 ABI 查找。
- MC 缺省或 `MIRVM_PACK_NO_MC=1` 时，同一内嵌 bytes 自动物化到
  `$MIRVM_HOME/package-native/<hash>.so` 再 dlopen；这只是装载策略，不是对预存缓存的依赖。

## 6. CLI 面

```
mirvm pack <proj-dir|script.rs> [-o <out.mirvm>]   # 默认 <名>.mirvm
mirvm run <x.mirvm> [-- <guest args>]
```

## 7. 验证计划

- **三维逐字节**：eco（cargo 大项目）/ c_faer_lu（dep global_asm + pulp
  LD_ST）/ c_wasmtime_wat（大依赖闭包 + fiber sym 跳过分支）——
  `mirvm run` 直接跑 vs `pack + run`（冷/热/包三跑）输出逐字节一致;
- **拒绝探针**：fmt_ver、whole/节 hash、重复 tag、节表截断、offset 溢出、
  节重叠、NATIVELIBS/MC 互证失败均返回错误，不 panic/OOM；
- **自包含酸试**：关闭 MC 打包，移走原 native/global-asm 缓存，换全新
  MIRVM_HOME，包仍按内容哈希物化并逐字节运行；
- **gate5 全量** + cargo test + diff 双态（SYNC）维持绿。

## 8. 边界与触发器（如实）

- C12 跨版本字节码：当前 build_id 精确匹配；**对外格式冻结**待 D4 触发器
  （mode B 立项已响，冻结评审在片③后）;
- 包运行期仍要求**固定基址可用**（与 L2 同契约；被占 = 拒绝非降级）;
- proc-macro/build.rs 只在 **pack 期**真执行（D9 §5 硬边界原样）;
- D15（砍 cargo）与本片正交：pack 期仍用 cargo 驱动 dep 构建，运行期
  不需要——D15 到来时换的是 pack 期的构建驱动，包格式与 run 不变;
- fat artifact 多 target：节表 tag 预留（`MODULE@<triple>` 形态），v1 评。

## 9. 实现位置

- `src/pack.rs`：容器读写、检查式解析、自产库内嵌与内容寻址物化；
- `src/vm/engine/mcload.rs`：MC ELF 进程内装载；
- `src/cli.rs`：pack 子命令和 run 的 magic 分流；
- 后续格式与零拷贝计划见 [product-capabilities-plan.md](product-capabilities-plan.md) P5。
