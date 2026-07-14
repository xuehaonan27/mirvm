//! `global_asm!` 与 naked fn 的物化（M5.2 D8h）：模块级/函数级 asm → 单个 `.s` →
//! `cc -shared` → `.so` → 加入 required_native_libs（在任何 guest dlsym 前 RTLD_NOW
//! 加载）。guest 引用 global_asm 定义的符号 / 调用 naked fn 都经 foreign dlsym 命中。
//!
//! cg_clif `global_asm.rs` + cg_ssa `naked_asm.rs` 同构（外部汇编器路线一致）。naked
//! fn 用 mangled 符号名 + `.globl`/`.type`/`.size` 包装（cg_ssa prefix_and_suffix 精简
//! 版：只做 ELF/x86_64、Rust ABI/extern-C 的裸机器码函数）。exotic 面（Label、非
//! Const/Sym 操作数、非 x86_64、link_section）响亮拒绝。

use std::fmt::Write as _;

use rustc_ast::ast::InlineAsmTemplatePiece;
use rustc_middle::mono::MonoItem;
use rustc_middle::ty::{Instance, TyCtxt, TypingEnv};

/// 收集 + 渲染 + 汇编。返回 `.so` 路径（供 required_native_libs），无 asm 时 None。
/// 失败 = Err（加载相响亮终止，不静默）。
pub(crate) fn materialize(tcx: TyCtxt<'_>) -> Result<Option<Box<str>>, String> {
    let mut asm = String::new();
    let parts = tcx.collect_and_partition_mono_items(());
    // 稳定序（跨 CGU）；重复 def 去一次
    let mut seen = std::collections::HashSet::new();
    for cgu in parts.codegen_units {
        for item in cgu.items().keys() {
            match *item {
                MonoItem::GlobalAsm(item_id) => {
                    if seen.insert(format!("ga:{item_id:?}")) {
                        render_global_asm(tcx, item_id, &mut asm)?;
                    }
                }
                MonoItem::Fn(inst) => {
                    if is_naked(tcx, inst) && seen.insert(format!("naked:{:?}", inst.def_id())) {
                        render_naked(tcx, inst, &mut asm)?;
                    }
                }
                MonoItem::Static(_) => {}
            }
        }
    }
    if asm.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(assemble(&asm)?))
}

fn is_naked(tcx: TyCtxt<'_>, inst: Instance<'_>) -> bool {
    use rustc_middle::middle::codegen_fn_attrs::CodegenFnAttrFlags;
    tcx.codegen_fn_attrs(inst.def_id())
        .flags
        .contains(CodegenFnAttrFlags::NAKED)
}

/// x86_64/Intel 语法头（asm 站点间独立；每站点自带 syntax 指令，避免相互污染）。
fn syntax_prefix(att: bool) -> &'static str {
    if att {
        "\n.att_syntax\n"
    } else {
        "\n.intel_syntax noprefix\n"
    }
}

fn ensure_x86(tcx: TyCtxt<'_>) -> Result<(), String> {
    use rustc_target::asm::InlineAsmArch;
    match tcx.sess.asm_arch {
        Some(InlineAsmArch::X86_64) => Ok(()),
        other => Err(format!(
            "global_asm/naked 仅支持 x86_64（arch={other:?}，D8l）"
        )),
    }
}

fn render_global_asm(
    tcx: TyCtxt<'_>,
    item_id: rustc_hir::ItemId,
    out: &mut String,
) -> Result<(), String> {
    use rustc_ast::InlineAsmOptions;
    use rustc_hir::{InlineAsmOperand, ItemKind};
    ensure_x86(tcx)?;
    let item = tcx.hir_item(item_id);
    let ItemKind::GlobalAsm { asm, .. } = item.kind else {
        return Err("GlobalAsm item 形态异常".into());
    };
    let att = asm.options.contains(InlineAsmOptions::ATT_SYNTAX);
    out.push_str(syntax_prefix(att));
    let owner = item_id.owner_id;
    for piece in asm.template {
        match piece {
            InlineAsmTemplatePiece::String(s) => out.push_str(s),
            InlineAsmTemplatePiece::Placeholder { operand_idx, .. } => {
                let (op, sp) = &asm.operands[*operand_idx];
                match op {
                    InlineAsmOperand::Const { anon_const } => {
                        let cv = tcx
                            .const_eval_poly(anon_const.def_id.to_def_id())
                            .map_err(|e| format!("global_asm const 求值失败: {e:?}"))?;
                        let ty = tcx
                            .typeck_body(anon_const.body)
                            .node_type(anon_const.hir_id);
                        let layout = tcx
                            .layout_of(TypingEnv::fully_monomorphized().as_query_input(ty))
                            .map_err(|e| format!("global_asm const layout: {e:?}"))?;
                        out.push_str(&rustc_codegen_ssa::common::asm_const_to_str(
                            tcx, *sp, cv, layout,
                        ));
                    }
                    InlineAsmOperand::SymFn { expr } => {
                        let ty = tcx.typeck(owner.def_id).expr_ty(expr);
                        let rustc_middle::ty::TyKind::FnDef(def_id, args) = ty.kind() else {
                            return Err(format!("global_asm sym fn 非 FnDef（{ty}）"));
                        };
                        let inst = Instance::expect_resolve(
                            tcx,
                            TypingEnv::fully_monomorphized(),
                            *def_id,
                            args,
                            rustc_span::DUMMY_SP,
                        );
                        out.push_str(tcx.symbol_name(inst).name);
                    }
                    InlineAsmOperand::SymStatic { path: _, def_id } => {
                        out.push_str(tcx.symbol_name(Instance::mono(tcx, *def_id)).name);
                    }
                    _ => return Err("global_asm 仅支持 const/sym 操作数（D8l）".into()),
                }
            }
        }
    }
    out.push('\n');
    Ok(())
}

fn render_naked<'tcx>(
    tcx: TyCtxt<'tcx>,
    inst: Instance<'tcx>,
    out: &mut String,
) -> Result<(), String> {
    use rustc_ast::InlineAsmOptions;
    use rustc_middle::mir::{InlineAsmOperand, START_BLOCK, TerminatorKind};
    ensure_x86(tcx)?;
    let attrs = tcx.codegen_fn_attrs(inst.def_id());
    if attrs.link_section.is_some() {
        return Err("naked fn 带 link_section（D8l）".into());
    }
    let mir = tcx.instance_mir(inst.def);
    let TerminatorKind::InlineAsm {
        template,
        operands,
        options,
        ..
    } = &mir.basic_blocks[START_BLOCK].terminator().kind
    else {
        return Err("naked fn body 非单 InlineAsm（D8l）".into());
    };
    let att = options.contains(InlineAsmOptions::ATT_SYNTAX);
    let name = tcx.symbol_name(inst).name;
    // cg_ssa prefix_and_suffix 精简：ELF/x86_64 裸机器码函数
    out.push_str(syntax_prefix(att));
    let _ = writeln!(out, ".pushsection .text.{name},\"ax\", @progbits");
    let _ = writeln!(out, ".balign 16");
    let _ = writeln!(out, ".globl {name}");
    let _ = writeln!(out, ".type {name}, @function");
    let _ = writeln!(out, "{name}:");
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
                        .map_err(|e| format!("naked const 求值失败: {e:?}"))?;
                    let layout = tcx
                        .layout_of(
                            TypingEnv::fully_monomorphized().as_query_input(value.const_.ty()),
                        )
                        .map_err(|e| format!("naked const layout: {e:?}"))?;
                    out.push_str(&rustc_codegen_ssa::common::asm_const_to_str(
                        tcx, *span, cv, layout,
                    ));
                }
                InlineAsmOperand::SymFn { value } => {
                    let rustc_middle::ty::TyKind::FnDef(def_id, args) = value.const_.ty().kind()
                    else {
                        return Err("naked sym fn 非 FnDef".into());
                    };
                    let callee = Instance::expect_resolve(
                        tcx,
                        TypingEnv::fully_monomorphized(),
                        *def_id,
                        args,
                        rustc_span::DUMMY_SP,
                    );
                    out.push_str(tcx.symbol_name(callee).name);
                }
                InlineAsmOperand::SymStatic { def_id } => {
                    out.push_str(tcx.symbol_name(Instance::mono(tcx, *def_id)).name);
                }
                _ => return Err("naked asm 仅支持 const/sym 操作数（D8l）".into()),
            },
        }
    }
    out.push('\n');
    let _ = writeln!(out, ".size {name}, . - {name}");
    let _ = writeln!(out, ".popsection");
    Ok(())
}

/// `.so` 里第一个未定义的、名字像 Rust mangled（`_R`/`_ZN`）的符号——guest fn 引用
/// 的信号。libc/系统符号（动态链接会解析）不算。用 `nm -D -u`。
fn undefined_nonlib_symbols(so: &std::path::Path) -> Option<String> {
    let out = std::process::Command::new("nm")
        .args(["-D", "-u"])
        .arg(so)
        .output()
        .ok()?;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let sym = line.rsplit(char::is_whitespace).next().unwrap_or("");
        if sym.starts_with("_R") || sym.starts_with("_ZN") {
            return Some(sym.to_owned());
        }
    }
    None
}

/// `.s` → `.so`（内容寻址缓存，与 asm-stub 工厂同款临时名+rename 原子发布）。
/// naked fn 混入模块级 asm，可能引用 guest 符号 → 不能 `-nostdlib`；用 `-nostartfiles`
/// 保留动态链接器解析（naked 内 sym 操作数指向的 guest fn 由 RTLD_GLOBAL 兜底）。
fn assemble(asm: &str) -> Result<Box<str>, String> {
    let hash = crate::lower::asm::fnv1a(asm.as_bytes());
    let dir = crate::sysroot::cache_dir().join("global-asm");
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建 global-asm 缓存目录失败: {e}"))?;
    let so = dir.join(format!("{hash:016x}.so"));
    if so.exists() {
        return Ok(so.display().to_string().into());
    }
    let s_path = dir.join(format!("{hash:016x}.s"));
    std::fs::write(&s_path, asm).map_err(|e| format!("写 global-asm .s 失败: {e}"))?;
    let tmp = dir.join(format!("{hash:016x}.so.tmp{}", std::process::id()));
    let status = std::process::Command::new("cc")
        .args(["-shared", "-fPIC", "-nostartfiles", "-o"])
        .arg(&tmp)
        .arg(&s_path)
        .status()
        .map_err(|e| format!("调用 cc 汇编 global-asm 失败（PATH 缺 cc？）: {e}"))?;
    if !status.success() {
        return Err(format!("cc 汇编 global-asm 失败（status={status}）"));
    }
    // 未定义符号审计（D8h 边界）：global_asm/naked 里 `sym` 引用的符号必须自身是
    // 机器码（另一 naked/global_asm 符号或动态库导出）；引用**解释执行的 guest
    // fn** 无机器码入口——.so 会留下未解析符号，dlopen 时才炸且诊断误导。此处提前
    // 响亮拒绝，指明这是 JIT 期能力（从机器码 jmp 进解释器需 per-fn trampoline）。
    let undef = undefined_nonlib_symbols(&tmp);
    if let Some(sym) = undef {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!(
            "global_asm/naked 引用未定义符号 `{sym}`：sym 操作数只能指向机器码符号\
             （另一 naked/global_asm 或动态库），不能指向解释执行的 guest fn（JIT 期能力，D8l）"
        ));
    }
    std::fs::rename(&tmp, &so).map_err(|e| format!("global-asm .so 原子发布失败: {e}"))?;
    Ok(so.display().to_string().into())
}
