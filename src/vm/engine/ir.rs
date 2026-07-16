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
/// inline asm 站点 id（M5.0 asm-stub 工厂）：索引 `Module.asm_stub_addrs`。
pub type AsmStubId = u32;

/// asm-stub 物化配方的单站点（M5.0 起；A2 起符号名与位序解耦，见 Module.asm_sites）。
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AsmSite {
    /// wrapper 的 dlsym 符号名（lower 生成 GAS 文本时烤入 .globl/.type/.size）
    pub name: Box<str>,
    /// wrapper GAS 全文
    pub text: String,
}

/// 标量宽度。W128 = 两槽通道（M4.1 第 3 步）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub struct Slot {
    pub off: u32,
    pub width: Width,
}

// ===== place 求值（M4.1 核心）=====

/// 地址表达式的基。
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub enum PlaceBase {
    /// 帧内局部：真地址 = 帧基址 + off
    Local(u32),
    /// 冻结区真地址（statics/常量池，M4.1 第 4 步物化）
    Static(u64),
}

/// 地址表达式的一步（lower 已把 Field/Downcast 折叠成 Offset）。
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum PlaceStep {
    /// 当前地址处读出指针（W64），地址切换为它
    Deref,
    /// 常量字节偏移（可负——slice 尾投影 `len-k` 折出负项）
    Offset(i32),
    /// 含 dyn 尾字段的 DST：`unaligned` 必须按 vtable 的运行期 alignment 向上取整。
    /// `packed` 对应外层 `repr(packed(N))` 对字段 alignment 的上限。
    VTableAlignOffset {
        meta: Operand,
        unaligned: u64,
        packed: Option<u64>,
    },
    /// 动态下标：地址 += 帧内 idx 槽值 × stride
    IndexScaled { idx: Slot, stride: u64 },
}

/// 地址表达式：引擎按序求值 → 真地址 u64。
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PlaceExpr {
    pub base: PlaceBase,
    pub steps: Box<[PlaceStep]>,
}

/// 标量位置：读/写一个 ≤64 位标量的落点。
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum ScalarPlace {
    /// 快路径：帧内静态槽
    Slot(Slot),
    /// 慢路径：地址表达式处的标量
    Mem { expr: PlaceExpr, width: Width },
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum Operand {
    /// 帧内静态槽（快路径）
    Slot(Slot),
    /// 地址表达式处的标量
    Mem {
        expr: PlaceExpr,
        width: Width,
    },
    Imm {
        bits: u64,
        width: Width,
    },
    /// place 的真地址本身（indirect 实参 = 传聚合的地址）
    AddrOf(PlaceExpr),
    /// 值减常量（Subslice 的 slice meta：len' = len − k；M4.4）
    SubImm {
        base: Box<Operand>,
        sub: u64,
    },
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

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
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

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub enum IntCc {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub enum OvfOp {
    Add,
    Sub,
    Mul,
}

/// 标量浮点宽度（M5.2 D8c：f16 进标量通道；f128 走 128 位宽通道，不在此）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum FloatW {
    F16,
    F32,
    F64,
}

/// f128 宽通道的标量侧类别（F128From/ToScalar）。Int 的宽度在语句 w 字段。
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub enum F128Scalar {
    F(FloatW),
    Int { signed: bool },
}

/// f128 单目（Neg + 一元数学族）。
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub enum F128UnOp {
    Neg,
    Math(MathUnOp),
}

/// F128MathBin 右操作数（powi 是 i32 标量）。
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum F128Rhs {
    Wide(PlaceExpr),
    Scalar(Operand),
}

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub enum FloatOp {
    Add,
    Sub,
    Mul,
    Div,
    /// IEEE fmod（Rust `%` 浮点语义）
    Rem,
}

/// 数学一元（must_be_overridden float intrinsic 的合成处置：宿主 f32/f64 直算，P7）
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
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
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub enum MathBinOp {
    Pow,
    Powi,
    Copysign,
    Minnum,
    Maxnum,
}

/// 位操作单目（ctpop/ctlz/cttz/bswap/bitreverse intrinsic 内建）
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub enum BitUnOp {
    Popcount,
    Ctlz,
    Cttz,
    Bswap,
    Bitreverse,
}

/// C++20 内存序（M5.2 D8j：lower 从 atomic intrinsic 的 const 泛型 `ORD` 冻结）。
/// 旧实现整体折叠 SeqCst——合规（强化序 = 允许集合子集）但违 concurrency-arch
/// "弱内存序自然恢复"承诺，且 x86 上 Relaxed store 白吃 xchg 代价。现按 guest
/// 请求的序映射宿主原子指令，弱序可见性行为与 native 同源恢复。
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum MemOrd {
    Relaxed,
    Acquire,
    Release,
    AcqRel,
    SeqCst,
}

/// 原子 RMW（fetch_* 家族；序由 MemOrd 冻结，D8j）。
/// fetch_max/min 的有符号性由 intrinsic 名冻结（atomic_max/min=有符号，atomic_umax/umin=无符号），
/// 执行器据此选 AtomicI*/AtomicU*。
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub enum RmwOp {
    Xchg,
    Add,
    Sub,
    And,
    Or,
    Xor,
    Nand,
    Max,
    Min,
    UMax,
    UMin,
}

/// SIMD lane 元素类别（M5.2 D8b）：所有 lane 运算按类别分派语义。
/// 历史教训：M4.1 最小集对全部 lane 按整数位运算——float lane 的 add/cmp 是
/// **静默错值**（+0.0/−0.0 相等性、NaN 自反性都不是位比较），当时仅因 corpus
/// 全为整数 lane 未爆雷。本类型使"忘带类别"在类型层不可表示。
/// 指针 lane 按 `Int{signed:false}` 处置（真实地址模型位透传）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum LaneKind {
    Int {
        signed: bool,
    },
    /// f32/f64（按 lane_bytes 分派；f16/f128 lane 在 lower 期拒绝，D8c）
    Float,
}

/// SIMD 逐 lane 双目（M5.2 D8b 全家族；有符号性/浮点性收敛进 LaneKind）。
/// 比较产出 mask lane（真=全 1）。
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub enum SimdBinOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
    Xor,
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    /// 饱和加/减（整数 lane 专属）
    SatAdd,
    SatSub,
    /// minimum/maximum_number_nsz（浮点 lane 专属）：minnum/maxnum 语义 +
    /// "±0.0 任取"自由——宿主 `f::min/max`（=minnum/maxnum）恒在允许集合内。
    MinNum,
    MaxNum,
    /// 左移对 signed/unsigned lane 的位级结果相同。
    Shl,
    /// 右移按 lane 类别选择算术/逻辑语义。
    Shr,
}

/// SIMD 逐 lane 单目（M5.2 D8b）。浮点族要求 Float lane；位族要求 Int lane
/// （lower 期校验）。超越函数逐 lane 调宿主 libm——native 无 fast-math 时
/// scalarize 到同一 libm，同源即位同。
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub enum SimdUnOp {
    Neg,
    Fabs,
    Fsqrt,
    Ceil,
    Floor,
    Round,
    RoundTiesEven,
    Trunc,
    Fsin,
    Fcos,
    Fexp,
    Fexp2,
    Flog,
    Flog2,
    Flog10,
    Ctlz,
    Cttz,
    Ctpop,
    Bswap,
    Bitreverse,
}

/// SIMD 横向归约（M5.2 D8b）：ordered/unordered 均按 lane 序折叠——unordered
/// 的"任意结合序"集合包含顺序折叠，故顺序实现恒合规。float min/max 用宿主
/// `f{32,64}::min/max`（minnum/maxnum 语义，与 LLVM reduce.fmin/fmax 一致）。
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub enum SimdReduceOp {
    Add,
    Mul,
    Min,
    Max,
    And,
    Or,
    Xor,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum Rvalue {
    Use(Operand),
    /// guest TLS 实例真地址（M4.4 D3）：Ctx.tls[id] 惰性物化（heap 分配 + 模板拷贝）。
    TlsRef(TlsId),
    // （Subslice 的 meta 走 Operand::SubImm，无独立 rvalue）
    IntBin {
        op: IntBinOp,
        signed: bool,
        a: Operand,
        b: Operand,
    },
    /// → bool（W8）
    IntCmp {
        cc: IntCc,
        signed: bool,
        a: Operand,
        b: Operand,
    },
    /// 按位取反（掩到宽度）
    NotBits(Operand),
    /// 逻辑取反（bool：xor 1）——rustc 对 bool 的 Not 语义
    NotBool(Operand),
    /// 二补数取负
    Neg(Operand),
    /// IntToInt：截断后按 from 的符号扩展到 to
    Cast {
        from: (Width, bool),
        to: Width,
        a: Operand,
    },
    /// 取 place 真地址（Ref/RawPtr 同一实现——真实地址模型）
    Ref(PlaceExpr),
    /// 指针算术：ptr + count × stride（BinOp::Offset 与 offset/arith_offset intrinsic）
    PtrOffset {
        ptr: Operand,
        count: Operand,
        stride: u64,
    },
    /// 三路比较（BinOp::Cmp）→ Ordering（i8：-1/0/1）
    IntCmp3 {
        signed: bool,
        a: Operand,
        b: Operand,
    },
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
    /// 浮点四则（位进位出：操作数是 f16/f32/f64 的位型；f128 走 F128Bin）
    FloatBin {
        op: FloatOp,
        fw: FloatW,
        a: Operand,
        b: Operand,
    },
    /// 数学一元/二元（宿主直算；M4.5 补 must_be_overridden float intrinsic 面）
    MathUn {
        op: MathUnOp,
        fw: FloatW,
        a: Operand,
    },
    /// 融合乘加（fma/fmuladd intrinsic，M5.2 D8i）：a*b+c 单次舍入（宿主 mul_add）。
    /// fmuladd 允许融合或不融合两种结果，融合实现在允许集合内。
    MathFma {
        fw: FloatW,
        a: Operand,
        b: Operand,
        c: Operand,
    },
    MathBin {
        op: MathBinOp,
        fw: FloatW,
        a: Operand,
        b: Operand,
    },
    /// 无符号取大（unsized 尾对齐 = max(sized_align, 运行期 vtable align)，M4.5）
    UMax {
        a: Operand,
        b: Operand,
    },
    /// 浮点比较（IEEE 语义，NaN 全 false 除 Ne）→ bool
    FloatCmp {
        cc: IntCc,
        fw: FloatW,
        a: Operand,
        b: Operand,
    },
    FloatNeg {
        fw: FloatW,
        a: Operand,
    },
    /// 标量浮点互转（f16/f32/f64；f128 参与的走 F128FromScalar/F128ToScalar）
    FloatCast {
        from: FloatW,
        to: FloatW,
        a: Operand,
    },
    /// float → int（Rust `as` 饱和语义：NaN→0、越界→边界）
    FloatToInt {
        from: FloatW,
        to: Width,
        signed: bool,
        a: Operand,
    },
    /// int → float
    IntToFloat {
        from: (Width, bool),
        to: FloatW,
        a: Operand,
    },
    /// f128 比较（16 字节 place 操作数）→ bool（IEEE 语义）
    F128Cmp {
        cc: IntCc,
        a: PlaceExpr,
        b: PlaceExpr,
    },
    /// 位操作单目（按操作数宽度语义：ctlz(W8) 是 8 位前导零）
    BitUn {
        op: BitUnOp,
        a: Operand,
    },
    /// 原子读（真宿主原子指令——spike4 义务；SeqCst）
    AtomicLoad {
        addr: Operand,
        width: Width,
        order: MemOrd,
    },
    /// 指针差（ptr_offset_from[_unsigned]）：(a - b) / stride（i64 除法）
    PtrDiff {
        a: Operand,
        b: Operand,
        stride: u64,
    },
    /// SIMD movemask：收集各 lane 最高位 → 整数标量（simd_bitmask）
    SimdBitmask {
        a: PlaceExpr,
        lanes: u16,
        lane_bytes: u8,
    },
    /// 字节比较（compare_bytes intrinsic = memcmp）→ i32（-1/0/1 语义按首异字节）
    MemCmp {
        a: Operand,
        b: Operand,
        n: Operand,
    },
    /// 128 位整数比较（TypeId 判等等；操作数是 16 字节 place）→ bool
    Cmp128 {
        cc: IntCc,
        signed: bool,
        a: PlaceExpr,
        b: PlaceExpr,
    },
    /// 饱和算术（saturating_add/sub intrinsic）
    IntSat {
        op: OvfOp,
        signed: bool,
        a: Operand,
        b: Operand,
    },
    /// SIMD 归约（simd_reduce_all/any：mask 向量全真/任真）→ bool
    SimdReduce {
        all: bool,
        a: PlaceExpr,
        lanes: u16,
        lane_bytes: u8,
    },
    /// SIMD 算术/位横向归约（M5.2 D8b：simd_reduce_{add,mul}_{ordered,unordered}
    /// 与 and/or/xor/min/max）→ lane 宽标量（float 归位型）。按 lane 序折叠。
    SimdReduceArith {
        op: SimdReduceOp,
        lane: LaneKind,
        a: PlaceExpr,
        lanes: u16,
        lane_bytes: u8,
    },
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
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
    AtomicStore {
        addr: Operand,
        val: Operand,
        order: MemOrd,
    },
    /// 等宽 volatile 整体读。执行器用 alignment=1 的 opaque `MaybeUninit`
    /// 字节载体搬运，不解释聚合值的 padding。后端能直接表示的宽度保持为
    /// 单个 volatile 事件；更宽的 memory-repr 值按目标可承载的块分解。
    VolatileLoad {
        addr: Operand,
        dst: PlaceExpr,
        size: u32,
    },
    /// 等宽 volatile 整体写；`src` 是位型来源 place，padding 只按原始字节
    /// 搬运。memory-repr 值对应 rustc 的 volatile memcpy 路径。aligned/unaligned
    /// intrinsic 在 guest 端的前置条件不同，但执行器共用对齐 1 的宿主载体，
    /// 避免增加额外对齐要求。
    VolatileStore {
        addr: Operand,
        src: PlaceExpr,
        size: u32,
    },
    /// 原子比较交换：dst_val = 旧值，dst_ok = 是否成功（succ/fail 双序，D8j）
    AtomicCxchg {
        addr: Operand,
        expected: Operand,
        new: Operand,
        dst_val: ScalarPlace,
        dst_ok: ScalarPlace,
        weak: bool,
        succ: MemOrd,
        fail: MemOrd,
    },
    /// 原子 RMW：dst = 旧值
    AtomicRmw {
        op: RmwOp,
        addr: Operand,
        val: Operand,
        dst: ScalarPlace,
        order: MemOrd,
    },
    /// 动态长度内存拷贝（copy/copy_nonoverlapping intrinsic：count × elem_size 字节）
    MemCopy {
        dst: Operand,
        src: Operand,
        count: Operand,
        elem_size: u64,
        overlap: bool,
    },
    /// 动态长度填充（write_bytes：val 是 u8，count × elem_size 字节）
    MemSet {
        dst: Operand,
        val: Operand,
        count: Operand,
        elem_size: u64,
    },
    /// SIMD 逐 lane 双目（dst/a/b 是向量 place；几何冻结自 layout）
    SimdBin {
        op: SimdBinOp,
        lane: LaneKind,
        dst: PlaceExpr,
        a: PlaceExpr,
        b: PlaceExpr,
        lanes: u16,
        lane_bytes: u8,
    },
    /// SIMD 逐 lane 单目（M5.2 D8b）
    SimdUn {
        op: SimdUnOp,
        lane: LaneKind,
        dst: PlaceExpr,
        a: PlaceExpr,
        lanes: u16,
        lane_bytes: u8,
    },
    /// SIMD 融合乘加（simd_fma/simd_relaxed_fma；Float lane 专属，宿主 mul_add
    /// 单次舍入——relaxed 允许融合/不融合，融合恒在允许集合内）
    SimdFma {
        dst: PlaceExpr,
        a: PlaceExpr,
        b: PlaceExpr,
        c: PlaceExpr,
        lanes: u16,
        lane_bytes: u8,
    },
    /// SIMD 漏斗移位（simd_funnel_shl/shr；Int lane，shift 是逐 lane 向量；
    /// shift ≥ lane 位宽 = guest UB → 响亮终止）
    SimdFunnel {
        left: bool,
        dst: PlaceExpr,
        a: PlaceExpr,
        b: PlaceExpr,
        shift: PlaceExpr,
        lanes: u16,
        lane_bytes: u8,
    },
    /// SIMD 逐 lane 转换（simd_cast/simd_as/指针族；lanes 两侧相同、宽度可异）。
    /// saturate：simd_as 的 float→int 语义（Rust `as`：饱和 + NaN→0）；
    /// simd_cast 的界外是 guest UB，实现同走饱和（UB 下任何值都在允许集合内）。
    SimdCast {
        dst: PlaceExpr,
        src: PlaceExpr,
        lanes: u16,
        src_lane: LaneKind,
        src_bytes: u8,
        dst_lane: LaneKind,
        dst_bytes: u8,
    },
    /// SIMD 逐 lane 选择（simd_select：mask lane 全 1 取 a、全 0 取 b——由
    /// 类型不变量保证，按符号位判；mask 向量 lane 宽可异于数据 lane）
    SimdSelect {
        mask: PlaceExpr,
        mask_bytes: u8,
        a: PlaceExpr,
        b: PlaceExpr,
        dst: PlaceExpr,
        lanes: u16,
        lane_bytes: u8,
    },
    /// SIMD 位掩码选择（simd_select_bitmask：标量掩码第 i 位选 lane i）
    SimdSelectBitmask {
        mask: Operand,
        a: PlaceExpr,
        b: PlaceExpr,
        dst: PlaceExpr,
        lanes: u16,
        lane_bytes: u8,
    },
    /// SIMD 散布地址读（simd_gather(val, ptr, mask)：mask lane 真→读 *ptr[i]，
    /// 假→取 passthru lane；逐 lane 条件访存，假 lane **绝不佯读**——防越界）
    SimdGather {
        passthru: PlaceExpr,
        ptrs: PlaceExpr,
        mask: PlaceExpr,
        mask_bytes: u8,
        dst: PlaceExpr,
        lanes: u16,
        lane_bytes: u8,
    },
    /// SIMD 散布地址写（simd_scatter(val, ptr, mask)；假 lane 绝不佯写）
    SimdScatter {
        values: PlaceExpr,
        ptrs: PlaceExpr,
        mask: PlaceExpr,
        mask_bytes: u8,
        lanes: u16,
        lane_bytes: u8,
    },
    /// SIMD 连续掩码读（simd_masked_load(mask, base, val)：base 是标量元素指针，
    /// lane i 地址 = base + i×lane_bytes；假 lane 取 passthru，绝不佯读）
    SimdMaskedLoad {
        mask: PlaceExpr,
        mask_bytes: u8,
        base: Operand,
        passthru: PlaceExpr,
        dst: PlaceExpr,
        lanes: u16,
        lane_bytes: u8,
    },
    /// SIMD 连续掩码写（simd_masked_store(mask, base, val)；假 lane 绝不佯写）
    SimdMaskedStore {
        mask: PlaceExpr,
        mask_bytes: u8,
        base: Operand,
        values: PlaceExpr,
        lanes: u16,
        lane_bytes: u8,
    },
    /// SIMD 运行期索引抽取（simd_extract_dyn；越界 = guest UB → 响亮终止）
    SimdExtractDyn {
        src: PlaceExpr,
        idx: Operand,
        dst: ScalarPlace,
        lanes: u16,
        lane_bytes: u8,
    },
    /// SIMD 运行期索引插入（simd_insert_dyn：dst = src 整体拷贝后改 idx lane）
    SimdInsertDyn {
        src: PlaceExpr,
        idx: Operand,
        val: Operand,
        dst: PlaceExpr,
        lanes: u16,
        lane_bytes: u8,
    },
    /// SIMD 指针逐 lane 位移（simd_arith_offset：ptr[i] + offset[i]×stride，
    /// wrapping——真实地址模型下即语义）
    SimdArithOffset {
        ptrs: PlaceExpr,
        offsets: PlaceExpr,
        stride: u64,
        dst: PlaceExpr,
        lanes: u16,
    },
    /// SIMD 广播（simd_splat / _mm_set1）：val 复制到每个 lane
    SimdSplat {
        dst: PlaceExpr,
        val: Operand,
        lanes: u16,
        lane_bytes: u8,
    },
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
    /// 128 位饱和算术（saturating_add/sub intrinsic 的宽形态；宿主 u128/i128 直算）
    Sat128 {
        op: OvfOp,
        signed: bool,
        a: PlaceExpr,
        b: PlaceExpr,
        dst: PlaceExpr,
    },
    /// 128 位整数 → 标量浮点（u128/i128 as f16/f32/f64；宿主直转）
    Wide128ToFloat {
        src: PlaceExpr,
        signed: bool,
        to: FloatW,
        dst: ScalarPlace,
    },
    /// 标量浮点 → 128 位整数（f16/f32/f64 as i128/u128；`as` 饱和语义，D8k）
    FloatToWide128 {
        src: Operand,
        from: FloatW,
        signed: bool,
        dst: PlaceExpr,
    },
    /// 128 位位单目，结果仍 128 位（bswap/bitreverse，D8k）
    Bit128 {
        op: BitUnOp,
        src: PlaceExpr,
        dst: PlaceExpr,
    },
    /// 128 位计数类位单目（ctpop/ctlz/cttz，结果 u32 标量，D8k）
    Bit128Count {
        op: BitUnOp,
        src: PlaceExpr,
        dst: ScalarPlace,
    },
    // ===== f128 宽通道（M5.2 D8c：16 字节值走 place，宿主 f128 直算——
    // rustc 把引擎自身的 f128 运算下降到与 native guest 同一批
    // compiler-builtins/__*tf* + glibc *f128 libm 符号，同源即位同）=====
    /// f128 四则（含 Rem=fmodf128）
    F128Bin {
        op: FloatOp,
        a: PlaceExpr,
        b: PlaceExpr,
        dst: PlaceExpr,
    },
    /// f128 数学二元（powi 的 rhs 是 i32 标量，其余 wide）
    F128MathBin {
        op: MathBinOp,
        a: PlaceExpr,
        b: F128Rhs,
        dst: PlaceExpr,
    },
    /// f128 单目（取负 + 全部一元数学）
    F128Un {
        op: F128UnOp,
        a: PlaceExpr,
        dst: PlaceExpr,
    },
    /// f128 融合乘加（宿主 mul_add 单次舍入）
    F128Fma {
        a: PlaceExpr,
        b: PlaceExpr,
        c: PlaceExpr,
        dst: PlaceExpr,
    },
    /// 标量（f16/f32/f64/整数 ≤64）→ f128
    F128FromScalar {
        src: Operand,
        kind: F128Scalar,
        dst: PlaceExpr,
    },
    /// f128 → 标量（float 互转 / `as` 饱和到整数）
    F128ToScalar {
        src: PlaceExpr,
        kind: F128Scalar,
        w: Width,
        dst: ScalarPlace,
    },
    /// i128/u128 ↔ f128（宿主 as）
    F128FromWideInt {
        src: PlaceExpr,
        signed: bool,
        dst: PlaceExpr,
    },
    F128ToWideInt {
        src: PlaceExpr,
        signed: bool,
        dst: PlaceExpr,
    },
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
    Fence {
        single_thread: bool,
        order: MemOrd,
    },
    /// `[expr; N]` 聚合元素通道（M4.4）：dst[0] 已写好，从它铺满 i∈[1,count)
    RepeatBytes {
        first: PlaceExpr,
        count: u64,
        elem_size: u64,
    },
}

/// unwind 处置（M4.2 起全语义：FrameGuard 动态 LSDA，spike3 协议）。
/// MIR 的 Unreachable 折进 Continue（unwind 到此=UB，fast 不检测）。
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub enum UnwindAction {
    Continue,
    Cleanup(Bb),
    /// unwind 到此即中止（double panic / extern "C" ABI 边界）
    Terminate,
}

/// `&'static str` 的 Copy 载体（M6 片2）：裸 `&str` 字段会让 serde 给容器推导
/// `'de: 'static` 借用约束；newtype + 手动 serde 隔断推导。反序列化 leak 一份——
/// Unsupported 变体每模块有界、模块本身经 Box::leak 进程级共享（Shared::new 同风格）。
#[derive(Clone, Copy, Debug)]
pub struct StaticStr(pub &'static str);

impl serde::Serialize for StaticStr {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.0)
    }
}

impl<'de> serde::Deserialize<'de> for StaticStr {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s: String = serde::Deserialize::deserialize(d)?;
        Ok(StaticStr(Box::leak(s.into_boxed_str())))
    }
}

/// 引擎原语（foreign 三路处置①，debt-map §2-B）：std 自己声明的 runtime extern 边界，
/// native 下由 codegen/链接器合成 shim——引擎在同一边界接管。
/// alloc 系的引擎实现是 M4.1 第 5 步（堆内建）；落地前 lower 前置 `Stmt::Trap` 防静默。
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
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
    /// `fork()`（M5.2 D8f）：guest 单线程时放行（子进程=全进程拷贝，解释器状态天然
    /// 一致）；多 guest 线程时响亮拒绝（native 下也是雷区）。解锁 Command::pre_exec
    /// 与单线程 daemonize。exec 族从 denylist 移出走 foreign 直通（进程替换本就正确）。
    HostFork,
    /// `atexit(fn)`/`__cxa_atexit(fn,arg,dso)`/`on_exit(fn,arg)`：注册 guest 退出
    /// 回调（D8g）。glibc 不导出 `atexit` 供 guest dlsym，故走 builtin：引擎自持
    /// LIFO 注册表，首注册时经引擎自身链接的 libc `atexit` 挂一个 native trampoline，
    /// 进程收尾按 LIFO 解释执行 guest 回调。返回 0（成功）。
    HostAtexit,
    HostCxaAtexit,
    HostOnExit,
    /// `syscall(nr, ...) -> long` 可变参直通（按实参个数分派）
    HostSyscall,
    /// `signal(signum, SIG_DFL|SIG_IGN)`：不含 guest 回调，可安全直通；其他 handler
    /// 执行期明确失败，直到有异步信号安全的专用 thunk。
    HostSignal,
    /// `sigaction(signum, act, oldact)` 的受限直通：查询（act=NULL）或
    /// act.handler=SIG_DFL/SIG_IGN；结构体中的 guest handler 仍明确失败。
    HostSigaction,
    /// 已知不能安全直通的宿主边界。执行到必须明确失败，绝不伪造成功。
    /// 包括需要异步安全专用实现的边界，以及需要 guest frame/context
    /// 翻译、不能把宿主解释器状态直接暴露给 guest 的 unwinder API。
    Unsupported(StaticStr),
    /// `_Unwind_DeleteException`：按 Itanium ABI 调用异常对象内的 cleanup 回调。
    UnwindDeleteException,
    /// backtrace 影子帧（M5.2 D8e）：Ctx 影子帧栈诚实回答，IP=合成 fn token。
    /// `_Unwind_Backtrace(trace_fn, arg)` 逐帧回调 guest trace_fn。
    UnwindBacktrace,
    /// `_Unwind_GetIP(ctx)` / `_Unwind_GetIPInfo(ctx, &ip_before)`：读 synth ctx 的 IP。
    UnwindGetIp,
    UnwindGetIpInfo,
    /// `_Unwind_FindEnclosingFunction(ip)`：合成 IP 即函数入口，返回 ip 自身。
    UnwindFindEnclosing,
    /// 不改变 guest 抽象机/RAM 状态的处理器 hint（如 `pause`、`vzeroupper`）。
    /// 解释器不持久化宿主向量寄存器状态，因此执行期可正确忽略。
    CpuHintNop,
    /// `core::intrinsics::breakpoint()`：执行真 int3——与 native 同为 SIGTRAP
    /// 可观测行为（未被跟踪时进程默认终止）。
    Breakpoint,
    /// `llvm.x86.addcarry.64(carry, a, b) -> (carry, result)`：
    /// LLVM unadjusted intrinsic 的 pair 字段顺序保持原样。
    AddCarry64,
    /// `llvm.x86.subborrow.64(borrow, a, b) -> (borrow, result)`。
    SubBorrow64,
    /// `llvm.x86.xgetbv(xcr) -> u64`：读取真实宿主扩展控制寄存器。
    Xgetbv,
    /// 无可移植 `simd_*` 等价的 x86 向量硬件 intrinsic。参数和返回向量仍通过
    /// frozen bytecode 的 indirect place ABI 传递；执行器助手调用真实宿主指令。
    X86Pshufb128,
    X86Pshufb256,
    X86Sha256Msg1,
    X86Sha256Msg2,
    X86Sha256Rnds2,
    /// `llvm.x86.sse2.psad.bw(a, b)`（`_mm_sad_epu8`）：两组 8 字节绝对差和，
    /// 分别以 u64 落 qword lane 0/1（其余位清零）。
    X86PsadBw128,
    /// `llvm.x86.avx2.psad.bw(a, b)`（`_mm256_sad_epu8`）：每 128 位 lane 同上，
    /// 共 4 个 u64 结果。
    X86PsadBw256,
    /// `llvm.x86.pclmulqdq(a, b, imm8)`（`_mm_clmulepi64_si128`）：imm8 bit0/bit4
    /// 各选 a/b 的 qword 做 64×64→128 无进位乘法；imm8 其余位硬件忽略。
    X86Pclmulqdq,
    /// `llvm.x86.aesni.aesenc(a, round_key)` 等 AES-NI 单轮系（128 位）。
    X86AesEnc,
    X86AesEncLast,
    X86AesDec,
    X86AesDecLast,
    /// `llvm.x86.aesni.aesimc(a)`：InvMixColumns（解密轮密钥变换）。
    X86AesImc,
    /// `llvm.x86.aesni.aeskeygenassist(a, imm8)`：SubWord/RotWord ⊕ RCON(=imm8)。
    X86AesKeygenAssist,
    /// `llvm.x86.sse42.crc32.32.8/16/32` 与 `.64.64`（`_mm_crc32_u8/16/32/64`）：
    /// CRC32C 硬件语义（反射多项式 0x82F63B78 / 64 位 0xC96C5795D7870F42，
    /// 无首尾取反——首尾取反由包装层负责）。标量通道。
    X86Crc32U8,
    X86Crc32U16,
    X86Crc32U32,
    X86Crc32U64,
    /// `llvm.x86.avx2.permd(a, idx)`（`_mm256_permutevar8x32_epi32`）：
    /// 跨 lane dword 置换，dst.dword[i] = a.dword[idx.dword[i] & 7]。
    X86Permd256,
    /// `llvm.x86.avx2.gather.q.pd.256(src, base, vindex, mask, scale)`：
    /// 分 lane 条件收集——mask lane 符号位置位才读 base+vindex*scale（f64），
    /// 否则拷 src lane；mask 关闭的 lane 绝不触内存（fault suppression）。
    X86GatherQPd256,
    /// `llvm.x86.avx2.gather.d.pd.256`：同上，但 vindex 是 4×i32（符号扩展到
    /// 64 位参与地址算术）。
    X86GatherDPd256,
    /// `llvm.x86.avx512.vpmadd52l/h.uq.128/256/512(a, b, c)`：52 位无符号乘加，
    /// dst.qword[i] = a[i] + (b[i][51:0]×c[i][51:0]) 的 bit[51:0]（l）或
    /// bit[103:52]（h），加法按 64 位回绕。
    X86Pmadd52Lo128,
    X86Pmadd52Hi128,
    X86Pmadd52Lo256,
    X86Pmadd52Hi256,
    X86Pmadd52Lo512,
    X86Pmadd52Hi512,
    /// `llvm.x86.ssse3.pmadd.ub.sw.128` / `llvm.x86.avx2.pmadd.ub.sw`
    /// （`_mm(256)_maddubs_epi16`）：a 无符号字节 × b 有符号字节，相邻两积之和
    /// 饱和到 i16（simd-adler32 主力）。
    X86PmaddUbSw128,
    X86PmaddUbSw256,
    /// `llvm.x86.sse2.pmadd.wd` / `llvm.x86.avx2.pmadd.wd`（`_mm(256)_madd_epi16`）：
    /// 相邻 i16 对积之和放 i32（MIN×MIN+MIN×MIN 回绕为 i32::MIN，硬件定义）。
    X86PmaddWd128,
    X86PmaddWd256,
    /// `llvm.x86.vcvtps2ph.128(a, rounding)`（`_mm_cvtps_ph`）：f32x4 → f16x4 打包
    /// 进低 64 位、高 64 位清零。`rounding`：imm[2]=0 → imm[1:0] 舍入模式
    /// （0=RNE/1=floor/2=ceil/3=trunc）；imm[2]=1 → MXCSR.RC（引擎恒宿默认 RNE）。
    /// 软件模型与硬件指令逐位一致（NaN：qbit 强置 + 载荷右移 13 位截断；
    /// 溢出/次正规/四种舍入模式见 x86.rs 对拍单测）。
    X86Cvtps2ph128,
    /// `llvm.x86.vcvtph2ps.128(a)`（`_mm_cvtph_ps`）：f16x8 低 64 位 → f32x4，
    /// 精确展开（NaN：qbit 强置 + 载荷左移 13 位；次正规精确规格化）。
    /// 注：晚近 stdarch 的 `_mm_cvtph_ps` 已 portable 化（simd_shuffle/simd_cast，
    /// 走 f16 lane 通道而非本符号）；本符号为旧发射面/直调保留。
    X86Cvtph2ps128,
    /// `llvm.x86.vcvtps2ph.256(a, rounding)`（`_mm256_cvtps_ph`）：f32x8 → f16x8，
    /// 返回 128 位。舍入语义同 .128。
    X86Cvtps2ph256,
    /// `llvm.x86.vcvtph2ps.256(a)`（`_mm256_cvtph_ps`）：f16x8 → f32x8，精确展开。
    X86Cvtph2ps256,
    /// `llvm.x86.sse.max.ps(a, b)` 与 `.min`（`_mm_max_ps`/`_mm_min_ps`）：
    /// `a>b ? a : b` / `a<b ? a : b`——unordered → 第二源、±0 相等 → 第二源、
    /// NaN 位透传（Rust 标量比较天然同构，对拍钉死）。
    X86MaxPs128,
    X86MinPs128,
    /// `llvm.x86.avx.max.ps.256` / `.min`：f32x8 逐 lane 同 .128 语义。
    X86MaxPs256,
    X86MinPs256,
    /// `llvm.x86.sse.cmp.ps(a, b, imm8)` / `llvm.x86.avx.cmp.ps.256`：
    /// 全 32 谓词表（EQ/LT/LE/UNORD/NEQ/NLT/NLE/ORD ×Q/S + EQ_UQ/NGE/NGT/FALSE/
    /// NEQ_OQ/GE/GT/TRUE ×Q/S——S/Q 只差异常旗标，值位相同），真 lane 成全 1。
    X86CmpPs128,
    X86CmpPs256,
    /// `llvm.x86.sse41.round.ps(a, imm8)` / `llvm.x86.avx.round.ps.256`：
    /// imm[3:0] 舍入（0=RNE/1=floor/2=ceil/3=trunc + bit2→MXCSR(=RNE) + bit3 仅
    /// 异常旗标抑制）。NaN：载荷保留 + qbit 强置（x86.rs 显式臂——libm/roundss
    /// 的 NaN 位行为随宿主构建目标漂移，不可依赖）。
    X86RoundPs128,
    X86RoundPs256,
    /// `llvm.x86.sse2.cvtps2dq(a)`（`_mm_cvtps_epi32`）：f32→i32 按 MXCSR.RC=RNE
    /// 取整；NaN/越界/±inf → 0x80000000（indefinite）。
    X86CvtPs2dq128,
    /// `llvm.x86.sse2.cvttps2dq(a)`（`_mm_cvttps_epi32`）：同上但截断取整。
    X86CvttPs2dq128,
    /// `llvm.x86.avx.cvt.ps2dq.256` / `.cvtt.ps2dq.256`：f32x8 版同上两符号。
    X86CvtPs2dq256,
    X86CvttPs2dq256,
    /// `llvm.x86.sse41.blendvps(a, b, mask)` / `llvm.x86.avx.blendv.ps.256`：
    /// mask lane 符号位置位取 b、清零取 a（纯位选择，无算术）。
    X86BlendvPs128,
    X86BlendvPs256,
    /// `llvm.x86.sse2.psll.d(a, count)`（`_mm_sll_epi32`）：v4i32 逻辑左移；
    /// count 为向量操作数低 64 位单一计数值，count>31 → 全零（tiny-skia
    /// lowp u32x4 通道实锤）。count 向量高位字节硬件照样读低 64 位忽略其余。
    X86PsllD128,
    /// `llvm.x86.sse2.psrl.d(a, count)`（`_mm_srl_epi32`）：v4i32 逻辑右移，同律。
    X86PsrlD128,
}

/// libffi 直通的参数/返回类别（lower 期从 fn sig layout 冻结；os:: P7 直通处置）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
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
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum Bin128Rhs {
    Wide(PlaceExpr),
    Scalar(Operand),
}

/// guest TLS 槽 id（`#[thread_local]` static 的稠密编号，M4.4 D3）。
pub type TlsId = u32;

/// guest TLS 槽描述（lower 冻结）：template = 初始字节在冻结区的真地址（含重定位），
/// 每线程首访时 heap 分配 size 字节拷模板。v1 记账：dtor 不跑（设计 D3）。
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub struct TlsSlot {
    pub template: u64,
    pub size: u64,
    pub align: u32,
}

/// 冻结的 foreign 签名。变参函数按**调用点实参**冻结尾参（fixed = 固定参数个数）。
/// Eq/Hash：thunk 工厂缓存键（M4.4 D1——(fn 条目地址, 逃逸位签名) → 真码地址）。
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
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
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
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
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub enum RetAbi {
    Zst,
    /// 标量：interp_frame 返回 lo
    Scalar(Slot),
    /// 标量对：返回 (lo, hi)
    Pair(Slot, Slot),
    /// 大聚合：caller 前插隐藏首实参 = 目的真地址；callee Return 时
    /// memcpy(隐藏指针槽, _0 槽, size)。隐藏指针槽附加在帧尾（sret_off）。
    Indirect {
        ret_off: u32,
        size: u32,
        sret_off: u32,
    },
}

/// Call 的返回落点（caller 侧）。
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum RetDest {
    /// 忽略（ZST 或无落点）
    Ignore,
    Scalar(ScalarPlace),
    /// pair 两半的落点（dst place + 冻结的两半偏移/宽度）
    Pair(ScalarPlace, ScalarPlace),
    /// 大聚合：caller 求好目的真地址，作为隐藏首实参传入（Call 时前插）
    Indirect(PlaceExpr),
}

/// `SwitchInt` 判别值。普通整数沿用标量 operand；i128/u128 保持在 place 中，执行期
/// 一次读取完整 128 位，不能先截成 u64。
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum SwitchDiscr {
    Scalar(Operand),
    Wide(PlaceExpr),
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum Terminator {
    Goto(Bb),
    SwitchInt {
        discr: SwitchDiscr,
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
    /// inline asm 站点（M5.0 asm-stub 工厂，corpus §2.2 三面孔归宿）：
    /// stub 索引 `Module.asm_stub_addrs`（加载相 cc 汇编 + dlopen 物化的 wrapper 真址，
    /// `fn(*mut u8)` 槽缓冲 ABI）。执行 = 栈开 buf_size 缓冲、按 ins 写入槽、call 真址、
    /// 按 outs 从槽读出落点。三面孔全 `unwind unreachable`（MAY_UNWIND 已在 lower 拒）。
    InlineAsm {
        stub: AsmStubId,
        buf_size: u32,
        /// (缓冲槽偏移, 输入值)——按操作数宽写入 8 字节槽低位
        ins: Vec<(u32, Operand)>,
        /// (缓冲槽偏移, 输出落点)——按落点宽从槽低位读出
        outs: Vec<(u32, ScalarPlace)>,
        target: Bb,
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

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    pub term: Terminator,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct EntryPlan {
    pub lang_start: FuncId,
    /// 用户 main 的 D4 条目真地址（lang_start 第一实参，经 CallIndirect 派发）
    pub main_addr: u64,
    pub argc: u64,
    /// argv C 串指针表的真地址（冻结区）
    pub argv_ptr: u64,
    pub sigpipe: u8,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Module {
    pub funcs: Vec<FuncBody>,
    /// 导出名（no_mangle 符号）→ FuncId，--vm-call 查找用
    pub exports: std::collections::HashMap<Box<str>, FuncId>,
    /// 冻结区（statics/常量池/fn 条目；lower 物化，发布后只读——static mut 例外）
    pub frozen: Option<super::frozen::FrozenArena>,
    /// fn-ptr 条目真地址 → FuncId（D4 反查；间接调用派发 M4.1 第 5 步）
    pub fn_addrs: std::collections::HashMap<u64, FuncId>,
    /// `-l` 链接指令的可选共享库候选路径；不存在时继续尝试其他候选。
    pub native_libs: Vec<Box<str>>,
    /// 已由加载相物化、执行 foreign 前必须成功 dlopen 的共享库（当前为 M5.1 Static
    /// archive `.a → .so` 产物）。失败不可退化为普通 dlsym miss。
    pub required_native_libs: Vec<Box<str>>,
    /// guest TLS 槽表（M4.4 D3：TlsId → 模板/尺寸；每线程实例在 Ctx.tls）
    pub tls: Vec<TlsSlot>,
    /// asm-stub wrapper 真地址（M5.0）：AsmStubId → `fn(*mut u8)` 机器地址（加载相
    /// cc 汇编 + dlopen + dlsym 物化）。执行相只读 u64 直调，纯度不破。
    /// **不进 L2 快照语义**——warm 路径以 asm_sites 幂等重物化后覆写。
    pub asm_stub_addrs: Vec<u64>,
    /// asm-stub 物化配方（M6 片2）：符号名 + wrapper GAS 全文，AsmStubId（= 位序）序。
    /// warm 加载用它重跑 asm::materialize（内容哈希命中 .so 缓存则只 dlopen+dlsym；
    /// 被清则重 cc，自愈）。**符号名与位序解耦**：A2 split 模式的最终位序收尾才知，
    /// 用类前缀名（mirvm_asm_xi{j}/xd{k}）；非 split 路径沿用位序名 mirvm_asm_{id}。
    pub asm_sites: Vec<AsmSite>,
    /// extern static（environ 类）/ extern fn（fn-ptr 取址）的宿主地址直嵌符号
    /// （M6 片2）：这些 dlsym 真地址已烤进字节码 const/冻结区重定位，ASLR 下跨进程
    /// 无效——**非空即不可入 L2 缓存**（ircache::store 拒绝；升级路径 = GOT 式间接）。
    pub foreign_static_syms: Vec<Box<str>>,
    /// main 启动链（M4.3；--vm-call 模式下为 None）
    pub entry: Option<EntryPlan>,
    /// S4/S3′ image 栈冻结区（absorb 时挂载底座 + 各依赖 image 的冻结区，与本模块
    /// 同寿命——delta 字节码里嵌了跨域绝对地址，这些域必须活到 guest 结束）。
    /// **不进 L2 快照**——image 文件各自有其生命周期，delta 条目只以键链引用（ircache 双验证）。
    #[serde(skip)]
    pub image_frozens: Vec<super::frozen::FrozenArena>,
}

impl Module {
    /// argv C 串表终结化（tier-0 setup_process_memory 同构；M6 片2 起从 lower 迁出）。
    /// argv 是**运行期输入**：不得进 L2 缓存快照，冷/热路径每次运行都在快照之后追加
    /// 分配并回填 EntryPlan——单一代码路径，杜绝冷热漂移。
    pub fn finalize_entry_argv(&mut self, argv: &[String]) {
        let Some(entry) = self.entry.as_mut() else {
            return;
        };
        let frozen = self.frozen.as_mut().expect("entry 存在则冻结区必在");
        let mut ptrs: Vec<u64> = Vec::with_capacity(argv.len());
        for a in argv {
            let bytes = a.as_bytes();
            let p = frozen.alloc(bytes.len() as u64 + 1, 1);
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), p as *mut u8, bytes.len());
                *((p + bytes.len() as u64) as *mut u8) = 0;
            }
            ptrs.push(p);
        }
        let table = frozen.alloc((ptrs.len() as u64 + 1) * 8, 8);
        for (i, &p) in ptrs.iter().enumerate() {
            unsafe { *((table + i as u64 * 8) as *mut u64) = p };
        }
        // 尾 NULL 由清零保证
        entry.argc = argv.len() as u64;
        entry.argv_ptr = table;
    }
}

#[cfg(test)]
mod tests {
    use super::Width;

    #[test]
    fn width_roundtrips_supported_byte_sizes_and_masks_values() {
        for (bytes, width, mask) in [
            (1, Width::W8, 0xff),
            (2, Width::W16, 0xffff),
            (4, Width::W32, 0xffff_ffff),
            (8, Width::W64, u64::MAX),
        ] {
            assert_eq!(Width::from_bytes(bytes), Some(width));
            assert_eq!(width.bytes() as u64, bytes);
            assert_eq!(width.mask(), mask);
        }
        assert_eq!(Width::from_bytes(0), None);
        assert_eq!(Width::from_bytes(16), None);
    }
}
