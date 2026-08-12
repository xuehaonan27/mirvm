//! run_blocks 主执行循环（自 interp.rs I14 整搬）：块序列解释（主执行与
//! cleanup 链共用）——Goto/SwitchInt/Call/CallForeign/CallIndirect/
//! CallBuiltin（T1-b 起薄臂：语义体提取至 call::exec_builtin，JIT 助手
//! 共享同一本体）/InlineAsm/Return/Resume/Terminate。
//! edge: Cell<Option<Bb>> 协议与 mod.rs 的 FrameGuard 同侧未拆。

use super::*;
use super::{
    call::{call_guarding_terminate, cleanup_edge, exec_builtin},
    stmt::exec_stmt,
};

pub(super) fn run_blocks(
    ctx: *mut Ctx,
    func: u32,
    base: usize,
    edge: &Cell<Option<Bb>>,
    entry: Bb,
) -> Exit {
    let module: &Module = unsafe { &(*(*ctx).shared).module };
    let body: &FuncBody = &module.funcs[func as usize];

    let mut blk = entry as usize;
    loop {
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
            } => {
                let mut av: Vec<u64> = Vec::with_capacity(aops.len() + 1);
                if let RetDest::Indirect(dst) = ret {
                    av.push(eval_place_addr(ctx, base, dst));
                }
                av.extend(aops.iter().map(|o| eval_operand(ctx, base, o).0));
                edge.set(cleanup_edge(unwind)); // callee 若 panic，本帧从这条边清理
                let (lo, hi) = call_guarding_terminate(unwind, || call_guest(ctx, *callee, &av)); // ← 宿主递归
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
                // M4.4 D1：fn-ptr 实参位——guest fn 条目地址逃逸给 native 前物化 thunk
                // 真码；NULL 与已是 native 真码（反查未命中，guest 转传）原样直传。
                // P1（§7.6）：FFI 可派生条目值本身已是 stub 码址——跳过二次物化。
                for (pos, inner) in &sig.thunk_args {
                    let v = av[*pos];
                    if v != 0
                        && !crate::vm::engine::codearena::is_stub_addr(v)
                        && let Some(&fid) = module.fn_addrs.get(&v)
                    {
                        let shared: &'static Shared = unsafe { &*(*ctx).shared };
                        av[*pos] = crate::vm::engine::thunks::get_or_create(shared, v, fid, inner);
                    }
                }
                edge.set(cleanup_edge(unwind));
                // C1：按值聚合返回 = Indirect 落点（调用点强制），ffi 层 memcpy 至
                // 目的真地址；标量返回照旧走 u64 通道
                let ret_dst = if let RetDest::Indirect(dst) = ret {
                    Some(eval_place_addr(ctx, base, dst))
                } else {
                    None
                };
                // D8a：guest 线程栈放大。解释帧宿主成本数十倍于 native 帧，按 guest
                // attr 原样创建的线程会在远浅于 native 的深度打穿宿主栈（SIGSEGV 而非
                // 诊断）。显式 stacksize（std::thread 恒显式）临时放大，调用后还原；
                // guest 自供栈（setstack）不动。栈尺寸属 unspecified（ram-spec §2）。
                let stack_restore = crate::vm::engine::ffi::amplify_pthread_stack(sym, &av);
                let r = call_guarding_terminate(unwind, || {
                    let ffi = unsafe { &mut (*ctx).ffi };
                    crate::vm::engine::ffi::call(ffi, module, sym, sig, &av, ret_dst)
                });
                if let Some((attr, orig)) = stack_restore {
                    crate::os::thread::attr_set_stack_size(attr, orig);
                }
                edge.set(None);
                let r = r.unwrap_or_else(|reason| {
                    engine_abort(&format!(
                        "foreign `{sym}` 的必需原生库装载失败（fn {}）: {reason}",
                        body.name
                    ))
                });
                let Some(r) = r else {
                    engine_abort(&format!(
                        "foreign `{sym}` 符号不存在（归档兜底表 / dlsym 全域均未命中；fn {}）",
                        body.name
                    ));
                };
                match ret {
                    RetDest::Ignore => {}
                    RetDest::Scalar(p) => place_write(ctx, base, p, r),
                    // C1：按值聚合字节已由 ffi 层 memcpy 至 dst
                    RetDest::Indirect(_) => {}
                    other => engine_abort(&format!("foreign 返回形态 {other:?} 未支持")),
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
                    // dyn 虚 drop 空槽：无 Drop 的类型 = 空操作
                    blk = *target as usize;
                    continue;
                }
                if addr == 0 {
                    // extern weak 符号缺席取址 = NULL（native 同语义）；调用空
                    // fn-ptr 在 native 是 UB/SIGSEGV——VM 响亮诊断而非宿主崩溃。
                    engine_abort(&format!("间接调用空 fn 指针（调用者 {}）", body.name));
                }
                let mut av: Vec<u64> = Vec::with_capacity(aops.len() + 1);
                if let RetDest::Indirect(dst) = ret {
                    av.push(eval_place_addr(ctx, base, dst));
                }
                av.extend(aops.iter().map(|o| eval_operand(ctx, base, o).0));
                edge.set(cleanup_edge(unwind));
                let (lo, hi) = if let Some(&fid) = module.fn_addrs.get(&addr) {
                    call_guarding_terminate(unwind, || call_guest(ctx, fid, &av))
                } else if let Some(nsig) = native_sig {
                    // FFI 反方向之二（M4.4）：guest 持 native 真码 fn ptr（运行期
                    // dlsym 所得，如 __pthread_get_minstack）→ 按冻结签名直调。
                    // C1：native_sig 聚合返回时首槽即目的地址（调用点已按
                    // RetDest::Indirect 压栈；libffi sret 不占参数位，剔除后直调）
                    let (ret_dst, arg_slice) = if matches!(nsig.ret, FfiKind::Agg(_)) {
                        (av.first().copied(), &av[1..])
                    } else {
                        (None, &av[..])
                    };
                    (
                        call_guarding_terminate(unwind, || {
                            crate::vm::engine::ffi::call_addr(
                                addr as usize,
                                nsig,
                                arg_slice,
                                ret_dst,
                            )
                        }),
                        0,
                    )
                } else {
                    engine_abort(&format!(
                        "间接调用目标 {addr:#x} 不是已知 fn 条目（调用者 {}）",
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
            } => {
                // T1-b：语义体提取至 exec_builtin（call.rs；JIT mirvm_call_builtin/
                // mirvm_alloc 助手共享同一本体）——本臂只余实参/ret_dst 求值与
                // 写回。builtin 无 sret 前插：RetDest::Indirect 落点独立求值
                //（x86 向量 lane 的 sret 真地址），edge 协议在本体内随体保留。
                let av: Vec<u64> = args.iter().map(|o| eval_operand(ctx, base, o).0).collect();
                let ret_dst = if let RetDest::Indirect(dst) = ret {
                    Some(eval_place_addr(ctx, base, dst))
                } else {
                    None
                };
                let (lo, hi) = exec_builtin(ctx, body, edge, builtin, &av, ret_dst, unwind);
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
                // asm-stub（M5.0 corpus §2.2 三面孔）：栈开 buf、按 ins 装槽、call
                // wrapper（fn(*mut u8)，rbx=buf 基址）、按 outs 取槽。三面孔无 unwind。
                #[repr(align(16))]
                struct AsmBuf([u8; 256]);
                let mut buf = AsmBuf([0u8; 256]);
                if *buf_size as usize > buf.0.len() {
                    engine_abort(&format!(
                        "asm 缓冲 {buf_size} 超上限 {}（fn {}）",
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
                        // 批10：向量字节通道（xmm/ymm/zmm 16/32/64B 全宽拷贝）
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
                let addr = module.asm_stub_addrs[*stub as usize];
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
                // region 恢复由 FrameGuard 统一（正常/unwind 两路径一致）
                return Exit::Ret(r.0, r.1);
            }
            Terminator::Resume => return Exit::Resume,
            Terminator::TerminateAbort => {
                eprintln!("mirvm[m4-engine]: UnwindTerminate（double panic/ABI 边界）——abort");
                std::process::abort()
            }
            Terminator::Unreachable => {
                engine_abort(&format!("到达 Unreachable（fn {}）", body.name))
            }
            Terminator::Trap(reason) => {
                engine_abort(&format!("TRAP: {reason}（fn {}）", body.name))
            }
        }
    }
}
