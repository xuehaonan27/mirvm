//! 调用与 FFI 入向（自 interp.rs I13 整搬）：call_fn_addr/cleanup_edge/
//! call_guarding_terminate/run_cleanup + ret_abi_of/call_guest_ffi/
//! interp_frame（模型 A 宿主递归，真栈字节守卫）。call_guest（发布协议
//! 读侧锚点）留在 mod.rs——与 jit/compiler worker 写侧注释不可分离。

use super::*;
use super::{
    runblocks::run_blocks,
    services::{
        AtexitKind, atexit_register, func_synth_ip, resolve_signal_handler, unwind_backtrace,
    },
};
use crate::vm::engine::ir;

pub(super) fn call_fn_addr(ctx: *mut Ctx, addr: u64, args: &[u64], caller: &str) -> (u64, u64) {
    let module: &Module = unsafe { &(*(*ctx).shared).module };
    let Some(&fid) = module.fn_addrs.get(&addr) else {
        engine_abort(&format!(
            "间接调用目标 {addr:#x} 不是已知 fn 条目（调用者 {caller}）"
        ));
    };
    call_guest(ctx, fid, args)
}

/// unwind 边 → cleanup 目标块。
#[inline]
pub(super) fn cleanup_edge(u: &UnwindAction) -> Option<Bb> {
    match u {
        UnwindAction::Cleanup(b) => Some(*b),
        _ => None,
    }
}

/// Terminate 边界的调用包装：panic 到此即 abort（double panic / extern "C" ABI 边界）。
#[inline]
pub(super) fn call_guarding_terminate<R>(unwind: &UnwindAction, f: impl FnOnce() -> R) -> R {
    if let UnwindAction::Terminate = unwind {
        crate::vm::engine::unwind::guard_terminate(f)
    } else {
        f()
    }
}

/// guard.drop 里的 cleanup 链执行（landing pad 的宿主 Rust 写法）：从 cleanup 块跑到
/// `Resume`。链中 Call 可再入混合执行；链中再 panic：Terminate 边 abort，Continue 边
/// 穿出 Drop = 宿主 double-panic abort（与 native 一致）。
pub(super) fn run_cleanup(ctx: *mut Ctx, func: u32, base: usize, entry: Bb) {
    // cleanup 内无嵌套 cleanup（MIR 不变量）——独立哑 edge
    let edge = Cell::new(None);
    match run_blocks(ctx, func, base, &edge, entry) {
        Exit::Resume => {} // 返回 guard，unwind 自动继续
        Exit::Ret(..) => engine_abort("cleanup 链以 Return 结束（MIR 不变量破坏）"),
    }
}

/// J1 单一派发点（M5.3a，m5.3-design §2.2）：guest 函数调用的必经口，收拢六处
/// 原 interp_frame 直调（Call/CallIndirect/CatchUnwind 回调/run_main/run_export/
/// thunk 蹦床；tsan_mt 豁免——Q4，TSan 通道不编 cranelift，收拢无意义）。
/// 槽非零 = 已发布编译码（M5.3b 起 i2c 直调 packed 入口）；零 = 计数 + 解释。
/// 计数 Relaxed（丢计只影响触发时刻）；槽 Acquire 配编译线程 Release（D4 协议）。
#[inline]
pub(crate) fn ret_abi_of(ctx: *mut Ctx, func: u32) -> RetAbi {
    let module: &Module = unsafe { &(*(*ctx).shared).module };
    module.funcs[func as usize].ret
}

/// C1 FFI 入向封送：C 侧实参（marshal_args 产出——标量原样 / 聚合 = 聚合字节
/// 真地址）按 callee ParamAbi 展开成 ABI 实参槽后 `call_guest`（thunk 工厂与
/// P1 条目蹦床共用）。ret_addr = 按值聚合返回时 libffi 的结果缓冲地址——仅当
/// callee RetAbi::Indirect 时作隐藏首实参槽（sret 直传）；小档由调用方对
/// (lo,hi) 做 FfiAgg 重打包。
pub(crate) fn call_guest_ffi(
    ctx: *mut Ctx,
    func: u32,
    kinds: &[FfiKind],
    vals: &[u64],
    ret_addr: Option<u64>,
) -> (u64, u64) {
    let module: &Module = unsafe { &(*(*ctx).shared).module };
    let body: &FuncBody = &module.funcs[func as usize];
    let mut av: Vec<u64> = Vec::with_capacity(vals.len() + body.params.len() + 1);
    if let RetAbi::Indirect { .. } = body.ret {
        av.push(ret_addr.expect("C1：callee 按值聚合返回（RetAbi::Indirect）但无结果地址"));
    }
    let mut ki = 0usize;
    for p in &body.params {
        match p {
            ParamAbi::Zst => {}
            ParamAbi::Scalar(_) => match kinds.get(ki) {
                Some(FfiKind::Agg(agg)) => {
                    av.push(unsafe { agg_leaf_at(*vals.get_unchecked(ki), agg, 0) });
                    ki += 1;
                }
                Some(_) => {
                    av.push(vals[ki]);
                    ki += 1;
                }
                None => engine_abort(&format!(
                    "C1 封送缺参（callee fn {} params {:?}）",
                    body.name, body.params
                )),
            },
            ParamAbi::Pair(_, _) => match kinds.get(ki) {
                Some(FfiKind::Agg(agg)) => {
                    av.push(unsafe { agg_leaf_at(*vals.get_unchecked(ki), agg, 0) });
                    av.push(unsafe { agg_leaf_at(*vals.get_unchecked(ki), agg, 1) });
                    ki += 1;
                }
                _ => engine_abort(&format!(
                    "C1 封送错配：callee `Pair` 参数遇到非标量 C 参（fn {} params {:?} kinds {:?}）",
                    body.name, body.params, kinds
                )),
            },
            ParamAbi::Indirect { .. } => match kinds.get(ki) {
                Some(FfiKind::Agg(_)) => {
                    av.push(vals[ki]);
                    ki += 1;
                }
                _ => engine_abort(&format!(
                    "C1 封送错配：callee 按址参数遇到非标量 C 参（fn {} params {:?} kinds {:?}）",
                    body.name, body.params, kinds
                )),
            },
        }
    }
    if ki != vals.len() {
        engine_abort(&format!(
            "C1 封送槽数错配：callee fn {} 消费 {ki}，marshal 供 {}",
            body.name,
            vals.len()
        ));
    }
    call_guest(ctx, func, &av)
}

/// 读聚合声明序第 idx 个字段的值（Scalar 叶按宽度读；顶层嵌套叶与 Pair/Scalar
/// 参数形态结构性互斥——同 rustc layout 推导，出现即引擎不变量破坏）。
pub(super) unsafe fn agg_leaf_at(addr: u64, agg: &FfiAgg, idx: usize) -> u64 {
    let Some(f) = agg.fields.get(idx) else {
        engine_abort("C1 封送：Pair 参数遇单字段聚合");
    };
    let FfiLeaf::Scalar(k) = &f.leaf else {
        engine_abort("C1 封送：顶层嵌套叶遇 Pair 参数");
    };
    let p = addr.wrapping_add(f.off as u64) as *const u8;
    unsafe {
        match k {
            FfiKind::I8 | FfiKind::U8 => p.read() as u64,
            FfiKind::I16 | FfiKind::U16 => (p as *const u16).read_unaligned() as u64,
            FfiKind::I32 | FfiKind::U32 | FfiKind::F32 => (p as *const u32).read_unaligned() as u64,
            FfiKind::I64 | FfiKind::U64 | FfiKind::F64 | FfiKind::Ptr => {
                (p as *const u64).read_unaligned()
            }
            FfiKind::Void | FfiKind::Agg(_) => engine_abort("C1 封送：非法叶类"),
        }
    }
}

/// 模型 A：guest 调用 = 宿主递归（spike1/3 验证的形状）。
/// 调用约定 v2：实参展平 `&[u64]`（pair 占 2 槽、indirect 传地址），返回 (lo, hi)。
/// guest 栈溢出防护（M5.2 D8a）= **真栈字节守卫**：以本地变量地址近似宿主 SP，
/// 低于 Ctx 冻结的安全下界（线程栈低端 + 边距）即诊断退出——帧数不设固定上限
///（旧 8000 帧硬编码对 native 栈界严重失真：native 8MiB 主栈可容 ~10 万浅帧）。
/// 随线程真实栈自适应；native 语义 = SIGSEGV→"has overflowed its stack"，此处
/// 为诊断替身（ram-spec §7：溢出深度 unspecified，只承诺近似 native）。
pub(crate) fn interp_frame(ctx: *mut Ctx, func: u32, args: &[u64]) -> (u64, u64) {
    let module: &Module = unsafe { &(*(*ctx).shared).module };
    let body: &FuncBody = &module.funcs[func as usize];

    // 序言也会发生 EngineFault（栈守卫、参数 ABI、操作数区穷尽）。
    // 从第一次修改 Ctx 状态起就建立分阶段守卫，只撤销已完成的步骤。
    let mut guard = FrameGuard {
        ctx,
        depth_active: false,
        base: None,
        shadow_active: false,
        unwind_edge: Cell::new(None),
    };
    let Some(depth) = (unsafe { (*ctx).depth.checked_add(1) }) else {
        engine_abort(&format!("guest 解释深度计数溢出（fn {}）", body.name));
    };
    unsafe { (*ctx).depth = depth };
    guard.depth_active = true;
    let sp_approx = &depth as *const u32 as usize;
    if unsafe { (*ctx).stack_floor } > sp_approx {
        engine_abort(&format!(
            "guest 栈溢出（宿主执行栈触及安全边距；解释深度 {depth}；fn {}）",
            body.name
        ));
    }

    let base = region_reserve(ctx, body.frame_size, body.frame_align);
    guard.base = Some(base);
    // prologue：按 ParamAbi 消费实参槽（槽数先验——不匹配给名字与期望，勿裸越界 panic）
    let needed: usize = matches!(body.ret, RetAbi::Indirect { .. }) as usize
        + body
            .params
            .iter()
            .map(|p| match p {
                ParamAbi::Zst => 0,
                ParamAbi::Scalar(_) | ParamAbi::Indirect { .. } => 1,
                ParamAbi::Pair(..) => 2,
            })
            .sum::<usize>()
        + body.caller_loc_off.is_some() as usize;
    if args.len() < needed {
        engine_abort(&format!(
            "ABI mismatch: fn `{}` expects {needed} arguments (params {:?} ret {:?} loc {:?}) but receives {}",
            body.name,
            body.params,
            body.ret,
            body.caller_loc_off,
            args.len()
        ));
    }
    let mut ai = 0usize;
    // Indirect 返回：隐藏首实参 = 目的真地址，存入 sret 槽
    if let RetAbi::Indirect { sret_off, .. } = body.ret {
        slot_write(
            ctx,
            base,
            Slot {
                off: sret_off,
                width: Width::W64,
            },
            args[ai],
        );
        ai += 1;
    }
    for p in &body.params {
        match p {
            ParamAbi::Zst => {}
            ParamAbi::Scalar(s) => {
                slot_write(ctx, base, *s, args[ai]);
                ai += 1;
            }
            ParamAbi::Pair(lo, hi) => {
                slot_write(ctx, base, *lo, args[ai]);
                slot_write(ctx, base, *hi, args[ai + 1]);
                ai += 2;
            }
            ParamAbi::Indirect { off, size } => {
                let src = args[ai];
                ai += 1;
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        src as *const u8,
                        (base as u64 + *off as u64) as *mut u8,
                        *size as usize,
                    )
                };
            }
        }
    }
    // #[track_caller]：&Location 隐藏尾实参
    if let Some(off) = body.caller_loc_off {
        let Some(&loc) = args.get(ai) else {
            engine_abort(&format!(
                "ABI mismatch: track_caller fn `{}` expects location 尾实参（收到 {} 槽）",
                body.name,
                args.len()
            ));
        };
        slot_write(
            ctx,
            base,
            Slot {
                off,
                width: Width::W64,
            },
            loc,
        );
    }

    // 帧守卫始终恢复操作数区；外围 raw catch 按实际异常身份选择 cleanup。
    // 影子帧入栈（D8e）：合成 IP = FUNC_IP_BASE + func×64（每 FuncId 唯一、非零、
    // 不可执行的 opaque token；作 backtrace 的 IP 恰好——从不解引用为代码）。
    let shadow_marker = 0u8;
    unsafe {
        (*ctx).shadow.push(crate::vm::engine::ctx::ShadowFrame {
            ip: func_synth_ip(ctx, func),
            cfa: &shadow_marker as *const u8 as u64,
        })
    };
    guard.shadow_active = true;
    match crate::vm::engine::unwind::catch_raw(|| {
        run_blocks(ctx, func, base, &guard.unwind_edge, 0)
    }) {
        Ok(Exit::Ret(lo, hi)) => (lo, hi), // guard drop → region 恢复
        Ok(Exit::Resume) => engine_abort(&format!("Resume 出现在正常执行路径（fn {}）", body.name)),
        Err(exception) => {
            if !exception.is_engine_fault()
                && let Some(cleanup) = guard.unwind_edge.get()
            {
                run_cleanup(ctx, func, base, cleanup);
            }
            exception.resume_or_rethrow()
        }
    }
}

/// T1-b（m5.4-design §3.2）：CallBuiltin 语义体（自 runblocks.rs 的 630 行臂
/// 机械提取，零行为变化）——interp 薄臂与 JIT mirvm_call_builtin/mirvm_alloc
/// 助手共享同一实现本体（蓝图铁律：helper 不复制逻辑）。av = 调用点已展平
/// 实参（builtin 无 sret 前插）；ret_dst = RetDest::Indirect 的目的真地址
/// （调用点已求值，x86 向量 lane 的 sret 落点）。返回 (lo, hi)：主标量
/// lane = (r, 0)；addcarry/subborrow pair lane = (flag, result)；x86 向量
/// lane 的 sret 字节已在本体内落盘、返 (0, 0)。ret 形态写回由调用点统一
/// （Ignore/Indirect 不写、Scalar=lo、Pair=(lo,hi)——lower 只发匹配形态，
/// 原臂内的形态诊断随统一写回退役）。edge 协议随体保留。
#[allow(clippy::too_many_arguments)]
pub(crate) fn exec_builtin(
    ctx: *mut Ctx,
    body: &ir::FuncBody,
    edge: &Cell<Option<Bb>>,
    builtin: &ir::Builtin,
    av: &[u64],
    ret_dst: Option<u64>,
    unwind: &ir::UnwindAction,
    role: ir::BuiltinCallRole,
) -> (u64, u64) {
    use crate::vm::engine::ir::Builtin;
    let _ = body; // 签名预留（两调用点诊断对称）；臂内不经 body（module 自 ctx 取）
    let module: &Module = unsafe { &(*(*ctx).shared).module };
    let a = |i: usize| av[i];
    edge.set(cleanup_edge(unwind)); // RaiseException 经此发起 unwind
    // x86 向量 intrinsic：参数是 indirect 向量地址，返回落到 sret place。
    // helper 本身带 target_feature，guest 的正常 CPUID 派发负责可达性。
    let vector_done = match builtin {
        Builtin::X86Pshufb128 => {
            let Some(dst) = ret_dst else {
                engine_abort("pshufb128 返回形态不是 indirect vector");
            };
            let dst = dst as *mut u8;
            unsafe { crate::arch::x86_64::pshufb128(dst, a(0) as *const u8, a(1) as *const u8) };
            true
        }
        Builtin::X86Pshufb256 => {
            let Some(dst) = ret_dst else {
                engine_abort("pshufb256 返回形态不是 indirect vector");
            };
            let dst = dst as *mut u8;
            unsafe { crate::arch::x86_64::pshufb256(dst, a(0) as *const u8, a(1) as *const u8) };
            true
        }
        Builtin::X86Sha256Msg1 | Builtin::X86Sha256Msg2 => {
            let Some(dst) = ret_dst else {
                engine_abort("sha256msg 返回形态不是 indirect vector");
            };
            let dst = dst as *mut u8;
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
            let Some(dst) = ret_dst else {
                engine_abort("sha256rnds2 返回形态不是 indirect vector");
            };
            let dst = dst as *mut u8;
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
            let Some(dst) = ret_dst else {
                engine_abort("psad.bw 返回形态不是 indirect vector");
            };
            let dst = dst as *mut u8;
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
            let Some(dst) = ret_dst else {
                engine_abort("pclmulqdq 返回形态不是 indirect vector");
            };
            let dst = dst as *mut u8;
            unsafe {
                crate::arch::x86_64::pclmulqdq(dst, a(0) as *const u8, a(1) as *const u8, a(2))
            };
            true
        }
        Builtin::X86AesEnc
        | Builtin::X86AesEncLast
        | Builtin::X86AesDec
        | Builtin::X86AesDecLast => {
            let Some(dst) = ret_dst else {
                engine_abort("aesni 返回形态不是 indirect vector");
            };
            let dst = dst as *mut u8;
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
            let Some(dst) = ret_dst else {
                engine_abort("aesimc 返回形态不是 indirect vector");
            };
            let dst = dst as *mut u8;
            unsafe { crate::arch::x86_64::aesimc(dst, a(0) as *const u8) };
            true
        }
        Builtin::X86AesKeygenAssist => {
            let Some(dst) = ret_dst else {
                engine_abort("aeskeygenassist 返回形态不是 indirect vector");
            };
            let dst = dst as *mut u8;
            unsafe { crate::arch::x86_64::aeskeygenassist(dst, a(0) as *const u8, a(1)) };
            true
        }
        Builtin::X86Permd256 => {
            let Some(dst) = ret_dst else {
                engine_abort("permd 返回形态不是 indirect vector");
            };
            let dst = dst as *mut u8;
            unsafe { crate::arch::x86_64::permd256(dst, a(0) as *const u8, a(1) as *const u8) };
            true
        }
        Builtin::X86PmaddUbSw128
        | Builtin::X86PmaddUbSw256
        | Builtin::X86PmaddWd128
        | Builtin::X86PmaddWd256 => {
            let Some(dst) = ret_dst else {
                engine_abort("pmadd 返回形态不是 indirect vector");
            };
            let dst = dst as *mut u8;
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
            let Some(dst) = ret_dst else {
                engine_abort("gather.pd.256 返回形态不是 indirect vector");
            };
            let dst = dst as *mut u8;
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
            let Some(dst) = ret_dst else {
                engine_abort("vpmadd52 返回形态不是 indirect vector");
            };
            let dst = dst as *mut u8;
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
            let Some(dst) = ret_dst else {
                engine_abort("max/min.ps 返回形态不是 indirect vector");
            };
            let dst = dst as *mut u8;
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
        Builtin::X86MaxSd | Builtin::X86MinSd => {
            let Some(dst) = ret_dst else {
                engine_abort("max/min.sd 返回形态不是 indirect vector");
            };
            let dst = dst as *mut u8;
            let (x, y) = (a(0) as *const u8, a(1) as *const u8);
            unsafe {
                if matches!(builtin, Builtin::X86MaxSd) {
                    crate::arch::x86_64::maxmin_pd::<1, true>(dst, x, y)
                } else {
                    crate::arch::x86_64::maxmin_pd::<1, false>(dst, x, y)
                }
            }
            true
        }
        Builtin::X86MaxPd128
        | Builtin::X86MinPd128
        | Builtin::X86MaxPd256
        | Builtin::X86MinPd256 => {
            let Some(dst) = ret_dst else {
                engine_abort("max/min.pd 返回形态不是 indirect vector");
            };
            let dst = dst as *mut u8;
            let (x, y) = (a(0) as *const u8, a(1) as *const u8);
            unsafe {
                match builtin {
                    Builtin::X86MaxPd128 => crate::arch::x86_64::maxmin_pd::<2, true>(dst, x, y),
                    Builtin::X86MinPd128 => crate::arch::x86_64::maxmin_pd::<2, false>(dst, x, y),
                    Builtin::X86MaxPd256 => crate::arch::x86_64::maxmin_pd::<4, true>(dst, x, y),
                    _ => crate::arch::x86_64::maxmin_pd::<4, false>(dst, x, y),
                }
            }
            true
        }
        Builtin::X86CmpPs128 | Builtin::X86CmpPs256 => {
            let Some(dst) = ret_dst else {
                engine_abort("cmp.ps 返回形态不是 indirect vector");
            };
            let dst = dst as *mut u8;
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
        Builtin::X86CmpPd128 | Builtin::X86CmpPd256 => {
            let Some(dst) = ret_dst else {
                engine_abort("cmp.pd 返回形态不是 indirect vector");
            };
            let dst = dst as *mut u8;
            let (x, y, imm) = (a(0) as *const u8, a(1) as *const u8, a(2));
            unsafe {
                if matches!(builtin, Builtin::X86CmpPd128) {
                    crate::arch::x86_64::cmp_pd::<2>(dst, x, y, imm)
                } else {
                    crate::arch::x86_64::cmp_pd::<4>(dst, x, y, imm)
                }
            }
            true
        }
        Builtin::X86RoundPs128 | Builtin::X86RoundPs256 => {
            let Some(dst) = ret_dst else {
                engine_abort("round.ps 返回形态不是 indirect vector");
            };
            let dst = dst as *mut u8;
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
            let Some(dst) = ret_dst else {
                engine_abort("cvt(t).ps2dq 返回形态不是 indirect vector");
            };
            let dst = dst as *mut u8;
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
            let Some(dst) = ret_dst else {
                engine_abort("blendv.ps 返回形态不是 indirect vector");
            };
            let dst = dst as *mut u8;
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
            let Some(dst) = ret_dst else {
                engine_abort("lddqu 返回形态不是 indirect vector");
            };
            let dst = dst as *mut u8;
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
            let Some(dst) = ret_dst else {
                engine_abort("vcvtps2ph 返回形态不是 indirect vector");
            };
            let dst = dst as *mut u8;
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
            let Some(dst) = ret_dst else {
                engine_abort("vcvtph2ps 返回形态不是 indirect vector");
            };
            let dst = dst as *mut u8;
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
            let Some(dst) = ret_dst else {
                engine_abort("ps{l,r}l.d 返回形态不是 indirect vector");
            };
            let dst = dst as *mut u8;
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
        return (0, 0);
    }
    // LLVM 的 addcarry/subborrow 返回 `(flag, result)` ScalarPair，而其他
    // 现有 builtin 都是单标量。pair 专用道返回 (flag, result) = (lo, hi)，
    // 保持字段顺序与冻结 ABI 一致；Pair 落点写回由调用点统一。
    let carry_result = match builtin {
        Builtin::AddCarry64 => {
            let carry_in = u64::from(a(0) != 0);
            let (partial, carry1) = a(1).overflowing_add(a(2));
            let (result, carry2) = partial.overflowing_add(carry_in);
            Some((carry1 || carry2, result))
        }
        Builtin::SubBorrow64 => {
            let borrow_in = u64::from(a(0) != 0);
            let (partial, borrow1) = a(1).overflowing_sub(a(2));
            let (result, borrow2) = partial.overflowing_sub(borrow_in);
            Some((borrow1 || borrow2, result))
        }
        _ => None,
    };
    if let Some((flag, result)) = carry_result {
        edge.set(None);
        return (u64::from(flag), result);
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
                let (lo, _) =
                    call_guarding_terminate(unwind, || call_guest(ctx, s.alloc, &[a(0), a(1)]));
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
        Builtin::HostWrite => crate::os::process::write_fd(a(0) as i32, a(1), a(2) as usize) as u64,
        Builtin::HostStrlen => crate::os::process::c_strlen(a(0)),
        Builtin::HostAbort => std::process::abort(),
        // fork（D8f）：仅 guest 单线程放行（子进程=全进程拷贝，解释器状态
        // 天然一致；无其他 guest 线程 ⇒ 无跨线程锁死锁面）。多线程 fork
        // 响亮拒绝（native 下同为雷区）。exec 族走 foreign 直通，不经此。
        Builtin::HostFork => {
            if unsafe { crate::vm::engine::ctx::guest_spawned_threads(ctx) } {
                engine_abort(
                    "fork() 时 guest 已派生额外线程：多线程 fork 后仅 forking \
                     线程存活、其他线程持有的锁在子进程永久锁死（native 亦 UB）。\
                     仅 guest 单线程时放行（D8f/D8l）",
                );
            }
            let pid = crate::os::process::fork();
            if pid == 0 {
                // The child has no writer thread and must never publish into
                // the copied parent generation. This hook is store-only and
                // runs before any JIT service is restarted.
                crate::telemetry::capture::after_fork_child();
                // 子进程：编译线程不随 fork 存活。SYNC 验证模式
                //（MIRVM_JIT_SYNC）的发布等待依赖活的编译服务——重启
                //（继承的已发布码页/槽表/eh_frames 仍有效；队列与 worker
                // 换新。非 sync 子进程维持解释兜底语义不变）
                #[cfg(feature = "cranelift")]
                if unsafe { &*(*ctx).shared }.jit.sync {
                    let shared = unsafe { (*ctx).shared_arc() };
                    crate::vm::engine::jit::start(&shared);
                }
            }
            pid as u64
        }
        // atexit 家族（D8g）：登记 guest 回调，返回 0（成功）。
        // __cxa_atexit(fn, arg, dso)：fn 收 arg；on_exit(fn, arg)：fn 收
        //（status, arg）。atexit(fn)：无参。统一存 (fn, 形态, arg)。
        Builtin::HostAtexit => atexit_register(ctx, a(0), AtexitKind::Plain, 0),
        Builtin::HostCxaAtexit => atexit_register(ctx, a(0), AtexitKind::CxaArg, a(1)),
        Builtin::HostOnExit => atexit_register(ctx, a(0), AtexitKind::OnExit, a(1)),
        Builtin::HostSignal => {
            let (signum, handler) = (a(0) as i32, a(1) as usize);
            let resolution =
                if handler == crate::os::signal::SIG_DFL || handler == crate::os::signal::SIG_IGN {
                    crate::vm::engine::thunks::SignalHandlerResolution::Unknown
                } else {
                    resolve_signal_handler(ctx, handler as u64)
                };
            let control = unsafe { (*ctx).shared().control() };
            match crate::vm::engine::signal::install_signal_resolved(
                control, signum, handler, resolution,
            ) {
                Ok(old) => old as u64,
                Err(error) => {
                    if let Some(errno) = error.libc_errno() {
                        crate::os::process::set_errno(errno);
                        crate::os::signal::SIG_ERR as u64
                    } else {
                        engine_abort(&error.to_string())
                    }
                }
            }
        }
        Builtin::HostRaise => crate::vm::engine::ctx::raise_signal(ctx, a(0) as i32) as u64,
        Builtin::HostSigaction => {
            let (signum, act, oldact) = (a(0) as i32, a(1), a(2));
            let action = unsafe { crate::os::signal::Sigaction::copy_from(act) };
            let resolution = action.as_ref().map(|action| {
                let handler = action.handler();
                if handler == crate::os::signal::SIG_DFL || handler == crate::os::signal::SIG_IGN {
                    crate::vm::engine::thunks::SignalHandlerResolution::Unknown
                } else {
                    resolve_signal_handler(ctx, handler as u64)
                }
            });
            let control = unsafe { (*ctx).shared().control() };
            match crate::vm::engine::signal::install_sigaction_resolved(
                control, signum, action, resolution, oldact,
            ) {
                Ok(result) => result as u64,
                Err(error) => {
                    if let Some(errno) = error.libc_errno() {
                        crate::os::process::set_errno(errno);
                        (-1i32) as u64
                    } else {
                        engine_abort(&error.to_string())
                    }
                }
            }
        }
        Builtin::Unsupported(name) => engine_abort(&format!("unsupported builtin `{}`", name.0)),
        Builtin::UnwindDeleteException => {
            // Itanium `_Unwind_Exception`：exception_class @0，cleanup fn @8。
            // guest panic 的 cleanup 是冻结 fn 条目；foreign exception 也可能
            // 带 native cleanup，因此按地址域选择解释调用或 native FFI。
            let exc = a(0);
            let cleanup = mem_read(exc + 8, Width::W64);
            if cleanup != 0 {
                let cav = [1, exc]; // _URC_FOREIGN_EXCEPTION_CAUGHT
                if module.fn_addrs.contains_key(&cleanup) {
                    call_fn_addr(ctx, cleanup, &cav, "_Unwind_DeleteException");
                } else {
                    let sig = crate::vm::engine::ir::ForeignSig {
                        args: vec![FfiKind::I32, FfiKind::Ptr],
                        ret: FfiKind::Void,
                        fixed: None,
                        thunk_args: vec![],
                        unwind: false,
                    };
                    crate::vm::engine::ffi::call_addr(cleanup as usize, &sig, &cav, None);
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
        Builtin::UnwindGetCfa => mem_read(a(0) + 8, Width::W64),
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
        Builtin::X86Crc32U64 => unsafe { crate::arch::x86_64::crc32_u64(a(0), a(1)) },
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
        | Builtin::X86CmpPd128
        | Builtin::X86CmpPd256
        | Builtin::X86MaxPd128
        | Builtin::X86MinPd128
        | Builtin::X86MaxPd256
        | Builtin::X86MinPd256
        | Builtin::X86MaxSd
        | Builtin::X86MinSd
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
        Builtin::HostSyscall => crate::os::process::syscall(a(0) as i64, &av[1..]) as u64,
        Builtin::HostSyscallTrace => {
            crate::telemetry::capture::host_syscall(a(0) as i64, &av[1..]) as u64
        }
        // rust_try：原始 unwinder catch；仅当前 Engine 的 guest panic
        // 调 catch_fn(data, exc) 返 1，异主/宿主异常继续展开。
        Builtin::CatchUnwind => {
            let (try_fn, data, catch_fn) = (a(0), a(1), a(2));
            let shared = unsafe { (*ctx).shared_arc() };
            let mut main_catch = crate::vm::engine::ctx::claim_main_panic_catch(ctx, role);
            match crate::vm::engine::unwind::catch_raw(|| {
                call_fn_addr(ctx, try_fn, &[data], "catch_unwind.try")
            }) {
                Ok(_) => 0,
                Err(exception) => match exception.at_guest_catch(&shared) {
                    crate::vm::engine::unwind::GuestCatchDisposition::Guest(payload) => {
                        if let Some(main_catch) = &mut main_catch {
                            main_catch.mark_panicked();
                        }
                        payload.transfer(|_, inner| {
                            call_fn_addr(ctx, catch_fn, &[data, inner], "catch_unwind.catch");
                            1
                        })
                    }
                    crate::vm::engine::unwind::GuestCatchDisposition::Resume(exception) => {
                        exception.resume_or_rethrow()
                    }
                },
            }
        }
    };
    edge.set(None);
    (r, 0)
}
