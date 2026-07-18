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

1. 首链照旧（`cc -z defs --whole-archive …`）。失败时解析 stderr 的
   `undefined reference to `<NAME>'` 行（GNU ld 实测格式；lld 用
   `undefined symbol: <NAME>` 一并覆盖）。
2. 全部 NAME 经 `canonical_link_name` 剥前缀后查 `exported_defs()`：
   - 命中且是 Fn ⇒ `linker.fn_entry_addr(inst)` 预算 → (name, stub_addr)；
   - 命中 Static ⇒ Err「数据符号在 rlib（C2 边界，按需另立）」；
   - 未命中 ⇒ 走原错误路径（响亮拒，文案同今）。
3. 发射跳板 `.s`（C7 global_asm 同款，**`.hidden <name>`**——只在 .so 内部
   绑定，不污染进程全局命名空间；native 下 rlib 定义本就是链接期静态绑定）：
   `.globl <name>; .hidden <name>; .type <name>,@function; <name>: movabs rax, <addr>; jmp rax`。
4. cc 编译跳板对象并入重链（`archive.a tramp.o -l…`）→ .so。
5. 缓存键：注入路径把**排序后的 (name,addr) 对**并入 `content_hash` 部件
   （首链成功路径键不动，行为兼容）；.so 是模块专属件——P1 码址跨进程稳定，
   同模块重复运行恒命中，不同模块天然异键。
6. 签名/调用面：`materialize_static_libraries(tcx)` → `(tcx, &mut Linker)`
   （单调用点 lower/mod.rs:2045，global_asm C7 同款改造先例）。

## 3. 验收矩阵（闭合性验收，全部逐字节三维）

1. **MRE 转正**：`/tmp/mre` 重构为 corpus 探针（`c_rlib_sym_probe`：
   helper.a 蹦床 + `#[export_name]` Rust 定义回写观察量）——三维绿。
2. **bzip2 C 后端复绿**：`c_bzip2_pure` 现走 0.6 纯 Rust 后端（绕行期决定）；
   新增/回切 driver 走 **vendored C 后端**（BZ_NO_STDIO 断言桩经 rlib 注入
   回调）——压缩/解压字节级 + **断言路径**（损档触发 `bz_internal_error` →
   Rust panic 穿出 = 101，与 native 同出口）三维一致。
3. **c_wasmtime_wat 换面**：层①消除后 driver 红 pattern 由
   `无法安全转换为共享库`/101 换为 `inline asm noreturn`/70（层② C3 入口；
   driver 头注既定接线策略原话执行）。
4. 回归零变化承诺：首链成功归档（rusqlite/aws_lc/libgit2/…）行为逐字节不变；
   全量 gate5 + cargo test + diff。

## 4. 风险与边界（评审诚实面）

- ld stderr 解析 = 我们自家 cc 调用的固定 flag 面（GNU ld 实测；lld 一并匹配）。
  解析不到的形态（跨归档重名等）按未命中走原错误路径，不放宽闭包纪律。
- `-z defs` 纪律不松动：注入只是把「rlib 导出 fn」纳入闭包判定，未命中
  NAME 仍响亮拒。
- 跳板 `.hidden` 避免 RTLD_GLOBAL 全局插桩（评审中识别并采纳）。
- 两归档同 crate 图引同一 guest fn：各自注入同 stub 地址（预算幂等），
  hidden 可见性无全局碰撞。
- L2/缓存：注入路径键含 (name,addr) 对；冷/热行为一致由既有 P1 启动相
  重建兜底（trampoline 只存码址，条目本体启动相复现）。
- 未立项：rlib **数据**符号（static 被 C 引用，ABS `.set` 语义对数据引用
  需另证）——响亮拒，按需另立。
