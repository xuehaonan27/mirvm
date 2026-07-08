//! mono 收集：复用 rustc 单态化收集器（D1）——与 native codegen 同一套可达
//! instance 集，正确性白拿（Rust 单态化静态，codegen 能工作的原因即此）。

use rustc_data_structures::fx::FxHashSet;
use rustc_middle::mono::MonoItem;
use rustc_middle::ty::{Instance, TyCtxt};

/// 全部 `MonoItem::Fn` instance（跨 CGU 去重，保持稳定序）。
/// `MonoItem::Static` 记账留 M4.1（引用点由 lower 以 Trap 占位）；GlobalAsm 忽略（引用点 Trap）。
pub fn collect<'tcx>(tcx: TyCtxt<'tcx>) -> Vec<Instance<'tcx>> {
    let parts = tcx.collect_and_partition_mono_items(());
    let mut seen = FxHashSet::default();
    let mut out = Vec::new();
    for cgu in parts.codegen_units {
        for item in cgu.items().keys() {
            if let MonoItem::Fn(inst) = item
                && seen.insert(*inst)
            {
                out.push(*inst);
            }
        }
    }
    out
}
