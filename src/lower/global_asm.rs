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
///
/// `sym` 指向解释态 guest fn（C7，2026-07-18）：经 linker 预算 P1 可执行条目地址，
/// 在 .s 头部发射 `.globl <名> + .set <名>, <址>`（ABS 符号）——机器码 call/jmp
/// 直接落到启动相物化的条目 stub，蹦床回解释器（native 链接期绑定同构）。签名不可
/// 派生（聚合/Rust ABI/变参）者响亮拒绝：机器码调此类 fn 本即 UB（残余边界）。
pub(crate) fn materialize<'tcx>(
    tcx: TyCtxt<'tcx>,
    linker: &mut super::Linker<'tcx>,
) -> Result<Option<Box<str>>, String> {
    let mut asm = String::new();
    let mut abs_defs: Vec<(Box<str>, u64)> = Vec::new();
    let parts = tcx.collect_and_partition_mono_items(());
    // 稳定序（跨 CGU）；重复 def 去一次
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
    // 可执行跳板统一前置。桥只烤入 RIP 相对的隐藏数据槽，不烤运行期 P1 地址；
    // 每个 Engine 装载自己的机器码映像/共享库副本后，把自己的 closure 地址写槽。
    let mut head = String::from(".intel_syntax noprefix\n");
    let mut dedup = std::collections::HashSet::new();
    let mut slots = std::collections::BTreeSet::new();
    for (name, addr) in abs_defs {
        if dedup.insert(name.clone()) {
            let slot = crate::vm::engine::ir::native_entry_slot_name(
                crate::vm::engine::ir::LinkAddr(addr),
            );
            let _ = writeln!(head, ".globl {name}");
            let _ = writeln!(head, ".hidden {name}");
            let _ = writeln!(head, ".type {name},@function");
            let _ = writeln!(head, "{name}:");
            let _ = writeln!(head, "    jmp QWORD PTR [rip + {slot}]");
            let _ = writeln!(head, ".size {name}, . - {name}");
            slots.insert(slot);
        }
    }
    if !slots.is_empty() {
        head.push_str(".pushsection .data.mirvm_p1,\"aw\",@progbits\n.balign 8\n");
        for slot in slots {
            let _ = writeln!(head, ".globl {slot}");
            let _ = writeln!(head, ".hidden {slot}");
            let _ = writeln!(head, ".type {slot},@object");
            let _ = writeln!(head, ".size {slot},8");
            let _ = writeln!(head, "{slot}:");
            let _ = writeln!(head, "    .quad 0");
        }
        head.push_str(".popsection\n");
    }
    let asm = format!("{head}{asm}");
    Ok(Some(assemble(&asm)?))
}

/// `sym` 指向解释态 guest fn（非 foreign、非 naked）时经 linker 预算 P1 条目 stub
/// 并登记 ABS 定义；不可派生签名响亮拒绝。符号名仍照常回写占位——`.set` 行使
/// 引用解析到 stub 码址。
fn absorb_guest_symfn<'tcx>(
    tcx: TyCtxt<'tcx>,
    linker: &mut super::Linker<'tcx>,
    inst: Instance<'tcx>,
    abs_defs: &mut Vec<(Box<str>, u64)>,
) -> Result<(), String> {
    if tcx.is_foreign_item(inst.def_id()) || is_naked(tcx, inst) {
        return Ok(());
    }
    if linker.entry_ffi_sig(inst).is_none() {
        return Err(format!(
            "global_asm/naked 的 sym 指向签名不可派生的 guest fn `{}`（聚合/Rust \
             ABI/变参）：机器码经 fn-ptr 调此类 fn 无 thunk ABI 可言（native 下同 \
             形本即 UB）——C7 残余边界，如实响亮拒绝",
            tcx.symbol_name(inst).name
        ));
    }
    let addr = linker
        .fn_entry_addr(inst)
        .map_err(|e| format!("global_asm sym guest fn 条目预算失败: {e}"))?;
    abs_defs.push((tcx.symbol_name(inst).name.into(), addr));
    Ok(())
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

// ===== C4：dep crate global_asm 的编译期抽取（decision-history §7.22）=====

/// dep 侧 sym 拒绝的统一前缀（materialize_dep_text 据此区分「跳过清单」
/// 与「真失败」两态；勿改文案而不同步分支判据）。
const DEP_SYM_GUEST: &str = "dep global_asm/naked 的 sym 指向 guest fn";

/// dep 侧的 sym fn 处理：与 absorb_guest_symfn 同一早退规则（foreign/naked
/// 无需跳板），其余响亮拒绝——C7 跨 crate 条目预算（trampoline 须在 bin
/// 链接上下文做）是 C4 片②活，pulp 等真实形态零操作数。
fn dep_absorb_symfn<'tcx>(
    tcx: TyCtxt<'tcx>,
    inst: Instance<'tcx>,
    _abs_defs: &mut Vec<(Box<str>, u64)>,
) -> Result<(), String> {
    if tcx.is_foreign_item(inst.def_id()) || is_naked(tcx, inst) {
        return Ok(());
    }
    Err(format!(
        "{DEP_SYM_GUEST} `{}`（聚合预算属 bin 链接上下文，C4 片②活）",
        tcx.symbol_name(inst).name
    ))
}

/// dep 清单三态（C4 片①语义边界）：
/// - Text：抽取成功，落清单装载；
/// - None：本 crate 无 asm（99%）；
/// - UnsupportedSym：含 sym 指向 dep 自身 guest fn 的站点（C7 跨 crate 条目
///   预算属片②）——**跳过清单**（= C4 前状态：符号维持未解析，被使用时按
///   既有 TRAP 响亮），绝不因「可能不用」而拖垮整个 dep 构建
///   （wasmtime fiber_start 实锤：fiber 面不被 c_wasmtime_wat 触达）。
pub(crate) enum DepAsmText {
    Text(String),
    None,
    UnsupportedSym,
}

/// dep 编译期抽取：mono 收集（对本次编译的 crate 恒成立——mirvm 就是 dep
/// crate 的编译器）→ 渲染全部 global_asm/naked 站点为 `.s` 文本。
/// 文本由调用方落盘为 rlib 旁挂清单（`<rlib 主名>.mirasm.s`）；汇编动作
/// 留给 bin 加载相的同一 assemble 通道（缓存自愈随之免费）。
pub(crate) fn materialize_dep_text(tcx: TyCtxt<'_>) -> Result<DepAsmText, String> {
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
                            Err(e) if e.starts_with(DEP_SYM_GUEST) => {
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
                            Err(e) if e.starts_with(DEP_SYM_GUEST) => {
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
    Ok(DepAsmText::Text(format!(".intel_syntax noprefix\n{asm}")))
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

/// `sym fn` 操作数的处理器（C4 参数化）：bin 侧 = absorb_guest_symfn
///（P1 条目预算 + ABS 跳板）；dep 侧 = dep_absorb_symfn（跨 crate 预算未接，
/// 响亮拒绝）。两渲染器只在这一臂耦合 Linker。
type SymFnAbsorb<'tcx, 'c> =
    dyn FnMut(TyCtxt<'tcx>, Instance<'tcx>, &mut Vec<(Box<str>, u64)>) -> Result<(), String> + 'c;

fn render_global_asm<'tcx>(
    tcx: TyCtxt<'tcx>,
    absorb: &mut SymFnAbsorb<'tcx, '_>,
    item_id: rustc_hir::ItemId,
    out: &mut String,
    abs_defs: &mut Vec<(Box<str>, u64)>,
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
                        absorb(tcx, inst, abs_defs)?;
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
    absorb: &mut SymFnAbsorb<'tcx, '_>,
    inst: Instance<'tcx>,
    out: &mut String,
    abs_defs: &mut Vec<(Box<str>, u64)>,
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
                    absorb(tcx, callee, abs_defs)?;
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

/// 剥 `//` 行注释（GAS/LIVE 语义差实锤：rustc 的目标汇编器 LLVM MC 把 `//`
/// 当行注释起始，GNU as 把 `//` 当除法运算符——global_asm 文本是 LLVM 语义
/// 域（wasmtime fiber 大量 `//` 注释实锤 cc 报 Error），馈 GAS 前必须剥除。
/// 引号态跟踪：`"`/`'` 字符串内的 `//` 不剥。
fn strip_slash_comments(asm: &mut String) {
    let mut out = String::with_capacity(asm.len());
    for line in asm.lines() {
        let mut in_str: Option<u8> = None;
        let mut cut = line.len();
        let b = line.as_bytes();
        let mut i = 0;
        while i + 1 < b.len() {
            match b[i] {
                q @ (b'"' | b'\'') => {
                    if in_str == Some(q) {
                        in_str = None;
                    } else if in_str.is_none() {
                        in_str = Some(q);
                    }
                }
                b'/' if b[i + 1] == b'/' && in_str.is_none() => {
                    cut = i;
                    break;
                }
                _ => {}
            }
            i += 1;
        }
        out.push_str(&line[..cut]);
        out.push('\n');
    }
    *asm = out;
}

/// `.s` → `.so`（内容寻址缓存，与 asm-stub 工厂同款临时名+rename 原子发布）。
/// naked fn 混入模块级 asm，可能引用 guest 符号 → 不能 `-nostdlib`；用 `-nostartfiles`
/// 保留动态链接器解析（naked 内 sym 操作数指向的 guest fn 由 RTLD_GLOBAL 兜底）。
/// C4：升为 pub(crate)——dep 清单文本经 bin 加载相同一通道物化。
pub(crate) fn assemble(asm: &str) -> Result<Box<str>, String> {
    // T5：与 asm-stub 同通道——global_asm/naked 内裸 `syscall` 指令 → 间接槽
    // 调用（改写发生在内容哈希前，缓存键与最终字节一致；槽随 .so 物化，
    // 装载 required_native_libs 时由 lower_inner 统一重填）
    let mut asm = asm.to_string();
    strip_slash_comments(&mut asm);
    crate::lower::asm::rewrite_syscall_text(&mut asm);
    asm.push('\n');
    asm.push_str(crate::native_archive::NATIVE_RUNTIME_BRIDGE_ASM);
    let mut hash_input = b"mirvm-global-asm-v3\0".to_vec();
    hash_input.extend_from_slice(asm.as_bytes());
    let hash = crate::lower::asm::fnv1a(&hash_input);
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
        .args(["-shared", "-fPIC", "-nostartfiles", "-Wl,-Bsymbolic", "-o"])
        .arg(&tmp)
        .arg(&s_path)
        .args(crate::native_archive::NATIVE_RUNTIME_WRAP_FLAGS)
        .status()
        .map_err(|e| format!("调用 cc 汇编 global-asm 失败（PATH 缺 cc？）: {e}"))?;
    if !status.success() {
        return Err(format!("cc 汇编 global-asm 失败（status={status}）"));
    }
    // 未定义符号审计：global_asm/naked 里 `sym` 引用的符号必须自身是机器码
    // （另一 naked/global_asm 符号、动态库导出，或经 C7 ABS 定义挂到 P1 条目
    // stub 的 guest fn）。裸引用未定义 guest 符号会留下 dlopen 时才炸且诊断
    // 误导的未解析项——此处提前响亮拒绝。
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
