//! Materialization of `global_asm!` and naked fns: module-level and function-level asm
//! become one `.s`, then a `cc -shared` `.so` that joins `required_native_libs` and is
//! loaded RTLD_NOW before any guest dlsym. A guest referencing a symbol defined by
//! global_asm, or calling a naked fn, resolves it through a foreign dlsym.
//!
//! This follows cg_clif's `global_asm.rs` and cg_ssa's `naked_asm.rs`, using the same
//! external-assembler route. A naked fn is wrapped in its mangled symbol name with
//! `.globl`/`.type`/`.size`, a trimmed cg_ssa `prefix_and_suffix` that only handles raw
//! machine-code functions with the Rust ABI or extern C. Exotic inputs (labels, operands other
//! than const/sym, another target than this build's, `link_section`) are rejected loudly.

use crate::lower::Error;
use crate::native::artifact::asmtext::{Region, Visibility, Vocabulary};
use crate::native::artifact::assembly::assemble;

use std::fmt::Write as _;

use rustc_ast::ast::InlineAsmTemplatePiece;
use rustc_middle::mono::MonoItem;
use rustc_middle::ty::{Instance, TyCtxt, TypingEnv};

/// Collects, renders and assembles. Returns the `.so` path for `required_native_libs`, or
/// `None` when there is no asm. Failure is an `Err`: the load phase aborts loudly rather
/// than silently.
///
/// A `sym` operand pointing at an interpreted guest fn has the linker pre-budget a P1
/// executable entry address, and the `.s` header emits `.globl <name>` plus
/// `.set <name>, <addr>`, an ABS symbol. Machine-code `call`/`jmp` then land directly on
/// the entry stub materialized at startup, which trampolines back to the interpreter,
/// mirroring native link-time binding. A signature that cannot be derived (aggregate, Rust
/// ABI, variadic) is rejected loudly, because machine code calling such a fn is undefined
/// behavior anyway.
pub(crate) fn materialize<'tcx>(
    tcx: TyCtxt<'tcx>,
    linker: &mut super::Linker<'tcx>,
) -> Result<Option<Box<str>>, Error> {
    let mut asm = String::new();
    let mut abs_defs: Vec<(Box<str>, u64)> = Vec::new();
    let parts = tcx.collect_and_partition_mono_items(());
    // Stable order across CGUs; dedup a repeated def once
    let mut seen = std::collections::HashSet::new();
    let absorb = &mut |tcx: TyCtxt<'tcx>, inst: Instance<'tcx>, defs: &mut Vec<(Box<str>, u64)>| {
        absorb_guest_symfn(tcx, linker, inst, defs)
    };
    for cgu in parts.codegen_units {
        for item in cgu.items().keys() {
            match *item {
                MonoItem::GlobalAsm(item_id) => {
                    if seen.insert(format!("ga:{item_id:?}")) {
                        render_global_asm(tcx, absorb, item_id, &mut asm, &mut abs_defs)?;
                    }
                }
                MonoItem::Fn(inst) => {
                    if is_naked(tcx, inst) && seen.insert(format!("naked:{:?}", inst.def_id())) {
                        render_naked(tcx, absorb, inst, &mut asm, &mut abs_defs)?;
                    }
                }
                MonoItem::Static(_) => {}
            }
        }
    }
    if asm.trim().is_empty() {
        return Ok(None);
    }
    // Executable trampolines all go up front. The bridge bakes in only a RIP-relative
    // hidden data slot, never a runtime P1 address; each Engine loads its own copy of the
    // machine-code image or shared library and then writes its closure address into the slot.
    let fmt = Vocabulary::of(crate::os::dll::OBJECT_FORMAT);
    let mut head = String::from(crate::arch::asm_text::DIRECTIVE_INTEL);
    let mut dedup = std::collections::HashSet::new();
    let mut slots = std::collections::BTreeSet::new();
    for (name, addr) in abs_defs {
        if dedup.insert(name.clone()) {
            let slot = crate::vm::ir::native_entry_slot_name(crate::vm::ir::LinkAddr(addr));
            fmt.define_fn(&mut head, &name, Visibility::Private);
            let _ = writeln!(head, "    jmp QWORD PTR [rip + {}]", fmt.symbol(&slot));
            fmt.end_fn(&mut head, &name);
            slots.insert(slot);
        }
    }
    if !slots.is_empty() {
        fmt.open(&mut head, Region::Slots { name: "mirvm_p1" });
        head.push_str(".balign 8\n");
        for slot in slots {
            fmt.define_slot(&mut head, &slot, Visibility::Private);
        }
        fmt.close(&mut head);
    }
    let asm = format!("{head}{asm}");
    Ok(Some(assemble(&asm)?))
}

/// When a `sym` operand points at an interpreted guest fn (not foreign, not naked), the
/// linker pre-budgets a P1 entry stub and an ABS definition is recorded. An underivable
/// signature is rejected loudly. The symbol name is still written back as the placeholder
/// value, and the `.set` line makes the reference resolve to the stub's code address.
fn absorb_guest_symfn<'tcx>(
    tcx: TyCtxt<'tcx>,
    linker: &mut super::Linker<'tcx>,
    inst: Instance<'tcx>,
    abs_defs: &mut Vec<(Box<str>, u64)>,
) -> Result<(), Error> {
    if tcx.is_foreign_item(inst.def_id()) || is_naked(tcx, inst) {
        return Ok(());
    }
    if linker.entry_ffi_sig(inst).is_none() {
        return Err(Error::internal(format!(
            "global_asm/naked sym points at guest fn `{}` with an underivable signature \
             (aggregate/Rust ABI/variadic): machine code calling such a fn through a fn \
             pointer has no thunk ABI to speak of and is undefined behavior natively \
             anyway, so reject it loudly",
            tcx.symbol_name(inst).name
        )));
    }
    let addr = linker.fn_entry_addr(inst).map_err(|e| {
        Error::unsupported(format!("global_asm sym guest fn entry budget failed: {e}"))
    })?;
    abs_defs.push((tcx.symbol_name(inst).name.into(), addr));
    Ok(())
}

fn is_naked(tcx: TyCtxt<'_>, inst: Instance<'_>) -> bool {
    use rustc_middle::middle::codegen_fn_attrs::CodegenFnAttrFlags;
    tcx.codegen_fn_attrs(inst.def_id())
        .flags
        .contains(CodegenFnAttrFlags::NAKED)
}

// ===== Compile-time extraction of dependency-crate global_asm =====

/// Dependency-side sym fn handling. It shares `absorb_guest_symfn`'s early return for
/// foreign and naked fns, which need no trampoline, and rejects everything else loudly: a
/// cross-crate entry budget must run in the bin link context, which is not implemented
/// yet, and real dependency forms such as pulp use no operands.
fn dep_absorb_symfn<'tcx>(
    tcx: TyCtxt<'tcx>,
    inst: Instance<'tcx>,
    _abs_defs: &mut Vec<(Box<str>, u64)>,
) -> Result<(), Error> {
    if tcx.is_foreign_item(inst.def_id()) || is_naked(tcx, inst) {
        return Ok(());
    }
    Err(Error::DepGuestSym {
        detail: format!(
            "`{}` (the aggregate budget belongs to the bin link context and is not implemented yet)",
            tcx.symbol_name(inst).name
        ),
    })
}

/// Outcome of dependency-manifest extraction:
/// - `Text`: extraction succeeded and the text joins the manifest load;
/// - `None`: this crate has no asm (the common case);
/// - `UnsupportedSym`: some site has a `sym` pointing at the dependency's own guest fn,
///   whose cross-crate entry budget is not implemented yet. The manifest is **skipped**,
///   leaving the symbol unresolved so that any use traps loudly, because a site that may
///   never be reached must not sink the whole dependency build. wasmtime's fiber_start is
///   the concrete case: the fiber surface is not reachable from c_wasmtime_wat.
pub(crate) enum DepAsmText {
    Text(String),
    None,
    UnsupportedSym,
}

/// Compile-time extraction for a dependency: collect mono items, which always holds for
/// the crate being compiled because mirvm is that dependency crate's compiler, then render
/// every global_asm/naked site as `.s` text. The caller writes the text to a manifest
/// sidecar next to the rlib (`<rlib stem>.mirasm.s`); assembly is left to the same
/// `assemble` channel used by the bin load phase, which makes cache self-healing free.
pub(crate) fn materialize_dep_text(tcx: TyCtxt<'_>) -> Result<DepAsmText, Error> {
    let mut asm = String::new();
    let mut abs_defs: Vec<(Box<str>, u64)> = Vec::new();
    let parts = tcx.collect_and_partition_mono_items(());
    let mut seen = std::collections::HashSet::new();
    for cgu in parts.codegen_units {
        for item in cgu.items().keys() {
            match *item {
                MonoItem::GlobalAsm(item_id) => {
                    if seen.insert(format!("ga:{item_id:?}")) {
                        match render_global_asm(
                            tcx,
                            &mut dep_absorb_symfn,
                            item_id,
                            &mut asm,
                            &mut abs_defs,
                        ) {
                            Ok(()) => {}
                            Err(Error::DepGuestSym { .. }) => {
                                return Ok(DepAsmText::UnsupportedSym);
                            }
                            Err(e) => return Err(e),
                        }
                    }
                }
                MonoItem::Fn(inst) => {
                    if is_naked(tcx, inst) && seen.insert(format!("naked:{:?}", inst.def_id())) {
                        match render_naked(
                            tcx,
                            &mut dep_absorb_symfn,
                            inst,
                            &mut asm,
                            &mut abs_defs,
                        ) {
                            Ok(()) => {}
                            Err(Error::DepGuestSym { .. }) => {
                                return Ok(DepAsmText::UnsupportedSym);
                            }
                            Err(e) => return Err(e),
                        }
                    }
                }
                MonoItem::Static(_) => {}
            }
        }
    }
    if asm.trim().is_empty() {
        return Ok(DepAsmText::None);
    }
    Ok(DepAsmText::Text(format!(
        "{}{asm}",
        crate::arch::asm_text::DIRECTIVE_INTEL
    )))
}

/// Whether the compiler is emitting asm for the CPU this build is for.
///
/// A guest is compiled for this host, so the only architecture that can arrive here is the one
/// `src/arch/` implements. That architecture names itself, and rustc's `InlineAsmArch` is its
/// spelling of the same fact — the comparison is here rather than there because the vocabulary is
/// rustc's, which `src/arch/` does not name. A rustc that spells its own architecture differently
/// is refused loudly rather than passed through.
fn ensure_host_arch(tcx: TyCtxt<'_>) -> Result<(), Error> {
    let host = crate::arch::asm_text::NAME;
    let emitted = tcx
        .sess
        .asm_arch
        .map(|arch| format!("{arch:?}").to_lowercase());
    if emitted.as_deref() == Some(host) {
        return Ok(());
    }
    Err(Error::unsupported(format!(
        "global_asm/naked supports the CPU this build is ({host}) only, and the compiler is \
         emitting asm for {:?}",
        tcx.sess.asm_arch
    )))
}

/// Handler for a `sym fn` operand, parameterized per side: the bin side uses
/// `absorb_guest_symfn` (P1 entry budget plus an ABS trampoline), the dependency side uses
/// `dep_absorb_symfn` (cross-crate budget not wired up, so it rejects loudly). This arm is
/// the only place either renderer couples to `Linker`.
type SymFnAbsorb<'tcx, 'c> =
    dyn FnMut(TyCtxt<'tcx>, Instance<'tcx>, &mut Vec<(Box<str>, u64)>) -> Result<(), Error> + 'c;

fn render_global_asm<'tcx>(
    tcx: TyCtxt<'tcx>,
    absorb: &mut SymFnAbsorb<'tcx, '_>,
    item_id: rustc_hir::ItemId,
    out: &mut String,
    abs_defs: &mut Vec<(Box<str>, u64)>,
) -> Result<(), Error> {
    use rustc_ast::InlineAsmOptions;
    use rustc_hir::{InlineAsmOperand, ItemKind};
    ensure_host_arch(tcx)?;
    let item = tcx.hir_item(item_id);
    let ItemKind::GlobalAsm { asm, .. } = item.kind else {
        return Err(Error::internal("GlobalAsm item has an unexpected shape"));
    };
    let att = asm.options.contains(InlineAsmOptions::ATT_SYNTAX);
    out.push_str(&crate::arch::asm_text::syntax_prefix(att));
    let owner = item_id.owner_id;
    for piece in asm.template {
        match piece {
            InlineAsmTemplatePiece::String(s) => out.push_str(s),
            InlineAsmTemplatePiece::Placeholder { operand_idx, .. } => {
                let (op, sp) = &asm.operands[*operand_idx];
                match op {
                    InlineAsmOperand::Const { anon_const } => {
                        let cv =
                            tcx.const_eval_poly(anon_const.def_id.to_def_id())
                                .map_err(|e| {
                                    Error::internal(format!(
                                        "global_asm const evaluation failed: {e:?}"
                                    ))
                                })?;
                        let ty = tcx
                            .typeck_body(anon_const.body)
                            .node_type(anon_const.hir_id);
                        let layout = tcx
                            .layout_of(TypingEnv::fully_monomorphized().as_query_input(ty))
                            .map_err(|e| {
                                Error::internal(format!("global_asm const layout: {e:?}"))
                            })?;
                        out.push_str(&rustc_codegen_ssa::common::asm_const_to_str(
                            tcx, *sp, cv, layout,
                        ));
                    }
                    InlineAsmOperand::SymFn { expr } => {
                        let ty = tcx.typeck(owner.def_id).expr_ty(expr);
                        let rustc_middle::ty::TyKind::FnDef(def_id, args) = ty.kind() else {
                            return Err(Error::internal(format!(
                                "global_asm sym fn is not a FnDef ({ty})"
                            )));
                        };
                        let inst = Instance::expect_resolve(
                            tcx,
                            TypingEnv::fully_monomorphized(),
                            *def_id,
                            args,
                            rustc_span::DUMMY_SP,
                        );
                        absorb(tcx, inst, abs_defs)?;
                        out.push_str(tcx.symbol_name(inst).name);
                    }
                    InlineAsmOperand::SymStatic { path: _, def_id } => {
                        out.push_str(tcx.symbol_name(Instance::mono(tcx, *def_id)).name);
                    }
                    _ => {
                        return Err(Error::unsupported(
                            "global_asm supports only const/sym operands",
                        ));
                    }
                }
            }
        }
    }
    out.push('\n');
    Ok(())
}

fn render_naked<'tcx>(
    tcx: TyCtxt<'tcx>,
    absorb: &mut SymFnAbsorb<'tcx, '_>,
    inst: Instance<'tcx>,
    out: &mut String,
    abs_defs: &mut Vec<(Box<str>, u64)>,
) -> Result<(), Error> {
    use rustc_ast::InlineAsmOptions;
    use rustc_middle::mir::{InlineAsmOperand, START_BLOCK, TerminatorKind};
    ensure_host_arch(tcx)?;
    let attrs = tcx.codegen_fn_attrs(inst.def_id());
    if attrs.link_section.is_some() {
        return Err(Error::internal("naked fn has a link_section"));
    }
    let mir = tcx.instance_mir(inst.def);
    let TerminatorKind::InlineAsm {
        template,
        operands,
        options,
        ..
    } = &mir.basic_blocks[START_BLOCK].terminator().kind
    else {
        return Err(Error::internal("naked fn body is not a single InlineAsm"));
    };
    let att = options.contains(InlineAsmOptions::ATT_SYNTAX);
    let fmt = Vocabulary::of(crate::os::dll::OBJECT_FORMAT);
    let name = tcx.symbol_name(inst).name;
    // A trimmed cg_ssa prefix_and_suffix: a raw machine-code function
    out.push_str(&crate::arch::asm_text::syntax_prefix(att));
    fmt.open(out, Region::Text { name });
    out.push_str(".balign 16\n");
    fmt.define_fn(out, name, Visibility::Exported);
    for piece in template.iter() {
        match piece {
            InlineAsmTemplatePiece::String(s) => out.push_str(s),
            InlineAsmTemplatePiece::Placeholder {
                operand_idx, span, ..
            } => match &operands[*operand_idx] {
                InlineAsmOperand::Const { value } => {
                    let cv = value
                        .const_
                        .eval(tcx, TypingEnv::fully_monomorphized(), value.span)
                        .map_err(|e| {
                            Error::internal(format!("naked const evaluation failed: {e:?}"))
                        })?;
                    let layout = tcx
                        .layout_of(
                            TypingEnv::fully_monomorphized().as_query_input(value.const_.ty()),
                        )
                        .map_err(|e| Error::internal(format!("naked const layout: {e:?}")))?;
                    out.push_str(&rustc_codegen_ssa::common::asm_const_to_str(
                        tcx, *span, cv, layout,
                    ));
                }
                InlineAsmOperand::SymFn { value } => {
                    let rustc_middle::ty::TyKind::FnDef(def_id, args) = value.const_.ty().kind()
                    else {
                        return Err(Error::internal("naked sym fn is not a FnDef"));
                    };
                    let callee = Instance::expect_resolve(
                        tcx,
                        TypingEnv::fully_monomorphized(),
                        *def_id,
                        args,
                        rustc_span::DUMMY_SP,
                    );
                    absorb(tcx, callee, abs_defs)?;
                    out.push_str(&fmt.symbol(tcx.symbol_name(callee).name));
                }
                InlineAsmOperand::SymStatic { def_id } => {
                    out.push_str(&fmt.symbol(tcx.symbol_name(Instance::mono(tcx, *def_id)).name));
                }
                _ => {
                    return Err(Error::unsupported(
                        "naked asm supports only const/sym operands",
                    ));
                }
            },
        }
    }
    out.push('\n');
    fmt.end_fn(out, name);
    fmt.close(out);
    Ok(())
}
