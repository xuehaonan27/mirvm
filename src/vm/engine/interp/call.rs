use super::*;
use super::{runblocks::run_blocks, services::func_synth_ip};

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
        match panic::catch_unwind(AssertUnwindSafe(f)) {
            Ok(r) => r,
            Err(_) => {
                eprintln!(
                    "mirvm[m4-engine]: unwind 抵达 Terminate 边界（double panic/ABI）——abort"
                );
                std::process::abort()
            }
        }
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
        av.push(
            ret_addr.expect("C1：callee 按值聚合返回（RetAbi::Indirect）但无结果地址"),
        );
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
                None => {
                    engine_abort(&format!("C1 封送缺参（callee fn {} params {:?}）", body.name, body.params))
                }
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
            FfiKind::I32 | FfiKind::U32 | FfiKind::F32 => {
                (p as *const u32).read_unaligned() as u64
            }
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

    let depth = unsafe {
        (*ctx).depth += 1;
        (*ctx).depth
    };
    let sp_approx = &depth as *const u32 as usize;
    if unsafe { (*ctx).stack_floor } > sp_approx {
        engine_abort(&format!(
            "guest 栈溢出（宿主执行栈触及安全边距；解释深度 {depth}；fn {}）",
            body.name
        ));
    }

    let base = region_reserve(ctx, body.frame_size, body.frame_align);
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
            "ABI 不匹配：fn `{}` 期望 {needed} 实参槽（params {:?} ret {:?} loc {:?}），收到 {}",
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
                "ABI 不匹配：track_caller fn `{}` 期望 location 尾实参（收到 {} 槽）",
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

    // 帧守卫：unwind 穿帧 = 跑 cleanup + 恢复区；正常返回 = 恢复区（edge 已空）
    // 影子帧入栈（D8e）：合成 IP = FUNC_IP_BASE + func×64（每 FuncId 唯一、非零、
    // 不可执行的 opaque token；作 backtrace 的 IP 恰好——从不解引用为代码）。
    unsafe { (*ctx).shadow.push(func_synth_ip(func)) };
    let guard = FrameGuard {
        ctx,
        func,
        base,
        unwind_edge: Cell::new(None),
    };
    match run_blocks(ctx, func, base, &guard.unwind_edge, 0) {
        Exit::Ret(lo, hi) => (lo, hi), // guard drop → region 恢复
        Exit::Resume => engine_abort(&format!("Resume 出现在正常执行路径（fn {}）", body.name)),
    }
}

