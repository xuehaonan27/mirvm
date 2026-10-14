#!/usr/bin/env python3
"""Install the frame-entry experiment into a disposable source copy."""
import argparse
from pathlib import Path
import shutil
import textwrap


def replace(path, before, after):
    source = path.read_text()
    if source.count(before) != 1:
        raise SystemExit(f"expected one patch site in {path}: {before!r}")
    path.write_text(source.replace(before, after))


parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("checkout", type=Path)
root = parser.parse_args().checkout.resolve()
if (root / ".git").exists():
    raise SystemExit("use a disposable source copy without .git")
shutil.copyfile(Path(__file__).with_name("regions.rs"), root / "src/shared_regions.rs")
shutil.copyfile(Path(__file__).with_name("policy.rs"), root / "src/shared_policy.rs")
shutil.copyfile(Path(__file__).with_name("scalars.rs"), root / "src/shared_scalars.rs")
with (root / "src/lib.rs").open("a") as stream:
    stream.write("\npub(crate) mod shared_regions;\n")

options = root / "src/options.rs"
for option, description in [
    ("shared_regions", "Compile admitted generic templates on demand through a private frame entry"),
    ("shared_region_calls", "Call concrete adapters directly from shared code"),
    ("shared_region_specialize", "Allow hot concrete instances to leave the shared tier"),
    ("shared_region_profile", "Sample interpreted and compiled own-body work without changing admission"),
    ("shared_region_policy", "Admit shared and concrete transitions using execution credit"),
    ("shared_region_ssa", "Keep nonaliasing shared scalar locals in SSA between frame boundaries"),
]:
    replace(options, '    /// Print the dedup probe:',
            f'    /// {description} (experiment only).\n'
            f'    dev {option} env("MIRVM_{option.upper()}") default("off") => reads(bool, |o| o.{option});\n'
            '    /// Print the dedup probe:')
    replace(options, '    generic_payback: bool,\n',
            f'    generic_payback: bool,\n    {option}: bool,\n')
    replace(options, '            generic_payback: flag("generic_payback"),\n',
            '            generic_payback: flag("generic_payback"),\n'
            f'            {option}: flag("{option}"),\n')

replace(root / "src/vm/jit/mod.rs", 'mod frame;\n', 'pub(crate) mod frame;\n')
frame = root / "src/vm/jit/frame.rs"
for declaration in ("struct FrameMap", "fn contains", "fn analyze_frame"):
    replace(frame, f'pub(super) {declaration}', f'pub(crate) {declaration}')

replace(root / "src/vm/ctx/engine.rs", '    pub module: Module,\n',
        '    pub module: Module,\n    pub(crate) shared_regions: crate::shared_regions::Regions,\n')
replace(root / "src/vm/ctx/load.rs", '        module.ensure_function_names();\n',
        '        let shared_regions = crate::shared_regions::Regions::prepare(&mut module, domain);\n'
        '        module.ensure_function_names();\n')
replace(root / "src/vm/ctx/load.rs", '        Ok(Shared {\n            id,\n',
        '        Ok(Shared {\n            id,\n            shared_regions,\n')

# Shared calls retain their concrete interpreter prologue until a concrete ABI entry is published.
replace(root / "src/vm/dispatch.rs", '    let jit = unsafe { &(*ctx).shared.jit };\n',
        '    if unsafe { &(*ctx).shared.shared_regions }.interpret(ctx, func) {\n'
        '        return crate::vm::interp::interp_frame(ctx, func, args);\n'
        '    }\n    let jit = unsafe { &(*ctx).shared.jit };\n')
replace(root / "src/vm/jit/mod.rs", '    if shared.jit.enabled {\n',
        '    if shared.jit.enabled && !shared.shared_regions.iterations(ctx, func, count) {\n')
replace(root / "src/vm/jit/compiler/mod.rs", '    if !shared.jit.enabled\n',
        '    shared.shared_regions.report(shared);\n    if !shared.jit.enabled\n')
replace(root / "src/vm/jit/compiler/translate.rs",
        '        let Some(body) = self.shared.module.funcs.get(func as usize) else {\n',
        '        let region = self.shared.shared_regions.body(self.shared, func);\n'
        '        let Some(body) = region.as_ref().or_else(|| self.shared.module.funcs.get(func as usize)) else {\n')
replace(root / "src/vm/jit/compiler/translate.rs", '                && *callee != func\n',
        '                && *callee != func\n'
        '                && !self.shared.shared_regions.is_adapter(*callee)\n')
replace(root / "src/vm/jit/compiler/imports.rs", '        jb.symbols(\n',
        '        jb.symbol("mirvm_shared_call", crate::shared_regions::call as *const u8);\n'
        '        jb.symbols(\n')
replace(root / "src/vm/jit/translate/term.rs", '        match t {\n',
        '''        match t {
            Terminator::Call { callee, args, ret, target, .. }
                if self.shared.shared_regions.is_adapter(*callee) => {
                let args: Vec<_> = args.iter().map(|arg| self.operand(arg).0).collect();
                let result = self.call_helper1("mirvm_shared_call", &args);
                self.write_ret(ret, result, result);
                self.b.ins().jump(blocks[*target as usize], &[]);
            }
''')

replace(root / "src/vm/jit/compiler/mod.rs", '        let started = shared.jit.work.as_ref().map(|_| std::time::Instant::now());\n',
        '        let Ok(policy_ticket) = shared.shared_regions.compile_start(&shared, func) else { continue; };\n'
        '        let started = shared.jit.work.as_ref().map(|_| std::time::Instant::now());\n')
replace(root / "src/vm/jit/compiler/mod.rs", '        c.compile(func);\n',
        '        c.compile(func);\n'
        '        shared.shared_regions.compile_complete(&shared, func, policy_ticket);\n')

# Extract the existing call implementation once: compiled adapters and interpretation share every
# argument, return, cleanup and main-panic-boundary operation, including future source changes.
runblocks = root / "src/vm/interp/runblocks.rs"
source = runblocks.read_text()
start = source.index('            Terminator::Call {\n')
end = source.index('            Terminator::CallForeign {\n', start)
arm = source[start:end]
pattern, implementation = arm.split('            } => {\n', 1)
implementation = implementation.rsplit('            }\n', 1)[0]
implementation = implementation.replace('                blk = *target as usize;\n', '                *target as usize\n')
implementation = implementation.replace('                let call = || guarding_terminate',
        '                if let Some(sample) = sample { sample.pause(); }\n'
        '                let call = || guarding_terminate')
implementation = implementation.replace('                edge.set(None);',
        '                if let Some(sample) = sample { sample.resume(); }\n'
        '                edge.set(None);')
function = ('\n#[inline(always)]\npub(crate) fn exec_call(ctx: *mut Ctx, base: usize, edge: &Cell<Option<Bb>>, term: &Terminator, sample: Option<&crate::shared_regions::Sample<\'_>>) -> usize {\n'
            '    let ' + textwrap.dedent(pattern).strip() + '\n    } = term else { unreachable!("direct call adapter") };\n'
            + textwrap.indent(textwrap.dedent(implementation), '    ') + '}\n')
runblocks.write_text(source[:start] + '            Terminator::Call { .. } => blk = exec_call(ctx, base, edge, term, policy_sample.as_ref()),\n' + source[end:] + function)
replace(root / "src/vm/interp/mod.rs", 'pub(crate) use call::interp_frame;\n',
        'pub(crate) use call::interp_frame;\npub(crate) use runblocks::exec_call;\n')
replace(runblocks, '    let mut work = body\n',
        '    let compiled_region = if entry == 0 {\n'
        '        unsafe { &(*ctx).shared.shared_regions }.select(ctx, func)\n'
        '    } else { 0 };\n'
        '    let policy_sample = unsafe { &(*ctx).shared.shared_regions }.sample(func, compiled_region != 0);\n'
        '    let mut work = body\n')
replace(runblocks, '        .as_ref()\n        .and_then',
        '        .as_ref()\n        .filter(|_| compiled_region == 0)\n        .and_then')
replace(runblocks,
        '        let current = blk;\n'
        '        let block: &Block = &body.code()[blk];\n'
        '        for stmt in &block.stmts {\n'
        '            exec_stmt(ctx, base, stmt);\n'
        '        }\n',
        '        let current = blk;\n'
        '        if compiled_region != 0 {\n'
        '            blk = crate::shared_regions::enter(ctx, compiled_region, base, blk, body, edge, policy_sample.as_ref());\n'
        '        }\n'
        '        let block: &Block = &body.code()[blk];\n'
        '        if compiled_region == 0 {\n'
        '            for stmt in &block.stmts {\n'
        '                exec_stmt(ctx, base, stmt);\n'
        '            }\n'
        '        }\n')

replace(runblocks, '                exec_stmt(ctx, base, stmt);\n            }\n        }\n',
        '                exec_stmt(ctx, base, stmt);\n            }\n'
        '            if let Some(sample) = &policy_sample { sample.statements(block.stmts.len()); }\n        }\n')
replace(runblocks, '\n            crate::vm::ctx::drain_pending_signals(ctx);\n',
        '\n            if let Some(sample) = &policy_sample { sample.pause(); }\n'
        '            crate::vm::ctx::drain_pending_signals(ctx);\n'
        '            if let Some(sample) = &policy_sample { sample.resume(); }\n')

replace(runblocks, '                crate::vm::ctx::drain_pending_signals(ctx);\n',
        '                if let Some(sample) = &policy_sample { sample.pause(); }\n'
        '                crate::vm::ctx::drain_pending_signals(ctx);\n'
        '                if let Some(sample) = &policy_sample { sample.resume(); }\n')
