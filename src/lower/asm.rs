//! Inline-asm site -> GAS wrapper text generation.
//!
//! This follows cg_clif's `rustc_codegen_cranelift/src/inline_asm.rs` closely. Cranelift
//! has no asm support, so cg_clif does its **own register allocation** and renders a
//! `fn(*mut u8)` GAS wrapper around a single pointer to a slot buffer: save clobbers, load
//! input registers from the buffer, run the asm template body, store output registers back,
//! restore clobbers, return. An external assembler (the `cc` step) then assembles it into
//! machine code.
//!
//! Load-bearing wrapper ABI invariants:
//! - `rbx` holds the buffer base (`push rbx; mov rbx,rdi`); every slot access is
//!   `[rbx+off]`.
//! - rustc **reserves rbx** as the LLVM base register and never allocates it to a `reg`
//!   operand, so the buffer base stays safe while the asm body runs. The cpuid idiom
//!   `mov {0:r},rbx; cpuid; xchg {0:r},rbx` saves and restores rbx around cpuid itself
//!   (cpuid writes ebx), which is exactly what makes this wrapper scheme work for cpuid.
//! - `.intel_syntax noprefix` wraps the body, restored by `.att_syntax` at the end.
//!
//! Only x86_64 is supported; the `func.rs` boundary already rejects non-x86_64, naked,
//! may_unwind, sym, label and const operands.

use std::fmt::Write as _;

use rustc_abi::{Align, Size};
use rustc_ast::ast::InlineAsmTemplatePiece;
use rustc_hir::def_id::DefId;
use rustc_middle::ty::TyCtxt;
use rustc_span::sym;
use rustc_target::asm::{
    InlineAsmArch, InlineAsmClobberAbi, InlineAsmReg, InlineAsmRegClass, InlineAsmRegOrRegClass,
    X86InlineAsmRegClass, allocatable_registers,
};

/// Constraint of one asm operand for wrapper generation. The value and destination are
/// carried separately by `func.rs` and never enter this module. `late` selects the class
/// operand's allocation phase: a non-late output is allocated first and is exclusive with
/// inputs, while a late output is allocated afterwards and may share a register with an
/// input. This matches cg_clif's `allocate_registers` semantics.
pub(crate) enum AsmOperand {
    In {
        reg: InlineAsmRegOrRegClass,
    },
    Out {
        reg: InlineAsmRegOrRegClass,
        late: bool,
        has_place: bool,
    },
    // An inout has no late distinction: it always allocates in phase 0, matching cg_clif's
    // `_late`, hence no field here.
    InOut {
        reg: InlineAsmRegOrRegClass,
        has_out_place: bool,
    },
    /// A const/sym operand: it has no register, its value or symbol name is rendered as
    /// literal text during lowering, and the placeholder expands to that text directly, as
    /// cg_clif also formats consts into the template. Register allocation skips it.
    Inline {
        text: String,
    },
}

/// Output of wrapper generation: the text, the buffer size, and each operand's input and
/// output slot offset. `func.rs` pairs those offsets with the lowered values and
/// destinations. The buffer offsets share one source with the wrapper's `[rbx+off]` accesses,
/// which is the basis of correctness.
pub(crate) struct GeneratedAsm {
    pub text: String,
    pub buf_size: u32,
    pub input_slot: Vec<Option<u32>>,
    pub output_slot: Vec<Option<u32>>,
}

/// Lowering entry point for an asm site, following cg_clif's `codegen_inline_asm_inner`.
/// `name` is the stub's globally unique symbol, used by dlsym.
pub(crate) fn generate<'tcx>(
    tcx: TyCtxt<'tcx>,
    enclosing_def_id: DefId,
    arch: InlineAsmArch,
    template: &[InlineAsmTemplatePiece],
    operands: &[AsmOperand],
    name: &str,
) -> GeneratedAsm {
    // The syntax and divergence options (noreturn, may_unwind, att_syntax) are handled at the
    // func.rs boundary, so this only generates a non-divergent Intel-syntax wrapper.
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
    /// **Conservative variant** of cg_clif's `allocate_registers`: explicit registers enter
    /// the allocated set, then out/inout classes allocate (they are more constrained), then
    /// in/lateout classes. One known divergence from cg_clif: cg_clif tests conflicts per
    /// (in-use, out-use) bit and so allows an in and a lateout to share a register, while this
    /// treats any `contains_key` as a conflict and shares nothing. The result is always a
    /// legal subset, at the cost of possibly failing to allocate for extremely dense operands,
    /// which panics into a Trap diagnostic rather than silently miscompiling.
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

        // Explicit registers enter the allocated set
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

        // Class operands allocate in two phases, in cg_clif's order: phase 0 allocates
        // non-late outs and inouts, which are more constrained and exclusive with inputs;
        // phase 1 allocates ins and lateouts, which may share registers other than phase 0's.
        // `late` selects which phase an Out lands in.
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
        panic!("inline asm: cannot allocate a register for register class {class:?}")
    }

    /// Follows cg_clif's `allocate_stack_slots`: clobbers that the C clobber ABI does not
    /// already cover, then inouts, which share one input/output slot, then inputs, then
    /// outputs. Output slots reuse the input range to save memory.
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

        // Clobber slots hold registers the wrapper must preserve but the asm would destroy.
        // Registers the C clobber ABI already covers need no save, since the calling
        // convention permits clobbering them.
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

        // inout: one slot shared by input and output
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
        // inputs
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
        // Outputs reuse the input range to save memory, as cg_clif does.
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

    /// Follows cg_clif's `generate_asm_wrapper`: the x86_64 branch with Intel syntax.
    fn generate_asm_wrapper(&self, name: &str) -> String {
        let fmt = crate::native::artifact::asmtext::Vocabulary::of(crate::os::dll::OBJECT_FORMAT);
        let mut s = String::new();
        fmt.open(
            &mut s,
            crate::native::artifact::asmtext::Region::Text { name },
        );
        fmt.define_fn(
            &mut s,
            name,
            crate::native::artifact::asmtext::Visibility::Exported,
        );
        s.push_str(crate::arch::asm_text::DIRECTIVE_INTEL);

        Self::prologue(&mut s);

        // Save clobber registers
        for (reg, slot) in self.iter_reg_slot(&self.slots_clobber) {
            Self::save_register(&mut s, reg, slot);
        }
        // Load input registers
        for (reg, slot) in self.iter_reg_slot(&self.slots_input) {
            Self::restore_register(&mut s, reg, slot);
        }

        // The asm template body
        for piece in self.template {
            match piece {
                InlineAsmTemplatePiece::String(text) => s.push_str(text),
                InlineAsmTemplatePiece::Placeholder {
                    operand_idx,
                    modifier,
                    ..
                } => {
                    // const/sym expands to its literal text; everything else to the allocated
                    // register name
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

        // Store output registers, restore clobbers, then finish
        for (reg, slot) in self.iter_reg_slot(&self.slots_output) {
            Self::save_register(&mut s, reg, slot);
        }
        for (reg, slot) in self.iter_reg_slot(&self.slots_clobber) {
            Self::restore_register(&mut s, reg, slot);
        }
        Self::epilogue(&mut s);

        s.push_str(crate::arch::asm_text::DIRECTIVE_ATT);
        fmt.end_fn(&mut s, name);
        fmt.close(&mut s);
        s.push_str("\n\n");
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

    /// Placeholder -> register name in Intel syntax, with cg_clif's special naming for xmm,
    /// ymm and zmm.
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
        // rbx is callee-saved and reserved by LLVM as the base register
        s.push_str("    push rbx\n");
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
