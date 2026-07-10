//! M4 引擎字节码 IR（类型化；纯 Rust，零 rustc 类型——完全自包含的冻结产物）。
//!
//! M4.1 升级（m4.1-design §3.1）：**静态槽 → place 求值**。Deref/Index 是运行期地址，
//! 静态偏移撑不住 → 地址表达式 `PlaceExpr`（lower 编译投影链，引擎按序求值得真地址）。
//! 帧基址是真地址（F6）⇒ 帧内/堆上/statics 统一为裸地址读写。
//! 快路径保留：纯帧内静态偏移的标量访问仍是 `Slot`（零求值开销）。
//!
//! 与 spike bytecode（../bytecode.rs）分离：spikes 是冻结的验证工件，本 IR 是 M4 真身。

pub type Bb = u32;
pub type FuncId = u32;

/// 标量宽度。W128 = 两槽通道（M4.1 第 3 步）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Width {
    W8,
    W16,
    W32,
    W64,
}

impl Width {
    #[inline]
    pub fn bytes(self) -> u32 {
        match self {
            Width::W8 => 1,
            Width::W16 => 2,
            Width::W32 => 4,
            Width::W64 => 8,
        }
    }
    #[inline]
    pub fn mask(self) -> u64 {
        match self {
            Width::W8 => 0xff,
            Width::W16 => 0xffff,
            Width::W32 => 0xffff_ffff,
            Width::W64 => u64::MAX,
        }
    }
    pub fn from_bytes(n: u64) -> Option<Width> {
        Some(match n {
            1 => Width::W8,
            2 => Width::W16,
            4 => Width::W32,
            8 => Width::W64,
            _ => return None,
        })
    }
}

/// 帧内标量槽（快路径）：冻结偏移（Field 投影已折进 off）。
#[derive(Clone, Copy, Debug)]
pub struct Slot {
    pub off: u32,
    pub width: Width,
}

// ===== place 求值（M4.1 核心）=====

/// 地址表达式的基。
#[derive(Clone, Copy, Debug)]
pub enum PlaceBase {
    /// 帧内局部：真地址 = 帧基址 + off
    Local(u32),
    /// 冻结区真地址（statics/常量池，M4.1 第 4 步物化）
    Static(u64),
}

/// 地址表达式的一步（lower 已把 Field/Downcast 折叠成 Offset）。
#[derive(Clone, Copy, Debug)]
pub enum PlaceStep {
    /// 当前地址处读出指针（W64），地址切换为它
    Deref,
    /// 常量字节偏移（可负——slice 尾投影 `len-k` 折出负项）
    Offset(i32),
    /// 动态下标：地址 += 帧内 idx 槽值 × stride
    IndexScaled { idx: Slot, stride: u64 },
}

/// 地址表达式：引擎按序求值 → 真地址 u64。
#[derive(Clone, Debug)]
pub struct PlaceExpr {
    pub base: PlaceBase,
    pub steps: Box<[PlaceStep]>,
}

/// 标量位置：读/写一个 ≤64 位标量的落点。
#[derive(Clone, Debug)]
pub enum ScalarPlace {
    /// 快路径：帧内静态槽
    Slot(Slot),
    /// 慢路径：地址表达式处的标量
    Mem { expr: PlaceExpr, width: Width },
}

#[derive(Clone, Debug)]
pub enum Operand {
    /// 帧内静态槽（快路径）
    Slot(Slot),
    /// 地址表达式处的标量
    Mem { expr: PlaceExpr, width: Width },
    Imm { bits: u64, width: Width },
    /// place 的真地址本身（indirect 实参 = 传聚合的地址）
    AddrOf(PlaceExpr),
    /// 值减常量（Subslice 的 slice meta：len' = len − k；M4.4）
    SubImm { base: Box<Operand>, sub: u64 },
}

impl Operand {
    #[inline]
    pub fn width(&self) -> Width {
        match self {
            Operand::Slot(s) => s.width,
            Operand::Mem { width, .. } => *width,
            Operand::Imm { width, .. } => *width,
            Operand::AddrOf(_) => Width::W64,
            Operand::SubImm { base, .. } => base.width(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum IntBinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
}

#[derive(Clone, Copy, Debug)]
pub enum IntCc {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Clone, Copy, Debug)]
pub enum OvfOp {
    Add,
    Sub,
    Mul,
}

#[derive(Clone, Copy, Debug)]
pub enum FloatOp {
    Add,
    Sub,
    Mul,
    Div,
    /// IEEE fmod（Rust `%` 浮点语义）
    Rem,
}

/// 数学一元（must_be_overridden float intrinsic 的合成处置：宿主 f32/f64 直算，P7）
#[derive(Clone, Copy, Debug)]
pub enum MathUnOp {
    Sqrt,
    Sin,
    Cos,
    Exp,
    Exp2,
    Ln,
    Log2,
    Log10,
    Fabs,
    Floor,
    Ceil,
    Trunc,
    Round,
    RoundTiesEven,
}

/// 数学二元（powf/powi/copysign/minnum/maxnum；powi 的 b 是 i32 位）
#[derive(Clone, Copy, Debug)]
pub enum MathBinOp {
    Pow,
    Powi,
    Copysign,
    Minnum,
    Maxnum,
}

/// 位操作单目（ctpop/ctlz/cttz/bswap/bitreverse intrinsic 内建）
#[derive(Clone, Copy, Debug)]
pub enum BitUnOp {
    Popcount,
    Ctlz,
    Cttz,
    Bswap,
    Bitreverse,
}

/// 原子 RMW（fetch_* 家族；全 SeqCst——最强序在 RAM non-det 包络内，order 细化 M4.4）
#[derive(Clone, Copy, Debug)]
pub enum RmwOp {
    Xchg,
    Add,
    Sub,
    And,
    Or,
    Xor,
    Nand,
}

/// SIMD 逐 lane 双目（M4.1 最小集：hashbrown SSE2 group 探测所需）。
/// 比较产出 mask lane（真=全 1）；位运算逐 lane。
#[derive(Clone, Copy, Debug)]
pub enum SimdBinOp {
    Eq,
    Ne,
    /// 有符号性来自 lane 元素类型（冻结）
    Lt { signed: bool },
    Le { signed: bool },
    Gt { signed: bool },
    Ge { signed: bool },
    And,
    Or,
    Xor,
    Add,
    Sub,
}

#[derive(Clone, Debug)]
pub enum Rvalue {
    Use(Operand),
    /// guest TLS 实例真地址（M4.4 D3）：Ctx.tls[id] 惰性物化（heap 分配 + 模板拷贝）。
    TlsRef(TlsId),
    // （Subslice 的 meta 走 Operand::SubImm，无独立 rvalue）
    IntBin { op: IntBinOp, signed: bool, a: Operand, b: Operand },
    /// → bool（W8）
    IntCmp { cc: IntCc, signed: bool, a: Operand, b: Operand },
    /// 按位取反（掩到宽度）
    NotBits(Operand),
    /// 逻辑取反（bool：xor 1）——rustc 对 bool 的 Not 语义
    NotBool(Operand),
    /// 二补数取负
    Neg(Operand),
    /// IntToInt：截断后按 from 的符号扩展到 to
    Cast { from: (Width, bool), to: Width, a: Operand },
    /// 取 place 真地址（Ref/RawPtr 同一实现——真实地址模型）
    Ref(PlaceExpr),
    /// 指针算术：ptr + count × stride（BinOp::Offset 与 offset/arith_offset intrinsic）
    PtrOffset { ptr: Operand, count: Operand, stride: u64 },
    /// 三路比较（BinOp::Cmp）→ Ordering（i8：-1/0/1）
    IntCmp3 { signed: bool, a: Operand, b: Operand },
    /// niche 编码判别式读（Direct 编码在 lower 期溶解为 Cast）：
    /// rel = (tag - niche_start) 按 tag 宽 wrapping；rel < len → variants_start+rel，
    /// 否则 untagged。niche 不变量：discr 值 == variant index（rustc layout sanity check）。
    NicheDiscr {
        tag: Operand,
        niche_start: u64,
        variants_start: u64,
        variants_len: u64,
        untagged: u64,
    },
    /// 浮点四则（位进位出：操作数是 f32/f64 的位型）
    FloatBin { op: FloatOp, is64: bool, a: Operand, b: Operand },
    /// 数学一元/二元（宿主直算；M4.5 补 must_be_overridden float intrinsic 面）
    MathUn { op: MathUnOp, is64: bool, a: Operand },
    MathBin { op: MathBinOp, is64: bool, a: Operand, b: Operand },
    /// 无符号取大（unsized 尾对齐 = max(sized_align, 运行期 vtable align)，M4.5）
    UMax { a: Operand, b: Operand },
    /// 浮点比较（IEEE 语义，NaN 全 false 除 Ne）→ bool
    FloatCmp { cc: IntCc, is64: bool, a: Operand, b: Operand },
    FloatNeg { is64: bool, a: Operand },
    /// f32↔f64
    FloatCast { from64: bool, to64: bool, a: Operand },
    /// float → int（Rust `as` 饱和语义：NaN→0、越界→边界）
    FloatToInt { from64: bool, to: Width, signed: bool, a: Operand },
    /// int → float
    IntToFloat { from: (Width, bool), to64: bool, a: Operand },
    /// 位操作单目（按操作数宽度语义：ctlz(W8) 是 8 位前导零）
    BitUn { op: BitUnOp, a: Operand },
    /// 原子读（真宿主原子指令——spike4 义务；SeqCst）
    AtomicLoad { addr: Operand, width: Width },
    /// 指针差（ptr_offset_from[_unsigned]）：(a - b) / stride（i64 除法）
    PtrDiff { a: Operand, b: Operand, stride: u64 },
    /// SIMD movemask：收集各 lane 最高位 → 整数标量（simd_bitmask）
    SimdBitmask { a: PlaceExpr, lanes: u16, lane_bytes: u8 },
    /// 字节比较（compare_bytes intrinsic = memcmp）→ i32（-1/0/1 语义按首异字节）
    MemCmp { a: Operand, b: Operand, n: Operand },
    /// 128 位整数比较（TypeId 判等等；操作数是 16 字节 place）→ bool
    Cmp128 { cc: IntCc, signed: bool, a: PlaceExpr, b: PlaceExpr },
    /// 饱和算术（saturating_add/sub intrinsic）
    IntSat { op: OvfOp, signed: bool, a: Operand, b: Operand },
    /// SIMD 归约（simd_reduce_all/any：mask 向量全真/任真）→ bool
    SimdReduce { all: bool, a: PlaceExpr, lanes: u16, lane_bytes: u8 },
}

#[derive(Clone, Debug)]
pub enum Stmt {
    Assign {
        dst: ScalarPlace,
        rv: Rvalue,
    },
    /// *WithOverflow：一次写 (值槽, 溢出旗标槽)——MIR 的 (T,bool) 标量对，
    /// 两个 dst 的 off 来自冻结的 pair 布局（.0/.1 的 Field 偏移）。
    AssignOverflow {
        op: OvfOp,
        signed: bool,
        a: Operand,
        b: Operand,
        dst_val: ScalarPlace,
        dst_flag: ScalarPlace,
    },
    /// 聚合搬运（memcpy 语义；pair/聚合整体拷贝的通道）
    Copy {
        dst: PlaceExpr,
        src: PlaceExpr,
        size: u32,
    },
    /// 重复填充：dst 起 count 个元素，每个 elem_size 字节，值来自 src 标量或 memcpy
    /// （`[expr; N]` 的 Repeat rvalue；elem ≤8 字节走标量循环）
    RepeatScalar {
        dst: PlaceExpr,
        val: Operand,
        count: u64,
        elem_size: u32,
    },
    /// 原子写（SeqCst）
    AtomicStore { addr: Operand, val: Operand },
    /// 原子比较交换：dst_val = 旧值，dst_ok = 是否成功（SeqCst/SeqCst）
    AtomicCxchg {
        addr: Operand,
        expected: Operand,
        new: Operand,
        dst_val: ScalarPlace,
        dst_ok: ScalarPlace,
        weak: bool,
    },
    /// 原子 RMW：dst = 旧值（SeqCst）
    AtomicRmw { op: RmwOp, addr: Operand, val: Operand, dst: ScalarPlace },
    /// 动态长度内存拷贝（copy/copy_nonoverlapping intrinsic：count × elem_size 字节）
    MemCopy { dst: Operand, src: Operand, count: Operand, elem_size: u64, overlap: bool },
    /// 动态长度填充（write_bytes：val 是 u8，count × elem_size 字节）
    MemSet { dst: Operand, val: Operand, count: Operand, elem_size: u64 },
    /// SIMD 逐 lane 双目（dst/a/b 是向量 place；几何冻结自 layout）
    SimdBin {
        op: SimdBinOp,
        dst: PlaceExpr,
        a: PlaceExpr,
        b: PlaceExpr,
        lanes: u16,
        lane_bytes: u8,
    },
    /// SIMD 广播（simd_splat / _mm_set1）：val 复制到每个 lane
    SimdSplat { dst: PlaceExpr, val: Operand, lanes: u16, lane_bytes: u8 },
    /// 128 位整数双目（宿主 u128 直算：读两半组 → 算 → 写两半）；
    /// with_overflow 时 dst 是 (u128, bool) 布局（旗标写 dst+16）
    Bin128 {
        op: IntBinOp,
        signed: bool,
        a: PlaceExpr,
        b: Bin128Rhs,
        dst: PlaceExpr,
        with_overflow: bool,
    },
    /// 128 位整数 → 浮点（u128/i128 as f32/f64；宿主直转，M4.5 tokio 定时器逼出）
    Wide128ToFloat { src: PlaceExpr, signed: bool, to64: bool, dst: ScalarPlace },
    /// 128 位 niche 判别式读（regex_automata 的 Result<DFA,_> 大 niche，M4.5）：
    /// rel = tag − niche_start（u128 wrapping）；rel < len → variants_start+rel，否则 untagged
    NicheDiscr128 {
        tag: PlaceExpr,
        niche_start: u128,
        variants_start: u64,
        variants_len: u64,
        untagged: u64,
        dst: ScalarPlace,
    },
    /// 语句级 Trap 占位：执行到即诊断退出，但**块的终止子照常降低**——
    /// 保住 Call 边，使 --vm-stats 的可达分析准确（仪器盲点修复）。
    Trap(Box<str>),
    Nop,
    /// 内存栅栏（M4.4 D4）：atomic_fence → 宿主 fence(SeqCst)；
    /// single_thread（atomic_singlethreadfence）→ compiler_fence(SeqCst)
    Fence { single_thread: bool },
    /// `[expr; N]` 聚合元素通道（M4.4）：dst[0] 已写好，从它铺满 i∈[1,count)
    RepeatBytes { first: PlaceExpr, count: u64, elem_size: u64 },
}

/// unwind 处置（M4.2 起全语义：FrameGuard 动态 LSDA，spike3 协议）。
/// MIR 的 Unreachable 折进 Continue（unwind 到此=UB，fast 不检测）。
#[derive(Clone, Copy, Debug)]
pub enum UnwindAction {
    Continue,
    Cleanup(Bb),
    /// unwind 到此即中止（double panic / extern "C" ABI 边界）
    Terminate,
}

/// 引擎原语（foreign 三路处置①，debt-map §2-B）：std 自己声明的 runtime extern 边界，
/// native 下由 codegen/链接器合成 shim——引擎在同一边界接管。
/// alloc 系的引擎实现是 M4.1 第 5 步（堆内建）；落地前 lower 前置 `Stmt::Trap` 防静默。
#[derive(Clone, Copy, Debug)]
pub enum Builtin {
    /// `__rust_alloc(size, align) -> ptr`
    RustAlloc,
    /// `__rust_dealloc(ptr, size, align)`
    RustDealloc,
    /// `__rust_realloc(ptr, old_size, align, new_size) -> ptr`
    RustRealloc,
    /// `__rust_alloc_zeroed(size, align) -> ptr`
    RustAllocZeroed,
    /// `__rust_no_alloc_shim_is_unstable_v2()`：分配前哨兵，空操作
    NoAllocShim,
    /// `_Unwind_RaiseException(exc) -> !`：unwind 原语（M4.2，spike3 的 raise）——
    /// 宿主 unwinder 载运 guest exception 指针（panic_unwind 结构在 guest 堆闭环）
    UnwindRaise,
    /// `catch_unwind(try_fn, data, catch_fn) -> i32` intrinsic（rust_try）：
    /// 宿主 catch + 间接调用派发；downcast 区分 GuestPanic/宿主 panic
    CatchUnwind,
    /// os:: 最小直通（panic 链需要，真实地址零编组；M4.3 换正式注册表 dlsym+libffi）
    HostGetenv,
    /// `write(fd, buf, len) -> isize`
    HostWrite,
    /// `strlen(s) -> usize`
    HostStrlen,
    /// `abort() -> !`（libc abort 语义；core::intrinsics::abort 也汇入）
    HostAbort,
    /// `syscall(nr, ...) -> long` 可变参直通（按实参个数分派）
    HostSyscall,
    /// stub：返回 0、无副作用（sigaction/sigaltstack/atexit/dl_iterate_phdr/
    /// _Unwind_Backtrace 等——真实现挂 M4.4 thunk 或永不需要）
    StubZero,
    /// stub：空操作无返回（_Unwind_DeleteException/llvm.x86.sse2.pause 等）
    StubNop,
}

/// libffi 直通的参数/返回类别（lower 期从 fn sig layout 冻结；os:: P7 直通处置）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FfiKind {
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
    F32,
    F64,
    Ptr,
    Void,
}

/// Bin128 的右操作数：128 位 place 或 ≤64 位标量（Shl/Shr 的移位量）。
#[derive(Clone, Debug)]
pub enum Bin128Rhs {
    Wide(PlaceExpr),
    Scalar(Operand),
}

/// guest TLS 槽 id（`#[thread_local]` static 的稠密编号，M4.4 D3）。
pub type TlsId = u32;

/// guest TLS 槽描述（lower 冻结）：template = 初始字节在冻结区的真地址（含重定位），
/// 每线程首访时 heap 分配 size 字节拷模板。v1 记账：dtor 不跑（设计 D3）。
#[derive(Clone, Copy, Debug)]
pub struct TlsSlot {
    pub template: u64,
    pub size: u64,
    pub align: u32,
}

/// 冻结的 foreign 签名。变参函数按**调用点实参**冻结尾参（fixed = 固定参数个数）。
/// Eq/Hash：thunk 工厂缓存键（M4.4 D1——(fn 条目地址, 逃逸位签名) → 真码地址）。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ForeignSig {
    pub args: Vec<FfiKind>,
    pub ret: FfiKind,
    /// Some(n) = 变参函数，前 n 个是固定参数（libffi prep_cif_var）
    pub fixed: Option<usize>,
    /// fn-ptr 类型的参数位（M4.4 D1）：位置 + 该 fn ptr 自身的冻结签名。
    /// 执行期：该位实参 = fn 条目地址（fn_addrs 反查命中）→ 换 thunk 真码；
    /// NULL 或已是 native 真码 → 原样直传。内层签名的 thunk_args 恒空（不嵌套）。
    pub thunk_args: Vec<(usize, ForeignSig)>,
}

/// 参数在 callee 帧内的落位（引擎调用约定 v2：实参展平为 `&[u64]` 槽序列）。
#[derive(Clone, Copy, Debug)]
pub enum ParamAbi {
    /// ZST：不占实参槽
    Zst,
    /// 标量：1 槽
    Scalar(Slot),
    /// 标量对：2 槽（lo, hi 各自的帧内槽，偏移来自冻结 pair 布局）
    Pair(Slot, Slot),
    /// 大聚合：1 槽 = src 真地址；prologue memcpy `size` 字节到帧内 `off`
    Indirect { off: u32, size: u32 },
}

/// 返回通道（引擎调用约定 v2）。
#[derive(Clone, Copy, Debug)]
pub enum RetAbi {
    Zst,
    /// 标量：interp_frame 返回 lo
    Scalar(Slot),
    /// 标量对：返回 (lo, hi)
    Pair(Slot, Slot),
    /// 大聚合：caller 前插隐藏首实参 = 目的真地址；callee Return 时
    /// memcpy(隐藏指针槽, _0 槽, size)。隐藏指针槽附加在帧尾（sret_off）。
    Indirect { ret_off: u32, size: u32, sret_off: u32 },
}

/// Call 的返回落点（caller 侧）。
#[derive(Clone, Debug)]
pub enum RetDest {
    /// 忽略（ZST 或无落点）
    Ignore,
    Scalar(ScalarPlace),
    /// pair 两半的落点（dst place + 冻结的两半偏移/宽度）
    Pair(ScalarPlace, ScalarPlace),
    /// 大聚合：caller 求好目的真地址，作为隐藏首实参传入（Call 时前插）
    Indirect(PlaceExpr),
}

#[derive(Clone, Debug)]
pub enum Terminator {
    Goto(Bb),
    SwitchInt {
        discr: Operand,
        targets: Vec<(u128, Bb)>,
        otherwise: Bb,
    },
    Call {
        callee: FuncId,
        args: Vec<Operand>,
        ret: RetDest,
        target: Bb,
        unwind: UnwindAction,
    },
    /// 引擎原语调用（不是 guest 函数，无 Call 边）。
    CallBuiltin {
        builtin: Builtin,
        args: Vec<Operand>,
        ret: RetDest,
        target: Bb,
        unwind: UnwindAction,
    },
    /// foreign 直通（os:: P7 处置①的通用道）：dlsym + libffi 按冻结签名直调——
    /// 真实地址模型零编组（guest 指针即宿主指针）。
    CallForeign {
        sym: Box<str>,
        sig: ForeignSig,
        args: Vec<Operand>,
        ret: RetDest,
        target: Bb,
        unwind: UnwindAction,
    },
    /// 间接调用（fn-ptr / dyn 虚派发）：callee 求值 = fn 条目真地址（D4），
    /// 经 Module.fn_addrs 反查 FuncId。--vm-stats 可达分析无出边（已知盲点）。
    /// null_ok：dyn 虚 drop 的 vtable 槽 0 可为 null（无 Drop 的类型）= 空操作。
    /// native_sig（M4.4，FFI 反方向之二）：extern "C" 系 fn-ptr 调用点的冻结签名——
    /// 反查未命中 = guest 持 native 真码（运行期 dlsym 所得，如 __pthread_get_minstack）
    /// → libffi 按此签名直调；None（Rust ABI / 不可类）时未命中即诊断退出。
    CallIndirect {
        callee: Operand,
        args: Vec<Operand>,
        ret: RetDest,
        target: Bb,
        unwind: UnwindAction,
        null_ok: bool,
        native_sig: Option<ForeignSig>,
    },
    Return,
    Unreachable,
    /// cleanup 链尾（MIR UnwindResume）：只在 guard.drop 的 cleanup 执行中出现——
    /// 返回即让宿主 unwind 自动继续（spike3：单条 native 栈，VM 侧零协调）
    Resume,
    /// MIR UnwindTerminate：到达即 abort
    TerminateAbort,
    /// ★ Trap-stub：未支持构造的占位（M4 增量协议的核心机制）。
    /// lowering 对收集全集是全量的——不认识的构造绝不中止，就地降为 Trap；
    /// 只有被执行的路径必须 trap-free。诊断串指出"哪一期欠的账"。
    Trap(Box<str>),
}

#[derive(Clone, Debug)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    pub term: Terminator,
}

#[derive(Clone, Debug)]
pub struct FuncBody {
    pub frame_size: u32,
    pub frame_align: u32,
    /// 返回通道（_0）
    pub ret: RetAbi,
    /// 参数落位（_1..=_argc；实参槽序 = 展平序）
    pub params: Vec<ParamAbi>,
    /// #[track_caller]：&Location 隐藏尾实参的帧内槽（ABI 幻影参，cg_ssa 同构）
    pub caller_loc_off: Option<u32>,
    pub blocks: Vec<Block>,
    /// 诊断用（符号名）
    pub name: Box<str>,
}

/// main 启动计划（cg_ssa create_entry_fn 同构）：
/// `lang_start(main fn-ptr, argc, argv, sigpipe) -> isize`（返回值 = 进程退出码）。
#[derive(Debug, Clone, Copy)]
pub struct EntryPlan {
    pub lang_start: FuncId,
    /// 用户 main 的 D4 条目真地址（lang_start 第一实参，经 CallIndirect 派发）
    pub main_addr: u64,
    pub argc: u64,
    /// argv C 串指针表的真地址（冻结区）
    pub argv_ptr: u64,
    pub sigpipe: u8,
}

#[derive(Debug, Default)]
pub struct Module {
    pub funcs: Vec<FuncBody>,
    /// 导出名（no_mangle 符号）→ FuncId，--vm-call 查找用
    pub exports: std::collections::HashMap<Box<str>, FuncId>,
    /// 冻结区（statics/常量池/fn 条目；lower 物化，发布后只读——static mut 例外）
    pub frozen: Option<super::frozen::FrozenArena>,
    /// fn-ptr 条目真地址 → FuncId（D4 反查；间接调用派发 M4.1 第 5 步）
    pub fn_addrs: std::collections::HashMap<u64, FuncId>,
    /// `-l` 链接指令的共享库候选路径（foreign 直通的 dlopen 清单，加载相收集）
    pub native_libs: Vec<Box<str>>,
    /// guest TLS 槽表（M4.4 D3：TlsId → 模板/尺寸；每线程实例在 Ctx.tls）
    pub tls: Vec<TlsSlot>,
    /// main 启动链（M4.3；--vm-call 模式下为 None）
    pub entry: Option<EntryPlan>,
}
