//! MIR analysis of the pinned std main panic catch site: from the lang item `start`, locate the
//! outer call wrapping the user `main` and the intrinsic call that performs the catch.

use crate::lower::Error;

use rustc_middle::ty::{self, Instance, TyCtxt, TypingEnv};

/// From the real MIR call graph of the lang item `start`, find the outer call wrapping the user
/// `main`, and finally the intrinsic call that performs the catch. Call relationships and
/// monomorphization args are derived from MIR; the paths only confirm that these derived nodes are
/// still the std implementations agreed upon by the pinned toolchain, without relying on
/// drift-prone DefId numbers.
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

    let start_calls = direct_calls(tcx, typing_env, &body(tcx, typing_env, start));
    let [(lang_start_internal, _)] = start_calls.as_slice() else {
        return Err(Error::internal(format!(
            "pinned toolchain start MIR expected exactly one direct call, found {}",
            start_calls.len()
        )));
    };
    let lang_start_path = tcx.def_path_str(lang_start_internal.def_id());
    if lang_start_path != "std::rt::lang_start_internal" {
        return Err(Error::internal(format!(
            "pinned toolchain start direct call changed from std::rt::lang_start_internal to \
             {lang_start_path}"
        )));
    }

    let internal_calls = direct_calls(
        tcx,
        typing_env,
        &body(tcx, typing_env, *lang_start_internal),
    );
    let outer_catches: Vec<_> = internal_calls
        .into_iter()
        .filter_map(|(call, _)| {
            call.args.types().find_map(|ty| match ty.kind() {
                ty::Closure(def_id, args) => Some((call, *def_id, args)),
                _ => None,
            })
        })
        .collect();
    let [(outer_catch, runtime_closure_def, runtime_closure_args)] = outer_catches.as_slice()
    else {
        return Err(Error::internal(format!(
            "pinned toolchain lang_start_internal MIR expected exactly one closure-typed direct call, found {}",
            outer_catches.len()
        )));
    };
    let outer_catch_path = tcx.def_path_str(outer_catch.def_id());
    if outer_catch_path != "std::panic::catch_unwind" {
        return Err(Error::internal(format!(
            "pinned toolchain lang_start_internal closure call changed from std::panic::catch_unwind \
             to {outer_catch_path}"
        )));
    }
    let runtime_closure = Instance::resolve_closure(
        tcx,
        *runtime_closure_def,
        runtime_closure_args,
        ty::ClosureKind::FnOnce,
    );
    let main_catches: Vec<_> =
        direct_calls(tcx, typing_env, &body(tcx, typing_env, runtime_closure))
            .into_iter()
            .filter(|(call, _)| call.def_id() == outer_catch.def_id())
            .collect();
    let [(main_catch, unwind)] = main_catches.as_slice() else {
        return Err(Error::internal(format!(
            "pinned toolchain lang_start runtime closure expected exactly one main catch call, found {}",
            main_catches.len()
        )));
    };
    if !matches!(unwind, rustc_middle::mir::UnwindAction::Continue) {
        return Err(Error::internal(format!(
            "pinned toolchain main catch call unwind changed from Continue to {unwind:?}"
        )));
    }

    let outer_body = body(tcx, typing_env, *main_catch);
    let internal_calls = direct_calls(tcx, typing_env, &outer_body);
    let [(internal_catch, internal_unwind)] = internal_calls.as_slice() else {
        return Err(Error::internal(format!(
            "pinned toolchain std::panic::catch_unwind MIR expected exactly one direct call, found {}",
            internal_calls.len()
        )));
    };
    let internal_path = tcx.def_path_str(internal_catch.def_id());
    if internal_path != "std::panicking::catch_unwind" {
        return Err(Error::internal(format!(
            "pinned toolchain std::panic::catch_unwind implementation call changed from \
             std::panicking::catch_unwind to {internal_path}"
        )));
    }
    if !matches!(internal_unwind, rustc_middle::mir::UnwindAction::Continue) {
        return Err(Error::internal(format!(
            "pinned toolchain std::panicking::catch_unwind call unwind changed from Continue to \
             {internal_unwind:?}"
        )));
    }

    let internal_body = body(tcx, typing_env, *internal_catch);
    let mut intrinsic_sites = Vec::new();
    for (bb, block) in internal_body.basic_blocks.iter_enumerated() {
        let rustc_middle::mir::TerminatorKind::Call {
            func, args, unwind, ..
        } = &block.terminator().kind
        else {
            continue;
        };
        let ty::FnDef(def_id, generic_args) = func.ty(&internal_body.local_decls, tcx).kind()
        else {
            continue;
        };
        let intrinsic = Instance::expect_resolve(
            tcx,
            typing_env,
            *def_id,
            generic_args,
            block.terminator().source_info.span,
        );
        if !matches!(intrinsic.def, ty::InstanceKind::Intrinsic(_))
            || tcx.item_name(intrinsic.def_id()).as_str() != "catch_unwind"
        {
            continue;
        }
        intrinsic_sites.push((intrinsic, args, *unwind, internal_body.terminator_loc(bb)));
    }
    let [(catch_intrinsic, args, intrinsic_unwind, intrinsic_loc)] = intrinsic_sites.as_slice()
    else {
        return Err(Error::internal(format!(
            "pinned toolchain std::panicking::catch_unwind MIR expected exactly one std \
             catch_unwind intrinsic, found {}",
            intrinsic_sites.len()
        )));
    };
    let intrinsic_path = tcx.def_path_str(catch_intrinsic.def_id());
    if intrinsic_path != "std::intrinsics::catch_unwind" {
        return Err(Error::internal(format!(
            "pinned toolchain catch intrinsic changed from std::intrinsics::catch_unwind to \
             {intrinsic_path}"
        )));
    }
    if args.len() != 3 {
        return Err(Error::internal(format!(
            "pinned toolchain std catch_unwind intrinsic expected 3 arguments, found {}",
            args.len()
        )));
    }
    if !matches!(
        intrinsic_unwind,
        rustc_middle::mir::UnwindAction::Unreachable
    ) {
        return Err(Error::internal(format!(
            "pinned toolchain std catch_unwind intrinsic unwind changed from Unreachable to \
             {intrinsic_unwind:?}"
        )));
    }
    let try_local = operand_local(&args[0].node).ok_or(Error::internal(
        "pinned toolchain std catch_unwind do_call argument no longer comes from a local fn-ptr",
    ))?;
    let catch_local = operand_local(&args[2].node).ok_or(Error::internal(
        "pinned toolchain std catch_unwind do_catch argument no longer comes from a local fn-ptr",
    ))?;
    let do_call = reified_fn(tcx, typing_env, &internal_body, try_local, *intrinsic_loc)
        .map_err(|reason| {
            Error::internal(format!(
                "pinned toolchain std catch_unwind do_call fn-ptr source cannot be confirmed: {reason}"
            ))
        })?;
    let do_catch = reified_fn(tcx, typing_env, &internal_body, catch_local, *intrinsic_loc)
        .map_err(|reason| {
            Error::internal(format!(
                "pinned toolchain std catch_unwind do_catch fn-ptr source cannot be confirmed: {reason}"
            ))
        })?;
    let do_call_path = tcx.def_path_str(do_call.def_id());
    let do_catch_path = tcx.def_path_str(do_catch.def_id());
    if do_call_path != "std::panicking::catch_unwind::do_call"
        || do_catch_path != "std::panicking::catch_unwind::do_catch"
    {
        return Err(Error::internal(format!(
            "pinned toolchain std catch_unwind callbacks changed: try={do_call_path}, \
             catch={do_catch_path}"
        )));
    }

    Ok((
        runtime_closure,
        *main_catch,
        *internal_catch,
        *catch_intrinsic,
    ))
}
