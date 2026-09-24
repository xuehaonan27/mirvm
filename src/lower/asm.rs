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

use crate::lower::Error;

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

use crate::utils::content::fnv1a;
use crate::vm::ir;

// ===== syscall interception =====
//
// A bare `syscall` instruction in guest inline asm or global_asm is rewritten at the text
// level, since mirvm already holds all the GAS text at its generation point and never has to
// scan a binary. The rewrite turns `syscall` into an indirect-slot
// `call QWORD PTR [rip+mirvm_syscall_slot]`. The slot ships with the `.so` and is refilled
// after dlopen with the real address of the `arch::asmstub` trampoline, under the
// same discipline as the startup-phase refill. The trampoline preserves the full syscall
// contract (integer, flags, xmm, mxcsr) and lands in `os::process::mirvm_syscall_dispatch`,
// which passes through and traces.
//
// Not covered, and none of these forms is known to occur in practice: `sysenter`,
// `int $0x80`, `.byte 0x0f,0x05` written to evade the rewrite, and multiple statements or
// label prefixes on one line. Supporting them needs a concrete crate that hits them.

/// Line-level mnemonic match with leading whitespace already stripped: after `syscall` only
/// end of line, whitespace or a comment (`#` or `;`) is allowed. Multi-statement lines and
/// label prefixes are not covered.
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

/// Rewrites every `syscall` instruction line in `src` into an indirect slot call. Returns
/// whether anything was rewritten, which decides whether the slot definition is appended and
/// whether the slot is refilled after dlopen.
pub(crate) fn rewrite_syscall_text(src: &mut String) -> bool {
    let mut hit = false;
    let mut out = String::with_capacity(src.len() + 64);
    for line in src.split_inclusive('\n') {
        if is_syscall_insn_line(line.trim_start()) {
            out.push_str(crate::os_arch::syscall_asm::CALL);
            hit = true;
        } else {
            out.push_str(line);
        }
    }
    *src = out;
    if hit {
        src.push_str(crate::os_arch::syscall_asm::SLOT_DEF);
    }
    hit
}

/// Refills the syscall indirect slot after dlopen. A system library has no such symbol, so an
/// absent slot is skipped silently.
pub(crate) fn refill_syscall_slot(handle: usize) {
    let slot = crate::os::dll::sym(handle, c"mirvm_syscall_slot");
    if slot != 0 {
        unsafe {
            *(slot as *mut u64) = crate::arch::asmstub::syscall_trampoline_addr();
        }
    }
}

/// Materializes asm stubs in one batch: concatenate every wrapper text into one `.s`,
/// assemble it to a `.so` with `cc`, `dlopen` it, then `dlsym` each site by its own symbol
/// name to build the address table (bit position -> u64). Names are decoupled from bit
/// positions because the final order is only known at the end, while the name is already
/// baked into the text during lowering.
///
/// The content hash addresses the cache at `~/.cache/mirvm/asm-stubs/<hash>.so`, so a warm
/// cache makes no `cc` call at all. The `dlopen` handle is intentionally leaked: it stays
/// resident for the process lifetime, which keeps the code addresses valid. Materialization
/// happens in the load phase, where OS interaction is allowed; the returned u64 table moves
/// to the `Module` and the execution phase only reads and calls directly, preserving purity.
/// An empty site set yields an empty table at zero cost.
pub(crate) fn materialize(sites: &[ir::AsmSite]) -> Vec<u64> {
    try_materialize(sites)
        .unwrap_or_else(|error| panic!("asm-stub materialization failed: {error}"))
}

/// Fallible counterpart of [`materialize`]: reports an assembly, caching or symbol failure as
/// an `Err` instead of panicking.
pub(crate) fn try_materialize(sites: &[ir::AsmSite]) -> Result<Vec<u64>, Error> {
    if sites.is_empty() {
        return Ok(Vec::new());
    }
    let mut src = String::new();
    src.push_str("# Generated by the mirvm asm-stub factory -- do not edit by hand\n");
    for site in sites {
        src.push_str(&site.text);
    }
    // Bare `syscall` instructions become indirect slot calls. The rewrite runs before content
    // hashing, so the cache key matches the final bytes.
    rewrite_syscall_text(&mut src);

    // FNV-1a content hash: stable, so runs reuse the same cache key
    let h = fnv1a(src.as_bytes());

    let dir = crate::store::ASM_STUBS.dir();
    std::fs::create_dir_all(&dir).map_err(|e| {
        Error::io(
            format!(
                "failed to create the asm-stub cache directory `{}`",
                dir.display()
            ),
            e,
        )
    })?;
    let so: PathBuf = dir.join(format!("{h:016x}.so"));

    if !so.exists() {
        // -shared -fPIC keeps the wrapper self-contained with no external symbols, so dlsym
        // yields each site's real address after dlopen. The assembly source takes the staging
        // name of the product, so concurrent instantiation in one process cannot truncate
        // another's file.
        let tmp = crate::store::staging_path(&so);
        let mut s_path = tmp.clone().into_os_string();
        s_path.push(".s");
        let s_path = PathBuf::from(s_path);
        std::fs::write(&s_path, &src).map_err(|e| {
            Error::io(
                format!(
                    "failed to write the asm-stub temporary assembly `{}`",
                    s_path.display()
                ),
                e,
            )
        })?;
        let output = std::process::Command::new("cc")
            .args(crate::os::linker::COMMON)
            .args(crate::os::linker::ASM_STUB)
            .arg("-o")
            .arg(&tmp)
            .arg(&s_path)
            .output()
            .map_err(|e| {
                Error::assemble(format!(
                    "failed to invoke cc to assemble the asm-stub (is cc missing from PATH?): {e}"
                ))
            })?;
        let _ = std::fs::remove_file(&s_path);
        if !output.status.success() {
            let _ = std::fs::remove_file(&tmp);
            return Err(Error::assemble(format!(
                "cc failed to assemble the asm-stub (status={}):\n{}{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        crate::store::publish(&so, &tmp).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            Error::io(
                format!(
                    "failed to atomically publish the asm-stub .so `{}`",
                    so.display()
                ),
                e,
            )
        })?;
    }

    let c_so = std::ffi::CString::new(so.as_os_str().as_encoded_bytes()).map_err(|_| {
        Error::internal(format!(
            "asm-stub path contains a NUL byte: {}",
            so.display()
        ))
    })?;
    let handle = crate::os::dll::open_with_flags(
        &c_so,
        crate::os::dll::RTLD_NOW | crate::os::dll::RTLD_LOCAL,
    )
    .map_err(|error| {
        Error::assemble(format!(
            "cannot dlopen the asm-stub .so `{}`: {error}",
            so.display()
        ))
    })?;
    refill_syscall_slot(handle);

    sites
        .iter()
        .map(|site| -> Result<u64, Error> {
            let name = std::ffi::CString::new(&*site.name).map_err(|_| {
                Error::internal(format!(
                    "asm-stub symbol name contains a NUL byte: {:?}",
                    site.name
                ))
            })?;
            let addr = crate::os::dll::sym(handle, &name);
            if addr == 0 {
                return Err(Error::assemble(format!(
                    "failed to dlsym asm-stub `{}`",
                    site.name
                )));
            }
            Ok(addr as u64)
        })
        .collect()
}

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
        let fmt = crate::native::asmtext::Vocabulary::of(crate::os::dll::OBJECT_FORMAT);
        let mut s = String::new();
        fmt.open(&mut s, crate::native::asmtext::Region::Text { name });
        fmt.define_fn(&mut s, name, crate::native::asmtext::Visibility::Exported);
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

#[cfg(all(test, target_arch = "x86_64"))]
mod tests {
    use super::{materialize, try_materialize};
    use crate::vm::ir;

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
    fn concurrent_materialization_of_one_hash_is_safe() {
        let name = format!("mirvm_asm_parallel_{}", std::process::id());
        let site = ir::AsmSite {
            name: name.clone().into(),
            text: format!(
                ".globl {name}\n.type {name},@function\n{name}:\n    ret\n.size {name}, .-{name}\n"
            ),
        };
        let threads = (0..8)
            .map(|_| {
                let site = site.clone();
                std::thread::spawn(move || try_materialize(&[site]))
            })
            .collect::<Vec<_>>();
        let addrs = threads
            .into_iter()
            .map(|thread| thread.join().unwrap().unwrap()[0])
            .collect::<Vec<_>>();
        assert!(addrs.iter().all(|addr| *addr == addrs[0]));
    }

    #[test]
    fn rewrite_syscall_text_hits_only_mnemonic_lines() {
        let mut src =
            "mov rax, 1\n    syscall\nsyscallx\n.byte 0x0f,0x05\n  syscall # c\nnop\n".to_string();
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
        // A bare SYS_getpid syscall must pass through and return the real pid (non-zero). The
        // syscall leaves rbx intact, and the trampoline must preserve it the same way.
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
        assert_ne!(
            pair[0], 0,
            "SYS_getpid passthrough must return the real pid"
        );
        assert_eq!(pair[0] as i32, unsafe { libc::getpid() });
        assert_eq!(
            pair[1], 0x0123_4567_89ab_cdef,
            "rbx was not preserved under syscall discipline"
        );
    }
}
