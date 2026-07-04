//! intrinsics：core 引擎兜底 + 原子操作（单线程语义）+ catch_unwind。
//! 参考 rust-lang/miri intrinsics/*（MIT/Apache-2.0）。

use rustc_const_eval::interpret::{
    Immediate, InterpResult, MPlaceTy, OpTy, PlaceTy, ReturnContinuation, interp_ok,
};
use rustc_middle::mir::interpret::Scalar;
use rustc_middle::{mir, throw_machine_stop, throw_unsup_format, ty};

use super::helpers::EcxExt as _;
use super::machine::{CatchUnwindData, Prov, Termination};
use super::MirvmInterpCx;

pub fn call_intrinsic<'tcx>(
    ecx: &mut MirvmInterpCx<'tcx>,
    instance: ty::Instance<'tcx>,
    args: &[OpTy<'tcx, Prov>],
    dest: &PlaceTy<'tcx, Prov>,
    ret: Option<mir::BasicBlock>,
    unwind: mir::UnwindAction,
) -> InterpResult<'tcx, Option<ty::Instance<'tcx>>> {
    // 1) core 引擎已实现的一大票（copy/ctpop/discriminant/size_of_val/...）
    if ecx.eval_intrinsic(instance, args, dest, ret)? {
        return interp_ok(None);
    }
    let name = ecx.tcx.item_name(instance.def_id());
    let name = name.as_str();

    // 2) 原子操作：单线程直译
    if let Some(op) = name.strip_prefix("atomic_") {
        emulate_atomic(ecx, op, args, dest)?;
        ecx.return_to_block(ret)?;
        return interp_ok(None);
    }

    // 3) 运行时特有
    match name {
        "catch_unwind" => {
            handle_catch_unwind(ecx, args, dest, ret)?;
            return interp_ok(None);
        }
        "abort" => {
            throw_machine_stop!(Termination::Abort(
                "程序执行了 abort() intrinsic".to_string()
            ));
        }
        "breakpoint" => {
            throw_machine_stop!(Termination::Abort("程序触发了 breakpoint".to_string()));
        }
        // 有效性断言：fast machine 假设程序合法，直接跳过（合法程序永不触发）
        "assert_inhabited" | "assert_zero_valid" | "assert_mem_uninitialized_valid" => {
            ecx.return_to_block(ret)?;
            return interp_ok(None);
        }
        _ => {}
    }

    // 4) fallback body（rustc 给很多 intrinsic 配了参考实现）
    if !ecx.tcx.intrinsic(instance.def_id()).unwrap().must_be_overridden {
        return interp_ok(Some(ty::Instance {
            def: ty::InstanceKind::Item(instance.def_id()),
            args: instance.args,
        }));
    }

    let _ = unwind;
    throw_unsup_format!("mirvm: 尚未实现的 intrinsic `{name}`");
}

/// catch_unwind(try_fn, data, catch_fn) -> i32（0 正常 / 1 捕获到 panic）。
/// 恢复数据挂在 try-fn 的栈帧上；unwinding 弹到该帧时由 after_stack_pop 接管。
fn handle_catch_unwind<'tcx>(
    ecx: &mut MirvmInterpCx<'tcx>,
    args: &[OpTy<'tcx, Prov>],
    dest: &PlaceTy<'tcx, Prov>,
    ret: Option<mir::BasicBlock>,
) -> InterpResult<'tcx> {
    let try_fn = ecx.read_pointer(&args[0])?;
    let data = ecx.read_immediate(&args[1])?;
    let catch_fn = ecx.read_pointer(&args[2])?;

    // dest 固化到内存（跨帧写入需要稳定位置）
    let dest: MPlaceTy<'tcx, Prov> = ecx.force_allocation(dest)?;

    let f = ecx.get_ptr_fn(try_fn)?.as_instance()?;
    ecx.call_function(
        f,
        &[data.clone()],
        None,
        ReturnContinuation::Goto { ret, unwind: mir::UnwindAction::Continue },
    )?;

    // 默认返回 0（无 panic）；发生 unwind 时 after_stack_pop 改写为 1
    ecx.write_scalar(Scalar::from_uint(0u128, dest.layout.size), &dest)?;

    // 恢复数据挂在刚压入的 try-fn 帧上
    ecx.frame_mut().extra.catch_unwind =
        Some(CatchUnwindData { catch_fn, data, dest, ret });
    interp_ok(())
}

/// 单线程原子操作：忽略 ordering，直接读写内存。
fn emulate_atomic<'tcx>(
    ecx: &mut MirvmInterpCx<'tcx>,
    op: &str,
    args: &[OpTy<'tcx, Prov>],
    dest: &PlaceTy<'tcx, Prov>,
) -> InterpResult<'tcx> {
    use mir::BinOp;

    // args[0] 是 *mut T：解出指向的 place
    let place = |ecx: &MirvmInterpCx<'tcx>| -> InterpResult<'tcx, MPlaceTy<'tcx, Prov>> {
        let ptr = ecx.read_pointer(&args[0])?;
        let pointee = args[0].layout.ty.builtin_deref(true).unwrap();
        let layout = ecx.layout_of(pointee)?;
        interp_ok(ecx.ptr_to_mplace(ptr, layout))
    };

    match op {
        "load" => {
            let val = ecx.read_immediate(&place(ecx)?)?;
            ecx.write_immediate(*val, dest)?;
        }
        "store" => {
            let val = ecx.read_immediate(&args[1])?;
            ecx.write_immediate(*val, &place(ecx)?)?;
        }
        "fence" | "singlethreadfence" => {}
        "xchg" => {
            let p = place(ecx)?;
            let old = ecx.read_immediate(&p)?;
            let new = ecx.read_immediate(&args[1])?;
            ecx.write_immediate(*new, &p)?;
            ecx.write_immediate(*old, dest)?;
        }
        "cxchg" | "cxchgweak" => {
            let p = place(ecx)?;
            let old = ecx.read_immediate(&p)?;
            let expect = ecx.read_immediate(&args[1])?;
            let new = ecx.read_immediate(&args[2])?;
            let eq = ecx.binary_op(BinOp::Eq, &old, &expect)?.to_scalar().to_bool()?;
            if eq {
                ecx.write_immediate(*new, &p)?;
            }
            ecx.write_immediate(
                Immediate::ScalarPair(old.to_scalar(), Scalar::from_bool(eq)),
                dest,
            )?;
        }
        "or" | "xor" | "and" | "nand" | "xadd" | "xsub" => {
            let p = place(ecx)?;
            let old = ecx.read_immediate(&p)?;
            let rhs = ecx.read_immediate(&args[1])?;
            let bin = match op {
                "or" => BinOp::BitOr,
                "xor" => BinOp::BitXor,
                "and" | "nand" => BinOp::BitAnd,
                "xadd" => BinOp::Add,
                "xsub" => BinOp::Sub,
                _ => unreachable!(),
            };
            let mut res = ecx.binary_op(bin, &old, &rhs)?;
            if op == "nand" {
                res = ecx.unary_op(mir::UnOp::Not, &res)?;
            }
            ecx.write_immediate(*res, &p)?;
            ecx.write_immediate(*old, dest)?;
        }
        "max" | "umax" | "min" | "umin" => {
            let p = place(ecx)?;
            let old = ecx.read_immediate(&p)?;
            let rhs = ecx.read_immediate(&args[1])?;
            let lt = ecx.binary_op(BinOp::Lt, &old, &rhs)?.to_scalar().to_bool()?;
            let take_rhs = match op {
                "max" | "umax" => lt,
                "min" | "umin" => !lt,
                _ => unreachable!(),
            };
            if take_rhs {
                ecx.write_immediate(*rhs, &p)?;
            }
            ecx.write_immediate(*old, dest)?;
        }
        _ => throw_unsup_format!("mirvm: 尚未实现的原子 intrinsic `atomic_{op}`"),
    }
    interp_ok(())
}
