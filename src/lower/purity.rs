//! Purity measurement probe (moved whole from lower/mod.rs M8): PurityStats/Purity/
//! classify_purity/arg_mentions_local -- the A2 split instance classifier and
//! --vm-stats ledger (gated by MIRVM_PURITY_STATS=1).

use super::*;

#[derive(Default)]
pub(super) struct PurityStats {
    local: (u64, u128),
    tainted: (u64, u128),
    pure: (u64, u128),
    /// pure set broken down by crate (source distribution of deps-image content)
    pure_crates: FxHashMap<Symbol, (u64, u128)>,
    /// tainted instances one by one (symbol, ns) -- for printing top; small volume (expected hundreds)
    tainted_insts: Vec<(Box<str>, u128)>,
}

pub(super) enum Purity {
    Local,
    Tainted,
    Pure,
}

impl Purity {
    /// image class = Pure (bin-independent instances, deps-image candidates); Local/Tainted = delta classes.
    pub(super) fn is_image(&self) -> bool {
        matches!(self, Purity::Pure)
    }
}

impl PurityStats {
    pub(super) fn record(&mut self, tcx: TyCtxt<'_>, inst: Instance<'_>, sym: &str, ns: u128) {
        let (cls, extra) = match classify_purity(inst) {
            Purity::Local => (&mut self.local, None),
            Purity::Tainted => {
                self.tainted_insts.push((sym.into(), ns));
                (&mut self.tainted, None)
            }
            Purity::Pure => {
                let krate = tcx.crate_name(inst.def_id().krate);
                (&mut self.pure, Some(krate))
            }
        };
        cls.0 += 1;
        cls.1 += ns;
        if let Some(krate) = extra {
            let e = self.pure_crates.entry(krate).or_default();
            e.0 += 1;
            e.1 += ns;
        }
    }

    pub(super) fn dump(&self) {
        fn ms(ns: u128) -> String {
            format!("{:.1}", ns as f64 / 1e6)
        }
        let (l, t, p) = (self.local, self.tainted, self.pure);
        eprintln!("[purity] local:   {} inst, {} ms", l.0, ms(l.1));
        eprintln!("[purity] tainted: {} inst, {} ms", t.0, ms(t.1));
        eprintln!("[purity] pure:    {} inst, {} ms", p.0, ms(p.1));
        eprintln!(
            "[purity] A2 re-lower per edit = local+tainted = {} inst, {} ms (total saved {} inst, {} ms)",
            l.0 + t.0,
            ms(l.1 + t.1),
            l.0 + t.0 + p.0,
            ms(l.1 + t.1 + p.1)
        );
        let mut crates: Vec<_> = self.pure_crates.iter().collect();
        crates.sort_by_key(|(_, v)| std::cmp::Reverse(v.0));
        for (k, (n, ns)) in crates.iter().take(12) {
            eprintln!("[purity]   pure crate {k}: {n} inst, {} ms", ms(*ns));
        }
        let mut t: Vec<_> = self.tainted_insts.iter().collect();
        t.sort_by_key(|(_, ns)| std::cmp::Reverse(*ns));
        for (sym, ns) in t.iter().take(10) {
            eprintln!("[purity]   tainted top: {} ms  {sym}", ms(*ns));
        }
    }
}

/// Purity classification of an instance (see PurityStats header for definition).
pub(super) fn classify_purity(inst: Instance<'_>) -> Purity {
    use rustc_hir::def_id::LOCAL_CRATE;
    use rustc_middle::ty::ShimKind;
    // Defining DefId: if any is local => this is the bin's own code (ClosureOnce of local closures, etc.).
    let local_def = match inst.def {
        InstanceKind::Item(d) | InstanceKind::Intrinsic(d) | InstanceKind::Virtual(d, _) => {
            d.krate == LOCAL_CRATE
        }
        InstanceKind::Shim(shim) => match shim {
            ShimKind::VTable(d)
            | ShimKind::Reify(d, _)
            | ShimKind::ThreadLocal(d)
            | ShimKind::FnPtr(d, _)
            | ShimKind::Clone(d, _)
            | ShimKind::FnPtrAddr(d, _)
            | ShimKind::AsyncDropGlueCtor(d, _)
            | ShimKind::AsyncDropGlue(d, _)
            | ShimKind::DropGlue(d, _)
            | ShimKind::FutureDropPoll(d, _, _)
            | ShimKind::ConstructCoroutineInClosure {
                coroutine_closure_def_id: d,
                ..
            } => d.krate == LOCAL_CRATE,
            ShimKind::ClosureOnce {
                call_once, closure, ..
            } => call_once.krate == LOCAL_CRATE || closure.krate == LOCAL_CRATE,
        },
    };
    if local_def {
        return Purity::Local;
    }
    // Extra types carried by shim (not in args).
    let shim_tys: &[rustc_middle::ty::Ty<'_>] = match inst.def {
        InstanceKind::Shim(ShimKind::FnPtr(_, t))
        | InstanceKind::Shim(ShimKind::Clone(_, t))
        | InstanceKind::Shim(ShimKind::FnPtrAddr(_, t))
        | InstanceKind::Shim(ShimKind::AsyncDropGlueCtor(_, t))
        | InstanceKind::Shim(ShimKind::AsyncDropGlue(_, t)) => &[t],
        InstanceKind::Shim(ShimKind::FutureDropPoll(_, t1, t2)) => &[t1, t2],
        InstanceKind::Shim(ShimKind::DropGlue(_, Some(t))) => &[t],
        _ => &[],
    };
    let tainted = inst.args.iter().any(|a| a.walk().any(arg_mentions_local))
        || shim_tys.iter().any(|&t| t.walk().any(arg_mentions_local));
    if tainted {
        Purity::Tainted
    } else {
        Purity::Pure
    }
}

/// Def at the top level that mentions LOCAL_CRATE? (Deep walk() traversal covers all nested bits)
pub(super) fn arg_mentions_local(arg: rustc_middle::ty::GenericArg<'_>) -> bool {
    use rustc_hir::def_id::LOCAL_CRATE;
    use rustc_middle::ty::TyKind;
    let Some(t) = arg.as_type() else { return false };
    let did = match t.kind() {
        TyKind::Adt(def, _) => Some(def.did()),
        &TyKind::FnDef(d, _)
        | &TyKind::Closure(d, _)
        | &TyKind::Coroutine(d, _)
        | &TyKind::CoroutineClosure(d, _)
        | &TyKind::CoroutineWitness(d, _)
        | &TyKind::Foreign(d) => Some(d),
        TyKind::Alias(_, at) => Some(match at.kind {
            rustc_middle::ty::AliasTyKind::Projection { def_id }
            | rustc_middle::ty::AliasTyKind::Inherent { def_id }
            | rustc_middle::ty::AliasTyKind::Opaque { def_id }
            | rustc_middle::ty::AliasTyKind::Free { def_id } => def_id,
        }),
        _ => None,
    };
    if did.is_some_and(|d| d.krate == LOCAL_CRATE) {
        return true;
    }
    // walk does not descend into trait-object predicate DefId (rustc_type_ir walk.rs Dynamic branch only pushes args)
    if let TyKind::Dynamic(preds, ..) = t.kind() {
        return preds
            .principal()
            .is_some_and(|p| p.skip_binder().def_id.krate == LOCAL_CRATE)
            || preds.auto_traits().any(|d| d.krate == LOCAL_CRATE);
    }
    false
}
