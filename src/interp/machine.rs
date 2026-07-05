//! MirvmMachine：跑真程序的"fast Machine"——Miri 减去全部检查。
//! 结构参考 rust-lang/miri（MIT/Apache-2.0）src/machine.rs。

use std::borrow::Cow;
use std::cell::RefCell;
use std::fmt;

use rustc_abi::{Align, Size};
use rustc_ast::expand::allocator::{self, SpecialAllocatorMethod};
use rustc_const_eval::interpret::{
    AllocBytes, AllocId, Allocation, CTFE_ALLOC_SALT, CtfeProvenance,
    FnArg, Frame, ImmTy, Immediate, InterpCx, InterpResult, MPlaceTy, MemoryKind, OpTy, PlaceTy,
    Pointer, Provenance as ProvenanceTrait, ReturnAction, ReturnContinuation, interp_ok,
};
use rustc_data_structures::fx::FxHashMap;
use rustc_middle::mir::interpret::MachineStopType;
use rustc_middle::query::TyCtxtAt;
use rustc_middle::ty::layout::TyAndLayout;
use rustc_middle::ty::{self, Ty, TyCtxt};
use rustc_middle::{mir, throw_exhaust, throw_unsup_format};
use rustc_span::Symbol;
use rustc_span::def_id::DefId;
use rustc_symbol_mangling::mangle_internal_symbol;
use rustc_target::callconv::FnAbi;

use super::addrs::AddrTable;
use super::helpers::EcxExt as _;
use super::mono_map::MonoHashMap;

/// mirvm 的指针 provenance：具体分配或 wildcard（int2ptr 产物）。
/// offset 字段存绝对地址（OFFSET_IS_ADDR = true）。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Prov {
    Concrete(AllocId),
    Wildcard,
}

/// 带 provenance 的指针（Miri 叫 StrictPointer）。
pub type MPtr = Pointer<Prov>;

impl ProvenanceTrait for Prov {
    const OFFSET_IS_ADDR: bool = true;
    const WILDCARD: Option<Self> = Some(Prov::Wildcard);

    fn get_alloc_id(self) -> Option<AllocId> {
        match self {
            Prov::Concrete(id) => Some(id),
            Prov::Wildcard => None,
        }
    }

    fn fmt(ptr: &Pointer<Self>, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (prov, addr) = ptr.into_raw_parts();
        write!(f, "{:#x}", addr.bytes())?;
        match prov {
            Prov::Concrete(id) => write!(f, "[{id:?}]"),
            Prov::Wildcard => write!(f, "[wildcard]"),
        }
    }
}

/// 机器自有的内存类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MirvmMemoryKind {
    /// `__rust_alloc` 系（Rust 全局分配器）
    Heap,
    /// 机器杂项（environ、panic 消息等）
    Machine,
    /// 从 tcx 拷贝进机器内存的全局
    Global,
    /// thread-local static 的每线程实例
    Tls,
}

impl fmt::Display for MirvmMemoryKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl rustc_const_eval::interpret::MayLeak for MirvmMemoryKind {
    fn may_leak(self) -> bool {
        true // 不做泄漏检查
    }
}

/// 终止原因（经 MachineStop 错误通道传出解释循环）。
#[derive(Debug)]
pub enum Termination {
    Exit(i32),
    Abort(String),
    Unsupported(String),
}

impl fmt::Display for Termination {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Termination::Exit(code) => write!(f, "程序退出，code = {code}"),
            Termination::Abort(msg) => write!(f, "{msg}"),
            Termination::Unsupported(msg) => write!(f, "{msg}"),
        }
    }
}

impl MachineStopType for Termination {}

/// catch_unwind 挂在 try-fn 栈帧上的恢复数据（Miri 同构）。
pub struct CatchUnwindData<'tcx> {
    pub catch_fn: Pointer<Option<Prov>>,
    pub data: ImmTy<'tcx, Prov>,
    pub dest: MPlaceTy<'tcx, Prov>,
    pub ret: Option<mir::BasicBlock>,
}

#[derive(Default)]
pub struct FrameExtra<'tcx> {
    pub catch_unwind: Option<CatchUnwindData<'tcx>>,
}

/// 分配器 shim 符号的处理方式。
pub enum AllocShim {
    Special(SpecialAllocatorMethod),
    /// 转发到另一个符号（如 #[global_allocator] 的用户实现）——M1 未支持
    Forward(Symbol),
}

pub struct MirvmMachine<'tcx> {
    pub stack: Vec<Frame<'tcx, Prov, FrameExtra<'tcx>>>,
    pub addrs: RefCell<AddrTable>,
    /// thread-local static → 其（单线程）实例
    pub tls_statics: FxHashMap<DefId, MPtr>,
    /// extern static 符号名 → 机器提供的分配
    pub extern_statics: FxHashMap<Symbol, MPtr>,
    /// 未被 catch 消费的 panic payload 栈
    pub unwind_payloads: Vec<ImmTy<'tcx, Prov>>,
    /// `__rust_alloc` 等 mangled 符号 → 处理方式
    pub allocator_shims: FxHashMap<Symbol, AllocShim>,
    /// `__rust_no_alloc_shim_is_unstable_v2` 的 mangled 符号（空操作哨兵）
    pub no_alloc_shim_sym: Symbol,
    /// 按符号名解析已导出函数的缓存（rust_begin_unwind 等）
    pub exported_symbols_cache: FxHashMap<Symbol, Option<ty::Instance<'tcx>>>,
    /// 环境变量名 → 值 C 串指针（getenv 查表；表本体见 eval::setup_process_memory）
    pub env_map: FxHashMap<Vec<u8>, MPtr>,
    /// 由解释程序打开的宿主 fd（open 系 shim 直通宿主）
    pub host_fds: rustc_data_structures::fx::FxHashSet<i32>,
    /// errno 单元（__errno_location shim；宿主调用后同步）
    pub errno_cell: Option<MPtr>,
    /// pthread TLS key（单线程：一把 key 一格值）
    pub pthread_tls: FxHashMap<u32, rustc_middle::mir::interpret::Scalar<Prov>>,
    pub next_pthread_key: u32,
    /// 确定性 getrandom 状态
    pub rng_state: u64,
}

impl<'tcx> MirvmMachine<'tcx> {
    pub fn new(tcx: TyCtxt<'tcx>) -> Self {
        MirvmMachine {
            stack: Vec::new(),
            addrs: RefCell::new(AddrTable::default()),
            tls_statics: FxHashMap::default(),
            extern_statics: FxHashMap::default(),
            unwind_payloads: Vec::new(),
            allocator_shims: Self::allocator_shims(tcx),
            no_alloc_shim_sym: Symbol::intern(&mangle_internal_symbol(
                tcx,
                rustc_ast::expand::allocator::NO_ALLOC_SHIM_IS_UNSTABLE,
            )),
            exported_symbols_cache: FxHashMap::default(),
            env_map: FxHashMap::default(),
            host_fds: rustc_data_structures::fx::FxHashSet::default(),
            errno_cell: None,
            pthread_tls: FxHashMap::default(),
            next_pthread_key: 1,
            rng_state: 0x6d69_7276_6d21,
        }
    }

    /// 分配器 shim 符号表（Miri 同款：拿 codegen 会生成的 shim 内容清单）。
    fn allocator_shims(tcx: TyCtxt<'tcx>) -> FxHashMap<Symbol, AllocShim> {
        use rustc_codegen_ssa::base::allocator_shim_contents;

        let Some(kind) = tcx.allocator_kind(()) else {
            return Default::default();
        };
        let mut out = FxHashMap::default();
        for method in allocator_shim_contents(tcx, kind) {
            let from =
                Symbol::intern(&mangle_internal_symbol(tcx, &allocator::global_fn_name(method.name)));
            let to = match method.special {
                Some(special) => AllocShim::Special(special),
                None => AllocShim::Forward(Symbol::intern(&mangle_internal_symbol(
                    tcx,
                    &allocator::default_fn_name(method.name),
                ))),
            };
            out.insert(from, to);
        }
        out
    }
}

impl<'tcx> rustc_const_eval::interpret::Machine<'tcx> for MirvmMachine<'tcx> {
    type MemoryKind = MirvmMemoryKind;
    type Provenance = Prov;
    type ProvenanceExtra = ();
    /// weak 符号 extern static 提供的"合成函数指针"，按符号名分发（Miri DynSym 同款思路）
    type ExtraFnVal = Symbol;
    type FrameExtra = FrameExtra<'tcx>;
    type AllocExtra = ();
    type Bytes = Box<[u8]>;
    type MemoryMap =
        MonoHashMap<AllocId, (MemoryKind<MirvmMemoryKind>, Allocation<Prov, (), Box<[u8]>>)>;

    const GLOBAL_KIND: Option<MirvmMemoryKind> = Some(MirvmMemoryKind::Global);
    const PANIC_ON_ALLOC_FAIL: bool = false;

    // ===== 检查全关：这是 "fast" 的核心 =====

    #[inline(always)]
    fn enforce_alignment(_ecx: &InterpCx<'tcx, Self>) -> bool {
        false
    }

    #[inline(always)]
    fn enforce_validity(_ecx: &InterpCx<'tcx, Self>, _layout: TyAndLayout<'tcx>) -> bool {
        false
    }

    #[inline(always)]
    fn ignore_optional_overflow_checks(_ecx: &InterpCx<'tcx, Self>) -> bool {
        false
    }

    fn runtime_checks(
        ecx: &InterpCx<'tcx, Self>,
        r: mir::RuntimeChecks,
    ) -> InterpResult<'tcx, bool> {
        use mir::RuntimeChecks::*;
        interp_ok(match r {
            UbChecks => false,
            ContractChecks => false,
            OverflowChecks => ecx.tcx.sess.overflow_checks(),
        })
    }

    #[inline(always)]
    fn protect_in_place_function_argument(
        _ecx: &mut InterpCx<'tcx, Self>,
        _mplace: &MPlaceTy<'tcx, Prov>,
    ) -> InterpResult<'tcx> {
        // 不做保护（Miri 用它抓 aliasing 问题；我们要速度）
        interp_ok(())
    }

    // ===== 函数分发 =====

    fn find_mir_or_eval_fn(
        ecx: &mut InterpCx<'tcx, Self>,
        instance: ty::Instance<'tcx>,
        abi: &FnAbi<'tcx, Ty<'tcx>>,
        args: &[FnArg<'tcx, Prov>],
        dest: &PlaceTy<'tcx, Prov>,
        ret: Option<mir::BasicBlock>,
        unwind: mir::UnwindAction,
    ) -> InterpResult<'tcx, Option<(&'tcx mir::Body<'tcx>, ty::Instance<'tcx>)>> {
        if ecx.tcx.is_foreign_item(instance.def_id()) {
            let args = InterpCx::<'tcx, Self>::copy_fn_args(args);
            let link_name = Symbol::intern(ecx.tcx.symbol_name(instance).name);
            return super::shims::emulate_foreign_item(ecx, link_name, abi, &args, dest, ret, unwind);
        }
        // std_detect 的 CPU 特性检测走 CPUID 内联汇编（解释器不支持）。
        // 拦截为"无任何特性"→ memchr/aho-corasick 等走标量路径，语义不变。
        // （Miri 靠 cfg(miri) 的 std 绕开；我们用干净 std，只能在这拦。）
        {
            let def_id = instance.def_id();
            if ecx.tcx.crate_name(def_id.krate).as_str() == "std_detect"
                && ecx.tcx.item_name(def_id).as_str() == "detect_features"
            {
                ecx.write_scalar(
                    rustc_middle::mir::interpret::Scalar::from_uint(0u128, dest.layout.size),
                    dest,
                )?;
                ecx.return_to_block(ret)?;
                return interp_ok(None);
            }
        }
        interp_ok(Some((ecx.load_mir(instance.def, None)?, instance)))
    }

    #[inline(always)]
    fn call_extra_fn(
        ecx: &mut InterpCx<'tcx, Self>,
        fn_val: Symbol,
        abi: &FnAbi<'tcx, Ty<'tcx>>,
        args: &[FnArg<'tcx, Prov>],
        dest: &PlaceTy<'tcx, Prov>,
        ret: Option<mir::BasicBlock>,
        unwind: mir::UnwindAction,
    ) -> InterpResult<'tcx> {
        let args = InterpCx::<'tcx, Self>::copy_fn_args(args);
        let body =
            super::shims::emulate_foreign_item(ecx, fn_val, abi, &args, dest, ret, unwind)?;
        assert!(body.is_none(), "合成函数指针不支持转发到 MIR");
        interp_ok(())
    }

    #[inline(always)]
    fn call_intrinsic(
        ecx: &mut InterpCx<'tcx, Self>,
        instance: ty::Instance<'tcx>,
        args: &[OpTy<'tcx, Prov>],
        dest: &PlaceTy<'tcx, Prov>,
        ret: Option<mir::BasicBlock>,
        unwind: mir::UnwindAction,
    ) -> InterpResult<'tcx, Option<ty::Instance<'tcx>>> {
        super::intrinsics::call_intrinsic(ecx, instance, args, dest, ret, unwind)
    }

    #[inline(always)]
    fn check_fn_target_features(
        _ecx: &InterpCx<'tcx, Self>,
        _instance: ty::Instance<'tcx>,
    ) -> InterpResult<'tcx> {
        interp_ok(())
    }

    // ===== panic 路径 =====

    fn assert_panic(
        ecx: &mut InterpCx<'tcx, Self>,
        msg: &mir::AssertMessage<'tcx>,
        unwind: mir::UnwindAction,
    ) -> InterpResult<'tcx> {
        ecx.assert_panic_impl(msg, unwind)
    }

    fn panic_nounwind(ecx: &mut InterpCx<'tcx, Self>, msg: &str) -> InterpResult<'tcx> {
        ecx.start_panic_nounwind(msg)
    }

    fn unwind_terminate(
        ecx: &mut InterpCx<'tcx, Self>,
        reason: mir::UnwindTerminateReason,
    ) -> InterpResult<'tcx> {
        let panic = ecx.tcx.lang_items().get(reason.lang_item()).unwrap();
        let panic = ty::Instance::mono(ecx.tcx.tcx, panic);
        ecx.call_function(
            panic,
            &[],
            None,
            ReturnContinuation::Goto { ret: None, unwind: mir::UnwindAction::Unreachable },
        )
    }

    // ===== 指针运算 =====

    fn binary_ptr_op(
        ecx: &InterpCx<'tcx, Self>,
        bin_op: mir::BinOp,
        left: &ImmTy<'tcx, Prov>,
        right: &ImmTy<'tcx, Prov>,
    ) -> InterpResult<'tcx, ImmTy<'tcx, Prov>> {
        use mir::BinOp::*;
        interp_ok(match bin_op {
            Eq | Ne | Lt | Le | Gt | Ge => {
                // 直接比 bits（OFFSET_IS_ADDR：offset 就是地址）。ScalarPair 按字典序。
                let size = ecx.tcx.data_layout.pointer_size();
                let to_pair = |imm: &ImmTy<'tcx, Prov>| -> InterpResult<'tcx, (u128, u128)> {
                    interp_ok(match **imm {
                        Immediate::Scalar(l) => (l.to_bits(size)?, 0),
                        Immediate::ScalarPair(l1, l2) => (l1.to_bits(size)?, l2.to_bits(size)?),
                        Immediate::Uninit => panic!("uninit data in binary_ptr_op"),
                    })
                };
                let l = to_pair(left)?;
                let r = to_pair(right)?;
                let res = match bin_op {
                    Eq => l == r,
                    Ne => l != r,
                    Lt => l < r,
                    Le => l <= r,
                    Gt => l > r,
                    Ge => l >= r,
                    _ => unreachable!(),
                };
                ImmTy::from_bool(res, *ecx.tcx)
            }
            // 原子指针运算等：RHS 是 usize
            Add | Sub | BitOr | BitAnd | BitXor => {
                let ptr = left.to_scalar().to_pointer(ecx)?;
                let usize_layout = ecx.layout_of(ecx.tcx.types.usize)?;
                let l = ImmTy::from_uint(ptr.addr().bytes(), usize_layout);
                let result = ecx.binary_op(bin_op, &l, right)?;
                let result_ptr = Pointer::new(
                    ptr.provenance,
                    Size::from_bytes(result.to_scalar().to_target_usize(ecx)?),
                );
                ImmTy::from_scalar(
                    rustc_middle::mir::interpret::Scalar::from_maybe_pointer(result_ptr, ecx),
                    left.layout,
                )
            }
            _ => span_bug!(ecx.cur_span(), "invalid pointer op {bin_op:?}"),
        })
    }

    #[inline(always)]
    fn float_fuse_mul_add(_ecx: &InterpCx<'tcx, Self>) -> bool {
        false
    }

    // ===== statics =====

    fn thread_local_static_pointer(
        ecx: &mut InterpCx<'tcx, Self>,
        def_id: DefId,
    ) -> InterpResult<'tcx, Pointer<Prov>> {
        if let Some(&ptr) = ecx.machine.tls_statics.get(&def_id) {
            return interp_ok(ptr);
        }
        if ecx.tcx.is_foreign_item(def_id) {
            throw_unsup_format!("不支持 foreign thread-local static");
        }
        // 单线程：给每个 TLS static 一份可变实例（Miri get_or_create_thread_local_alloc 同款）
        let alloc = ecx.tcx.eval_static_initializer(def_id)?;
        let mut alloc = alloc.inner().adjust_from_tcx(
            &ecx.tcx,
            |bytes, align| {
                interp_ok(<Box<[u8]> as AllocBytes>::from_bytes(Cow::Borrowed(bytes), align, ()))
            },
            |ptr| ecx.global_root_pointer(ptr),
        )?;
        alloc.mutability = rustc_hir::Mutability::Mut;
        let ptr = ecx.insert_allocation(alloc, MemoryKind::Machine(MirvmMemoryKind::Tls))?;
        ecx.machine.tls_statics.insert(def_id, ptr);
        interp_ok(ptr)
    }

    fn extern_static_pointer(
        ecx: &InterpCx<'tcx, Self>,
        def_id: DefId,
    ) -> InterpResult<'tcx, Pointer<Prov>> {
        let instance = ty::Instance::mono(ecx.tcx.tcx, def_id);
        let name = Symbol::intern(ecx.tcx.symbol_name(instance).name);
        match ecx.machine.extern_statics.get(&name) {
            Some(&ptr) => interp_ok(ptr),
            None => throw_unsup_format!("不支持的 extern static: `{name}`"),
        }
    }

    // ===== 地址 / provenance =====

    fn ptr_from_addr_cast(
        _ecx: &InterpCx<'tcx, Self>,
        addr: u64,
    ) -> InterpResult<'tcx, Pointer<Option<Prov>>> {
        interp_ok(Pointer::new(Some(Prov::Wildcard), Size::from_bytes(addr)))
    }

    #[inline(always)]
    fn expose_provenance(_ecx: &InterpCx<'tcx, Self>, _provenance: Prov) -> InterpResult<'tcx> {
        // 全部分配都视为已 expose，无需记录
        interp_ok(())
    }

    fn ptr_get_alloc(
        ecx: &InterpCx<'tcx, Self>,
        ptr: Pointer<Prov>,
        size: i64,
    ) -> Option<(AllocId, Size, ())> {
        let (prov, addr) = ptr.into_raw_parts();
        let addr = addr.bytes();
        let alloc_id = match prov {
            Prov::Concrete(id) => id,
            Prov::Wildcard => ecx.machine.addrs.borrow().lookup(addr, size, |id| {
                ecx.get_alloc_info(id).size.bytes()
            })?,
        };
        let base = ecx.machine.addrs.borrow().base_of(alloc_id)?;
        Some((alloc_id, Size::from_bytes(addr.wrapping_sub(base)), ()))
    }

    fn adjust_alloc_root_pointer(
        ecx: &InterpCx<'tcx, Self>,
        ptr: Pointer<CtfeProvenance>,
        _kind: Option<MemoryKind<MirvmMemoryKind>>,
    ) -> InterpResult<'tcx, Pointer<Prov>> {
        let (prov, offset) = ptr.prov_and_relative_offset();
        let alloc_id = prov.alloc_id();
        let info = ecx.get_alloc_info(alloc_id);
        let base = ecx.machine.addrs.borrow_mut().addr_for(alloc_id, info.size, info.align);
        interp_ok(Pointer::new(
            Prov::Concrete(alloc_id),
            Size::from_bytes(base + offset.bytes()),
        ))
    }

    fn adjust_global_allocation<'b>(
        ecx: &InterpCx<'tcx, Self>,
        _id: AllocId,
        alloc: &'b Allocation,
    ) -> InterpResult<'tcx, Cow<'b, Allocation<Prov, (), Box<[u8]>>>> {
        let alloc = alloc.adjust_from_tcx(
            &ecx.tcx,
            |bytes, align| {
                interp_ok(<Box<[u8]> as AllocBytes>::from_bytes(Cow::Borrowed(bytes), align, ()))
            },
            |ptr| ecx.global_root_pointer(ptr),
        )?;
        interp_ok(Cow::Owned(alloc))
    }

    fn init_local_allocation(
        _ecx: &InterpCx<'tcx, Self>,
        _id: AllocId,
        _kind: MemoryKind<MirvmMemoryKind>,
        _size: Size,
        _align: Align,
    ) -> InterpResult<'tcx, ()> {
        interp_ok(())
    }

    fn before_memory_deallocation(
        _tcx: TyCtxtAt<'tcx>,
        machine: &mut Self,
        _alloc_extra: &mut (),
        _ptr: Pointer<Option<Prov>>,
        (alloc_id, _): (AllocId, ()),
        _size: Size,
        _align: Align,
        _kind: MemoryKind<MirvmMemoryKind>,
    ) -> InterpResult<'tcx> {
        machine.addrs.get_mut().on_dealloc(alloc_id);
        interp_ok(())
    }

    // ===== 栈帧 =====

    fn init_frame(
        ecx: &mut InterpCx<'tcx, Self>,
        frame: Frame<'tcx, Prov>,
    ) -> InterpResult<'tcx, Frame<'tcx, Prov, FrameExtra<'tcx>>> {
        if ecx.machine.stack.len() >= 1_000_000 {
            throw_exhaust!(StackFrameLimitReached);
        }
        interp_ok(frame.with_extra(FrameExtra::default()))
    }

    #[inline(always)]
    fn stack<'a>(
        ecx: &'a InterpCx<'tcx, Self>,
    ) -> &'a [Frame<'tcx, Prov, FrameExtra<'tcx>>] {
        &ecx.machine.stack
    }

    #[inline(always)]
    fn stack_mut<'a>(
        ecx: &'a mut InterpCx<'tcx, Self>,
    ) -> &'a mut Vec<Frame<'tcx, Prov, FrameExtra<'tcx>>> {
        &mut ecx.machine.stack
    }

    fn after_stack_pop(
        ecx: &mut InterpCx<'tcx, Self>,
        mut frame: Frame<'tcx, Prov, FrameExtra<'tcx>>,
        unwinding: bool,
    ) -> InterpResult<'tcx, ReturnAction> {
        if let (true, Some(catch)) = (unwinding, frame.extra.catch_unwind.take()) {
            // 弹掉的是 catch_unwind 压的 try-fn 帧：写 1 到 dest，调 catch_fn。
            ecx.write_scalar(
                rustc_middle::mir::interpret::Scalar::from_uint(1u128, catch.dest.layout.size),
                &catch.dest,
            )?;
            let payload = ecx.machine.unwind_payloads.pop().unwrap();
            let f = ecx.get_ptr_fn(catch.catch_fn)?.as_instance()?;
            ecx.call_function(
                f,
                &[catch.data, payload],
                None,
                ReturnContinuation::Goto {
                    ret: catch.ret,
                    unwind: mir::UnwindAction::Unreachable,
                },
            )?;
            return interp_ok(ReturnAction::NoJump);
        }
        interp_ok(ReturnAction::Normal)
    }

    // ===== 杂项 =====

    #[inline(always)]
    fn get_global_alloc_salt(
        _ecx: &InterpCx<'tcx, Self>,
        _instance: Option<ty::Instance<'tcx>>,
    ) -> usize {
        CTFE_ALLOC_SALT
    }

    #[inline(always)]
    fn get_default_alloc_params(&self) -> <Box<[u8]> as AllocBytes>::AllocParams {}
}

use rustc_middle::span_bug;
