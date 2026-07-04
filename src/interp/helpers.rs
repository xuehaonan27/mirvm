//! InterpCx<MirvmMachine> 的扩展方法（Miri helpers/panic shims 的精简移植）。

use rustc_abi::Size;
use rustc_const_eval::interpret::{
    FnArg, ImmTy, MPlaceTy, Pointer, ReturnContinuation, interp_ok,
};
use rustc_middle::mir;
use rustc_middle::mir::interpret::Scalar;
use rustc_middle::ty::{self, Instance};
use rustc_middle::ty::layout::FnAbiOf;

use super::machine::Prov;
use super::MirvmInterpCx;

pub type InterpResult<'tcx, T = ()> = rustc_const_eval::interpret::InterpResult<'tcx, T>;

pub trait EcxExt<'tcx> {
    /// 压栈调用一个函数（参数必须是标量/立即数）。dest 为 None 时要求返回 ()。
    fn call_function(
        &mut self,
        f: Instance<'tcx>,
        args: &[ImmTy<'tcx, Prov>],
        dest: Option<&MPlaceTy<'tcx, Prov>>,
        cont: ReturnContinuation,
    ) -> InterpResult<'tcx>;

    /// 以 `&str` 消息调用 panic lang item（可 unwind）。
    fn start_panic(&mut self, msg: &str, unwind: mir::UnwindAction) -> InterpResult<'tcx>;

    /// 以 `&str` 消息调用 panic_nounwind lang item。
    fn start_panic_nounwind(&mut self, msg: &str) -> InterpResult<'tcx>;

    /// Assert 终结符触发 panic：转发到对应 lang item。
    fn assert_panic_impl(
        &mut self,
        msg: &mir::AssertMessage<'tcx>,
        unwind: mir::UnwindAction,
    ) -> InterpResult<'tcx>;

    /// 把 MPlace 变成 `&T` 立即数（胖/瘦引用均可）。
    fn mplace_to_ref(&self, mplace: &MPlaceTy<'tcx, Prov>) -> InterpResult<'tcx, ImmTy<'tcx, Prov>>;

    /// 宿主字节串 → 解释器内的 `&str`（用于 panic 消息）。
    fn alloc_str_ref(&mut self, s: &str) -> InterpResult<'tcx, ImmTy<'tcx, Prov>>;
}

impl<'tcx> EcxExt<'tcx> for MirvmInterpCx<'tcx> {
    fn call_function(
        &mut self,
        f: Instance<'tcx>,
        args: &[ImmTy<'tcx, Prov>],
        dest: Option<&MPlaceTy<'tcx, Prov>>,
        cont: ReturnContinuation,
    ) -> InterpResult<'tcx> {
        let body = self.load_mir(f.def, None)?;
        let dest = match dest {
            Some(dest) => dest.clone(),
            None => MPlaceTy::fake_alloc_zst(self.layout_of(self.tcx.types.unit)?),
        };
        // 用被调方自身的 ABI 充当 caller ABI：我们构造的实参与形参一一对应。
        let fn_abi = self.fn_abi_of_instance(f, ty::List::empty())?;
        let args: Vec<FnArg<'tcx, Prov>> =
            args.iter().map(|a| FnArg::Copy(a.clone().into())).collect();
        // track_caller 函数的 ABI 末尾有 Location 参数：引擎不真传，
        // caller_location intrinsic 靠走栈取（见 rustc call.rs init_stack_frame）。
        let with_caller_location = f.def.requires_caller_location(*self.tcx);
        self.init_stack_frame(f, body, fn_abi, &args, with_caller_location, &dest.into(), cont)
    }

    fn start_panic(&mut self, msg: &str, unwind: mir::UnwindAction) -> InterpResult<'tcx> {
        let msg = self.alloc_str_ref(msg)?;
        let panic = self.tcx.lang_items().panic_fn().unwrap();
        let panic = Instance::mono(self.tcx.tcx, panic);
        self.call_function(panic, &[msg], None, ReturnContinuation::Goto { ret: None, unwind })
    }

    fn start_panic_nounwind(&mut self, msg: &str) -> InterpResult<'tcx> {
        let msg = self.alloc_str_ref(msg)?;
        let panic = self.tcx.lang_items().panic_nounwind().unwrap();
        let panic = Instance::mono(self.tcx.tcx, panic);
        self.call_function(
            panic,
            &[msg],
            None,
            ReturnContinuation::Goto { ret: None, unwind: mir::UnwindAction::Unreachable },
        )
    }

    fn assert_panic_impl(
        &mut self,
        msg: &mir::AssertMessage<'tcx>,
        unwind: mir::UnwindAction,
    ) -> InterpResult<'tcx> {
        use rustc_middle::mir::AssertKind::*;
        match msg {
            BoundsCheck { index, len } => {
                let index = self.read_immediate(&self.eval_operand(index, None)?)?;
                let len = self.read_immediate(&self.eval_operand(len, None)?)?;
                let f = self.tcx.lang_items().panic_bounds_check_fn().unwrap();
                let f = Instance::mono(self.tcx.tcx, f);
                self.call_function(
                    f,
                    &[index, len],
                    None,
                    ReturnContinuation::Goto { ret: None, unwind },
                )
            }
            MisalignedPointerDereference { required, found } => {
                let required = self.read_immediate(&self.eval_operand(required, None)?)?;
                let found = self.read_immediate(&self.eval_operand(found, None)?)?;
                let f = self.tcx.lang_items().panic_misaligned_pointer_dereference_fn().unwrap();
                let f = Instance::mono(self.tcx.tcx, f);
                self.call_function(
                    f,
                    &[required, found],
                    None,
                    ReturnContinuation::Goto { ret: None, unwind },
                )
            }
            _ => {
                let f = self.tcx.require_lang_item(msg.panic_function(), self.tcx.span);
                let f = Instance::mono(self.tcx.tcx, f);
                self.call_function(f, &[], None, ReturnContinuation::Goto { ret: None, unwind })
            }
        }
    }

    fn mplace_to_ref(&self, mplace: &MPlaceTy<'tcx, Prov>) -> InterpResult<'tcx, ImmTy<'tcx, Prov>> {
        let imm = mplace.to_ref(self);
        let layout = self.layout_of(rustc_middle::ty::Ty::new_imm_ref(
            self.tcx.tcx,
            self.tcx.lifetimes.re_erased,
            mplace.layout.ty,
        ))?;
        interp_ok(ImmTy::from_immediate(imm, layout))
    }

    fn alloc_str_ref(&mut self, s: &str) -> InterpResult<'tcx, ImmTy<'tcx, Prov>> {
        let mplace = self.allocate_str_dedup(s)?;
        self.mplace_to_ref(&mplace)
    }
}

/// 便捷：向标量 place 写整数。
pub fn write_uint<'tcx>(
    ecx: &mut MirvmInterpCx<'tcx>,
    v: u128,
    dest: &rustc_const_eval::interpret::PlaceTy<'tcx, Prov>,
) -> InterpResult<'tcx> {
    ecx.write_scalar(Scalar::from_uint(v, dest.layout.size), dest)
}

/// 便捷：写指针。
pub fn write_ptr<'tcx>(
    ecx: &mut MirvmInterpCx<'tcx>,
    ptr: Pointer<Prov>,
    dest: &rustc_const_eval::interpret::PlaceTy<'tcx, Prov>,
) -> InterpResult<'tcx> {
    ecx.write_scalar(Scalar::from_maybe_pointer(Pointer::new(Some(ptr.provenance), ptr.into_raw_parts().1), ecx), dest)
}

/// 便捷：Size 常用转换。
pub fn size(bytes: u64) -> Size {
    Size::from_bytes(bytes)
}
