#!/usr/bin/env python3
"""Install the execution experiment into a disposable MIRVM source copy."""
import argparse
from pathlib import Path
import shutil


def replace(path, before, after):
    text = path.read_text()
    if text.count(before) != 1:
        raise SystemExit(f"expected exactly one patch site in {path}")
    path.write_text(text.replace(before, after))


parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("checkout", type=Path)
args = parser.parse_args()
root = args.checkout.resolve()
if (root / ".git").exists():
    raise SystemExit("use a disposable source copy without .git")
shutil.copyfile(Path(__file__).with_name("prototype.rs"), root / "src/generic_prototype.rs")
(root / "src/generic_prototype").mkdir(exist_ok=True)
shutil.copyfile(Path(__file__).with_name("frontend.rs"), root / "src/generic_prototype/frontend.rs")
with (root / "src/lib.rs").open("a") as stream:
    stream.write("\npub mod generic_prototype;\n")
(root / "src/bin").mkdir(exist_ok=True)
(root / "src/bin/generic_prototype.rs").write_text(
    "#![feature(rustc_private)]\nfn main() { mirvm::generic_prototype::main(); }\n"
)
replace(root / "src/vm/ir/program/funcs.rs",
        "    pub fn get(&self, index: usize) -> Option<&FuncBody> {\n",
        "    pub fn get(&self, index: usize) -> Option<&FuncBody> {\n"
        "        if let Some(body) = crate::generic_prototype::published(index) {\n"
        "            return Some(body);\n        }\n")
replace(root / "src/vm/jit/mod.rs",
        "    if shared.jit.enabled {\n        shared.jit.note_iterations(func, count);\n",
        "    if shared.jit.enabled && !crate::generic_prototype::active() {\n"
        "        shared.jit.note_iterations(func, count);\n")
with (root / "src/vm/verify/mod.rs").open("a") as stream:
    stream.write("""
pub(crate) fn prototype_body(module: &Module, instance: &Instance, body: &FuncBody)
    -> Result<(), crate::error::Error>
{
    Verifier::new(module, instance, Prefix::default(), &[])?.body(body)
}
""")
