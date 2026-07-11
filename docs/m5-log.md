# M5 施工日志（JIT —— asm 清零 + 方法级 Cranelift 加速）

> 承 `docs/m4-log.md`（M4 关账）。设计见 `docs/m5-design.md`（D1-D7 已批准，
> D5=T 骨架+触发器）。每期 gate 结果 + 教训 + 遗留归期，格式同 m4-log。

## M5.0 asm-stub 工厂（轨 A 起步）—— **完成**（2026-07-11）

**做了什么**（cg_clif `rustc_codegen_cranelift/src/inline_asm.rs` 逐段同构）：
inline asm 站点不再降 Trap，而是**自做寄存器分配 + 渲染 GAS wrapper**（`fn(*mut u8)`
槽缓冲 ABI：rbx=缓冲基址，clobber 保存 → 从槽装输入寄存器 → asm 模板本体 → 回存
输出寄存器 → 恢复 clobber → ret），**批量交外部汇编器 cc** 汇编成 .so → dlopen →
dlsym 得真地址；解释器执行 = 栈开缓冲、按 ins 装槽、call wrapper、按 outs 取槽。

- `src/lower/asm.rs`（新）：`generate`（allocate_registers 两相 + allocate_stack_slots
  + generate_asm_wrapper，intel 语法/ELF/x86_64 分支）+ `materialize`（拼 .s → cc
  -shared -fPIC -nostdlib → dlopen RTLD_NOW|LOCAL → dlsym `mirvm_asm_{i}`；FNV-1a
  内容哈希缓存 `~/.cache/mirvm/asm-stubs/<hash>.{s,so}`，热缓存零 cc）。
- `ir.rs`：`Terminator::InlineAsm { stub, buf_size, ins:Vec<(off,Operand)>,
  outs:Vec<(off,ScalarPlace)>, target }` + `Module.asm_stub_addrs: Vec<u64>` +
  `AsmStubId`。
- `func.rs`：`LowerCx.def_id`（asm_target_features 查询）；`lower_inline_asm`——MIR
  操作数 → wrapper 约束 + 配对已降低值/落点与 wrapper 槽偏移（同源一致）。拒
  naked/may_unwind/noreturn/att_syntax/sym/label/const/带 cleanup/非 x86_64 →
  Trap-stub（诊断留痕）。
- `interp.rs`：InlineAsm 终止子执行（栈 `#[repr(align(16))]` 256B 缓冲上限守卫）。

**Gate 结果**：asm-stub 工厂三面孔全验证，corpus **15→16 pass**（tempfile 转绿），
零回归。

| corpus 用例 | asm 面孔 | M5.0 前 | M5.0 后 |
|---|---|---|---|
| **tempfile** | 裸 syscall（rustix） | asm 红 | **全绿** ✓（真 read/write/fstat，文件 IO 对拍一致） |
| numbigint | div（128/64 宽除法） | asm 红（死于 div_wide） | 推进过 div（50!/modpow 正确）→ 撞 `llvm.x86.addcarry.64`（M5.1） |
| blake3 | cpuid（特性检测） | asm 红（死于 cpuid） | 推进过 cpuid → 撞 `llvm.x86.xgetbv`（M5.1） |
| sha2 | cpuid | asm 红 | 推进过 cpuid → 撞 `llvm.x86.ssse3.pshuf.b.128`（M5.1 SIMD） |
| ecosystem（diff_cargo） | cpuid（regex/std_detect） | asm 红 | 推进过 cpuid（serde 正确）→ 撞 `llvm.x86.xgetbv`（M5.1） |

**回归无损**：diff 16/16、gate0/1/2 九用例、spike1-5、TSan 零竞争、纯度门禁、
diff_cargo ffi_zlib+project 绿；加载相 fib 420ms（≈ M4.5 的 413ms，asm 物化无感）。

### 经验与教训

1. **asm 三面孔的 wrapper 生成一次成型，实证 cg_clif 同构可靠**：div wrapper 实测
   `mov rcx,[rbx+0x10]`（divisor→分配得 rcx）/`div rcx`/存商余——机器码逐条正确。
   侦察先行（先 dump 三面孔真 MIR + grep rustc_target::asm 全 API）值回票价：一次
   编译通过、三面孔一次跑对。**Cranelift 本身无 asm 能力**（corpus §2.2 的"Cranelift
   有 inline-asm 降低"不确，已修正为 cg_clif=寄存器分配+wrapper+外部汇编器）。
2. **"修完撞下一层"再次应验**（M4.5 同律）：设计 M5.0 gate 写"numbigint 绿"，实测
   div 只是 numbigint 的**第一层**asm 债——修完 div 立刻撞 `llvm.x86.addcarry.64`
   （scalar 进位加 intrinsic，非 asm）。**这不是 M5.0 缺陷，是设计对 numbigint 依赖
   面的低估**：div 是 asm-stub 的活儿（已清），addcarry 是 intrinsic 的活儿（D7）。
   四个 asm-红用例修完 asm 面后，**下一层全部收敛到 `llvm.x86.*` intrinsic**
   （addcarry/xgetbv/pshufb）——正是 M5.1 的 D7"SIMD/intrinsic 补面"范围。机制不同
   （asm-stub 终止子 vs intrinsic 内建/SIMD lane），故不并入 M5.0，M5.1 统一清。
3. **rbx 基址不变量是 wrapper 成立的支点**（照抄勿创新）：rustc 保留 rbx（LLVM 基址
   寄存器）永不分配给 reg 类操作数；cpuid 惯用法 `mov {0:r},rbx;cpuid;xchg {0:r},rbx`
   自己保 rbx 跨 cpuid（cpuid 写 ebx）——正因此 wrapper 用 rbx 存缓冲基址而安全。
4. **`late` 字段的死代码 lint 陷阱**：InlineAsmOperand 的 late 只在 Out 相区分有用
   （inout 恒相 0，cg_clif 命名 `_late` 回避）；字面模式匹配 `late: true` 不算 lint
   的"读"——两相分配循环里 bind `late` 变量比较才消警（顺带更清晰）。
5. **eager 物化的载入面影响可控**：asm 站点现在**总是**物化（非惰性 Trap），任何拉入
   std asm 的程序会在加载相触发 cc——但内容哈希缓存 + 三面孔外 std asm 多被 Trap-stub
   拒（sym/label/复杂形态）→ fib 载入无感（420ms）。**防静默错值**：att_syntax 模板
   与 intel wrapper 冲突 → 拒而非误汇编。

### 遗留（归期明确）

- **`llvm.x86.*` intrinsic 补面 = M5.1（D7）**：scalar（addcarry.64/xgetbv）+ SIMD
  lane（ssse3.pshuf.b.128 等）→ numbigint/blake3/sha2/ecosystem 全绿。xgetbv 是
  cpuid 姊妹（特性检测），"虚拟 CPU=真宿主 CPU"下宜执行真指令。
- **静态原生归档装载 = M5.1（D2）**：blake3 修完 xgetbv 后下一层是 `.S` 归档符号
  （build.rs 产物，dlsym 不到）——.a→.so 转换 + native_libraries 收集。
- asm 支持面（sym/label/const/may_unwind/非 x86_64）按需再补（三面孔未触及）。
