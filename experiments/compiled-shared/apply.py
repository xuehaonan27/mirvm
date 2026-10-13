#!/usr/bin/env python3
"""Install the frame-entry experiment into a disposable source copy."""
import argparse
from pathlib import Path
import shutil


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
with (root / "src/lib.rs").open("a") as stream:
    stream.write("\npub(crate) mod shared_regions;\n")

options = root / "src/options.rs"
replace(options, '    /// Print the dedup probe:',
        '    /// Compile generic instruction regions through a shared frame entry (experiment only).\n'
        '    dev shared_regions env("MIRVM_SHARED_REGIONS") default("off") => reads(bool, |o| o.shared_regions);\n'
        '    /// Print the dedup probe:')
replace(options, '    generic_payback: bool,\n',
        '    generic_payback: bool,\n    shared_regions: bool,\n')
replace(options, '            generic_payback: flag("generic_payback"),\n',
        '            generic_payback: flag("generic_payback"),\n'
        '            shared_regions: flag("shared_regions"),\n')

replace(root / "src/vm/ctx/engine.rs", '    pub module: Module,\n',
        '    pub module: Module,\n    pub(crate) shared_regions: crate::shared_regions::Regions,\n')
replace(root / "src/vm/ctx/load.rs", '        module.ensure_function_names();\n',
        '        let shared_regions = crate::shared_regions::Regions::prepare(&mut module, domain)?;\n'
        '        module.ensure_function_names();\n')
replace(root / "src/vm/ctx/load.rs", '        Ok(Shared {\n            id,\n',
        '        Ok(Shared {\n            id,\n            shared_regions,\n')

# Original guest identities still enter their concrete interpreter prologue. Compiled callers use
# the existing c2i entry. The experiment never publishes a differently shaped entry in their PLT.
replace(root / "src/vm/dispatch.rs", '    let jit = unsafe { &(*ctx).shared.jit };\n',
        '    if unsafe { &(*ctx).shared.shared_regions }.contains(func) {\n'
        '        return crate::vm::interp::interp_frame(ctx, func, args);\n'
        '    }\n    let jit = unsafe { &(*ctx).shared.jit };\n')
replace(root / "src/vm/jit/compiler/mod.rs", '        let func = request.func;\n',
        '        let func = shared.shared_regions.target(request.func);\n')
replace(root / "src/vm/jit/compiler/mod.rs", '    if !shared.jit.enabled\n',
        '    shared.shared_regions.report(shared);\n    if !shared.jit.enabled\n')

runblocks = root / "src/vm/interp/runblocks.rs"
replace(runblocks, '    let mut blk = entry as usize;\n',
        '    let compiled_region = if entry == 0 {\n'
        '        unsafe { &(*ctx).shared.shared_regions }.select(ctx, func)\n'
        '    } else { 0 };\n'
        '    let mut blk = entry as usize;\n')
replace(runblocks,
        '        let current = blk;\n'
        '        let block: &Block = &body.code()[blk];\n'
        '        for stmt in &block.stmts {\n'
        '            exec_stmt(ctx, base, stmt);\n'
        '        }\n',
        '        let current = blk;\n'
        '        if compiled_region != 0 {\n'
        '            blk = crate::shared_regions::enter(ctx, compiled_region, base, blk);\n'
        '        }\n'
        '        let block: &Block = &body.code()[blk];\n'
        '        if compiled_region == 0 {\n'
        '            for stmt in &block.stmts {\n'
        '                exec_stmt(ctx, base, stmt);\n'
        '            }\n'
        '        }\n')
