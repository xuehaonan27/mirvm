use super::*;
use super::{stmt::exec_stmt, call::{cleanup_edge, call_guarding_terminate, call_fn_addr}, services::{signal_thunk, unwind_backtrace, atexit_register, AtexitKind}};

pub(super) fn run_blocks(ctx: *mut Ctx, func: u32, base: usize, edge: &Cell<Option<Bb>>, entry: Bb) -> Exit {
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
                let optional_libs: &[Box<str>] = &module.native_libs;
                let required_libs: &[Box<str>] = &module.required_native_libs;
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
                let r = {
                    let ffi = unsafe { &mut (*ctx).ffi };
                    crate::vm::engine::ffi::call(ffi, optional_libs, required_libs, sym, sig, &av, ret_dst)
                };
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
                        crate::vm::engine::ffi::call_addr(addr as usize, nsig, arg_slice, ret_dst),
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
                use crate::vm::engine::ir::Builtin;
                let a = |i: usize| eval_operand(ctx, base, &args[i]).0;
                edge.set(cleanup_edge(unwind)); // RaiseException 经此发起 unwind
                // x86 向量 intrinsic：参数是 indirect 向量地址，返回落到 sret place。
                // helper 本身带 target_feature，guest 的正常 CPUID 派发负责可达性。
                let vector_done = match builtin {
                    Builtin::X86Pshufb128 => {
                        let RetDest::Indirect(dst) = ret else {
                            engine_abort("pshufb128 返回形态不是 indirect vector");
                        };
                        let dst = eval_place_addr(ctx, base, dst) as *mut u8;
                        unsafe { crate::arch::x86_64::pshufb128(dst, a(0) as *const u8, a(1) as *const u8) };
                        true
                    }
                    Builtin::X86Pshufb256 => {
                        let RetDest::Indirect(dst) = ret else {
                            engine_abort("pshufb256 返回形态不是 indirect vector");
                        };
                        let dst = eval_place_addr(ctx, base, dst) as *mut u8;
                        unsafe { crate::arch::x86_64::pshufb256(dst, a(0) as *const u8, a(1) as *const u8) };
                        true
                    }
                    Builtin::X86Sha256Msg1 | Builtin::X86Sha256Msg2 => {
                        let RetDest::Indirect(dst) = ret else {
                            engine_abort("sha256msg 返回形态不是 indirect vector");
                        };
                        let dst = eval_place_addr(ctx, base, dst) as *mut u8;
                        unsafe {
                            if matches!(builtin, Builtin::X86Sha256Msg1) {
                                crate::arch::x86_64::sha256msg1(dst, a(0) as *const u8, a(1) as *const u8);
                            } else {
                                crate::arch::x86_64::sha256msg2(dst, a(0) as *const u8, a(1) as *const u8);
                            }
                        }
                        true
                    }
                    Builtin::X86Sha256Rnds2 => {
                        let RetDest::Indirect(dst) = ret else {
                            engine_abort("sha256rnds2 返回形态不是 indirect vector");
                        };
                        let dst = eval_place_addr(ctx, base, dst) as *mut u8;
                        unsafe {
                            crate::arch::x86_64::sha256rnds2(
                                dst,
                                a(0) as *const u8,
                                a(1) as *const u8,
                                a(2) as *const u8,
                            );
                        }
                        true
                    }
                    Builtin::X86PsadBw128 | Builtin::X86PsadBw256 => {
                        let RetDest::Indirect(dst) = ret else {
                            engine_abort("psad.bw 返回形态不是 indirect vector");
                        };
                        let dst = eval_place_addr(ctx, base, dst) as *mut u8;
                        unsafe {
                            if matches!(builtin, Builtin::X86PsadBw128) {
                                crate::arch::x86_64::psad_bw128(dst, a(0) as *const u8, a(1) as *const u8);
                            } else {
                                crate::arch::x86_64::psad_bw256(dst, a(0) as *const u8, a(1) as *const u8);
                            }
                        }
                        true
                    }
                    Builtin::X86Pclmulqdq => {
                        let RetDest::Indirect(dst) = ret else {
                            engine_abort("pclmulqdq 返回形态不是 indirect vector");
                        };
                        let dst = eval_place_addr(ctx, base, dst) as *mut u8;
                        unsafe {
                            crate::arch::x86_64::pclmulqdq(dst, a(0) as *const u8, a(1) as *const u8, a(2))
                        };
                        true
                    }
                    Builtin::X86AesEnc
                    | Builtin::X86AesEncLast
                    | Builtin::X86AesDec
                    | Builtin::X86AesDecLast => {
                        let RetDest::Indirect(dst) = ret else {
                            engine_abort("aesni 返回形态不是 indirect vector");
                        };
                        let dst = eval_place_addr(ctx, base, dst) as *mut u8;
                        let (x, k) = (a(0) as *const u8, a(1) as *const u8);
                        unsafe {
                            match builtin {
                                Builtin::X86AesEnc => crate::arch::x86_64::aesenc(dst, x, k),
                                Builtin::X86AesEncLast => crate::arch::x86_64::aesenclast(dst, x, k),
                                Builtin::X86AesDec => crate::arch::x86_64::aesdec(dst, x, k),
                                _ => crate::arch::x86_64::aesdeclast(dst, x, k),
                            }
                        }
                        true
                    }
                    Builtin::X86AesImc => {
                        let RetDest::Indirect(dst) = ret else {
                            engine_abort("aesimc 返回形态不是 indirect vector");
                        };
                        let dst = eval_place_addr(ctx, base, dst) as *mut u8;
                        unsafe { crate::arch::x86_64::aesimc(dst, a(0) as *const u8) };
                        true
                    }
                    Builtin::X86AesKeygenAssist => {
                        let RetDest::Indirect(dst) = ret else {
                            engine_abort("aeskeygenassist 返回形态不是 indirect vector");
                        };
                        let dst = eval_place_addr(ctx, base, dst) as *mut u8;
                        unsafe { crate::arch::x86_64::aeskeygenassist(dst, a(0) as *const u8, a(1)) };
                        true
                    }
                    Builtin::X86Permd256 => {
                        let RetDest::Indirect(dst) = ret else {
                            engine_abort("permd 返回形态不是 indirect vector");
                        };
                        let dst = eval_place_addr(ctx, base, dst) as *mut u8;
                        unsafe { crate::arch::x86_64::permd256(dst, a(0) as *const u8, a(1) as *const u8) };
                        true
                    }
                    Builtin::X86PmaddUbSw128
                    | Builtin::X86PmaddUbSw256
                    | Builtin::X86PmaddWd128
                    | Builtin::X86PmaddWd256 => {
                        let RetDest::Indirect(dst) = ret else {
                            engine_abort("pmadd 返回形态不是 indirect vector");
                        };
                        let dst = eval_place_addr(ctx, base, dst) as *mut u8;
                        let (x, y) = (a(0) as *const u8, a(1) as *const u8);
                        unsafe {
                            match builtin {
                                Builtin::X86PmaddUbSw128 => crate::arch::x86_64::pmaddubsw128(dst, x, y),
                                Builtin::X86PmaddUbSw256 => crate::arch::x86_64::pmaddubsw256(dst, x, y),
                                Builtin::X86PmaddWd128 => crate::arch::x86_64::pmaddwd128(dst, x, y),
                                _ => crate::arch::x86_64::pmaddwd256(dst, x, y),
                            }
                        }
                        true
                    }
                    Builtin::X86GatherQPd256 | Builtin::X86GatherDPd256 => {
                        let RetDest::Indirect(dst) = ret else {
                            engine_abort("gather.pd.256 返回形态不是 indirect vector");
                        };
                        let dst = eval_place_addr(ctx, base, dst) as *mut u8;
                        // (src vec, base 标量指针, vindex vec, mask vec, scale imm)
                        unsafe {
                            if matches!(builtin, Builtin::X86GatherQPd256) {
                                crate::arch::x86_64::gather_q_pd_256(
                                    dst,
                                    a(0) as *const u8,
                                    a(1),
                                    a(2) as *const u8,
                                    a(3) as *const u8,
                                    a(4),
                                );
                            } else {
                                crate::arch::x86_64::gather_d_pd_256(
                                    dst,
                                    a(0) as *const u8,
                                    a(1),
                                    a(2) as *const u8,
                                    a(3) as *const u8,
                                    a(4),
                                );
                            }
                        }
                        true
                    }
                    Builtin::X86Pmadd52Lo128
                    | Builtin::X86Pmadd52Hi128
                    | Builtin::X86Pmadd52Lo256
                    | Builtin::X86Pmadd52Hi256
                    | Builtin::X86Pmadd52Lo512
                    | Builtin::X86Pmadd52Hi512 => {
                        let RetDest::Indirect(dst) = ret else {
                            engine_abort("vpmadd52 返回形态不是 indirect vector");
                        };
                        let dst = eval_place_addr(ctx, base, dst) as *mut u8;
                        let (x, y, z) = (a(0) as *const u8, a(1) as *const u8, a(2) as *const u8);
                        unsafe {
                            match builtin {
                                Builtin::X86Pmadd52Lo128 => {
                                    crate::arch::x86_64::vpmadd52::<2, false>(dst, x, y, z)
                                }
                                Builtin::X86Pmadd52Hi128 => {
                                    crate::arch::x86_64::vpmadd52::<2, true>(dst, x, y, z)
                                }
                                Builtin::X86Pmadd52Lo256 => {
                                    crate::arch::x86_64::vpmadd52::<4, false>(dst, x, y, z)
                                }
                                Builtin::X86Pmadd52Hi256 => {
                                    crate::arch::x86_64::vpmadd52::<4, true>(dst, x, y, z)
                                }
                                Builtin::X86Pmadd52Lo512 => {
                                    crate::arch::x86_64::vpmadd52::<8, false>(dst, x, y, z)
                                }
                                _ => crate::arch::x86_64::vpmadd52::<8, true>(dst, x, y, z),
                            }
                        }
                        true
                    }
                    Builtin::X86MaxPs128
                    | Builtin::X86MinPs128
                    | Builtin::X86MaxPs256
                    | Builtin::X86MinPs256 => {
                        let RetDest::Indirect(dst) = ret else {
                            engine_abort("max/min.ps 返回形态不是 indirect vector");
                        };
                        let dst = eval_place_addr(ctx, base, dst) as *mut u8;
                        let (x, y) = (a(0) as *const u8, a(1) as *const u8);
                        unsafe {
                            match builtin {
                                Builtin::X86MaxPs128 => crate::arch::x86_64::maxmin_ps::<4, true>(dst, x, y),
                                Builtin::X86MinPs128 => crate::arch::x86_64::maxmin_ps::<4, false>(dst, x, y),
                                Builtin::X86MaxPs256 => crate::arch::x86_64::maxmin_ps::<8, true>(dst, x, y),
                                _ => crate::arch::x86_64::maxmin_ps::<8, false>(dst, x, y),
                            }
                        }
                        true
                    }
                    Builtin::X86CmpPs128 | Builtin::X86CmpPs256 => {
                        let RetDest::Indirect(dst) = ret else {
                            engine_abort("cmp.ps 返回形态不是 indirect vector");
                        };
                        let dst = eval_place_addr(ctx, base, dst) as *mut u8;
                        let (x, y, imm) = (a(0) as *const u8, a(1) as *const u8, a(2));
                        unsafe {
                            if matches!(builtin, Builtin::X86CmpPs128) {
                                crate::arch::x86_64::cmp_ps::<4>(dst, x, y, imm)
                            } else {
                                crate::arch::x86_64::cmp_ps::<8>(dst, x, y, imm)
                            }
                        }
                        true
                    }
                    Builtin::X86RoundPs128 | Builtin::X86RoundPs256 => {
                        let RetDest::Indirect(dst) = ret else {
                            engine_abort("round.ps 返回形态不是 indirect vector");
                        };
                        let dst = eval_place_addr(ctx, base, dst) as *mut u8;
                        let (x, imm) = (a(0) as *const u8, a(1));
                        unsafe {
                            if matches!(builtin, Builtin::X86RoundPs128) {
                                crate::arch::x86_64::round_ps::<4>(dst, x, imm)
                            } else {
                                crate::arch::x86_64::round_ps::<8>(dst, x, imm)
                            }
                        }
                        true
                    }
                    Builtin::X86CvtPs2dq128
                    | Builtin::X86CvttPs2dq128
                    | Builtin::X86CvtPs2dq256
                    | Builtin::X86CvttPs2dq256 => {
                        let RetDest::Indirect(dst) = ret else {
                            engine_abort("cvt(t).ps2dq 返回形态不是 indirect vector");
                        };
                        let dst = eval_place_addr(ctx, base, dst) as *mut u8;
                        let x = a(0) as *const u8;
                        unsafe {
                            match builtin {
                                Builtin::X86CvtPs2dq128 => crate::arch::x86_64::cvt_ps2dq::<4, false>(dst, x),
                                Builtin::X86CvttPs2dq128 => crate::arch::x86_64::cvt_ps2dq::<4, true>(dst, x),
                                Builtin::X86CvtPs2dq256 => crate::arch::x86_64::cvt_ps2dq::<8, false>(dst, x),
                                _ => crate::arch::x86_64::cvt_ps2dq::<8, true>(dst, x),
                            }
                        }
                        true
                    }
                    Builtin::X86BlendvPs128 | Builtin::X86BlendvPs256 => {
                        let RetDest::Indirect(dst) = ret else {
                            engine_abort("blendv.ps 返回形态不是 indirect vector");
                        };
                        let dst = eval_place_addr(ctx, base, dst) as *mut u8;
                        let (x, y, m) = (a(0) as *const u8, a(1) as *const u8, a(2) as *const u8);
                        unsafe {
                            if matches!(builtin, Builtin::X86BlendvPs128) {
                                crate::arch::x86_64::blendv_ps::<4>(dst, x, y, m)
                            } else {
                                crate::arch::x86_64::blendv_ps::<8>(dst, x, y, m)
                            }
                        }
                        true
                    }
                    Builtin::X86Lddqu128 | Builtin::X86Lddqu256 => {
                        let RetDest::Indirect(dst) = ret else {
                            engine_abort("lddqu 返回形态不是 indirect vector");
                        };
                        let dst = eval_place_addr(ctx, base, dst) as *mut u8;
                        let src = a(0) as *const u8;
                        unsafe {
                            if matches!(builtin, Builtin::X86Lddqu128) {
                                crate::arch::x86_64::lddqu::<16>(dst, src)
                            } else {
                                crate::arch::x86_64::lddqu::<32>(dst, src)
                            }
                        }
                        true
                    }
                    Builtin::X86Cvtps2ph128 | Builtin::X86Cvtps2ph256 => {
                        let RetDest::Indirect(dst) = ret else {
                            engine_abort("vcvtps2ph 返回形态不是 indirect vector");
                        };
                        let dst = eval_place_addr(ctx, base, dst) as *mut u8;
                        let (x, imm) = (a(0) as *const u8, a(1));
                        unsafe {
                            if matches!(builtin, Builtin::X86Cvtps2ph128) {
                                crate::arch::x86_64::cvtps2ph::<4>(dst, x, imm)
                            } else {
                                crate::arch::x86_64::cvtps2ph::<8>(dst, x, imm)
                            }
                        }
                        true
                    }
                    Builtin::X86Cvtph2ps128 | Builtin::X86Cvtph2ps256 => {
                        let RetDest::Indirect(dst) = ret else {
                            engine_abort("vcvtph2ps 返回形态不是 indirect vector");
                        };
                        let dst = eval_place_addr(ctx, base, dst) as *mut u8;
                        let x = a(0) as *const u8;
                        unsafe {
                            if matches!(builtin, Builtin::X86Cvtph2ps128) {
                                crate::arch::x86_64::cvtph2ps::<4>(dst, x)
                            } else {
                                crate::arch::x86_64::cvtph2ps::<8>(dst, x)
                            }
                        }
                        true
                    }
                    Builtin::X86PsllD128 | Builtin::X86PsrlD128 => {
                        let RetDest::Indirect(dst) = ret else {
                            engine_abort("ps{l,r}l.d 返回形态不是 indirect vector");
                        };
                        let dst = eval_place_addr(ctx, base, dst) as *mut u8;
                        let (x, c) = (a(0) as *const u8, a(1) as *const u8);
                        unsafe {
                            if matches!(builtin, Builtin::X86PsllD128) {
                                crate::arch::x86_64::pshift32::<4, true>(dst, x, c)
                            } else {
                                crate::arch::x86_64::pshift32::<4, false>(dst, x, c)
                            }
                        }
                        true
                    }
                    _ => false,
                };
                if vector_done {
                    edge.set(None);
                    blk = *target as usize;
                    continue;
                }
                // LLVM 的 addcarry/subborrow 返回 `(flag, result)` ScalarPair，而其他
                // 现有 builtin 都是单标量。先走 pair 专用道，保持字段顺序与冻结 ABI 一致。
                let carry_result = match builtin {
                    Builtin::AddCarry64 => {
                        let carry_in = u64::from(a(0) != 0);
                        let (partial, carry1) = a(1).overflowing_add(a(2));
                        let (result, carry2) = partial.overflowing_add(carry_in);
                        Some(("addcarry.64", carry1 || carry2, result))
                    }
                    Builtin::SubBorrow64 => {
                        let borrow_in = u64::from(a(0) != 0);
                        let (partial, borrow1) = a(1).overflowing_sub(a(2));
                        let (result, borrow2) = partial.overflowing_sub(borrow_in);
                        Some(("subborrow.64", borrow1 || borrow2, result))
                    }
                    _ => None,
                };
                if let Some((name, flag, result)) = carry_result {
                    edge.set(None);
                    match ret {
                        RetDest::Pair(flag_dst, value) => {
                            place_write(ctx, base, flag_dst, u64::from(flag));
                            place_write(ctx, base, value, result);
                        }
                        other => {
                            engine_abort(&format!("{name} 返回形态 {other:?}，期望 ScalarPair"))
                        }
                    }
                    blk = *target as usize;
                    continue;
                }
                let r = match builtin {
                    // 分配前哨兵：空操作
                    Builtin::NoAllocShim => 0,
                    // 托管 Rust Heap（D3：mimalloc 后端，真地址直出）。
                    // 自定义 #[global_allocator]（corpus 批7 c_mimalloc 实锤修）：
                    // 分配是**程序级**语义——本模块登记 shim 时，任何镜像来源的
                    // builtin 臂（含 base 按 Default 会话烘的）一律经 guest shim
                    // 走用户分配器，否则跨堆 free = mimalloc 元数据 SIGSEGV。
                    Builtin::RustAlloc => match module.custom_alloc_shims {
                        Some(s) => {
                            let (lo, _) = call_guarding_terminate(unwind, || {
                                call_guest(ctx, s.alloc, &[a(0), a(1)])
                            });
                            lo
                        }
                        None => crate::vm::engine::heap::alloc(a(0), a(1)),
                    },
                    Builtin::RustAllocZeroed => match module.custom_alloc_shims {
                        Some(s) => {
                            let (lo, _) = call_guarding_terminate(unwind, || {
                                call_guest(ctx, s.alloc_zeroed, &[a(0), a(1)])
                            });
                            lo
                        }
                        None => crate::vm::engine::heap::alloc_zeroed(a(0), a(1)),
                    },
                    Builtin::RustRealloc => match module.custom_alloc_shims {
                        Some(s) => {
                            let (lo, _) = call_guarding_terminate(unwind, || {
                                call_guest(ctx, s.realloc, &[a(0), a(1), a(2), a(3)])
                            });
                            lo
                        }
                        None => crate::vm::engine::heap::realloc(a(0), a(1), a(2), a(3)),
                    },
                    Builtin::RustDealloc => {
                        match module.custom_alloc_shims {
                            Some(s) => {
                                let _ = call_guarding_terminate(unwind, || {
                                    call_guest(ctx, s.dealloc, &[a(0), a(1), a(2)])
                                });
                            }
                            None => crate::vm::engine::heap::dealloc(a(0), a(1), a(2)),
                        }
                        0
                    }
                    // unwind 原语（spike3 的 raise）：宿主 unwinder 载 guest exception 指针
                    Builtin::UnwindRaise => raise_guest(a(0)),
                    // os:: 最小直通（真实地址零编组；M4.3 正式注册表）
                    Builtin::HostGetenv => crate::os::process::getenv(a(0)),
                    Builtin::HostWrite => {
                        crate::os::process::write_fd(a(0) as i32, a(1), a(2) as usize) as u64
                    }
                    Builtin::HostStrlen => crate::os::process::c_strlen(a(0)),
                    Builtin::HostAbort => {
                        eprintln!("mirvm[m4-engine]: guest abort()");
                        std::process::abort()
                    }
                    // fork（D8f）：仅 guest 单线程放行（子进程=全进程拷贝，解释器状态
                    // 天然一致；无其他 guest 线程 ⇒ 无跨线程锁死锁面）。多线程 fork
                    // 响亮拒绝（native 下同为雷区）。exec 族走 foreign 直通，不经此。
                    Builtin::HostFork => {
                        if crate::vm::engine::ctx::guest_spawned_threads() {
                            engine_abort(
                                "fork() 时 guest 已派生额外线程：多线程 fork 后仅 forking \
                                 线程存活、其他线程持有的锁在子进程永久锁死（native 亦 UB）。\
                                 仅 guest 单线程时放行（D8f/D8l）",
                            );
                        }
                        crate::os::process::fork() as u64
                    }
                    // atexit 家族（D8g）：登记 guest 回调，返回 0（成功）。
                    // __cxa_atexit(fn, arg, dso)：fn 收 arg；on_exit(fn, arg)：fn 收
                    //（status, arg）。atexit(fn)：无参。统一存 (fn, 形态, arg)。
                    Builtin::HostAtexit => atexit_register(a(0), AtexitKind::Plain, 0),
                    Builtin::HostCxaAtexit => atexit_register(a(0), AtexitKind::CxaArg, a(1)),
                    Builtin::HostOnExit => atexit_register(a(0), AtexitKind::OnExit, a(1)),
                    Builtin::HostSignal => {
                        let (signum, handler) = (a(0) as i32, a(1) as usize);
                        // guest handler（非 DFL/IGN）：async 信号 → 物化 AS-trampoline
                        //（D8d）；sync 故障信号 → 响亮拒绝（宿主/guest 故障不可分辨）。
                        let real = if handler != crate::os::signal::SIG_DFL
                            && handler != crate::os::signal::SIG_IGN
                        {
                            signal_thunk(ctx, signum, handler as u64)
                        } else {
                            handler
                        };
                        crate::os::signal::signal(signum, real) as u64
                    }
                    Builtin::HostSigaction => {
                        let (signum, act, oldact) = (a(0) as i32, a(1), a(2));
                        // guest handler 藏在 sigaction 结构里：thunk 后写一份改过 handler
                        // 的副本给内核（原结构不动——guest 可能复用/读回）。
                        let mut patched = unsafe { crate::os::signal::Sigaction::copy_from(act) };
                        if let Some(p) = patched.as_mut() {
                            let h = p.handler();
                            if h != crate::os::signal::SIG_DFL && h != crate::os::signal::SIG_IGN
                            {
                                p.set_handler(signal_thunk(ctx, signum, h as u64));
                            }
                        }
                        crate::os::signal::sigaction(signum, patched.as_ref(), oldact) as u64
                    }
                    Builtin::Unsupported(name) => {
                        engine_abort(&format!("unsupported builtin `{}`", name.0))
                    }
                    Builtin::UnwindDeleteException => {
                        // Itanium `_Unwind_Exception`：exception_class @0，cleanup fn @8。
                        // guest panic 的 cleanup 是冻结 fn 条目；foreign exception 也可能
                        // 带 native cleanup，因此按地址域选择解释调用或 native FFI。
                        let exc = a(0);
                        let cleanup = mem_read(exc + 8, Width::W64);
                        if cleanup != 0 {
                            let av = [1, exc]; // _URC_FOREIGN_EXCEPTION_CAUGHT
                            if module.fn_addrs.contains_key(&cleanup) {
                                call_fn_addr(ctx, cleanup, &av, "_Unwind_DeleteException");
                            } else {
                                let sig = crate::vm::engine::ir::ForeignSig {
                                    args: vec![FfiKind::I32, FfiKind::Ptr],
                                    ret: FfiKind::Void,
                                    fixed: None,
                                    thunk_args: vec![],
                                };
                                crate::vm::engine::ffi::call_addr(cleanup as usize, &sig, &av, None);
                            }
                        }
                        0
                    }
                    // backtrace 影子帧（D8e）
                    Builtin::UnwindBacktrace => unwind_backtrace(ctx, a(0), a(1)),
                    Builtin::UnwindGetIp => mem_read(a(0), Width::W64),
                    Builtin::UnwindGetIpInfo => {
                        // (ctx, *ip_before_insn) → IP；*ip_before_insn=0（合成帧无此区分）
                        if a(1) != 0 {
                            mem_write(a(1), Width::W32, 0);
                        }
                        mem_read(a(0), Width::W64)
                    }
                    // 合成 IP 即函数入口 → 返回 ip 自身（enclosing fn start）
                    Builtin::UnwindFindEnclosing => a(0),
                    Builtin::CpuHintNop => 0,
                    Builtin::Breakpoint => {
                        // 真 int3：未被跟踪时 = SIGTRAP 终止（native 同语义）
                        crate::arch::x86_64::asmstub::int3();
                        0
                    }
                    Builtin::AddCarry64 => unreachable!("addcarry.64 已由 pair 通道处理"),
                    Builtin::SubBorrow64 => unreachable!("subborrow.64 已由 pair 通道处理"),
                    Builtin::Xgetbv => crate::arch::x86_64::asmstub::xgetbv(a(0) as u32),
                    Builtin::X86Crc32U8 => unsafe {
                        u64::from(crate::arch::x86_64::crc32_u8(a(0) as u32, a(1) as u8))
                    },
                    Builtin::X86Crc32U16 => unsafe {
                        u64::from(crate::arch::x86_64::crc32_u16(a(0) as u32, a(1) as u16))
                    },
                    Builtin::X86Crc32U32 => unsafe {
                        u64::from(crate::arch::x86_64::crc32_u32(a(0) as u32, a(1) as u32))
                    },
                    Builtin::X86Crc32U64 => unsafe {
                        crate::arch::x86_64::crc32_u64(a(0), a(1))
                    },
                    Builtin::X86Pshufb128
                    | Builtin::X86Pshufb256
                    | Builtin::X86Sha256Msg1
                    | Builtin::X86Sha256Msg2
                    | Builtin::X86Sha256Rnds2
                    | Builtin::X86PsadBw128
                    | Builtin::X86PsadBw256
                    | Builtin::X86Pclmulqdq
                    | Builtin::X86AesEnc
                    | Builtin::X86AesEncLast
                    | Builtin::X86AesDec
                    | Builtin::X86AesDecLast
                    | Builtin::X86AesImc
                    | Builtin::X86AesKeygenAssist
                    | Builtin::X86Permd256
                    | Builtin::X86GatherQPd256
                    | Builtin::X86GatherDPd256
                    | Builtin::X86Pmadd52Lo128
                    | Builtin::X86Pmadd52Hi128
                    | Builtin::X86Pmadd52Lo256
                    | Builtin::X86Pmadd52Hi256
                    | Builtin::X86Pmadd52Lo512
                    | Builtin::X86Pmadd52Hi512
                    | Builtin::X86PmaddUbSw128
                    | Builtin::X86PmaddUbSw256
                    | Builtin::X86PmaddWd128
                    | Builtin::X86PmaddWd256
                    | Builtin::X86Cvtps2ph128
                    | Builtin::X86Cvtph2ps128
                    | Builtin::X86Cvtps2ph256
                    | Builtin::X86Cvtph2ps256
                    | Builtin::X86MaxPs128
                    | Builtin::X86MinPs128
                    | Builtin::X86MaxPs256
                    | Builtin::X86MinPs256
                    | Builtin::X86CmpPs128
                    | Builtin::X86CmpPs256
                    | Builtin::X86RoundPs128
                    | Builtin::X86RoundPs256
                    | Builtin::X86CvtPs2dq128
                    | Builtin::X86CvttPs2dq128
                    | Builtin::X86CvtPs2dq256
                    | Builtin::X86CvttPs2dq256
                    | Builtin::X86BlendvPs128
                    | Builtin::X86BlendvPs256
                    | Builtin::X86Lddqu128
                    | Builtin::X86Lddqu256
                    | Builtin::X86PsllD128
                    | Builtin::X86PsrlD128 => {
                        unreachable!("x86 vector builtin 已由 indirect vector 通道处理")
                    }
                    Builtin::HostSyscall => {
                        let av: Vec<u64> = (1..args.len()).map(&a).collect();
                        crate::os::process::syscall(a(0) as i64, &av) as u64
                    }
                    // rust_try：宿主 catch；guest panic → 调 catch_fn(data, exc) 返 1
                    Builtin::CatchUnwind => {
                        let (try_fn, data, catch_fn) = (a(0), a(1), a(2));
                        match panic::catch_unwind(AssertUnwindSafe(|| {
                            call_fn_addr(ctx, try_fn, &[data], "catch_unwind.try")
                        })) {
                            Ok(_) => 0,
                            Err(e) => match e.downcast::<GuestPanic>() {
                                Ok(gp) => {
                                    call_fn_addr(
                                        ctx,
                                        catch_fn,
                                        &[data, gp.exception],
                                        "catch_unwind.catch",
                                    );
                                    1
                                }
                                // 宿主 panic（VM bug）不是 guest 异常：原样续传
                                Err(host) => panic::resume_unwind(host),
                            },
                        }
                    }
                };
                edge.set(None);
                match ret {
                    RetDest::Scalar(p) => place_write(ctx, base, p, r),
                    RetDest::Ignore => {}
                    other => engine_abort(&format!("引擎原语返回形态 {other:?} 未支持")),
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
