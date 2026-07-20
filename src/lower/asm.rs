//! inline asm 站点 → GAS wrapper 文本生成（M5.0 asm-stub 工厂，轨 A）。
//!
//! cg_clif `rustc_codegen_cranelift/src/inline_asm.rs` 逐段同构移植：Cranelift 本身
//! 无 asm 能力，cg_clif 的做法是**自做寄存器分配** + 渲染一个 `fn(*mut u8)` 的 GAS
//! wrapper（单指针指向槽缓冲：clobber 保存 → 从缓冲装输入寄存器 → asm 模板本体 →
//! 回存输出寄存器 → 恢复 clobber → ret），交外部汇编器（步 2 的 cc）汇编成机器码。
//!
//! wrapper ABI 的承重不变量（照抄，勿创新）：
//! - `rbx` = 缓冲基址（`push rbx; mov rbx,rdi`）；所有槽访问 `[rbx+off]`。
//! - rustc **保留 rbx**（LLVM 基址寄存器），永不分配给 `reg` 类操作数 → 缓冲基址
//!   在 asm 本体执行期间安全。cpuid 惯用法 `mov {0:r},rbx; cpuid; xchg {0:r},rbx`
//!   自己保存/恢复 rbx 跨 cpuid（cpuid 写 ebx）——正因此该 wrapper 方案对 cpuid 成立。
//! - `.intel_syntax noprefix` 包裹（末尾 `.att_syntax` 复原）。
//!
//! M5.0 仅 x86_64（func.rs 边界已拒非 x86_64/naked/may_unwind/sym/label/const）。

use std::fmt::Write as _;
use std::path::PathBuf;

use rustc_abi::{Align, Size};
use rustc_ast::ast::InlineAsmTemplatePiece;
use rustc_hir::def_id::DefId;
use rustc_middle::ty::TyCtxt;
use rustc_span::sym;
use rustc_target::asm::{
    InlineAsmArch, InlineAsmClobberAbi, InlineAsmReg, InlineAsmRegClass, InlineAsmRegOrRegClass,
    X86InlineAsmRegClass, allocatable_registers,
};

use crate::vm::engine::ir;

/// asm-stub 批量物化（M5.0 步 2）：全部 wrapper 文本拼一个 .s → cc 汇编成 .so →
/// dlopen → 逐站点**自带符号名** dlsym → 真地址表（AsmStubId = 位序 → u64）。
/// 符号名与位序解耦（A2 split：最终位序收尾才知，名字在 lower 期已烤进文本）。
///
/// 内容哈希缓存 `~/.cache/mirvm/asm-stubs/<hash>.so`——热缓存零 cc 调用。dlopen 句柄
/// 泄漏（进程生命周期常驻，代码地址随之有效）。物化在加载相（OS 交互合法域）；返回
/// 的 u64 表移交 Module，执行相只读直调、纯度不破。空站点集 = 空表（零开销）。
/// FNV-1a 64 位内容哈希（asm-stub 与 global-asm 缓存键共用）。
pub(crate) fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

// ===== T5 syscall 拦截（decision-history §7.18，E19③④）=====
//
// guest inline-asm 与 global_asm 内的裸 `syscall` 指令：mirvm 本就持有全部 GAS
// 文本（生成点在手，无需扫二进制）。文本改写 `syscall` → 经间接槽的
// `call QWORD PTR [rip+mirvm_syscall_slot]`；槽随 .so 物化、dlopen 后重填为
// arch::x86_64::asmstub 的 trampoline 真地址（P2 启动相重填同款纪律）；
// trampoline 保 syscall 全契约（整数/flags/xmm/mxcsr）后落
// os::process::mirvm_syscall_dispatch（v1 直通 + TRACE）。
// 覆盖边界（如实，无真实形态）：`sysenter`/`int $0x80`、`.byte 0x0f,0x05` 对抗
// 书写、行内多语句/标签前缀花式——不接；重开需实锤 crate。

/// 间接槽定义（命中即随 .s 附带一次；`%rip` 相对寻址，.so 自洽无需外部符号）。
const SYSCALL_SLOT_DEF: &str =
    ".data\n.globl mirvm_syscall_slot\n.p2align 3\nmirvm_syscall_slot: .quad 0\n.text\n";

/// `syscall` 指令的替换体（RIP 相对间接调用；asm-stub 全区为 `.intel_syntax
/// noprefix` 包裹——必须用 Intel 形式，AT&T 的 `*(%rip)` 会被 GAS 拒）。
/// 两级间接（PIC 纪律）：GOT 项（动态链接器装载期填）→ 命名 .data 槽
/// （mirvm dlopen 后重填 trampoline 真址）；r11 恰为 syscall 契约的可坏
/// 寄存器，作跳板不违约。
const SYSCALL_CALL: &str = "    mov r11, QWORD PTR [rip+mirvm_syscall_slot@GOTPCREL]\n    call [r11]\n";

/// 行级助记符匹配：前导空白已剥；`syscall` 后只容许行尾/空白/注释（# 或 ;）。
/// 多语句同行与标签前缀形态不接（覆盖边界，见上）。
fn is_syscall_insn_line(trimmed: &str) -> bool {
    let Some(rest) = trimmed.strip_prefix("syscall") else {
        return false;
    };
    let mut cs = rest.chars();
    match cs.next() {
        None => true,
        Some(c) => c.is_whitespace() || c == '#' || c == ';',
    }
}

/// 改写 src 中全部 `syscall` 指令行为间接槽调用。返回是否有改写（决定是否
/// 附带槽定义与 dlopen 后重填）。
pub(crate) fn rewrite_syscall_text(src: &mut String) -> bool {
    let mut hit = false;
    let mut out = String::with_capacity(src.len() + 64);
    for line in src.split_inclusive('\n') {
        if is_syscall_insn_line(line.trim_start()) {
            out.push_str(SYSCALL_CALL);
            hit = true;
        } else {
            out.push_str(line);
        }
    }
    *src = out;
    if hit {
        src.push_str(SYSCALL_SLOT_DEF);
    }
    hit
}

/// dlopen 后重填 syscall 间接槽（在场才填——系统库无此符号，静默跳过）。
pub(crate) fn refill_syscall_slot(handle: usize) {
    let slot = crate::os::dll::sym(handle, c"mirvm_syscall_slot");
    if slot != 0 {
        unsafe {
            *(slot as *mut u64) = crate::arch::x86_64::asmstub::syscall_trampoline_addr();
        }
    }
}

pub(crate) fn materialize(sites: &[ir::AsmSite]) -> Vec<u64> {
    if sites.is_empty() {
        return Vec::new();
    }
    let mut src = String::new();
    src.push_str("# mirvm asm-stub 工厂产物（M5.0）——请勿手改\n");
    for site in sites {
        src.push_str(&site.text);
    }
    // T5：裸 `syscall` 指令 → 间接槽调用（改写发生在内容哈希前，缓存键与
    // 最终字节一致）
    rewrite_syscall_text(&mut src);

    // FNV-1a 内容哈希（稳定、跨运行可复用缓存键）
    let h = fnv1a(src.as_bytes());

    let dir = crate::sysroot::cache_dir().join("asm-stubs");
    std::fs::create_dir_all(&dir).expect("创建 asm-stub 缓存目录失败");
    let so: PathBuf = dir.join(format!("{h:016x}.so"));

    if !so.exists() {
        let s_path = dir.join(format!("{h:016x}.s"));
        std::fs::write(&s_path, &src).expect("写 asm-stub .s 失败");
        // -shared -fPIC：wrapper 自包含（无外部符号），dlopen 后 dlsym 各站点即得真址。
        // 先写临时名再 rename 原子发布——并发 mirvm 进程同键物化时绝不 dlopen 半成品
        let tmp = dir.join(format!("{h:016x}.so.tmp.{}", std::process::id()));
        let status = std::process::Command::new("cc")
            .args(["-shared", "-fPIC", "-nostdlib", "-o"])
            .arg(&tmp)
            .arg(&s_path)
            .status()
            .expect("调用 cc 汇编 asm-stub 失败（PATH 缺 cc？）");
        assert!(
            status.success(),
            "cc 汇编 asm-stub 失败（源：{}）",
            s_path.display()
        );
        std::fs::rename(&tmp, &so).expect("asm-stub .so 原子发布失败");
    }

    let c_so = std::ffi::CString::new(so.as_os_str().as_encoded_bytes()).unwrap();
    let handle = crate::os::dll::open_with_flags(&c_so, crate::os::dll::RTLD_NOW | crate::os::dll::RTLD_LOCAL)
        .unwrap_or_else(|_| panic!("dlopen asm-stub .so 失败: {}", so.display()));
    refill_syscall_slot(handle);

    sites
        .iter()
        .map(|site| {
            let name = std::ffi::CString::new(&*site.name).unwrap();
            let addr = crate::os::dll::sym(handle, &name);
            assert!(addr != 0, "dlsym {} 失败", site.name);
            addr as u64
        })
        .collect()
}

/// 一个 asm 操作数供 wrapper 生成用的约束（值/落点由 func.rs 另配，不进本模块）。
/// `late` 决定类操作数分配相位（非 late 输出先分配，与输入互斥；late 输出后分配、
/// 可与输入共寄存器）——cg_clif allocate_registers 的语义。M5.x 补 Const/sym 操作数。
pub(crate) enum AsmOperand {
    In {
        reg: InlineAsmRegOrRegClass,
    },
    Out {
        reg: InlineAsmRegOrRegClass,
        late: bool,
        has_place: bool,
    },
    // inout 无 late 相区分（恒在相 0 分配，与 cg_clif `_late` 同——故此处不带）
    InOut {
        reg: InlineAsmRegOrRegClass,
        has_out_place: bool,
    },
    /// const/sym 操作数（M5.2 D8h）：无寄存器，值/符号名在 lower 期渲染成字面文本，
    /// 占位符直接展开为该文本（cg_clif 同样把 const 格式化进模板）。寄存器分配跳过。
    Inline {
        text: String,
    },
}

/// wrapper 生成产物：文本 + 缓冲大小 + 每操作数的输入/输出槽偏移（供 func.rs 配对
/// 已降低的值/落点，缓冲偏移与 wrapper 内 `[rbx+off]` 同源一致——正确性的地基）。
pub(crate) struct GeneratedAsm {
    pub text: String,
    pub buf_size: u32,
    pub input_slot: Vec<Option<u32>>,
    pub output_slot: Vec<Option<u32>>,
}

/// asm 站点降低入口（cg_clif codegen_inline_asm_inner 同构）。
/// `name` = 该 stub 的全局唯一符号（步 2 dlsym 用）。
pub(crate) fn generate<'tcx>(
    tcx: TyCtxt<'tcx>,
    enclosing_def_id: DefId,
    arch: InlineAsmArch,
    template: &[InlineAsmTemplatePiece],
    operands: &[AsmOperand],
    name: &str,
) -> GeneratedAsm {
    // 语法/相位选项（noreturn/may_unwind/att_syntax）已在 func.rs 边界处置，此处只生成
    // intel 语法非发散 wrapper。
    let mut g = Gen {
        tcx,
        arch,
        enclosing_def_id,
        template,
        operands,
        registers: vec![None; operands.len()],
        slots_clobber: vec![None; operands.len()],
        slots_input: vec![None; operands.len()],
        slots_output: vec![None; operands.len()],
        slot_size: Size::ZERO,
    };
    g.allocate_registers();
    g.allocate_stack_slots();
    let text = g.generate_asm_wrapper(name);
    GeneratedAsm {
        text,
        buf_size: g.slot_size.bytes() as u32,
        input_slot: g
            .slots_input
            .iter()
            .map(|s| s.map(|s| s.bytes() as u32))
            .collect(),
        output_slot: g
            .slots_output
            .iter()
            .map(|s| s.map(|s| s.bytes() as u32))
            .collect(),
    }
}

struct Gen<'a, 'tcx> {
    tcx: TyCtxt<'tcx>,
    arch: InlineAsmArch,
    enclosing_def_id: DefId,
    template: &'a [InlineAsmTemplatePiece],
    operands: &'a [AsmOperand],
    registers: Vec<Option<InlineAsmReg>>,
    slots_clobber: Vec<Option<Size>>,
    slots_input: Vec<Option<Size>>,
    slots_output: Vec<Option<Size>>,
    slot_size: Size,
}

impl<'tcx> Gen<'_, 'tcx> {
    /// cg_clif allocate_registers 的**保守变体**：显式寄存器入已分配集 → out/inout
    /// 类先分配（约束更紧）→ in/lateout 类后分配。**与 cg_clif 的一处已知分歧**：
    /// cg_clif 按 (in用,out用) 位分别判冲突（允许 in↔lateout 共享一个寄存器），此处
    /// contains_key 全冲突不共享——分配结果恒为合法子集，代价是极端密集操作数时可能
    /// 提前分配不出（panic → catch_lower → Trap 诊断，响亮不静默）。按需再对齐。
    fn allocate_registers(&mut self) {
        let sess = self.tcx.sess;
        let map = allocatable_registers(
            self.arch,
            sess.relocation_model(),
            self.tcx.asm_target_features(self.enclosing_def_id),
            &sess.target,
        );
        let mut allocated =
            rustc_data_structures::fx::FxHashMap::<InlineAsmReg, (bool, bool)>::default();
        let mut regs = vec![None; self.operands.len()];

        // 显式寄存器入已分配集
        for (i, op) in self.operands.iter().enumerate() {
            let (reg, is_in, is_out) = match op {
                AsmOperand::In {
                    reg: InlineAsmRegOrRegClass::Reg(r),
                } => (*r, true, false),
                AsmOperand::Out {
                    reg: InlineAsmRegOrRegClass::Reg(r),
                    late: true,
                    ..
                } => (*r, false, true),
                AsmOperand::Out {
                    reg: InlineAsmRegOrRegClass::Reg(r),
                    ..
                }
                | AsmOperand::InOut {
                    reg: InlineAsmRegOrRegClass::Reg(r),
                    ..
                } => (*r, true, true),
                _ => continue,
            };
            regs[i] = Some(reg);
            let e = allocated.entry(reg).or_default();
            e.0 |= is_in;
            e.1 |= is_out;
        }

        // 类操作数分两相（cg_clif 顺序）：相 0 = out(非 late)/inout（约束更紧，与输入
        // 互斥）先分配；相 1 = in/lateout（可与相 0 之外的寄存器共用）。`late` 决定
        // Out 落哪一相。
        for phase_late in [false, true] {
            for (i, op) in self.operands.iter().enumerate() {
                let class = match op {
                    AsmOperand::Out {
                        reg: InlineAsmRegOrRegClass::RegClass(c),
                        late,
                        ..
                    } if *late == phase_late => *c,
                    AsmOperand::InOut {
                        reg: InlineAsmRegOrRegClass::RegClass(c),
                        ..
                    } if !phase_late => *c,
                    AsmOperand::In {
                        reg: InlineAsmRegOrRegClass::RegClass(c),
                    } if phase_late => *c,
                    _ => continue,
                };
                let reg = Self::alloc_reg(&map, class, &allocated);
                regs[i] = Some(reg);
                allocated.insert(reg, (true, true));
            }
        }

        self.registers = regs;
    }

    fn alloc_reg(
        map: &rustc_data_structures::fx::FxHashMap<
            InlineAsmRegClass,
            rustc_data_structures::fx::FxIndexSet<InlineAsmReg>,
        >,
        class: InlineAsmRegClass,
        allocated: &rustc_data_structures::fx::FxHashMap<InlineAsmReg, (bool, bool)>,
    ) -> InlineAsmReg {
        for &reg in &map[&class] {
            let mut used = false;
            reg.overlapping_regs(|r| {
                if allocated.contains_key(&r) {
                    used = true;
                }
            });
            if !used {
                return reg;
            }
        }
        panic!("inline asm: 无法为寄存器类 {class:?} 分配寄存器")
    }

    /// cg_clif allocate_stack_slots 同构：clobber（非 C-clobber 覆盖者）→ inout（输入/
    /// 输出共槽）→ input → output（与 input 重叠复用省内存）。
    fn allocate_stack_slots(&mut self) {
        let mut slot_size = Size::ZERO;
        let arch = self.arch;
        let new_slot = |slot_size: &mut Size, class: InlineAsmRegClass| -> Size {
            let reg_size = class
                .supported_types(arch, true)
                .iter()
                .map(|(ty, _)| ty.size())
                .max()
                .unwrap();
            let align = Align::from_bytes(reg_size.bytes()).unwrap();
            let offset = slot_size.align_to(align);
            *slot_size = offset + reg_size;
            offset
        };

        // clobber 槽：保存 wrapper 要保留但 asm 会破坏的寄存器（C-clobber ABI 覆盖者
        // 免存——调用约定本就允许破坏）。
        let abi_clobber = InlineAsmClobberAbi::parse(
            self.arch,
            &self.tcx.sess.target,
            &self.tcx.sess.unstable_target_features,
            sym::C,
        )
        .unwrap()
        .clobbered_regs();
        for (i, reg) in self
            .registers
            .iter()
            .enumerate()
            .filter_map(|(i, r)| r.map(|r| (i, r)))
        {
            let mut need_save = true;
            for r in abi_clobber {
                r.overlapping_regs(|r| {
                    if r == reg {
                        need_save = false;
                    }
                });
                if !need_save {
                    break;
                }
            }
            if need_save {
                self.slots_clobber[i] = Some(new_slot(&mut slot_size, reg.reg_class()));
            }
        }

        // inout：输入输出共槽
        for (i, op) in self.operands.iter().enumerate() {
            if let AsmOperand::InOut {
                reg,
                has_out_place: true,
                ..
            } = op
            {
                let slot = new_slot(&mut slot_size, reg.reg_class());
                self.slots_input[i] = Some(slot);
                self.slots_output[i] = Some(slot);
            }
        }

        let slot_size_before_input = slot_size;
        // input
        for (i, op) in self.operands.iter().enumerate() {
            match op {
                AsmOperand::In { reg }
                | AsmOperand::InOut {
                    reg,
                    has_out_place: false,
                    ..
                } => {
                    self.slots_input[i] = Some(new_slot(&mut slot_size, reg.reg_class()));
                }
                _ => {}
            }
        }
        // output 与 input 区间重叠复用（省内存；照抄 cg_clif）
        let slot_size_after_input = slot_size;
        slot_size = slot_size_before_input;
        for (i, op) in self.operands.iter().enumerate() {
            if let AsmOperand::Out {
                reg,
                has_place: true,
                ..
            } = op
            {
                self.slots_output[i] = Some(new_slot(&mut slot_size, reg.reg_class()));
            }
        }
        slot_size = slot_size.max(slot_size_after_input);

        self.slot_size = slot_size;
    }

    /// cg_clif generate_asm_wrapper 同构（ELF/x86_64 分支；intel 语法）。
    fn generate_asm_wrapper(&self, name: &str) -> String {
        let mut s = String::new();
        writeln!(s, ".globl {name}").unwrap();
        writeln!(s, ".type {name},@function").unwrap();
        writeln!(s, ".section .text.{name},\"ax\",@progbits").unwrap();
        writeln!(s, "{name}:").unwrap();
        s.push_str(".intel_syntax noprefix\n");

        Self::prologue(&mut s);

        // 保存 clobber 寄存器
        for (reg, slot) in self.iter_reg_slot(&self.slots_clobber) {
            Self::save_register(&mut s, reg, slot);
        }
        // 装入输入寄存器
        for (reg, slot) in self.iter_reg_slot(&self.slots_input) {
            Self::restore_register(&mut s, reg, slot);
        }

        // asm 模板本体
        for piece in self.template {
            match piece {
                InlineAsmTemplatePiece::String(text) => s.push_str(text),
                InlineAsmTemplatePiece::Placeholder {
                    operand_idx,
                    modifier,
                    ..
                } => {
                    // const/sym：字面文本；其余：分配到的寄存器名
                    if let AsmOperand::Inline { text } = &self.operands[*operand_idx] {
                        s.push_str(text);
                    } else {
                        let reg = self.registers[*operand_idx].unwrap();
                        Self::emit_reg(&mut s, reg, *modifier);
                    }
                }
            }
        }
        s.push('\n');

        // 存输出寄存器 → 恢复 clobber → 收尾
        for (reg, slot) in self.iter_reg_slot(&self.slots_output) {
            Self::save_register(&mut s, reg, slot);
        }
        for (reg, slot) in self.iter_reg_slot(&self.slots_clobber) {
            Self::restore_register(&mut s, reg, slot);
        }
        Self::epilogue(&mut s);

        s.push_str(".att_syntax\n");
        writeln!(s, ".size {name}, .-{name}").unwrap();
        s.push_str(".text\n\n\n");
        s
    }

    fn iter_reg_slot<'s>(
        &'s self,
        slots: &'s [Option<Size>],
    ) -> impl Iterator<Item = (InlineAsmReg, Size)> + 's {
        self.registers
            .iter()
            .zip(slots.iter().copied())
            .filter_map(|(r, s)| r.zip(s))
    }

    /// 占位符 → 寄存器名（intel 语法；xmm/ymm/zmm 特殊命名照抄 cg_clif）。
    fn emit_reg(s: &mut String, reg: InlineAsmReg, modifier: Option<char>) {
        if let InlineAsmReg::X86(r) = reg
            && matches!(
                r.reg_class(),
                X86InlineAsmRegClass::xmm_reg
                    | X86InlineAsmRegClass::ymm_reg
                    | X86InlineAsmRegClass::zmm_reg
            )
        {
            let n = r.name();
            match modifier {
                Some(prefix) => write!(s, "{prefix}mm{}", &n[3..]).unwrap(),
                None => write!(s, "{n}").unwrap(),
            }
            return;
        }
        reg.emit(s, InlineAsmArch::X86_64, modifier).unwrap();
    }

    fn prologue(s: &mut String) {
        s.push_str("    push rbp\n");
        s.push_str("    mov rbp,rsp\n");
        s.push_str("    push rbx\n"); // rbx callee-saved 且被 LLVM 保留为基址
        s.push_str("    mov rbx,rdi\n");
    }

    fn epilogue(s: &mut String) {
        s.push_str("    pop rbx\n");
        s.push_str("    pop rbp\n");
        s.push_str("    ret\n");
    }

    fn save_register(s: &mut String, reg: InlineAsmReg, offset: Size) {
        if let InlineAsmReg::X86(r) = reg
            && matches!(
                r.reg_class(),
                X86InlineAsmRegClass::xmm_reg
                    | X86InlineAsmRegClass::ymm_reg
                    | X86InlineAsmRegClass::zmm_reg
            )
        {
            let n = r.name();
            let mov = if n.starts_with("xmm") {
                "movups"
            } else {
                "vmovups"
            };
            writeln!(s, "    {mov} [rbx+0x{:x}], {n}", offset.bytes()).unwrap();
            return;
        }
        write!(s, "    mov [rbx+0x{:x}], ", offset.bytes()).unwrap();
        reg.emit(s, InlineAsmArch::X86_64, None).unwrap();
        s.push('\n');
    }

    fn restore_register(s: &mut String, reg: InlineAsmReg, offset: Size) {
        if let InlineAsmReg::X86(r) = reg
            && matches!(
                r.reg_class(),
                X86InlineAsmRegClass::xmm_reg
                    | X86InlineAsmRegClass::ymm_reg
                    | X86InlineAsmRegClass::zmm_reg
            )
        {
            let n = r.name();
            let mov = if n.starts_with("xmm") {
                "movups"
            } else {
                "vmovups"
            };
            writeln!(s, "    {mov} {n}, [rbx+0x{:x}]", offset.bytes()).unwrap();
            return;
        }
        s.push_str("    mov ");
        reg.emit(s, InlineAsmArch::X86_64, None).unwrap();
        writeln!(s, ", [rbx+0x{:x}]", offset.bytes()).unwrap();
    }
}

#[cfg(all(test, target_arch = "x86_64"))]
mod tests {
    use super::materialize;
    use crate::vm::engine::ir;

    #[test]
    fn materialized_stub_is_callable_and_writes_its_buffer() {
        let site = ir::AsmSite {
            name: "mirvm_asm_0".into(),
            text: r#"
.globl mirvm_asm_0
.type mirvm_asm_0,@function
.section .text.mirvm_asm_0,"ax",@progbits
mirvm_asm_0:
    .intel_syntax noprefix
    movabs rax, 0x0123456789abcdef
    mov QWORD PTR [rdi], rax
    ret
    .att_syntax
.size mirvm_asm_0, .-mirvm_asm_0
.text
"#
            .to_owned(),
        };
        let addrs = materialize(&[site]);
        let mut value = 0u64;
        let stub: unsafe extern "C" fn(*mut u8) = unsafe { std::mem::transmute(addrs[0]) };
        unsafe { stub((&mut value as *mut u64).cast()) };
        assert_eq!(value, 0x0123_4567_89ab_cdef);
    }

    #[test]
    fn rewrite_syscall_text_hits_only_mnemonic_lines() {
        let mut src = "mov rax, 1\n    syscall\nsyscallx\n.byte 0x0f,0x05\n  syscall # c\nnop\n"
            .to_string();
        assert!(super::rewrite_syscall_text(&mut src));
        assert_eq!(src.matches("mirvm_syscall_slot@GOTPCREL").count(), 2);
        assert_eq!(src.matches("call [r11]").count(), 2);
        assert!(src.contains("syscallx"));
        assert!(src.contains(".byte 0x0f,0x05"));
        assert!(src.contains("mirvm_syscall_slot: .quad 0"));
        let mut plain = "mov rax, 1\nsysenter\nint $0x80\n".to_string();
        assert!(!super::rewrite_syscall_text(&mut plain));
        assert!(!plain.contains("mirvm_syscall_slot"));
    }

    #[test]
    fn rewritten_syscall_stub_passthrough_and_register_discipline() {
        // SYS_getpid 裸 syscall：直通应得真 pid（非 0）；rbx 约定不被 syscall
        // 破坏，trampoline 必须同纪律保全
        let site = ir::AsmSite {
            name: "mirvm_asm_sc".into(),
            text: r#"
.globl mirvm_asm_sc
.type mirvm_asm_sc,@function
.section .text.mirvm_asm_sc,"ax",@progbits
mirvm_asm_sc:
    .intel_syntax noprefix
    mov rbx, 0x0123456789abcdef
    mov rax, 39
    syscall
    mov rcx, rbx
    mov QWORD PTR [rdi], rax
    mov QWORD PTR [rdi+8], rcx
    ret
    .att_syntax
.size mirvm_asm_sc, .-mirvm_asm_sc
.text
"#
            .to_owned(),
        };
        let addrs = materialize(&[site]);
        let mut pair = [0u64; 2];
        let stub: unsafe extern "C" fn(*mut u8) = unsafe { std::mem::transmute(addrs[0]) };
        unsafe { stub((&mut pair as *mut u64).cast()) };
        assert_ne!(pair[0], 0, "SYS_getpid 直通应得真 pid");
        assert_eq!(pair[0] as i32, unsafe { libc::getpid() });
        assert_eq!(pair[1], 0x0123_4567_89ab_cdef, "rbx 未按 syscall 纪律保全");
    }
}
