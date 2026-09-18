//! mono collection: reuse the rustc monomorphization collector (D1) — the same reachable
//! instance set as native codegen, so correctness comes for free (Rust monomorphization is static;
//! this is why codegen works).

use rustc_data_structures::fx::FxHashSet;
use rustc_middle::mono::MonoItem;
use rustc_middle::ty::{Instance, TyCtxt};

/// All `MonoItem::Fn` instances (deduplicated across CGUs, stable order).
/// `MonoItem::Static` is booked for M4.1 (reference sites are Trap placeholders from lower); GlobalAsm is ignored (reference sites Trap).
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
