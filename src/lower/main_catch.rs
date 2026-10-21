//! MIR analysis of the pinned std startup boundary and the catch wrapping user `main`.

use crate::lower::Error;

use rustc_middle::ty::{self, Instance, TyCtxt, TypingEnv};

#[derive(Clone, Copy)]
struct CatchSite<'tcx> {
    owner: Instance<'tcx>,
    intrinsic: Instance<'tcx>,
    do_call: Instance<'tcx>,
    boundary: Option<(Instance<'tcx>, Instance<'tcx>)>,
}

/// Derive the startup closure and catch callbacks from MIR, including inlined source scopes.
/// Paths confirm the pinned std implementations; instance arguments and reified fn-ptr definitions
/// distinguish the user main catch from the outer runtime catch and payload cleanup catches.
pub(super) fn discover_main_catch_site<'tcx>(
    tcx: TyCtxt<'tcx>,
    typing_env: TypingEnv<'tcx>,
    start: Instance<'tcx>,
) -> Result<
    (
        Instance<'tcx>,
        Instance<'tcx>,
        Instance<'tcx>,
        Instance<'tcx>,
    ),
    Error,
> {
    fn body<'tcx>(
        tcx: TyCtxt<'tcx>,
        typing_env: TypingEnv<'tcx>,
        instance: Instance<'tcx>,
    ) -> rustc_middle::mir::Body<'tcx> {
        let source = tcx.instance_mir(instance.def);
        instance.instantiate_mir_and_normalize_erasing_regions(
            tcx,
            typing_env,
            rustc_middle::ty::EarlyBinder::bind(tcx, source.clone()),
        )
    }

    fn direct_calls<'tcx>(
        tcx: TyCtxt<'tcx>,
        typing_env: TypingEnv<'tcx>,
        body: &rustc_middle::mir::Body<'tcx>,
    ) -> Vec<(Instance<'tcx>, rustc_middle::mir::UnwindAction)> {
        body.basic_blocks
            .iter()
            .filter_map(|block| {
                let rustc_middle::mir::TerminatorKind::Call { func, unwind, .. } =
                    &block.terminator().kind
                else {
                    return None;
                };
                let ty::FnDef(def_id, args) = func.ty(&body.local_decls, tcx).kind() else {
                    return None;
                };
                Some((
                    Instance::expect_resolve(
                        tcx,
                        typing_env,
                        *def_id,
                        args,
                        block.terminator().source_info.span,
                    ),
                    *unwind,
                ))
            })
            .collect()
    }

    fn operand_local<'tcx>(
        operand: &rustc_middle::mir::Operand<'tcx>,
    ) -> Option<rustc_middle::mir::Local> {
        match operand {
            rustc_middle::mir::Operand::Copy(place) | rustc_middle::mir::Operand::Move(place)
                if place.projection.is_empty() =>
            {
                Some(place.local)
            }
            _ => None,
        }
    }

    fn reified_fn<'tcx>(
        tcx: TyCtxt<'tcx>,
        typing_env: TypingEnv<'tcx>,
        body: &rustc_middle::mir::Body<'tcx>,
        local: rustc_middle::mir::Local,
        use_loc: rustc_middle::mir::Location,
    ) -> Result<Instance<'tcx>, Error> {
        use rustc_middle::mir::visit::{PlaceContext, Visitor};

        struct Writes {
            local: rustc_middle::mir::Local,
            locations: Vec<rustc_middle::mir::Location>,
        }

        impl<'tcx> Visitor<'tcx> for Writes {
            fn visit_place(
                &mut self,
                place: &rustc_middle::mir::Place<'tcx>,
                context: PlaceContext,
                location: rustc_middle::mir::Location,
            ) {
                if place.local == self.local && context.is_mutating_use() {
                    self.locations.push(location);
                }
                self.super_place(place, context, location);
            }
        }

        let mut writes = Writes {
            local,
            locations: Vec::new(),
        };
        writes.visit_body(body);
        let [definition] = writes.locations.as_slice() else {
            return Err(Error::internal(format!(
                "local {local:?} expected exactly one write, found {}",
                writes.locations.len()
            )));
        };
        if !definition.dominates(use_loc, body.basic_blocks.dominators()) {
            return Err(Error::internal(format!(
                "the only write to local {local:?} at {definition:?} does not dominate the catch call {use_loc:?}"
            )));
        }
        let block = &body.basic_blocks[definition.block];
        let Some(statement) = block.statements.get(definition.statement_index) else {
            return Err(Error::internal(format!(
                "the only write to local {local:?} occurs at a terminator, not a fn-ptr reify assignment"
            )));
        };
        let rustc_middle::mir::StatementKind::Assign(assign) = &statement.kind else {
            return Err(Error::internal(format!(
                "the only write to local {local:?} is not Assign"
            )));
        };
        let (destination, rvalue) = &**assign;
        if destination.local != local || !destination.projection.is_empty() {
            return Err(Error::internal(format!(
                "the only write to local {local:?} is not a whole-local assignment"
            )));
        }
        let rustc_middle::mir::Rvalue::Cast(
            rustc_middle::mir::CastKind::PointerCoercion(
                ty::adjustment::PointerCoercion::ReifyFnPointer(..),
                _,
            ),
            operand,
            _,
        ) = rvalue
        else {
            return Err(Error::internal(format!(
                "the only write to local {local:?} is not a fn-ptr reify"
            )));
        };
        let ty::FnDef(def_id, args) = operand.ty(&body.local_decls, tcx).kind() else {
            return Err(Error::internal(format!(
                "the reify source for local {local:?} is not FnDef"
            )));
        };
        Instance::resolve_for_fn_ptr(tcx, typing_env, *def_id, args).ok_or_else(|| {
            Error::internal(format!(
                "cannot resolve fn-ptr instance for local {local:?}"
            ))
        })
    }

    // Follow only wrappers carrying the expected callback type. Inlining may put the intrinsic
    // in any of them; its reified callbacks still prove which catch it belongs to.
    fn find_catch<'tcx>(
        tcx: TyCtxt<'tcx>,
        typing_env: TypingEnv<'tcx>,
        root: Instance<'tcx>,
        callback: ty::Ty<'tcx>,
        runtime_closure: Option<(ty::Ty<'tcx>, Instance<'tcx>)>,
    ) -> Result<CatchSite<'tcx>, Error> {
        let mut pending = vec![(root, None)];
        let mut seen = rustc_data_structures::fx::FxHashSet::default();
        let mut sites = Vec::new();
        while let Some((owner, boundary)) = pending.pop() {
            if !seen.insert(owner) {
                continue;
            }
            let mir = body(tcx, typing_env, owner);
            for (bb, block) in mir.basic_blocks.iter_enumerated() {
                let rustc_middle::mir::TerminatorKind::Call {
                    func, args, unwind, ..
                } = &block.terminator().kind
                else {
                    continue;
                };
                let ty::FnDef(def, generic_args) = func.ty(&mir.local_decls, tcx).kind() else {
                    continue;
                };
                if !tcx
                    .intrinsic(*def)
                    .is_some_and(|i| i.name.as_str() == "catch_unwind")
                {
                    continue;
                }
                if args.len() != 3
                    || !matches!(unwind, rustc_middle::mir::UnwindAction::Unreachable)
                {
                    return Err(Error::internal("pinned std catch intrinsic shape changed"));
                }
                let location = mir.terminator_loc(bb);
                let mut callbacks = Vec::new();
                for index in [0, 2] {
                    let local = operand_local(&args[index].node).ok_or(Error::internal(
                        "pinned std catch callback is not a local fn-ptr",
                    ))?;
                    callbacks.push(reified_fn(tcx, typing_env, &mir, local, location)?);
                }
                let [do_call, do_catch] = callbacks.as_slice() else {
                    unreachable!()
                };
                if tcx.def_path_str(do_call.def_id()) != "std::panicking::catch_unwind::do_call"
                    || tcx.def_path_str(do_catch.def_id())
                        != "std::panicking::catch_unwind::do_catch"
                    || do_call.args != do_catch.args
                {
                    return Err(Error::internal(
                        "pinned std catch callback implementations changed",
                    ));
                }
                if do_call.args.type_at(0) == callback {
                    let intrinsic = Instance::expect_resolve(
                        tcx,
                        typing_env,
                        *def,
                        generic_args,
                        block.terminator().source_info.span,
                    );
                    sites.push(CatchSite {
                        owner,
                        intrinsic,
                        do_call: *do_call,
                        boundary,
                    });
                }
            }
            for (callee, unwind) in direct_calls(tcx, typing_env, &mir) {
                if matches!(
                    callee.def,
                    ty::InstanceKind::Intrinsic(_) | ty::InstanceKind::Virtual(..)
                ) || tcx.is_foreign_item(callee.def_id())
                {
                    continue;
                }
                if callee.args.types().any(|ty| {
                    ty == callback || runtime_closure.is_some_and(|(closure, _)| ty == closure)
                }) || runtime_closure.is_some_and(|(_, closure)| callee == closure)
                {
                    if boundary.is_none()
                        && !matches!(unwind, rustc_middle::mir::UnwindAction::Continue)
                    {
                        return Err(Error::internal(
                            "pinned std runtime callback must use Continue unwind",
                        ));
                    }
                    pending.push((callee, boundary.or(Some((owner, callee)))));
                }
            }
        }
        let [site] = sites.as_slice() else {
            return Err(Error::internal(format!(
                "pinned std startup expected one catch for {callback}, found {}",
                sites.len()
            )));
        };
        Ok(*site)
    }

    let start_calls = direct_calls(tcx, typing_env, &body(tcx, typing_env, start));
    let [(lang_start_internal, rustc_middle::mir::UnwindAction::Continue)] = start_calls.as_slice()
    else {
        return Err(Error::internal(
            "pinned std start must call one runtime entry with Continue unwind",
        ));
    };
    if tcx.def_path_str(lang_start_internal.def_id()) != "std::rt::lang_start_internal" {
        return Err(Error::internal("pinned std start runtime entry changed"));
    }
    let runtime = body(tcx, typing_env, *lang_start_internal);
    let calls = direct_calls(tcx, typing_env, &runtime);
    let outer_catches: Vec<_> = calls
        .iter()
        .map(|(call, _)| *call)
        .chain(
            runtime
                .source_scopes
                .iter()
                .filter_map(|scope| scope.inlined.map(|(call, _)| call)),
        )
        .filter(|call| tcx.def_path_str(call.def_id()) == "std::panic::catch_unwind")
        .filter_map(|call| {
            call.args
                .types()
                .find(|ty| matches!(ty.kind(), ty::Closure(..)))
        })
        .collect();
    let [closure_ty] = outer_catches.as_slice() else {
        return Err(Error::internal(format!(
            "pinned std runtime expected one outer closure catch, found {}",
            outer_catches.len()
        )));
    };
    let ty::Closure(def, args) = *closure_ty.kind() else {
        unreachable!()
    };
    let closure = Instance::resolve_closure(tcx, def, args, ty::ClosureKind::FnOnce);
    let outer = find_catch(tcx, typing_env, *lang_start_internal, *closure_ty, None)?;
    let main_ty = runtime.local_decls[runtime.args_iter().next().unwrap()].ty;
    let main = find_catch(
        tcx,
        typing_env,
        outer.do_call,
        main_ty,
        Some((*closure_ty, closure)),
    )?;

    let (caller, callee) = main.boundary.ok_or(Error::internal(
        "pinned std runtime callback has no physical call surrounding the main catch",
    ))?;
    // This call belongs to the common runtime closure, independently of the user's Termination
    // type. Only the proven inner intrinsic may claim it; the outer runtime catch stays normal.
    Ok((caller, callee, main.owner, main.intrinsic))
}
