//! Call family (moved whole from func.rs F12): lower_virtual_call/finish_call/
//! untuple_rust_call_arg/finish_call_inner — Callee three-form dispatch, FFI aggregate,
//! sret, track_caller. Single entry point = the Call arm of term.rs.

use super::*;

impl<'tcx> LowerCx<'tcx, '_> {
    /// dyn virtual dispatch (InstanceKind::Virtual): receiver fat pointer (data, vtable),
    /// callee = *(vtable + idx*8) (vtable materialized in step 4, slot holds real D4 fn entry address),
    /// receiver actual arg swapped to data half (&dyn → &Concrete thinned).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn lower_virtual_call(
        &mut self,
        idx: usize,
        loc_arg: Option<Operand>,
        rust_call: bool,
        args: &[rustc_span::Spanned<mir::Operand<'tcx>>],
        destination: &mir::Place<'tcx>,
        target: Option<mir::BasicBlock>,
        unwind: mir::UnwindAction,
    ) -> Result<(Vec<Stmt>, Terminator), String> {
        let (data, vt) = match self.lower_operand(&args[0].node) {
            Ok(LoweredOp::Pair(data, vt)) => (data, vt),
            // by-value dyn dispatch (isomorphic to cg_ssa Ref(PlaceValue{llextra:Some(meta)}) arm):
            // receiver is a move of an unsized dyn place (inside Box<dyn FnOnce>::call_once
            // `F::call_once(move (*self))`) — data = real place address, callee looked up via place meta
            // in the vtable slot (slot contains ShimKind::VTable shim: takes *mut Self thin pointer then moves out,
            // this nightly instance.rs resolve_for_vtable). Same shape as dyn Drop.
            _ => {
                let Some(pl) = args[0].node.place() else {
                    return Err("dyn receiver is neither fat pointer nor place".into());
                };
                let p = self.resolve_place(&pl)?;
                if !matches!(p.ty.kind(), ty::Dynamic(..)) {
                    return Err(format!("dyn receiver shape unknown (ty={})", p.ty));
                }
                let meta = p
                    .meta
                    .clone()
                    .ok_or_else(|| format!("by-value dyn receiver has no meta (ty={})", p.ty))?;
                (Operand::AddrOf(p.expr()), meta)
            }
        };
        let callee = operand_deref_at(vt, (idx * 8) as u32)?;
        self.finish_call_inner(
            CallTarget::Indirect(callee, None),
            loc_arg,
            rust_call,
            Some(data),
            args,
            destination,
            target,
            unwind,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn finish_call(
        &mut self,
        ct: CallTarget,
        loc_arg: Option<Operand>,
        rust_call: bool,
        args: &[rustc_span::Spanned<mir::Operand<'tcx>>],
        destination: &mir::Place<'tcx>,
        target: Option<mir::BasicBlock>,
        unwind: mir::UnwindAction,
    ) -> Result<(Vec<Stmt>, Terminator), String> {
        self.finish_call_inner(
            ct,
            loc_arg,
            rust_call,
            None,
            args,
            destination,
            target,
            unwind,
        )
    }

    /// rust-call ABI tail tuple arg unpacking at call site (isomorphic to cg_ssa codegen_arguments_untupled):
    /// Physical convention = field flattening (closure body MIR params already unpacked; shim spread_arg unfolds symmetrically).
    pub(super) fn untuple_rust_call_arg(
        &mut self,
        op: &mir::Operand<'tcx>,
        out: &mut Vec<Operand>,
    ) -> Result<(), String> {
        let ty = self.op_ty(op)?;
        let ty::Tuple(fields) = ty.kind() else {
            return Err(format!("rust-call tail arg is not a tuple ({ty})"));
        };
        let layout = self.layout_of(ty)?;
        // empty/all-ZST tuple (including const forms like `(fn_item,)`): no fields to pass
        if fields.is_empty() || layout.is_zst() {
            return Ok(());
        }
        // place projectable by field; const tuple (e.g. `(None,)` promoted const) maps fields by overall classification
        let p = if let Some(pl) = op.place() {
            self.resolve_place(&pl)?
        } else {
            match self.lower_operand(op)? {
                LoweredOp::Zst => return Ok(()),
                // Indirect const already materialized in frozen region → has place, project fields as usual
                LoweredOp::Bytes { place, .. } => place,
                // whole scalar = the sole non-ZST field is the whole (ZST fields skipped on both sides)
                LoweredOp::Scalar(o) => {
                    out.push(o);
                    return Ok(());
                }
                // whole pair: both halves claimed by offset (tuple fields may be reordered), emitted in field order
                LoweredOp::Pair(a, b) => {
                    let ValKind::Pair((ao, _), (bo, _)) = self.classify(ty)? else {
                        return Err(format!("const tuple classification drift ({ty})"));
                    };
                    let (mut ha, mut hb) = (Some(a), Some(b));
                    for (i, fty) in fields.iter().enumerate() {
                        if matches!(self.classify(fty)?, ValKind::Zst) {
                            continue;
                        }
                        let off = layout.fields.offset(i).bytes() as u32;
                        if off == ao
                            && let Some(x) = ha.take()
                        {
                            out.push(x);
                        } else if off == bo
                            && let Some(x) = hb.take()
                        {
                            out.push(x);
                        }
                    }
                    if ha.is_some() || hb.is_some() {
                        return Err(format!("const pair tuple field claim failed ({ty})"));
                    }
                    return Ok(());
                }
            }
        };
        for (i, fty) in fields.iter().enumerate() {
            let off = layout.fields.offset(i).bytes() as u32;
            match self.classify(fty)? {
                ValKind::Zst => {}
                ValKind::Scalar(w) => out.push(p.half_operand(off, w)),
                ValKind::Pair((ao, aw), (bo, bw)) => {
                    out.push(p.half_operand(off + ao, aw));
                    out.push(p.half_operand(off + bo, bw));
                }
                // aggregate field: pass real field address (callee side ParamAbi::Indirect memcpy reassembly)
                ValKind::Other { .. } => out.push(Operand::AddrOf(p.expr_plus(off))),
            }
        }
        Ok(())
    }

    /// Common call finish path: args flattened (ABI v2, fail-fast Trap to protect call edge) → four return destinations
    /// → diverging destination synthesized → terminator emitted by CallTarget. loc_arg = track_caller hidden tail arg.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn finish_call_inner(
        &mut self,
        ct: CallTarget,
        loc_arg: Option<Operand>,
        rust_call: bool,
        first_override: Option<Operand>,
        args: &[rustc_span::Spanned<mir::Operand<'tcx>>],
        destination: &mir::Place<'tcx>,
        target: Option<mir::BasicBlock>,
        unwind: mir::UnwindAction,
    ) -> Result<(Vec<Stmt>, Terminator), String> {
        // variadic foreign: tail FfiKind frozen by call-site actual args (tail position aggregate = C1 boundary, loud reject)
        let variadic_foreign = matches!(
            &ct,
            CallTarget::Direct(Callee::Foreign { variadic: true, .. })
        );
        let mut tail_kinds: Vec<ir::FfiKind> = Vec::new();
        let mut pre: Vec<Stmt> = Vec::new();
        let mut ir_args = Vec::new();
        // C1: foreign/native_sig looks up FfiKind by position — aggregate args are not split into scalar/pair slots,
        // instead takes real place address (libffi avalue points directly to contiguous bytes)
        let ffi_arg_kinds: Option<&[ir::FfiKind]> = match &ct {
            CallTarget::Direct(Callee::Foreign { args: fixed, .. }) => Some(fixed),
            CallTarget::Indirect(_, Some(sig)) => Some(&sig.args),
            _ => None,
        };
        for (i, a) in args.iter().enumerate() {
            if i == 0
                && let Some(o) = &first_override
            {
                ir_args.push(o.clone());
                continue;
            }
            // rust-call ABI: tail tuple unpacked field by field (physical convention = field flattening)
            if rust_call && i == args.len() - 1 {
                if let Err(e) = self.untuple_rust_call_arg(&a.node, &mut ir_args) {
                    pre.push(Stmt::Trap(
                        format!("rust-call tail arg: {e}").into_boxed_str(),
                    ));
                    ir_args.clear();
                }
                continue;
            }
            let agg_pos = matches!(
                ffi_arg_kinds.and_then(|ks| ks.get(i)),
                Some(ir::FfiKind::Agg(_))
            );
            match self.lower_operand(&a.node) {
                Ok(LoweredOp::Zst) => {}
                Ok(LoweredOp::Scalar(o)) if !agg_pos => {
                    if variadic_foreign {
                        let t = self.op_ty(&a.node)?;
                        let k = crate::lower::ffi_kind_of(self.tcx, self.typing_env, t)
                            .map_err(|e| format!("variadic arg {t}: {e}"))?;
                        if matches!(k, ir::FfiKind::Agg(_)) {
                            return Err(format!(
                                "variadic tail arg passed by-value aggregate ({t}, C1 boundary)"
                            ));
                        }
                        tail_kinds.push(k);
                    }
                    ir_args.push(o);
                }
                Ok(LoweredOp::Pair(l, h)) if !agg_pos => {
                    ir_args.push(l);
                    ir_args.push(h);
                }
                Ok(LoweredOp::Bytes { place, .. }) => {
                    // F-07: variadic fixed aggregate must also occupy tail_kinds — end drains by nfixed
                    // position alignment drain, missing slots misclassify real tail arg types
                    if variadic_foreign {
                        let k = ffi_arg_kinds
                            .and_then(|ks| ks.get(i))
                            .expect("aggregate arg position must have FfiKind")
                            .clone();
                        tail_kinds.push(k);
                    }
                    ir_args.push(Operand::AddrOf(place.expr()));
                }
                Ok(LoweredOp::Scalar(_) | LoweredOp::Pair(..)) => {
                    // C1 by-value aggregate arg (≤16B split into scalar/pair slots by lower_operand):
                    // this MIR actual arg is a place (move/copy), the whole value lives in contiguous guest-frame memory —
                    // directly hand real place address to libffi avalue
                    if variadic_foreign {
                        // F-07: same tail_kinds position occupancy as above
                        let k = ffi_arg_kinds
                            .and_then(|ks| ks.get(i))
                            .expect("aggregate arg position must have FfiKind")
                            .clone();
                        tail_kinds.push(k);
                    }
                    let Some(pl) = a.node.place() else {
                        pre.push(Stmt::Trap(
                            "C1 by-value aggregate arg is not a place (const expansion not wired)"
                                .to_string()
                                .into_boxed_str(),
                        ));
                        ir_args.clear();
                        break;
                    };
                    let dp = match self.resolve_place(&pl) {
                        Ok(dp) => dp,
                        Err(e) => {
                            pre.push(Stmt::Trap(
                                format!("C1 arg destination: {e}").into_boxed_str(),
                            ));
                            ir_args.clear();
                            break;
                        }
                    };
                    ir_args.push(Operand::AddrOf(dp.expr()));
                }
                Err(e) => {
                    pre.push(Stmt::Trap(format!("call arg: {e}").into_boxed_str()));
                    ir_args.clear();
                    break;
                }
            }
        }
        // #[track_caller]: &Location hidden tail arg (isomorphic to cg_ssa, placed last after flattening)
        if pre.is_empty()
            && let Some(loc) = loc_arg
        {
            ir_args.push(loc);
        }
        // return destination (ABI v2 four-way)
        let ret = if pre.is_empty() {
            match self.resolve_place(destination).and_then(|dp| {
                let kind = self.classify(dp.ty)?;
                Ok(match kind {
                    ValKind::Zst => RetDest::Ignore,
                    ValKind::Scalar(w) => RetDest::Scalar(dp.scalar_place(w)),
                    ValKind::Pair((ao, aw), (bo, bw)) => {
                        RetDest::Pair(dp.half_place(ao, aw), dp.half_place(bo, bw))
                    }
                    ValKind::Other { .. } => RetDest::Indirect(dp.expr()),
                })
            }) {
                Ok(r) => r,
                Err(e) => {
                    pre.push(Stmt::Trap(
                        format!("call return destination: {e}").into_boxed_str(),
                    ));
                    RetDest::Ignore
                }
            }
        } else {
            RetDest::Ignore
        };
        // C1: by-value aggregate return of foreign/native_sig forced to Indirect destination (libffi writes struct bytes
        // into result buffer, interp layer memcpy to dst) — ≤16B pair case
        // and >16B sret case unified through this path
        let ret = if matches!(
            &ct,
            CallTarget::Direct(Callee::Foreign {
                ret: ir::FfiKind::Agg(_),
                ..
            }) | CallTarget::Indirect(
                _,
                Some(ir::ForeignSig {
                    ret: ir::FfiKind::Agg(_),
                    ..
                })
            )
        ) {
            match self.resolve_place(destination) {
                Ok(dp) => RetDest::Indirect(dp.expr()),
                Err(e) => {
                    pre.push(Stmt::Trap(
                        format!("C1 return destination: {e}").into_boxed_str(),
                    ));
                    RetDest::Ignore
                }
            }
        } else {
            ret
        };
        // diverging call (target=None) → synthesize Unreachable destination block
        let tgt = match target {
            Some(b) => b.as_u32(),
            None => {
                let idx = (self.mir_block_count + self.extra_blocks.len()) as Bb;
                self.extra_blocks.push(ir::Block {
                    stmts: vec![],
                    term: Terminator::Unreachable,
                });
                idx
            }
        };
        let unwind = self.lower_unwind(unwind);
        let term = match ct {
            CallTarget::Direct(Callee::Func(id)) => {
                let role = self
                    .linker
                    .main_catch_site
                    .filter(|site| {
                        site.boundary_caller == self.instance && site.boundary_callee == id
                    })
                    .map_or(ir::CallRole::Normal, |_| ir::CallRole::MainPanicBoundary);
                Terminator::Call {
                    callee: id,
                    args: ir_args,
                    ret,
                    target: tgt,
                    unwind,
                    role,
                }
            }
            CallTarget::Direct(Callee::Builtin(b)) => Terminator::CallBuiltin {
                builtin: b,
                args: ir_args,
                ret,
                target: tgt,
                unwind,
                role: ir::BuiltinCallRole::Normal,
            },
            CallTarget::Direct(Callee::Foreign {
                sym,
                args: fixed,
                ret: fret,
                variadic,
                thunk_args,
                unwind: ffi_unwind,
            }) => {
                let sig = if variadic {
                    let nfixed = fixed.len();
                    let mut all = fixed;
                    all.extend(tail_kinds.drain(nfixed.min(tail_kinds.len())..));
                    ir::ForeignSig {
                        args: all,
                        ret: fret,
                        fixed: Some(nfixed),
                        thunk_args,
                        unwind: ffi_unwind,
                    }
                } else {
                    ir::ForeignSig {
                        args: fixed,
                        ret: fret,
                        fixed: None,
                        thunk_args,
                        unwind: ffi_unwind,
                    }
                };
                Terminator::CallForeign {
                    sym,
                    sig,
                    args: ir_args,
                    ret,
                    target: tgt,
                    unwind,
                }
            }
            CallTarget::Indirect(callee, native_sig) => Terminator::CallIndirect {
                callee,
                args: ir_args,
                ret,
                target: tgt,
                unwind,
                null_ok: false,
                native_sig,
            },
        };
        Ok((pre, term))
    }
}
