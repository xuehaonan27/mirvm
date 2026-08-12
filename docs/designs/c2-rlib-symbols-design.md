# C2：native-archive 闭包「符号在 rlib」（评审报告 + 施工方案）

> 2026-07-18 专题评审（open-issues C2，旧 debt §10①）。实锤两例：
> ① wasmtime v46 `libwasmtime-helpers.a` 蹦床调 `resolve_vmctx_memory_ptr_46_0_1`
> 等（`#[export_name]`，versioned-export-macros，定义在 wasmtime 自身 rlib）；
> ② bzip2-sys vendored BZ_NO_STDIO 断言桩 `bz_internal_error`（`#[no_mangle]`
> `extern "C" fn(c_int)`，定义在 Rust rlib）。
> 最小复现 `/tmp/mre`（build.rs cc 蹦床 `c_side_trampoline` → `#[export_name]`
> `rust_side_def`）已复跑实证：当前响亮拒「无法安全转换为共享库」，
> ld stderr = `undefined reference to `rust_side_def'`。

## 0. 结论（评审判定）

**原理可完全闭合，路线 = 链接期注入 rlib 符号的 P1 stub 跳板**。
native 语义里，rlib 的 `#[export_name/no_mangle]` fn 是 final link 集符的目标
文件符号；mirvm 的对偶 = 该 Rust fn 的 **P1 可执行条目**（`4202317`：
签名可派生 ⇒ stub 码址 + libffi closure 蹦床回解释器）。把「闭包判定」从
「archive 自闭合」扩成「archive + crate 图 rlib 导出 fn 集」——undefined
符号凡在 rlib 导出集即物化 P1 条目并注入同名隐藏跳板，-z defs 闭包仍成立。

**被否路线**：①运行期懒解析——RTLD_NOW 纪律下 .so 未定义符号在 dlopen 即炸；
RTLD_LAZY 违反引擎必需库纪律且 P1 stub 不在进程动态符号表，无处可解析。
②把 rlib 当 native 对象链——metadata-only rlib（S2 `-Zno-codegen`）根本没有
机器码（正是 dep global_asm（C4）同根因）。

**闭合契约**：「.a→.so 闭包缺口：符号在 rlib」全形态（函数符号）按上法闭合；
数据符号（rlib static 被 C 引用）如实响亮拒绝（`按需另立`）。

## 1. 时序与消费面（评审核实）

- 转换点：`lower/mod.rs:2044`（排干 worklist 前）——global_asm C7 同款预算
  窗口，条目预算 `fn_entry_addr(inst)` 立即可用（P1 配方位序=启动相同序复现，
  地址跨进程稳定）。
- 名称→Instance 权威表：`Linker::exported_defs()`（`tcx.symbol_name` 为键，
  覆盖本地 + `dependency_formats(Executable)` 收集的依赖 crate 非泛型导出——
  正是 native final link 的集符集，rustc_middle queries.rs:2358 文档佐证）。
  两侧键名统一走 `canonical_link_name` 剥 `\x01`（aws-lc 前缀家族先例）。
- 预算：`linker.fn_entry_addr(inst)`——dep crate fn 自动落 image 域
  （alloc_entry_stub image_side 分支），delta 域函数落本域；幂等。
  签名可派生为前提：`-> !`（bz 断言桩）layout ZST ⇒ `FfiKind::Void` 可派生；
  不可派生者保持现状响亮拒绝（真实缺口的既有边界，C1 已把聚合解掉一族）。

## 2. 施工方案（改动面小，单调用点）

`src/native_archive.rs::materialize_for_target_in` 失败救援链（只动失败路径，
**首链成功的全部归档行为零变化**）：

1. 首链照旧（`cc -z defs --whole-archive …`）。失败时进入 rlib 注入评估——
   **不解析任何工具文本输出**（ad-hoc 审查后修订，见 §5）：用仓库既有的
   `src/elfsym.rs` ELF64 符号表解析器扩一个 ar 成员遍历，二进制级枚举归档
   全部成员的 `SHN_UNDEF` 全局/弱符号（跳过 LOCAL 与 ar 符号表成员）。
2. 枚举结果经 `canonical_link_name` 剥前缀后与 `exported_defs()` 求交：
   - 命中且是 Fn ⇒ `linker.fn_entry_addr(inst)` 预算 → (name, stub_addr)；
   - 命中 Static ⇒ Err「数据符号在 rlib（C2 边界，按需另立）」；
   - 交集为空 ⇒ 直接走原错误路径（响亮拒，文案同今）。
3. 发射跳板 `.s`（C7 global_asm 同款，**`.hidden <name>`**——只在 .so 内部
   绑定，不污染进程全局命名空间；native 下 rlib 定义本就是链接期静态绑定）：
   `.globl <name>; .hidden <name>; .type <name>,@function; <name>: movabs rax, <addr>; jmp rax`。
4. cc 编译跳板对象并入**重链**（`archive.a tramp.o -l…`）。重链仍失败
   （交集外还有未解符号）⇒ 原错误路径（与今日逐字节同款诊断）。
5. 缓存键：注入路径把**排序后的 (name,addr) 对**并入 `content_hash` 部件
   （首链成功路径键不动，行为兼容）；.so 是模块专属件——P1 码址跨进程稳定，
   同模块重复运行恒命中，不同模块天然异键。
6. 签名/调用面：`materialize_static_libraries(tcx)` → `(tcx, &mut Linker)`
   （单调用点 lower/mod.rs:2045，global_asm C7 同款改造先例）。

## 3. 验收矩阵（闭合性验收，全部逐字节三维）

1. **MRE 转正**：`/tmp/mre` 重构为 corpus 探针（`c_rlib_sym_probe`：
   helper.a 蹦床 + `#[export_name]` Rust 定义回写观察量）——三维绿。
2. **bzip2 C 后端复绿**：`c_bzip2_pure` 现走 0.6 纯 Rust后端（绕行期决定）；
   新增/回切 driver 走 **vendored C 后端**，验证 BZ_NO_STDIO 断言桩经 rlib
   注入回调的符号救援与正常压缩/解压。该 workload 的正常路径不会触发断言回调，
   不能据此宣称回调 panic 已执行；该回调是普通 `extern "C"`，真 panic 逃逸时应终止。
3. **c_wasmtime_wat 换面**：层①消除后 driver 红 pattern 由
   `无法安全转换为共享库`/101 换为 `inline asm noreturn`/70（层② C3 入口；
   driver 头注既定接线策略原话执行）。
4. 回归零变化承诺：首链成功归档（rusqlite/aws_lc/libgit2/…）行为逐字节不变；
   全量 gate5 + cargo test + diff。

## 4. 风险与边界（评审诚实面）

- 闭包判定集 = ar 成员 `SHN_UNDEF` ∩ crate 图 rlib 导出集——二进制级静态
  枚举（`src/elfsym.rs` 同源解析器，无工具文本依赖）。交集外的未解符号
  （跨归档重名、第三方库缺件）重链仍失败 ⇒ 原错误路径，不放宽闭包纪律。
- `-z defs` 纪律不松动：注入只是把「rlib 导出 fn」纳入闭包判定，未命中
  NAME 仍响亮拒。
- 跳板 `.hidden` 避免 RTLD_GLOBAL 全局插桩（评审中识别并采纳）。
- 两归档同 crate 图引同一 guest fn：各自注入同 stub 地址（预算幂等），
  hidden 可见性无全局碰撞。
- L2/缓存：注入路径键含 (name,addr) 对；冷/热行为一致由既有 P1 启动相
  重建兜底（trampoline 只存码址，条目本体启动相复现）。
- 未立项：rlib **数据**符号（static 被 C 引用，ABS `.set` 语义对数据引用
  需另证）——响亮拒，按需另立。

## 5. ad-hoc 自查记录（2026-07-18，用户点名复核）

初版 §2-1 计划「解析 ld stderr 的 `undefined reference to` 行」——复核判定
为 ad-hoc 件：依赖 ld 诊断文本格式（GNU ld/lld 两系），脆弱且无格式契约。
**修订为**：`src/elfsym.rs` 同源 ELF64 解析器 + ar 成员遍历，二进制级静态
枚举 `SHN_UNDEF` 符号（无任何工具文本解析；elfsym 自 M5.1 起即归档装载的
二进制面同源件）。方案其余面自查结论：**非 ad-hoc**——
①闭包集「rlib 导出 fn」= rustc `exported_non_generic_symbols`
（native final link 的集符集本机权威，非按实例白名单）；
②跳板 = P1 可执行条目的链接期物化（native 静态绑定语义对偶，非 wasmtime
特例；hidden 可见性还是评审期对全局插桩的通用防护）；
③失败触发的两段链 = 兼容性刻意选择（首链成功归档的 cc 行逐字节不变、
缓存键不动），非绕过式设计；
④`movabs+jmp` 跳板形 = C7 已验收的同族手法（GAS Intel `call ABS` 实锤后
的统一形制）。
