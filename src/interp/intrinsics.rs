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
        // volatile 读写：协作式单线程下等同普通读写（无编译器重排、每 step 原子）。
        // copy_op 处理任意布局（标量/标量对/聚合）。
        "volatile_load" | "unaligned_volatile_load" => {
            let ptr = ecx.read_pointer(&args[0])?;
            let pointee = args[0].layout.ty.builtin_deref(true).unwrap();
            let layout = ecx.layout_of(pointee)?;
            let src = ecx.ptr_to_mplace(ptr, layout);
            ecx.copy_op(&src, dest)?;
            ecx.return_to_block(ret)?;
            return interp_ok(None);
        }
        "volatile_store" | "unaligned_volatile_store" => {
            let ptr = ecx.read_pointer(&args[0])?;
            let dst = ecx.ptr_to_mplace(ptr, args[1].layout);
            ecx.copy_op(&args[1], &dst)?;
            ecx.return_to_block(ret)?;
            return interp_ok(None);
        }
        _ => {}
    }

    // 3.5) libm 系数学 intrinsic（powf64/sqrtf64/sinf64/...）：复用 shims 的宿主直算表。
    //      float 数学 intrinsic 是普遍缺口（任何数值代码都碰），一次性桥接。
    if try_math_intrinsic(ecx, name, args, dest)?.is_some() {
        ecx.return_to_block(ret)?;
        return interp_ok(None);
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

/// float 数学 intrinsic → 宿主直算。名带显式位宽（`powf64`/`sqrtf32`），
/// 翻译成 libm 命名后复用 shims::emulate_libm；powi/fma 无 libm 对应，特殊处理。
/// 返回 None = 非数学 intrinsic（调用方继续走后续路径）。
fn try_math_intrinsic<'tcx>(
    ecx: &mut MirvmInterpCx<'tcx>,
    name: &str,
    args: &[OpTy<'tcx, Prov>],
    dest: &PlaceTy<'tcx, Prov>,
) -> InterpResult<'tcx, Option<()>> {
    use rustc_apfloat::Float as _;
    let (base, is_f32) = if let Some(b) = name.strip_suffix("f64") {
        (b, false)
    } else if let Some(b) = name.strip_suffix("f32") {
        (b, true)
    } else {
        return interp_ok(None);
    };

    let rd = |ecx: &mut MirvmInterpCx<'tcx>, i: usize| -> InterpResult<'tcx, f64> {
        let s = ecx.read_scalar(&args[i])?;
        interp_ok(if is_f32 {
            f32::from_bits(s.to_f32()?.to_bits() as u32) as f64
        } else {
            f64::from_bits(s.to_f64()?.to_bits() as u64)
        })
    };

    // powi（整数幂）/ fma（a*b+c）：libm 表里没有，直接算。
    if let "powi" | "fma" | "fmuladd" = base {
        let a = rd(ecx, 0)?;
        let r = match base {
            "powi" => a.powi(ecx.read_scalar(&args[1])?.to_i32()?),
            _ => a.mul_add(rd(ecx, 1)?, rd(ecx, 2)?),
        };
        let scalar = if is_f32 {
            Scalar::from_f32(rustc_apfloat::ieee::Single::from_bits((r as f32).to_bits() as u128))
        } else {
            Scalar::from_f64(rustc_apfloat::ieee::Double::from_bits(r.to_bits() as u128))
        };
        ecx.write_scalar(scalar, dest)?;
        return interp_ok(Some(()));
    }

    // 其余翻译成 libm 名（少数别名），复用 shims 的直算表。
    let libm = match base {
        "minnum" => "fmin",
        "maxnum" => "fmax",
        "roundeven" => "rint",
        b => b,
    };
    let libm_name = if is_f32 { format!("{libm}f") } else { libm.to_string() };
    if super::shims::emulate_libm(ecx, &libm_name, args, dest)?.is_some() {
        interp_ok(Some(()))
    } else {
        interp_ok(None)
    }
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
