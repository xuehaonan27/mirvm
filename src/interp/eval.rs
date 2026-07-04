//! 求值入口：搭 InterpCx、直接调用 `fn main`、驱动 step 循环、翻译退出码。
//!
//! M1 取舍：不走 `start` lang item（rt::init 需要 mmap/sigaltstack/poll 等一票
//! shims），直接调用 main。已知偏差（记录于 DESIGN.md）：
//! - std::env::args() 为空（.init_array 未执行）
//! - 主线程名为 "<unnamed>"（panic 消息头与 native 的 "main" 不同）
//! - 进程退出时不 flush 行缓冲之外的 stdout 残留

use rustc_const_eval::interpret::{
    InterpCx, MPlaceTy, MemoryKind, ReturnContinuation, interp_ok,
};
use rustc_middle::mir::interpret::InterpErrorKind;
use rustc_middle::ty::{self, TyCtxt};
use rustc_span::Symbol;
use rustc_span::def_id::DefId;

use super::helpers::EcxExt as _;
use super::machine::{MirvmMachine, MirvmMemoryKind, Termination};

/// 解释执行 entry fn，返回进程退出码（错误自行打印）。
pub fn eval_main(tcx: TyCtxt<'_>, entry_def_id: DefId) -> i32 {
    let typing_env = ty::TypingEnv::fully_monomorphized();
    let mut ecx = InterpCx::new(
        tcx,
        rustc_span::DUMMY_SP,
        typing_env,
        MirvmMachine::new(tcx),
    );

    let res = (|| -> super::helpers::InterpResult<'_, ()> {
        setup_extern_statics(&mut ecx)?;

        // 直接调 main（M1；M2 改走 start lang item 以获得完整 rt 语义）
        let instance = ty::Instance::mono(tcx, entry_def_id);
        let unit = ecx.layout_of(tcx.types.unit)?;
        let ret_place = MPlaceTy::fake_alloc_zst(unit);
        ecx.call_function(
            instance,
            &[],
            Some(&ret_place),
            ReturnContinuation::Stop { cleanup: true },
        )?;

        // 主循环
        while ecx.step()? {}
        interp_ok(())
    })();

    match res.report_err() {
        Ok(()) => 0,
        Err(err) => {
            let (kind, backtrace) = err.into_parts();
            backtrace.print_backtrace();
            match kind {
                InterpErrorKind::MachineStop(info) => {
                    let info = info.downcast_ref::<Termination>().expect("非 mirvm 的 MachineStop");
                    match info {
                        Termination::Exit(code) => *code,
                        Termination::Abort(msg) => {
                            eprintln!("mirvm: {msg}");
                            134 // SIGABRT 语义
                        }
                        Termination::Unsupported(msg) => {
                            eprintln!("{msg}");
                            print_stacktrace(&ecx);
                            1
                        }
                    }
                }
                kind => {
                    // 直接调 main 的设计下，panic 穿出 main = 引擎的
                    // "unwinding past the topmost frame"。native 语义（lang_start
                    // 捕获后）= 退出码 101，消息已由 panic hook 打印。
                    let msg = format!("{kind:?}");
                    if msg.contains("unwinding past the topmost frame") {
                        return 101;
                    }
                    eprintln!("mirvm: 解释器错误: {kind:?}");
                    print_stacktrace(&ecx);
                    1
                }
            }
        }
    }
}

fn print_stacktrace<'tcx>(ecx: &InterpCx<'tcx, MirvmMachine<'tcx>>) {
    for (i, frame) in ecx.generate_stacktrace().into_iter().enumerate().take(16) {
        eprintln!("  [{i}] {frame:?}");
    }
}

/// 机器提供的 extern statics：
/// - environ：指向空表（getenv → 全部 None）
/// - weak 符号（getrandom/gettid/statx/strlen）：值为合成函数指针，
///   调用时按符号名走 shims（Miri weak_symbol_extern_statics 同款）
fn setup_extern_statics<'tcx>(
    ecx: &mut InterpCx<'tcx, MirvmMachine<'tcx>>,
) -> super::helpers::InterpResult<'tcx, ()> {
    use rustc_const_eval::interpret::{FnVal, ImmTy};
    use rustc_middle::mir::interpret::Scalar;

    let cptr = ecx.layout_of(rustc_middle::ty::Ty::new_imm_ptr(ecx.tcx.tcx, ecx.tcx.types.u8))?;

    // environ：一个只含 NULL 的表
    let table = ecx.allocate(cptr, MemoryKind::Machine(MirvmMemoryKind::Machine))?;
    ecx.write_scalar(Scalar::from_target_usize(0, ecx), &table)?;
    let environ_val = ImmTy::from_scalar(Scalar::from_maybe_pointer(table.ptr(), ecx), cptr);
    alloc_extern_static(ecx, "environ", environ_val)?;

    // Linux weak 符号（std 会探测这些）
    for name in ["getrandom", "gettid", "statx", "strlen"] {
        let fnptr = ecx.fn_ptr(FnVal::Other(Symbol::intern(name)));
        let val = ImmTy::from_scalar(Scalar::from_pointer(fnptr, ecx), cptr);
        alloc_extern_static(ecx, name, val)?;
    }
    interp_ok(())
}

fn alloc_extern_static<'tcx>(
    ecx: &mut InterpCx<'tcx, MirvmMachine<'tcx>>,
    name: &str,
    val: rustc_const_eval::interpret::ImmTy<'tcx, super::machine::Prov>,
) -> super::helpers::InterpResult<'tcx, ()> {
    let place = ecx.allocate(val.layout, MemoryKind::Machine(MirvmMemoryKind::Machine))?;
    ecx.write_immediate(*val, &place)?;
    let ptr = place.ptr().into_pointer_or_addr().expect("extern static 必须有 provenance");
    ecx.machine.extern_statics.insert(Symbol::intern(name), ptr);
    interp_ok(())
}
