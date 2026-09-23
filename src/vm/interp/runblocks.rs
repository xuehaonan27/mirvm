//! The `run_blocks` execution loop, shared by normal execution and cleanup chains:
//! Goto/SwitchInt/Call/CallForeign/CallIndirect/CallBuiltin/InlineAsm/Return/Resume/
//! Terminate. CallBuiltin is a thin arm here; its semantics live in
//! `semantics::builtin::exec_builtin`, which the JIT helpers share so the two backends cannot
//! drift.
//!
//! `edge` hands the current cleanup target to `interp_frame`'s raw catch boundary.

use super::stmt::exec_stmt;
use super::*;
use crate::vm::ir::CallRole;
use crate::vm::semantics::builtin::exec_builtin;
use crate::vm::unwind::guarding_terminate;

pub(super) fn run_blocks(
    ctx: *mut Ctx,
    func: u32,
    base: usize,
    edge: &Cell<Option<Bb>>,
    entry: Bb,
) -> Exit {
    let module: &Module = unsafe { &(*(*ctx).shared).module };
    let instance: &Instance = unsafe { &(*(*ctx).shared).instance };
    let body: &FuncBody = &module.funcs[func as usize];

    let mut blk = entry as usize;
    loop {
        crate::vm::ctx::drain_pending_signals(ctx);
        let block: &Block = &body.blocks[blk];
        for stmt in &block.stmts {
            exec_stmt(ctx, base, stmt);
        }
        match &block.term {
            Terminator::Goto(t) => blk = *t as usize,
            Terminator::SwitchInt {
                discr,
                targets,
                otherwise,
            } => {
                let d = match discr {
                    SwitchDiscr::Scalar(discr) => eval_operand(ctx, base, discr).0 as u128,
                    SwitchDiscr::Wide(discr) => {
                        let addr = eval_place_addr(ctx, base, discr);
                        unsafe { (addr as *const u128).read_unaligned() }
                    }
                };
                blk = targets
                    .iter()
                    .find(|(v, _)| *v == d)
                    .map(|(_, b)| *b)
                    .unwrap_or(*otherwise) as usize;
            }
            Terminator::Call {
                callee,
                args: aops,
                ret,
                target,
                unwind,
                role,
            } => {
                let mut av: Vec<u64> = Vec::with_capacity(aops.len() + 1);
                if let RetDest::Indirect(dst) = ret {
                    av.push(eval_place_addr(ctx, base, dst));
                }
                av.extend(aops.iter().map(|o| eval_operand(ctx, base, o).0));
                // If the callee panics, this frame cleans up along this edge.
                edge.set(unwind.cleanup_edge());
                let call = || guarding_terminate(unwind, || call_guest(ctx, *callee, &av));
                let (lo, hi) = match role {
                    CallRole::Normal => call(),
                    CallRole::MainPanicBoundary => {
                        crate::vm::ctx::call_main_panic_boundary(ctx, call)
                    }
                };
                edge.set(None);
                match ret {
                    RetDest::Ignore | RetDest::Indirect(_) => {}
                    RetDest::Scalar(p) => place_write(ctx, base, p, lo),
                    RetDest::Pair(pl, ph) => {
                        place_write(ctx, base, pl, lo);
                        place_write(ctx, base, ph, hi);
                    }
                }
                blk = *target as usize;
            }
            Terminator::CallForeign {
                sym,
                sig,
                args: aops,
                ret,
                target,
                unwind,
            } => {
                let mut av: Vec<u64> = aops.iter().map(|o| eval_operand(ctx, base, o).0).collect();
                let shared: &'static Shared = unsafe { &*(*ctx).shared };
                let callbacks =
                    crate::vm::thunks::prepare_foreign_callbacks(shared, sym, sig, &mut av);
                edge.set(unwind.cleanup_edge());
                // C1: a by-value aggregate return uses the Indirect destination (enforced at
                // the call site) and the FFI layer memcpys to that true address, while a
                // scalar return keeps the u64 channel.
                let ret_dst = if let RetDest::Indirect(dst) = ret {
                    Some(eval_place_addr(ctx, base, dst))
                } else {
                    None
                };
                // Guest thread stack amplification: an interpreted frame costs the host tens
                // of times what a native frame does, so a thread created from the guest attr
                // as-is would blow through the host stack at a far shallower depth than
                // native, giving SIGSEGV instead of a diagnostic. Enlarge the explicit
                // stacksize temporarily around the call and restore it afterwards; a
                // guest-owned stack (setstack) is left alone. The stack size is unspecified.
                let stack_restore = crate::vm::ffi::amplify_pthread_stack(sym, &av);
                // Symbol resolution mutates the per-thread FFI cache, so this mutable borrow
                // must end before entering native: native can synchronously call back into the
                // guest, and that callback can call foreign again on the same Ctx.
                let resolved = guarding_terminate(unwind, || {
                    let ffi = unsafe { &mut (*ctx).ffi };
                    ffi.resolve(
                        sym,
                        &module.native_libs,
                        &module.required_native_libs,
                        &instance.native_images,
                        &instance.mc_images,
                    )
                });
                let r = match resolved {
                    Ok(Some(fnptr)) => Ok(Some(guarding_terminate(unwind, || {
                        crate::vm::ffi::call_addr(fnptr, sig, &av, ret_dst)
                    }))),
                    Ok(None) => Ok(None),
                    Err(reason) => Err(reason),
                };
                if let Some((attr, orig)) = stack_restore {
                    crate::os::thread::attr_set_stack_size(attr, orig);
                }
                edge.set(None);
                let r = r.unwrap_or_else(|reason| {
                    engine_abort(&format!(
                        "failed to load a required native library for foreign `{sym}` (fn {}): {reason}",
                        body.name
                    ))
                });
                let Some(r) = r else {
                    engine_abort(&format!(
                        "foreign `{sym}` symbol not found (neither the archive fallback table nor a full-domain dlsym matched; fn {})",
                        body.name
                    ));
                };
                callbacks.complete(r);
                match ret {
                    RetDest::Ignore => {}
                    RetDest::Scalar(p) => place_write(ctx, base, p, r),
                    // C1: the by-value aggregate bytes were already memcpy'd to dst by the FFI layer.
                    RetDest::Indirect(_) => {}
                    other => engine_abort(&format!("unsupported foreign return form {other:?}")),
                }
                blk = *target as usize;
            }
            Terminator::CallIndirect {
                callee,
                args: aops,
                ret,
                target,
                unwind,
                null_ok,
                native_sig,
            } => {
                let (addr, _) = eval_operand(ctx, base, callee);
                if *null_ok && addr == 0 {
                    // Empty slot of a dyn virtual drop: a type without Drop is a no-op.
                    blk = *target as usize;
                    continue;
                }
                if addr == 0 {
                    // Taking the address of an absent extern weak symbol yields NULL, as under
                    // native; calling a null fn pointer is UB/SIGSEGV under native, so the VM
                    // diagnoses loudly instead of crashing the host.
                    engine_abort(&format!(
                        "indirect call through a null fn pointer (caller {})",
                        body.name
                    ));
                }
                let mut av: Vec<u64> = Vec::with_capacity(aops.len() + 1);
                if let RetDest::Indirect(dst) = ret {
                    av.push(eval_place_addr(ctx, base, dst));
                }
                av.extend(aops.iter().map(|o| eval_operand(ctx, base, o).0));
                edge.set(unwind.cleanup_edge());
                let (lo, hi) = if let Some(&fid) = instance.fn_addrs.get(&addr) {
                    guarding_terminate(unwind, || call_guest(ctx, fid, &av))
                } else if let Some(nsig) = native_sig {
                    // The reverse FFI direction: the guest holds a native fn pointer to real
                    // code (obtained at runtime through dlsym, e.g. __pthread_get_minstack), so
                    // call it directly with the frozen signature. C1: for an aggregate return
                    // the first slot is the destination (the call site pushed it for
                    // RetDest::Indirect); libffi's sret takes no argument slot, so drop it
                    // before the direct call.
                    let (ret_dst, arg_slice) = if matches!(nsig.ret, FfiKind::Agg(_)) {
                        (av.first().copied(), &av[1..])
                    } else {
                        (None, &av[..])
                    };
                    (
                        guarding_terminate(unwind, || {
                            crate::vm::ffi::call_addr(addr as usize, nsig, arg_slice, ret_dst)
                        }),
                        0,
                    )
                } else {
                    engine_abort(&format!(
                        "indirect call target {addr:#x} is not a known fn entry (caller {})",
                        body.name
                    ));
                };
                edge.set(None);
                match ret {
                    RetDest::Ignore | RetDest::Indirect(_) => {}
                    RetDest::Scalar(p) => place_write(ctx, base, p, lo),
                    RetDest::Pair(pl, ph) => {
                        place_write(ctx, base, pl, lo);
                        place_write(ctx, base, ph, hi);
                    }
                }
                blk = *target as usize;
            }
            Terminator::CallBuiltin {
                builtin,
                args,
                ret,
                target,
                unwind,
                role,
            } => {
                // Thin arm: the semantics live in exec_builtin (shared with the JIT
                // mirvm_call_builtin/mirvm_alloc helpers), leaving only argument and ret_dst
                // evaluation plus write-back here. A builtin has no leading sret slot, so a
                // RetDest::Indirect destination (the sret true address of an x86 vector lane)
                // is evaluated separately. The edge protocol stays inside the body.
                let av: Vec<u64> = args.iter().map(|o| eval_operand(ctx, base, o).0).collect();
                let ret_dst = if let RetDest::Indirect(dst) = ret {
                    Some(eval_place_addr(ctx, base, dst))
                } else {
                    None
                };
                let (lo, hi) = exec_builtin(ctx, body, edge, builtin, &av, ret_dst, unwind, *role);
                match ret {
                    RetDest::Ignore | RetDest::Indirect(_) => {}
                    RetDest::Scalar(p) => place_write(ctx, base, p, lo),
                    RetDest::Pair(pl, ph) => {
                        place_write(ctx, base, pl, lo);
                        place_write(ctx, base, ph, hi);
                    }
                }
                blk = *target as usize;
            }
            Terminator::InlineAsm {
                stub,
                buf_size,
                ins,
                outs,
                target,
            } => {
                // asm stub: open a buffer on the stack, store the `ins` slots, call the wrapper
                // (fn(*mut u8), rbx = buffer base), then read the `outs` slots. The stub never
                // unwinds.
                #[repr(align(16))]
                struct AsmBuf([u8; 256]);
                let mut buf = AsmBuf([0u8; 256]);
                if *buf_size as usize > buf.0.len() {
                    engine_abort(&format!(
                        "asm buffer {buf_size} exceeds the limit {} (fn {})",
                        buf.0.len(),
                        body.name
                    ));
                }
                let bufp = buf.0.as_mut_ptr();
                for (off, op) in ins {
                    match op {
                        AsmIoVal::Scalar(o) => {
                            let (v, _) = eval_operand(ctx, base, o);
                            unsafe {
                                std::ptr::write_unaligned(bufp.add(*off as usize) as *mut u64, v)
                            };
                        }
                        // Vector byte channel: a full-width 16/32/64 B copy for xmm/ymm/zmm.
                        AsmIoVal::VecBytes(pe, size) => {
                            let src = eval_place_addr(ctx, base, pe);
                            unsafe {
                                std::ptr::copy_nonoverlapping(
                                    src as *const u8,
                                    bufp.add(*off as usize),
                                    *size as usize,
                                )
                            };
                        }
                    }
                }
                let addr = instance.asm_stub_addrs[*stub as usize];
                let f: unsafe extern "C" fn(*mut u8) =
                    unsafe { std::mem::transmute::<u64, unsafe extern "C" fn(*mut u8)>(addr) };
                unsafe { f(bufp) };
                for (off, dst) in outs {
                    match dst {
                        AsmIoDst::Scalar(sp) => {
                            let v = unsafe {
                                std::ptr::read_unaligned(bufp.add(*off as usize) as *const u64)
                            };
                            place_write(ctx, base, sp, v);
                        }
                        AsmIoDst::VecBytes(pe, size) => {
                            let dst_addr = eval_place_addr(ctx, base, pe);
                            unsafe {
                                std::ptr::copy_nonoverlapping(
                                    bufp.add(*off as usize) as *const u8,
                                    dst_addr as *mut u8,
                                    *size as usize,
                                )
                            };
                        }
                    }
                }
                blk = *target as usize;
            }
            Terminator::Return => {
                crate::vm::ctx::drain_pending_signals(ctx);
                let r = match body.ret {
                    RetAbi::Zst => (0, 0),
                    RetAbi::Scalar(rs) => (slot_read(ctx, base, rs), 0),
                    RetAbi::Pair(lo, hi) => (slot_read(ctx, base, lo), slot_read(ctx, base, hi)),
                    RetAbi::Indirect {
                        ret_off,
                        size,
                        sret_off,
                    } => {
                        let dst = slot_read(
                            ctx,
                            base,
                            Slot {
                                off: sret_off,
                                width: Width::W64,
                            },
                        );
                        unsafe {
                            std::ptr::copy_nonoverlapping(
                                (base as u64 + ret_off as u64) as *const u8,
                                dst as *mut u8,
                                size as usize,
                            )
                        };
                        (0, 0)
                    }
                };
                // FrameGuard restores the region uniformly on both the normal and unwind paths.
                return Exit::Ret(r.0, r.1);
            }
            Terminator::Resume => return Exit::Resume,
            Terminator::TerminateAbort => {
                eprintln!("mirvm[m4-engine]: UnwindTerminate (double panic/ABI boundary) -- abort");
                std::process::abort()
            }
            Terminator::Unreachable => {
                engine_abort(&format!("reached Unreachable (fn {})", body.name))
            }
            Terminator::Trap(reason) => engine_abort(&format!("TRAP: {reason} (fn {})", body.name)),
        }
    }
}
