//! 求值入口：搭 InterpCx、构造进程内存（argv/envp）、跑 .init_array 全局构造器、
//! 调用 `fn main`、驱动 step 循环、翻译退出码。
//!
//! M2 起 args/env 已转正（.init_array 里 std 的 ARGV 捕获构造器会真实执行）。
//! 仍然直接调 main 而非 `start` lang item——剩余偏差（记录于 DESIGN.md）：
//! - 主线程名为 "<unnamed>"（panic 消息头与 native 的 "main" 不同）
//! - 进程退出时不跑 rt cleanup（print! 的残留缓冲会丢；println! 不受影响）

use rustc_abi::{Align, Size};
use rustc_const_eval::interpret::{
    AllocInit, FnVal, ImmTy, InterpCx, MPlaceTy, MemoryKind, Pointer, ReturnContinuation,
    interp_ok,
};
use rustc_middle::mir::interpret::{InterpErrorKind, Scalar};
use rustc_middle::ty::layout::FnAbiOf;
use rustc_middle::ty::{self, TyCtxt};
use rustc_span::Symbol;
use rustc_span::def_id::DefId;

use super::helpers::{EcxExt as _, InterpResult};
use super::machine::{MPtr, MirvmMachine, MirvmMemoryKind, Prov, Termination};

pub struct EvalConfig {
    /// 被解释程序的 argv（argv[0] = 脚本/二进制路径）
    pub argv: Vec<String>,
}

type Ecx<'tcx> = InterpCx<'tcx, MirvmMachine<'tcx>>;

/// 解释执行 entry fn，返回进程退出码（错误自行打印）。
pub fn eval_main(tcx: TyCtxt<'_>, entry_def_id: DefId, config: EvalConfig) -> i32 {
    let typing_env = ty::TypingEnv::fully_monomorphized();
    let mut ecx = InterpCx::new(tcx, rustc_span::DUMMY_SP, typing_env, MirvmMachine::new(tcx));

    let res = (|| -> InterpResult<'_, ()> {
        let (argc, argv, envp) = setup_process_memory(&mut ecx, &config)?;
        run_global_ctors(&mut ecx, argc, argv, envp)?;

        // 直接调 main（M2 仍不走 start lang item）
        let instance = ty::Instance::mono(tcx, entry_def_id);
        let unit = ecx.layout_of(tcx.types.unit)?;
        let ret_place = MPlaceTy::fake_alloc_zst(unit);
        ecx.call_function(
            instance,
            &[],
            Some(&ret_place),
            ReturnContinuation::Stop { cleanup: true },
        )?;
        run_scheduler(&mut ecx)?;
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
                    // "unwinding past the topmost frame"。native 语义 = 退出码 101。
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

fn print_stacktrace<'tcx>(ecx: &Ecx<'tcx>) {
    for (i, frame) in ecx.generate_stacktrace().into_iter().enumerate().take(16) {
        eprintln!("  [{i}] {frame:?}");
    }
}

// ===== 协作式调度主循环（策略层；语义层见 threads.rs）=====

fn run_scheduler<'tcx>(ecx: &mut Ecx<'tcx>) -> InterpResult<'tcx, ()> {
    use super::threads::{MAIN_THREAD, STEPS_PER_SLICE, Schedule, ThreadState};

    loop {
        let need_switch = {
            let t = &ecx.machine.threads;
            t.yield_requested
                || t.steps_in_slice >= STEPS_PER_SLICE
                || t.active().state != ThreadState::Runnable
        };
        if need_switch {
            match ecx.machine.threads.schedule() {
                Schedule::Run => {}
                Schedule::Done => return interp_ok(()), // main 返回 = 进程结束（native 语义）
                Schedule::SleepUntil(at, due) => {
                    let now = std::time::Instant::now();
                    if at > now {
                        std::thread::sleep(at - now);
                    }
                    let futex_timeouts = ecx.machine.threads.wake_due(&due);
                    for tid in futex_timeouts {
                        futex_timeout_result(ecx, tid)?;
                    }
                    continue;
                }
                Schedule::Deadlock => {
                    return super::threads::deadlock_error(&ecx.machine.threads);
                }
            }
        }
        match ecx.step()? {
            true => ecx.machine.threads.steps_in_slice += 1,
            false => {
                // active 线程根帧已返回
                let tid = ecx.machine.threads.active_id();
                if tid != MAIN_THREAD && run_one_tls_dtor(ecx)? {
                    continue; // 压了一个 TLS 析构帧，继续跑本线程
                }
                ecx.machine.threads.on_thread_terminated(tid);
                // main 退出：native 语义 = 进程结束（其余线程随之消亡）
                if tid == MAIN_THREAD {
                    return interp_ok(());
                }
            }
        }
    }
}

/// futex 等待超时：把推测写入的 0 改成 -1，并给该线程置 ETIMEDOUT。
fn futex_timeout_result<'tcx>(
    ecx: &mut Ecx<'tcx>,
    tid: super::threads::ThreadId,
) -> InterpResult<'tcx, ()> {
    let Some(wake) = ecx.machine.threads.get_mut(tid).unwrap().futex_wake.take() else {
        return interp_ok(());
    };
    ecx.write_scalar(
        Scalar::from_int(-1i128, wake.dest.layout.size),
        &wake.dest,
    )?;
    super::shims::set_thread_errno(ecx, tid, libc::ETIMEDOUT)?;
    interp_ok(())
}

/// 运行一个 pthread key 析构（glibc 轮次语义的简化版）。返回是否压了新帧。
/// 不变式：pthread_tls 里只存非空值（setspecific(null) 即删除）。
fn run_one_tls_dtor<'tcx>(ecx: &mut Ecx<'tcx>) -> InterpResult<'tcx, bool> {
    let candidate = {
        let mgr = &ecx.machine.threads;
        let th = mgr.active();
        th.pthread_tls
            .iter()
            .find_map(|(k, &v)| mgr.key_dtors.get(k).copied().flatten().map(|d| (*k, v, d)))
    };
    let Some((key, val, dtor_ptr)) = candidate else {
        return interp_ok(false);
    };
    ecx.machine.threads.active_mut().pthread_tls.remove(&key);
    let instance = ecx.get_ptr_fn(dtor_ptr)?.as_instance()?;
    let arg_layout =
        ecx.layout_of(ty::Ty::new_mut_ptr(ecx.tcx.tcx, ecx.tcx.types.u8))?;
    let arg = ImmTy::from_scalar(val, arg_layout);
    ecx.call_function(instance, &[arg], None, ReturnContinuation::Stop { cleanup: true })?;
    interp_ok(true)
}

// ===== 进程内存：argv / envp / extern statics =====

/// 在解释器内存里布置 argv 与 envp 表，初始化 extern statics。
/// 返回 (argc, argv表指针, envp表指针)。
fn setup_process_memory<'tcx>(
    ecx: &mut Ecx<'tcx>,
    config: &EvalConfig,
) -> InterpResult<'tcx, (u64, MPtr, MPtr)> {
    // argv：C 字符串数组
    let arg_ptrs: Vec<MPtr> = config
        .argv
        .iter()
        .map(|a| alloc_c_string(ecx, a.as_bytes()))
        .collect::<InterpResult<'tcx, _>>()?;
    let argv_table = alloc_ptr_table(ecx, &arg_ptrs)?;

    // envp：透传宿主环境（"K=V" C 字符串数组）；同时建 getenv 的查找表
    use std::os::unix::ffi::OsStrExt;
    let mut env_ptrs = Vec::new();
    for (k, v) in std::env::vars_os() {
        let (k, v) = (k.as_bytes(), v.as_bytes());
        if k.contains(&0) || v.contains(&0) || k.contains(&b'=') {
            continue;
        }
        let mut kv = Vec::with_capacity(k.len() + 1 + v.len());
        kv.extend_from_slice(k);
        kv.push(b'=');
        kv.extend_from_slice(v);
        let entry = alloc_c_string(ecx, &kv)?;
        // getenv 返回值指向 '=' 之后
        let value_ptr = entry.wrapping_offset(Size::from_bytes(k.len() as u64 + 1), ecx);
        ecx.machine.env_map.insert(k.to_vec(), value_ptr);
        env_ptrs.push(entry);
    }
    let envp_table = alloc_ptr_table(ecx, &env_ptrs)?;

    // extern statics
    let cptr = ecx.layout_of(ty::Ty::new_imm_ptr(ecx.tcx.tcx, ecx.tcx.types.u8))?;
    // environ 的值 = envp 表（glibc 语义）
    let environ_val = ImmTy::from_scalar(Scalar::from_pointer(envp_table, ecx), cptr);
    alloc_extern_static(ecx, "environ", environ_val)?;
    // weak 符号：getrandom/gettid/strlen 提供合成函数指针；
    // statx 置 NULL 让 std 走 fstat 系回退（真实现太重）
    for name in ["getrandom", "gettid", "strlen"] {
        let fnptr = ecx.fn_ptr(FnVal::Other(Symbol::intern(name)));
        let val = ImmTy::from_scalar(Scalar::from_pointer(fnptr, ecx), cptr);
        alloc_extern_static(ecx, name, val)?;
    }
    let null = ImmTy::from_scalar(Scalar::from_target_usize(0, ecx), cptr);
    // statx：NULL → std 走 fstat 系回退（真实现太重）
    // __cxa_thread_atexit_impl：NULL → std 用纯 Rust 的 TLS 析构回退表
    // pidfd_spawnp：NULL → std 的 posix_spawn 回退到不带 pidfd 的普通路径
    for name in ["statx", "__cxa_thread_atexit_impl", "pidfd_spawnp"] {
        alloc_extern_static(ecx, name, null.clone())?;
    }

    interp_ok((config.argv.len() as u64, argv_table, envp_table))
}

/// 分配并写入以 NUL 结尾的字节串，返回首地址。
fn alloc_c_string<'tcx>(ecx: &mut Ecx<'tcx>, bytes: &[u8]) -> InterpResult<'tcx, MPtr> {
    let ptr = ecx.allocate_ptr(
        Size::from_bytes(bytes.len() as u64 + 1),
        Align::ONE,
        MemoryKind::Machine(MirvmMemoryKind::Machine),
        AllocInit::Zero,
    )?;
    ecx.write_bytes_ptr(ptr.into(), bytes.iter().copied())?;
    interp_ok(ptr)
}

/// 分配 NULL 结尾的指针表。
fn alloc_ptr_table<'tcx>(ecx: &mut Ecx<'tcx>, ptrs: &[MPtr]) -> InterpResult<'tcx, MPtr> {
    let psize = ecx.tcx.data_layout.pointer_size();
    let table = ecx.allocate_ptr(
        psize * (ptrs.len() as u64 + 1),
        ecx.tcx.data_layout.pointer_align().abi,
        MemoryKind::Machine(MirvmMemoryKind::Machine),
        AllocInit::Zero, // 末位 NULL 由零初始化保证
    )?;
    let cptr = ecx.layout_of(ty::Ty::new_imm_ptr(ecx.tcx.tcx, ecx.tcx.types.u8))?;
    for (i, p) in ptrs.iter().enumerate() {
        let slot = ecx.ptr_to_mplace(table.wrapping_offset(psize * (i as u64), ecx).into(), cptr);
        ecx.write_scalar(Scalar::from_pointer(*p, ecx), &slot)?;
    }
    interp_ok(table)
}

fn alloc_extern_static<'tcx>(
    ecx: &mut Ecx<'tcx>,
    name: &str,
    val: ImmTy<'tcx, Prov>,
) -> InterpResult<'tcx, ()> {
    let place = ecx.allocate(val.layout, MemoryKind::Machine(MirvmMemoryKind::Machine))?;
    ecx.write_immediate(*val, &place)?;
    let ptr = place.ptr().into_pointer_or_addr().expect("extern static 必须有 provenance");
    ecx.machine.extern_statics.insert(Symbol::intern(name), ptr);
    interp_ok(())
}

// ===== .init_array 全局构造器 =====

/// 收集并执行 .init_array 构造器（std 的 ARGV/envp 捕获靠它）。
/// glibc 约定构造器签名为 `extern "C" fn(argc, argv, envp)`，也兼容零参形式。
fn run_global_ctors<'tcx>(
    ecx: &mut Ecx<'tcx>,
    argc: u64,
    argv: MPtr,
    envp: MPtr,
) -> InterpResult<'tcx, ()> {
    let tcx = ecx.tcx.tcx;

    // 收集：所有已链接 crate 里 link_section == .init_array* 的静态量
    let mut ctor_ptrs: Vec<Pointer<Option<Prov>>> = Vec::new();
    let mut collect = |ecx: &mut Ecx<'tcx>, def_id: DefId| -> InterpResult<'tcx, ()> {
        let attrs = tcx.codegen_fn_attrs(def_id);
        let Some(section) = attrs.link_section else { return interp_ok(()) };
        if !section.as_str().starts_with(".init_array") {
            return interp_ok(());
        }
        let instance = ty::Instance::mono(tcx, def_id);
        let val = ecx.eval_global(instance)?;
        match val.layout.ty.kind() {
            ty::FnPtr(..) => {
                let imm = ecx.read_immediate(&val)?;
                ctor_ptrs.push(imm.to_scalar().to_pointer(ecx)?);
            }
            ty::Array(elem, _) if matches!(elem.kind(), ty::FnPtr(..)) => {
                let mut elems = ecx.project_array_fields(&val)?;
                while let Some((_, e)) = elems.next(ecx)? {
                    let imm = ecx.read_immediate(&e)?;
                    ctor_ptrs.push(imm.to_scalar().to_pointer(ecx)?);
                }
            }
            _ => {}
        }
        interp_ok(())
    };
    super::shims::for_each_linked_def(tcx, |def_id| collect(ecx, def_id))?;

    // 执行：每个构造器跑到栈空
    for ptr in ctor_ptrs {
        let instance = ecx.get_ptr_fn(ptr)?.as_instance()?;
        let nargs = ecx.fn_abi_of_instance(instance, ty::List::empty())?.args.len();
        let args: Vec<ImmTy<'tcx, Prov>> = if nargs == 3 {
            let i32_layout = ecx.layout_of(ecx.tcx.types.i32)?;
            let cptr = ecx.layout_of(ty::Ty::new_imm_ptr(ecx.tcx.tcx, ecx.tcx.types.u8))?;
            vec![
                ImmTy::from_int(argc as i128, i32_layout),
                ImmTy::from_scalar(Scalar::from_pointer(argv, ecx), cptr),
                ImmTy::from_scalar(Scalar::from_pointer(envp, ecx), cptr),
            ]
        } else {
            vec![]
        };
        ecx.call_function(instance, &args, None, ReturnContinuation::Stop { cleanup: true })?;
        while ecx.step()? {}
    }
    interp_ok(())
}
