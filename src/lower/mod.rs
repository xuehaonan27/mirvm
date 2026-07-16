//! 加载相：MIR → M4 引擎字节码的降低（rustc_private 域，tcx 关在这里）。
//!
//! 编排（M4.0 设计 §3 + D1 修正，debt-map §2-A）：mono 收集给**种子**（collector 是
//! codegen/链接视角，跨 crate 非泛型函数不收）→ **worklist 闭包扩集**（lower 遇到不在表
//! 里的 callee 就分配 FuncId 入队——解释视角没有"链接 libstd.so"可言，一切 MIR 自己降）
//! → 产出纯 Rust 的 `ir::Module`。
//!
//! **Trap-stub 全覆盖**：对全集 lowering 是全量的——不认识的构造绝不中止，
//! 就地降为 `Trap(诊断)`；只有被执行到的路径必须 trap-free（M4 增量协议）。

pub mod asm;
pub mod collect;
pub mod frame;
pub mod func;
pub mod global_asm;

use std::collections::VecDeque;

use rustc_data_structures::fx::FxHashMap;
use rustc_middle::mir::interpret::{AllocId, ConstAllocation, GlobalAlloc};
use rustc_middle::ty::{Instance, InstanceKind, TyCtxt, TypingEnv};
use rustc_span::Symbol;

use crate::vm::engine::frozen::FrozenArena;
use crate::vm::engine::ir;

/// 调用目标的解析结果（foreign 三路处置，debt-map §2-B）。
pub(crate) enum Callee {
    /// 普通 guest 函数（含链接仿真②解出的 std 实现、intrinsic fallback body 补收）
    Func(ir::FuncId),
    /// 引擎原语①（std runtime extern 边界：alloc/unwind 系 + stub）
    Builtin(ir::Builtin),
    /// os:: 直通③（dlsym+libffi）：固定参数 FfiKind 已冻结；变参尾由调用点实参补。
    /// thunk_args = fn-ptr 类型的参数位 + 其内层冻结签名（M4.4 D1 thunk 工厂）
    Foreign {
        sym: Box<str>,
        args: Vec<ir::FfiKind>,
        ret: ir::FfiKind,
        variadic: bool,
        thunk_args: Vec<(usize, ir::ForeignSig)>,
    },
}

/// 危险符号（P7 denylist）：绝不直通 native——会绕开进程/线程模型。
/// M4.4 D2：pthread_create/join/detach 已移出（真线程直通，fn-ptr 实参经 thunk 工厂）。
/// M4.5 D3：posix_spawn 系移出（子体立即 exec，VM 状态从不在子进程运行——与裸 fork
/// 带完整 VM 镜像着陆本质不同；file_actions/attr 是不透明指针，真实地址直传成立）。
/// 保留 pthread_exit（glibc 强制 unwind 绕过 FrameGuard）与裸 fork/exec/setjmp 系。
/// M5.2 D8f：fork 移出（→ HostFork builtin，guest 单线程时放行）；exec 移出
/// DENY_PREFIX（进程替换语义 = VM 状态消失本就正确，foreign 直通）。vfork/clone/
/// setjmp 系维持拒绝（帧模型级工程，D8l）。
const DENY_EXACT: &[&str] = &[
    "vfork",
    "clone",
    "clone3",
    "setjmp",
    "longjmp",
    "sigsetjmp",
    "siglongjmp",
    "pthread_exit",
    "pthread_atfork",
];
const DENY_PREFIX: &[&str] = &[];

/// A2 split 标签位（s3b-a2-design §4.2）：image 类 id = `IMAGE_TAG | 位序`，
/// delta 类 id = 今日路径的 untagged 值。rebase 前绝不进执行相（2^31 实例不可能）。
/// FuncId/TlsId/AsmStubId 同构（均 u32）。
const IMAGE_TAG: u32 = 0x8000_0000;

/// A2 split 状态（s3b-a2-design §4）：双队列/双 arena/双去重表。
/// delta 侧沿用 Linker 主字段（queue/funcs/frozen/alloc_addrs/tls_slots/asm_sites）。
struct Split<'tcx> {
    /// image 类实例的冻结区（样条 k=0 域，0x6A00）
    image_frozen: FrozenArena,
    /// image 类待降低队列（标签 id）
    image_queue: VecDeque<(ir::FuncId, Instance<'tcx>)>,
    /// image 类函数体（位序 j → 标签 id `IMAGE_TAG|j`）
    image_funcs: Vec<Option<ir::FuncBody>>,
    /// 下一个 image 类 id 序数
    image_fn_next: ir::FuncId,
    /// image 类 TLS 槽（标签 TlsId 同构）
    image_tls_slots: Vec<ir::TlsSlot>,
    /// image 类 asm 站点（符号名 mirvm_asm_xi{j}）
    image_asm_sites: Vec<ir::AsmSite>,
    /// image 区常量去重表（delta 区 = Linker.alloc_addrs；提升 = 双份物化，见 §4.3）
    image_alloc_addrs: FxHashMap<AllocId, u64>,
    /// image 区 fn 条目表（instance → 条目地址；含底座命中但在 image 区补建者——
    /// 装载端 fn_entry_syms 索引的唯一权威，保"总量恰一份"的单一身份可复现）
    image_fn_entries: FxHashMap<Instance<'tcx>, u64>,
    /// 当前降低实例是否为 image 类（ensure_alloc Memory 路由 + closure 护栏用）
    current_image: bool,
    /// image 类实例表（rebase 后写盘自检用：逐 instance 复查无 LOCAL_CRATE 沾染）
    image_insts: Vec<Instance<'tcx>>,
}

/// A2 split 产物（s3b-a2-design）：deps-image 模块 + 栈索引素材（BaseExports 同构）。
/// 模块冻结区在样条 k=0 域；fn/TLS/asm 与 exports/fn_addrs 已 rebase 成绝对 id。
pub struct SplitImage {
    pub module: ir::Module,
    pub fn_entry_syms: Vec<(Box<str>, u64)>,
    pub static_syms: Vec<(Box<str>, u64)>,
    pub tls_syms: Vec<(Box<str>, ir::TlsId)>,
}

impl SplitImage {
    /// 包装成栈层（A2-1 内存态 absorb；A2-2 写盘后由文件装载取代）。
    /// fp = 构建会话的降低指纹（同会话构建，与栈恒一致）。
    pub fn into_base_image(self, fp: (bool, bool, bool)) -> crate::baseimage::BaseImage {
        crate::baseimage::BaseImage {
            fn_by_sym: self
                .module
                .exports
                .iter()
                .map(|(s, id)| (s.clone(), *id))
                .collect(),
            entry_by_sym: self.fn_entry_syms.into_iter().collect(),
            static_by_sym: self.static_syms.into_iter().collect(),
            tls_by_sym: self.tls_syms.into_iter().collect(),
            lowering_fp: fp,
            key: "a2-inmem".into(),
            module: self.module,
        }
    }
}

/// 加载相"链接器"：FuncId 分配 + worklist 闭包扩集（D1 修正），外加 native 链接器
/// 职责的仿真——**特判的不是"panic 是什么"，是"链接器本来会做什么"**（debt-map §2-B）：
/// ① 引擎原语表（codegen allocator-shim 的同一符号清单）；
/// ② 导出符号解析（weak lang item：core 的 extern `panic_impl` → std 的 `rust_begin_unwind`）；
/// ③ 未知 foreign 暂 Trap（os:: 注册表 M4.3）。
pub(crate) struct Linker<'tcx> {
    tcx: TyCtxt<'tcx>,
    /// instance → FuncId（去重集；含已降与在队的）
    ids: FxHashMap<Instance<'tcx>, ir::FuncId>,
    /// 待降低队列（FuncId 已分配，体未产出）——split 模式下为 **delta 类**队列
    queue: VecDeque<(ir::FuncId, Instance<'tcx>)>,
    /// A2 split 状态（None = 非 split 路径，行为与 S4/S3′a 完全一致）
    split: Option<Split<'tcx>>,
    /// ①引擎原语表：mangled 符号 → Builtin
    builtins: FxHashMap<Symbol, ir::Builtin>,
    /// ②链接仿真：导出符号名 → (定义 instance, is_weak)（strong 覆盖 weak；惰性构建）
    exports: Option<FxHashMap<Symbol, (Instance<'tcx>, bool)>>,
    /// 冻结区（statics/常量池/fn 条目）——lower 期物化，结束移交 Module
    frozen: FrozenArena,
    /// 已物化的 alloc → 冻结区真地址（去重 + 先分后填破指针环）
    alloc_addrs: FxHashMap<AllocId, u64>,
    /// fn-ptr 条目：instance → 条目真地址（D4 每 instance 一个真地址身份）
    fn_entries: FxHashMap<Instance<'tcx>, u64>,
    /// 反查：条目真地址 → FuncId（间接调用派发用，移交 Module）
    fn_addrs: FxHashMap<u64, ir::FuncId>,
    /// guest TLS：`#[thread_local]` static → 稠密 TlsId + 槽表（M4.4 D3，移交 Module）
    tls_ids: FxHashMap<rustc_hir::def_id::DefId, ir::TlsId>,
    tls_slots: Vec<ir::TlsSlot>,
    /// asm-stub wrapper 文本（M5.0）：AsmStubId → 符号名+GAS 源；lower 结束批量
    /// cc+dlopen 物化。A2 起名字与位序解耦（split 模式最终位序收尾才知）。
    asm_sites: Vec<ir::AsmSite>,
    /// extern static/fn 的宿主地址直嵌符号（M6 片2）：非空 ⇒ 模块不可缓存
    foreign_static_syms: Vec<Box<str>>,
    /// extern fn 被当作值取址（fn-ptr）的条目：instance → dlsym 真地址（D4 条目
    /// 语义的外延）。不进 fn_addrs——执行相反查未命中正是 CallIndirect 的
    /// native_sig libffi 直调通道（M4.4 FFI 反方向之二）的触发条件。
    foreign_fn_entries: FxHashMap<Instance<'tcx>, u64>,
    /// 必需归档库的 hidden 符号兜底表（装载基址, 符号→st_value）：只收不进
    /// .dynsym 的符号（-fvisibility=hidden 归档，ring/zstd-sys 一族）；extern
    /// static/fn 取址的降低期解析**先于 dlsym 全域**——native 链接期绑定语义
    /// （归档内定义恒胜全局命名空间；宿主 libLLVM 内嵌 ZSTD_* 静默截胡的实锤，
    /// 见 elfsym 模块头注）。lower_inner 头部随 dlopen 一并构建。
    archive_fallbacks: Vec<(u64, std::collections::HashMap<Box<str>, u64>)>,
    // ===== S4 底座（s4-base-image-design；偏移合并）=====
    /// sym → 底座 FuncId（命中即复用，不入队）。空表 = 无底座/底座构建模式。
    base_fns: FxHashMap<Box<str>, ir::FuncId>,
    /// sym → 底座 fn 条目真地址（仅被取址过的）
    base_fn_entries: FxHashMap<Box<str>, u64>,
    /// sym → 底座 static 真地址（双份物化 = static mut 精神分裂，必须去重）
    base_statics: FxHashMap<Box<str>, u64>,
    /// sym → 底座 TlsId（线程局部身份同理必须去重）
    base_tls: FxHashMap<Box<str>, ir::TlsId>,
    /// delta 的 fn/TLS/asm id 起点 = 底座各表长度（absorb 时 base++delta 拼单表）
    delta_first_fn: ir::FuncId,
    delta_first_tls: ir::TlsId,
    delta_first_asm: ir::AsmStubId,
    /// 下一个待分配 FuncId（不能再用 ids.len()：底座命中也占 ids 条目）
    next_fn: ir::FuncId,
    /// 底座导出素材：本会话物化的非 foreign static（DefId, 冻结区地址）
    static_defs: Vec<(rustc_hir::def_id::DefId, u64)>,
}

impl<'tcx> Linker<'tcx> {
    /// S3′a：base-maps = image 栈的并集查找；delta 起编 = 栈累积量。frozen 由调用方
    /// 按目标域构造（程序 delta = new()、底座 = new_base_image()、依赖 image = new_image(k)）。
    fn new(tcx: TyCtxt<'tcx>, stack: &crate::baseimage::ImageStack, frozen: FrozenArena) -> Self {
        fn clone_map<V: Copy>(
            m: &std::collections::HashMap<Box<str>, V>,
        ) -> FxHashMap<Box<str>, V> {
            m.iter().map(|(k, v)| (k.clone(), *v)).collect()
        }
        let base_fns = clone_map(stack.fn_by_sym());
        let base_fn_entries = clone_map(stack.entry_by_sym());
        let base_statics = clone_map(stack.static_by_sym());
        let base_tls = clone_map(stack.tls_by_sym());
        let delta_first_fn = stack.total_fns() as ir::FuncId;
        Linker {
            tcx,
            ids: FxHashMap::default(),
            queue: VecDeque::new(),
            builtins: engine_builtins(tcx),
            exports: None,
            frozen,
            alloc_addrs: FxHashMap::default(),
            fn_entries: FxHashMap::default(),
            fn_addrs: FxHashMap::default(),
            tls_ids: FxHashMap::default(),
            tls_slots: Vec::new(),
            asm_sites: Vec::new(),
            foreign_static_syms: Vec::new(),
            foreign_fn_entries: FxHashMap::default(),
            archive_fallbacks: Vec::new(),
            split: None,
            base_fns,
            base_fn_entries,
            base_statics,
            base_tls,
            delta_first_fn,
            delta_first_tls: stack.total_tls() as ir::TlsId,
            delta_first_asm: stack.total_asm() as ir::AsmStubId,
            next_fn: delta_first_fn,
            static_defs: Vec::new(),
        }
    }

    /// A2 split 激活（s3b-a2-design §4）：image 冻结区落样条 k=0 域。
    /// 域被占 = 回退动态基址（语义不变；A2-2 写盘阶段会拒序列化自愈）。
    fn activate_split(&mut self) {
        self.split = Some(Split {
            image_frozen: FrozenArena::new_image(0),
            image_queue: VecDeque::new(),
            image_funcs: Vec::new(),
            image_fn_next: 0,
            image_tls_slots: Vec::new(),
            image_asm_sites: Vec::new(),
            image_alloc_addrs: FxHashMap::default(),
            image_fn_entries: FxHashMap::default(),
            current_image: false,
            image_insts: Vec::new(),
        });
    }

    /// 预留一个 asm-stub 槽（M5.0），返回 (AsmStubId, 符号名)；文本随后 set_asm_stub 回填。
    /// 分两步是因为 wrapper 名要先于文本生成确定（自引用 .size 指令）。
    /// S4：id 从底座计数起编（wrapper 名跨域唯一白拿）。A2 split 按当前类分轨：
    /// image 类 = 标签 id + `mirvm_asm_xi{j}` 名，delta 类 = 原 id 空间 + `mirvm_asm_xd{k}`
    /// 名（最终位序收尾才知，名字与位序解耦）；非 split 路径沿用位序名不变。
    fn reserve_asm_stub(&mut self) -> (ir::AsmStubId, Box<str>) {
        if let Some(s) = &mut self.split {
            if s.current_image {
                let j = s.image_asm_sites.len() as ir::AsmStubId;
                let name: Box<str> = format!("mirvm_asm_xi{j}").into();
                s.image_asm_sites.push(ir::AsmSite {
                    name: name.clone(),
                    text: String::new(),
                });
                return (IMAGE_TAG | j, name);
            }
            let k = self.delta_first_asm + self.asm_sites.len() as ir::AsmStubId;
            let name: Box<str> = format!("mirvm_asm_xd{k}").into();
            self.asm_sites.push(ir::AsmSite {
                name: name.clone(),
                text: String::new(),
            });
            return (k, name);
        }
        let id = self.delta_first_asm + self.asm_sites.len() as ir::AsmStubId;
        let name: Box<str> = format!("mirvm_asm_{id}").into();
        self.asm_sites.push(ir::AsmSite {
            name: name.clone(),
            text: String::new(),
        });
        (id, name)
    }
    fn set_asm_stub(&mut self, id: ir::AsmStubId, text: String) {
        if id & IMAGE_TAG != 0 {
            let s = self.split.as_mut().expect("标签 stub id 仅 split 模式存在");
            s.image_asm_sites[(id & !IMAGE_TAG) as usize].text = text;
        } else {
            self.asm_sites[(id - self.delta_first_asm) as usize].text = text;
        }
    }

    /// `#[thread_local]` static → 稠密 TlsId（M4.4 D3）。模板 = 初始化器求值产物
    /// 物化进冻结区（ensure_alloc 复用，重定位白拿——运行期只作字节源，无人写）。
    pub(crate) fn tls_id(&mut self, def_id: rustc_hir::def_id::DefId) -> Result<ir::TlsId, String> {
        if let Some(&id) = self.tls_ids.get(&def_id) {
            return Ok(id);
        }
        // S4 底座 TLS 去重：TlsId 是线程局部身份，双份 = 同一 #[thread_local] 在
        // 底座函数与 delta 函数眼中是两个变量（错值级），必须复用。
        if !self.base_tls.is_empty() {
            let sym = self.tcx.symbol_name(Instance::mono(self.tcx, def_id)).name;
            if let Some(&id) = self.base_tls.get(sym) {
                self.tls_ids.insert(def_id, id);
                return Ok(id);
            }
        }
        let alloc = self
            .tcx
            .eval_static_initializer(def_id)
            .map_err(|e| format!("TLS static 初始化器求值失败: {e:?}"))?;
        let (size, align) = (alloc.inner().size().bytes(), alloc.inner().align.bytes());
        let alloc_id = self.tcx.reserve_and_set_static_alloc(def_id);
        let template = self.ensure_alloc(alloc_id)?;
        // A2 split：TLS 身份按 def_id.krate 定域（非本地 → image 槽区，单一身份）；
        // image 上下文遇本地 TLS = purity 向下封闭被破坏（分类器 bug），响亮拒绝。
        if let Some(s) = &mut self.split {
            if def_id.krate != rustc_hir::def_id::LOCAL_CRATE {
                let j = s.image_tls_slots.len() as ir::TlsId;
                s.image_tls_slots.push(ir::TlsSlot {
                    template,
                    size,
                    align: align as u32,
                });
                let id = IMAGE_TAG | j;
                self.tls_ids.insert(def_id, id);
                return Ok(id);
            }
            if s.current_image {
                panic!("A2 closure violation：image 实例引用本地 TLS static（分类器漏判）");
            }
        }
        let id = self.delta_first_tls + self.tls_slots.len() as ir::TlsId;
        self.tls_slots.push(ir::TlsSlot {
            template,
            size,
            align: align as u32,
        });
        self.tls_ids.insert(def_id, id);
        Ok(id)
    }

    /// fn-ptr 条目地址（D4）：每 instance 一个 16 对齐真地址；内容 = FuncId（调试用）。
    /// 比较/转型语义正确；间接调用经反查表派发（M4.1 第 5 步接 CallIndirect）。
    /// S4：底座函数已有条目则复用（单一地址身份；底座 vtable 与 delta 取址一致）；
    /// 底座函数无条目（构建时没被取址）则在 delta 区补一个——总量仍恰一份。
    /// A2 split：条目按 instance 类定域（image 类 → image 区，单一地址身份不变）；
    /// image 上下文遇 delta 类 = purity 封闭被破坏（分类器 bug），响亮拒绝。
    /// extern fn（fn 体内 extern 块声明的内核函数被当 fn-ptr 用，ring 的派发模式）：
    /// 无 MIR 可降，走 foreign_fn_entry_addr——值 = native 链接器解析出的真符号地址。
    pub(crate) fn fn_entry_addr(&mut self, inst: Instance<'tcx>) -> Result<u64, String> {
        if let Some(&a) = self.fn_entries.get(&inst) {
            return Ok(a);
        }
        if self.tcx.is_foreign_item(inst.def_id()) {
            return self.foreign_fn_entry_addr(inst);
        }
        let fid = self.func_id(inst);
        if fid < self.delta_first_fn
            && let Some(&a) = self.base_fn_entries.get(self.tcx.symbol_name(inst).name)
        {
            self.fn_entries.insert(inst, a);
            return Ok(a);
        }
        let addr = if let Some(s) = &mut self.split {
            if fid & IMAGE_TAG != 0 || fid < self.delta_first_fn {
                // image 类，或底座命中但底座无条目（S4 补建条目的 split 变体）：
                // image 区——单一地址身份（delta 引用 image 域恒稳定；delta 区对
                // image 字节码是跨运行不稳定域，绝不能去）。
                let a = s.image_frozen.alloc(8, 16);
                s.image_fn_entries.insert(inst, a);
                a
            } else {
                if s.current_image {
                    panic!(
                        "A2 closure violation：image 实例引用 delta 类 fn 条目（分类器漏判）: {}",
                        self.tcx.symbol_name(inst).name
                    );
                }
                self.frozen.alloc(8, 16)
            }
        } else {
            self.frozen.alloc(8, 16)
        };
        unsafe { (addr as *mut u64).write(fid as u64) };
        self.fn_entries.insert(inst, addr);
        self.fn_addrs.insert(addr, fid);
        Ok(addr)
    }

    /// extern fn 条目地址（fn-ptr 取址）：无 MIR 的 foreign item 不能入 worklist
    /// （instance_mir = rustc query panic）；其 fn-ptr 值语义 = native 链接器解析
    /// 出的真符号地址。解析序与 resolve_call 同构：①引擎内建 ②导出符号仿真
    /// ③denylist/llvm/rust-internal ④归档 hidden 兜底表 → dlsym 全域。
    /// 烤入的是宿主真地址（ASLR 跨进程无效）⇒ 登记符号名：含此类地址的模块不入
    /// L2/image 缓存（与 extern static 同规则，ircache/depsimage/baseimage 三判据）。
    fn foreign_fn_entry_addr(&mut self, inst: Instance<'tcx>) -> Result<u64, String> {
        if let Some(&a) = self.foreign_fn_entries.get(&inst) {
            return Ok(a);
        }
        let name = self.tcx.symbol_name(inst).name;
        // host_baked=false：弱符号缺席的 NULL——无宿主地址烤入，不污染可缓存性
        let bake = |this: &mut Self, addr: u64, host_baked: bool| {
            if host_baked {
                this.foreign_static_syms.push(name.into());
            }
            this.foreign_fn_entries.insert(inst, addr);
            addr
        };
        let link_name = Symbol::intern(name);
        // ①引擎内建：纯直通快路径（语义与通用 dlsym+libffi 道逐位一致）可给真地址；
        // 其余内建（alloc/unwind/fork/atexit/signal/backtrace 系）是引擎接管的语义，
        // 无地址可物化——响亮 Trap（宿主进程也导出 __rust/_Unwind 系符号，直取 =
        // 打穿引擎的堆/panic/unwind 模型）。
        if let Some(&b) = self.builtins.get(&link_name) {
            use ir::Builtin as B;
            if !matches!(b, B::HostGetenv | B::HostWrite | B::HostStrlen | B::HostAbort) {
                return Err(format!(
                    "extern fn `{name}` 被当作值取址（fn-ptr），但它是引擎内建语义符号，无地址可物化"
                ));
            }
        }
        let rust_internal = name.starts_with("__rust")
            || name.starts_with("__rdl")
            || name.starts_with("rust_");
        // ②链接仿真：符号由已链接 crate 的导出定义提供 → 值 = 该 guest 定义的
        // 条目地址；weak 定义让位于动态库强符号（Rust 内部符号除外——见①注）。
        let exported = self.exported_defs().get(&link_name).copied();
        if let Some((target, is_weak)) = exported {
            if is_weak && !rust_internal {
                let cname = std::ffi::CString::new(name).map_err(|_| "符号名含 NUL".to_string())?;
                let strong = unsafe { libc::dlsym(std::ptr::null_mut(), cname.as_ptr()) };
                if !strong.is_null() {
                    return Ok(bake(self, strong as u64, true));
                }
            }
            return self.fn_entry_addr(target);
        }
        // ③与 resolve_call 同一纪律的绝不直通清单
        if DENY_EXACT.contains(&name) || DENY_PREFIX.iter().any(|p| name.starts_with(p)) {
            return Err(format!(
                "foreign `{name}` 被当作值取址（denylist：线程 M4.4 / 进程模型不直通）"
            ));
        }
        if name.starts_with("llvm.") {
            return Err(format!("foreign `{name}` 被当作值取址（LLVM 内部符号，按需内建）"));
        }
        if rust_internal {
            return Err(format!(
                "foreign `{name}` 被当作值取址（Rust 内部 ABI 符号，宿主进程亦有导出，不能直取）"
            ));
        }
        // ④归档 hidden 符号兜底表先于 dlsym 全域（native 链接期绑定：静态归档
        // 成员链进 guest 后其定义恒胜全局命名空间——宿主 libLLVM 内嵌 ZSTD_* 一族
        // 会静默截胡，corpus c_zstd_stream 实锤；兜底表只收不进 .dynsym 的符号，
        // dynsym 可见面维持 dlsym 解析，物化期 reject_symbol_ambiguity 已拒其与
        // 全局的碰撞）。归档 `.so` 已在排干 worklist 前 RTLD_GLOBAL 加载进全局域
        //（lower_inner 头部），dlsym 全域可达其 dynsym 可见符号。
        let mut p = 0u64;
        for (bias, syms) in &self.archive_fallbacks {
            if let Some(&v) = syms.get(name) {
                p = bias + v;
                break;
            }
        }
        if p == 0 {
            let cname = std::ffi::CString::new(name).map_err(|_| "符号名含 NUL".to_string())?;
            p = unsafe { libc::dlsym(std::ptr::null_mut(), cname.as_ptr()) } as u64;
        }
        if p == 0 {
            // weak 符号缺席 = NULL（native 未定义弱符号的取址语义）；经它间接调用
            // 在执行相响亮终止（CallIndirect 的空指针诊断）
            let weak = self.tcx.codegen_fn_attrs(inst.def_id()).import_linkage
                == Some(rustc_hir::attrs::Linkage::ExternalWeak);
            if weak {
                return Ok(bake(self, 0, false));
            }
            return Err(format!(
                "extern fn `{name}` 被当作值取址，但符号未命中（归档兜底表 / dlsym 全域均无）"
            ));
        }
        Ok(bake(self, p, true))
    }

    /// 裸字节物化进冻结区（128 位常量等小常量的通用道）。
    /// A2 split：纯字节无指针无 locality，按当前类定域即可。
    pub(crate) fn frozen_alloc_bytes(&mut self, bytes: &[u8]) -> u64 {
        let arena: &mut FrozenArena = match &mut self.split {
            Some(s) if s.current_image => &mut s.image_frozen,
            _ => &mut self.frozen,
        };
        let p = arena.alloc(bytes.len() as u64, 16);
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), p as *mut u8, bytes.len()) };
        p
    }

    /// alloc → 冻结区真地址（按需递归物化；先分后填 ⇒ 指针环安全）。
    /// A2 split 路由（s3b-a2-design §4.3）：
    /// - image 上下文只接受 image 域地址（跨运行稳定域）；delta 上下文两域皆可
    ///   （delta map 优先，image map 兜底复用——delta→image 向下稳定）。
    /// - 身份必需通道（static/weak cell/fn 条目）按 krate/类定域并**双表登记**
    ///   （防双份精神分裂）；Memory/vtable 身份 unspecified，按上下文定域，
    ///   image 上下文对 delta 区已有者**提升**（双份物化，常量只读安全）。
    pub(crate) fn ensure_alloc(&mut self, id: AllocId) -> Result<u64, String> {
        let ctx_image = self.split.as_ref().is_some_and(|s| s.current_image);
        if ctx_image {
            if let Some(&a) = self
                .split
                .as_ref()
                .expect("split")
                .image_alloc_addrs
                .get(&id)
            {
                return Ok(a);
            }
        } else {
            if let Some(&a) = self.alloc_addrs.get(&id) {
                return Ok(a);
            }
            if let Some(s) = &self.split
                && let Some(&a) = s.image_alloc_addrs.get(&id)
            {
                return Ok(a);
            }
        }
        match self.tcx.global_alloc(id) {
            GlobalAlloc::Memory(alloc) => self.materialize_in(id, alloc, ctx_image),
            GlobalAlloc::Static(def_id) => {
                // extern static = 真符号（os:: 直通）：
                // - weak（gettid 等 fn 符号判空模式）：判空 cell 写 0（缺席）——weak
                //   **fn** 符号即便存在也不能给真地址（guest 拿去调用 = 跳 native 代码，
                //   条目反查失败；M4.4 thunk 前统一走 fallback 路径）
                // - 非 weak（environ 等数据符号）：alloc 基址 = dlsym 真地址
                if self.tcx.is_foreign_item(def_id) {
                    // dlsym 用链接符号名（#[link_name] 前缀——ring 的 prefixed_extern
                    // 静态量；item_name 会丢掉前缀），与 resolve_call 的 fn 路径同源
                    let name = self
                        .tcx
                        .symbol_name(Instance::mono(self.tcx, def_id))
                        .name;
                    // extern block 内 item 的 linkage 在 import_linkage 字段
                    let weak = self.tcx.codegen_fn_attrs(def_id).import_linkage
                        == Some(rustc_hir::attrs::Linkage::ExternalWeak);
                    if weak {
                        // 判空 cell：&static 地址身份必需（krate 定域 + 双表登记）
                        let cell = if let Some(s) = &mut self.split
                            && def_id.krate != rustc_hir::def_id::LOCAL_CRATE
                        {
                            s.image_frozen.alloc(8, 8)
                        } else {
                            self.frozen.alloc(8, 8) // 清零 cell = 符号缺席
                        };
                        self.record_both(id, cell);
                        return Ok(cell);
                    }
                    // 归档 hidden 符号兜底表先于 dlsym 全域（与 fn 取址④同序——
                    // native 链接期绑定：归档内定义恒胜全局命名空间）
                    let mut p = 0u64;
                    for (bias, syms) in &self.archive_fallbacks {
                        if let Some(&v) = syms.get(name) {
                            p = bias + v;
                            break;
                        }
                    }
                    if p == 0 {
                        let cname =
                            std::ffi::CString::new(name).map_err(|_| "符号名含 NUL".to_string())?;
                        p = unsafe { libc::dlsym(std::ptr::null_mut(), cname.as_ptr()) } as u64;
                    }
                    if p == 0 {
                        return Err(format!(
                            "extern static `{name}` 未命中（归档兜底表 / dlsym 全域均无）"
                        ));
                    }
                    // 宿主真地址直嵌（&environ 语义要求就是 libc 变量本体地址）——
                    // ASLR 下跨进程无效 ⇒ 登记符号，含此类地址的模块不入 L2 缓存
                    //（M6 片2 gate 实测：c_process 热回放上进程 libc 地址 SIGSEGV）。
                    // 升级路径 = GOT 式 Operand 间接（IR 设计变更，M6 后续）。
                    // A2：deps-image 同规则拒（写盘自检，baseimage 三判据同构）。
                    self.foreign_static_syms.push(name.into());
                    self.record_both(id, p);
                    return Ok(p);
                }
                // S4 底座静态去重：同一 static 双份物化 = static mut/内部可变性的
                // 精神分裂（两处地址各自演化），命中必须复用底座地址。
                if !self.base_statics.is_empty() {
                    let sym = self.tcx.symbol_name(Instance::mono(self.tcx, def_id)).name;
                    if let Some(&addr) = self.base_statics.get(sym) {
                        self.record_both(id, addr);
                        return Ok(addr);
                    }
                }
                // A2 split：static 身份必需（static mut/内部可变性/&static 相等性），
                // 一律按 def_id.krate 定域（非本地 → image 区）；image 上下文遇本地
                // static = purity 封闭被破坏（分类器 bug），响亮拒绝。
                let to_image = if let Some(s) = &self.split {
                    if def_id.krate == rustc_hir::def_id::LOCAL_CRATE {
                        if s.current_image {
                            panic!("A2 closure violation：image 实例引用本地 static（分类器漏判）");
                        }
                        false
                    } else {
                        true
                    }
                } else {
                    false
                };
                // static 的字节 = 初始化器求值产物；可写（static mut/内部可变性）
                let alloc = self
                    .tcx
                    .eval_static_initializer(def_id)
                    .map_err(|e| format!("static 初始化器求值失败: {e:?}"))?;
                let addr = self.materialize_in(id, alloc, to_image)?;
                if to_image {
                    self.record_both(id, addr);
                }
                self.static_defs.push((def_id, addr)); // 底座/image 导出素材
                Ok(addr)
            }
            GlobalAlloc::Function { instance } => {
                let addr = self.fn_entry_addr(instance)?;
                self.record_both(id, addr);
                Ok(addr)
            }
            GlobalAlloc::VTable(ty, dyn_ty) => {
                // A2 split 护栏：image 上下文遇本地 Self 类型的 vtable = 封闭破坏。
                // vtable 地址身份 unspecified（rustc 自身 per-CGU 复制）⇒ 按上下文
                // 定域（提升双份合规），无需 krate 定域。
                if self.split.as_ref().is_some_and(|s| s.current_image)
                    && ty.walk().any(arg_mentions_local)
                {
                    panic!("A2 closure violation：image 实例引用本地类型 vtable（分类器漏判）");
                }
                // 现成的 vtable 分配（F5）——递归走 Memory 路径（含 fn 条目重定位）
                let principal = dyn_ty
                    .principal()
                    .map(|b| self.tcx.instantiate_bound_regions_with_erased(b));
                let vt_id = self.tcx.vtable_allocation((ty, principal));
                let addr = self.ensure_alloc(vt_id)?;
                self.record_addr(id, addr, ctx_image);
                Ok(addr)
            }
            GlobalAlloc::TypeId { .. } => {
                // TypeId"分配"：基址 0——重定位 base+addend 后值 = 128 位类型哈希的
                // 指针宽片段本身（tier-0 resolve_addr/Miri 同款）
                self.record_addr(id, 0, ctx_image);
                Ok(0)
            }
        }
    }

    /// 按上下文登记去重表（split；非 split 恒 delta 表）
    fn record_addr(&mut self, id: AllocId, addr: u64, ctx_image: bool) {
        match &mut self.split {
            Some(s) if ctx_image => {
                s.image_alloc_addrs.insert(id, addr);
            }
            _ => {
                self.alloc_addrs.insert(id, addr);
            }
        }
    }

    /// 身份必需通道的双表登记（split：两上下文都能以同一地址复现 = 单一身份）
    fn record_both(&mut self, id: AllocId, addr: u64) {
        self.alloc_addrs.insert(id, addr);
        if let Some(s) = &mut self.split {
            s.image_alloc_addrs.insert(id, addr);
        }
    }

    /// 物化一个内存分配：分地址 → 拷字节 → 重定位（provenance 表逐项写真地址+addend）。
    /// image=true 落 image 域并登记 image 表（split 专用）；false 落 delta 域（今日路径）。
    fn materialize_in(
        &mut self,
        id: AllocId,
        alloc: ConstAllocation<'tcx>,
        image: bool,
    ) -> Result<u64, String> {
        let a = alloc.inner();
        let size = a.size().bytes();
        let align = a.align.bytes();
        let base = if image {
            let s = self.split.as_mut().expect("image 物化仅 split 模式");
            let base = s.image_frozen.alloc(size, align);
            s.image_alloc_addrs.insert(id, base); // 先分后填（环安全）
            base
        } else {
            let base = self.frozen.alloc(size, align);
            self.alloc_addrs.insert(id, base); // 先分后填（环安全）
            base
        };
        let bytes = a.inspect_with_uninit_and_ptr_outside_interpreter(0..size as usize);
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), base as *mut u8, size as usize) };
        // 重定位：ptr 位置存的 8 字节 = 目标内偏移（addend）→ 换成目标真地址 + addend
        for (off, prov) in a.provenance().ptrs().iter() {
            let target = self.ensure_alloc(prov.alloc_id())?;
            let at = (base + off.bytes()) as *mut u64;
            unsafe {
                let addend = at.read_unaligned();
                at.write_unaligned(target.wrapping_add(addend));
            }
        }
        Ok(base)
    }

    /// instance → FuncId；首见分配 id 并入待降低队列（worklist 扩集的入口）。
    /// S4：首见先查底座（v0 symbol_name 键）——命中即复用底座 id，不入队；
    /// symbol_name 只在首见且有底座时计算一次（无底座路径零额外成本）。
    /// A2 split：非底座实例按 purity 分轨——image 类（Pure）得标签 id 入 image
    /// 队列，delta 类（Local/Tainted）走今日 untagged 空间入 delta 队列。
    pub(crate) fn func_id(&mut self, inst: Instance<'tcx>) -> ir::FuncId {
        if let Some(&id) = self.ids.get(&inst) {
            return id;
        }
        if !self.base_fns.is_empty()
            && let Some(&bid) = self.base_fns.get(self.tcx.symbol_name(inst).name)
        {
            self.ids.insert(inst, bid);
            return bid;
        }
        if let Some(s) = &mut self.split {
            let id = if classify_purity(inst).is_image() {
                let j = s.image_fn_next;
                s.image_fn_next += 1;
                let id = IMAGE_TAG | j;
                s.image_queue.push_back((id, inst));
                if s.image_funcs.len() <= j as usize {
                    s.image_funcs.resize_with(j as usize + 1, || None);
                }
                s.image_insts.push(inst);
                id
            } else {
                let id = self.next_fn;
                self.next_fn += 1;
                self.queue.push_back((id, inst));
                id
            };
            self.ids.insert(inst, id);
            return id;
        }
        let id = self.next_fn;
        self.next_fn += 1;
        self.ids.insert(inst, id);
        self.queue.push_back((id, inst));
        id
    }

    /// 调用点的 callee 解析（Call 终止子用）。Err = 该块 Trap（带分期诊断）。
    pub(crate) fn resolve_call(&mut self, inst: Instance<'tcx>) -> Result<Callee, String> {
        // intrinsic（D5）：fallback body 按普通函数补收（collector 因 backend
        // replaced_intrinsics 跳过收集，解释视角必须自己收——构造同 collector 源码：
        // Instance::new_raw）；must_be_overridden 的等引擎内建表（M4.1 第 5 步）。
        if let InstanceKind::Intrinsic(def_id) = inst.def {
            let intrinsic = self
                .tcx
                .intrinsic(def_id)
                .expect("InstanceKind::Intrinsic 必有 IntrinsicDef");
            if intrinsic.must_be_overridden {
                return Err(format!(
                    "intrinsic `{}` 无 fallback（引擎内建表，M4.1）",
                    intrinsic.name
                ));
            }
            let item = Instance::new_raw(def_id, inst.args);
            return Ok(Callee::Func(self.func_id(item)));
        }
        if let InstanceKind::Virtual(..) = inst.def {
            return Err("dyn 虚调用派发（M4.1+）".into());
        }
        if self.tcx.is_foreign_item(inst.def_id()) {
            let link_name = Symbol::intern(self.tcx.symbol_name(inst).name);
            // ①引擎原语（alloc/unwind/stub/快路径直通）
            if let Some(&b) = self.builtins.get(&link_name) {
                return Ok(Callee::Builtin(b));
            }
            // ②链接仿真：按符号名在已链接 crate 的导出定义里找（tier-0
            // find_exported_symbol 同构；panic_impl→rust_begin_unwind、__rdl_* 走此路）
            let target = self.exported_defs().get(&link_name).copied();
            if let Some((target, is_weak)) = target {
                // native 链接器语义：weak 定义让位于动态库强符号（compiler-builtins
                // 的 weak sqrt/memcmp vs libc/libm）。Rust 内部 ABI 符号（__rust/
                // __rdl/rust_ 前缀）除外——宿主进程（librustc_driver）也导出它们，
                // 直通会打穿引擎的堆/panic 模型。
                let name = link_name.as_str();
                let rust_internal = name.starts_with("__rust")
                    || name.starts_with("__rdl")
                    || name.starts_with("rust_");
                if is_weak && !rust_internal {
                    let cname = std::ffi::CString::new(name).unwrap();
                    let strong = unsafe { libc::dlsym(std::ptr::null_mut(), cname.as_ptr()) };
                    if !strong.is_null() {
                        return self.freeze_foreign_sig(inst, name);
                    }
                }
                return Ok(Callee::Func(self.func_id(target)));
            }
            // ③os:: 直通（P7）：denylist 拒 → 其余 dlsym+libffi 按冻结签名直调
            let name = link_name.as_str();
            if DENY_EXACT.contains(&name) || DENY_PREFIX.iter().any(|p| name.starts_with(p)) {
                return Err(format!(
                    "foreign `{name}`（denylist：线程 M4.4 / 进程模型不直通）"
                ));
            }
            if name.starts_with("llvm.") {
                return Err(format!("foreign `{name}`（LLVM 内部符号，按需内建）"));
            }
            return self.freeze_foreign_sig(inst, name);
        }
        // naked fn（D8h）：函数体是裸机器码，无常规 MIR body。物化进 global-asm
        // `.so`（收集阶段已做），调用点按真 ABI 走 foreign 直调其 mangled 符号。
        if self
            .tcx
            .codegen_fn_attrs(inst.def_id())
            .flags
            .contains(rustc_middle::middle::codegen_fn_attrs::CodegenFnAttrFlags::NAKED)
        {
            let name = self.tcx.symbol_name(inst).name;
            return self.freeze_foreign_sig(inst, name);
        }
        // 普通函数：worklist 闭包扩集（跨 crate 非泛型函数不在 collector 种子集）
        Ok(Callee::Func(self.func_id(inst)))
    }

    /// os:: 直通签名冻结：foreign fn sig → FfiKind 列表（tier-0 ty_to_ffitype 同构）。
    /// fn-ptr 类型的参数（pthread_create 的 thread_start 等）额外冻结**内层签名**
    /// （M4.4 D1）：执行期该位若收到 fn 条目地址，thunk 工厂物化真机器码后再直传。
    fn freeze_foreign_sig(&mut self, inst: Instance<'tcx>, name: &str) -> Result<Callee, String> {
        let sig = self
            .tcx
            .fn_sig(inst.def_id())
            .instantiate(self.tcx, inst.args)
            .skip_binder();
        let env = TypingEnv::fully_monomorphized();
        let mut args = Vec::with_capacity(sig.inputs().len());
        let mut thunk_args = Vec::new();
        for (i, &t) in sig.inputs().iter().enumerate() {
            args.push(ffi_kind_of(self.tcx, env, t).map_err(|e| {
                format!("foreign `{name}` 参数 {t}: {e}（libffi 直通仅标量/指针）")
            })?);
            // fn ptr 参数位：裸 fn ptr + `Option<fn>`（可空回调——pthread_key_create 的
            // dtor 等；niche 布局下 None=0 原样直传）。内层签名不可冻结 = 整调用点
            // Trap（防静默错值：条目地址直传给 native 是静默崩溃）。
            let fnptr_ty = if t.is_fn_ptr() {
                Some(t)
            } else if let rustc_middle::ty::TyKind::Adt(def, sub) = t.kind()
                && self
                    .tcx
                    .is_diagnostic_item(rustc_span::sym::Option, def.did())
                && sub.type_at(0).is_fn_ptr()
            {
                Some(sub.type_at(0))
            } else {
                None
            };
            if let Some(t) = fnptr_ty {
                let inner = t.fn_sig(self.tcx).skip_binder();
                if inner.c_variadic() {
                    return Err(format!("foreign `{name}` 参数 {t}: 变参回调不支持 thunk"));
                }
                let mut in_args = Vec::with_capacity(inner.inputs().len());
                for &it in inner.inputs() {
                    let k = ffi_kind_of(self.tcx, env, it).map_err(|e| {
                        format!("foreign `{name}` 回调参数 {it}: {e}（thunk 仅标量/指针）")
                    })?;
                    if k == ir::FfiKind::Void {
                        return Err(format!(
                            "foreign `{name}` 回调参数 {it}: ZST 不可作 cif 参数"
                        ));
                    }
                    in_args.push(k);
                }
                let in_ret = ffi_kind_of(self.tcx, env, inner.output())
                    .map_err(|e| format!("foreign `{name}` 回调返回 {}: {e}", inner.output()))?;
                thunk_args.push((
                    i,
                    ir::ForeignSig {
                        args: in_args,
                        ret: in_ret,
                        fixed: None,
                        thunk_args: vec![],
                    },
                ));
            }
        }
        let ret = ffi_kind_of(self.tcx, env, sig.output())
            .map_err(|e| format!("foreign `{name}` 返回 {}: {e}", sig.output()))?;
        Ok(Callee::Foreign {
            sym: name.into(),
            args,
            ret,
            variadic: sig.c_variadic(),
            thunk_args,
        })
    }

    /// 导出符号表（②），惰性一次构建：遍历"最终二进制会链接到"的全部非泛型导出 def
    /// （tier-0 `for_each_linked_def` 同构），符号名 → mono instance，strong 覆盖 weak。
    fn exported_defs(&mut self) -> &FxHashMap<Symbol, (Instance<'tcx>, bool)> {
        let tcx = self.tcx;
        self.exports.get_or_insert_with(|| {
            use rustc_hir::def_id::LOCAL_CRATE;
            use rustc_middle::middle::codegen_fn_attrs::CodegenFnAttrFlags;
            use rustc_middle::middle::exported_symbols::ExportedSymbol;
            use rustc_session::config::CrateType;

            // (instance, is_weak)：非 weak 覆盖 weak
            let mut map: FxHashMap<Symbol, (Instance<'tcx>, bool)> = FxHashMap::default();
            let mut add = |def_id: rustc_hir::def_id::DefId| {
                if tcx.is_foreign_item(def_id)
                    || !matches!(tcx.def_kind(def_id), rustc_hir::def::DefKind::Fn)
                {
                    return;
                }
                let inst = Instance::mono(tcx, def_id);
                let name = Symbol::intern(tcx.symbol_name(inst).name);
                let is_weak = tcx.codegen_fn_attrs(def_id).linkage
                    == Some(rustc_hir::attrs::Linkage::WeakAny);
                match map.entry(name) {
                    std::collections::hash_map::Entry::Occupied(mut e) => {
                        if e.get().1 && !is_weak {
                            e.insert((inst, false));
                        }
                    }
                    std::collections::hash_map::Entry::Vacant(e) => {
                        e.insert((inst, is_weak));
                    }
                }
            };

            // 本地 crate：遍历 HIR（exported_symbols 会漏 #[used]）
            for def_id in tcx.hir_crate_items(()).definitions() {
                if !tcx.def_kind(def_id).has_codegen_attrs() {
                    continue;
                }
                let attrs = tcx.codegen_fn_attrs(def_id);
                let exported = attrs.contains_extern_indicator()
                    || attrs.flags.contains(CodegenFnAttrFlags::USED_COMPILER)
                    || attrs.flags.contains(CodegenFnAttrFlags::USED_LINKER);
                if !exported || tcx.generics_of(def_id).requires_monomorphization(tcx) {
                    continue;
                }
                add(def_id.into());
            }
            // 依赖 crate 的非泛型导出符号
            let dependency_formats = tcx.dependency_formats(());
            if let Some(format) = dependency_formats.get(&CrateType::Executable) {
                for (cnum, &linkage) in format.iter_enumerated() {
                    if cnum == LOCAL_CRATE
                        || linkage == rustc_middle::middle::dependency_format::Linkage::NotLinked
                    {
                        continue;
                    }
                    for &(symbol, _) in tcx.exported_non_generic_symbols(cnum) {
                        if let ExportedSymbol::NonGeneric(def_id) = symbol {
                            add(def_id);
                        }
                    }
                }
            }
            map
        })
    }
}

/// extern "C" 系 fn-ptr 类型 → 冻结 ForeignSig（M4.4 FFI 反方向之二：调用点带上，
/// 执行期条目反查未命中 = guest 持 native 真码 → libffi 按此直调）。
/// None = Rust ABI / 变参 / 参数不可类——该调用点只能派发 guest 条目（未命中即诊断）。
pub(crate) fn freeze_c_fnptr_sig<'tcx>(
    tcx: TyCtxt<'tcx>,
    env: TypingEnv<'tcx>,
    ty: rustc_middle::ty::Ty<'tcx>,
) -> Option<ir::ForeignSig> {
    use rustc_abi::ExternAbi;
    let sig = ty.fn_sig(tcx).skip_binder();
    if !matches!(sig.abi(), ExternAbi::C { .. } | ExternAbi::System { .. }) || sig.c_variadic() {
        return None;
    }
    let mut args = Vec::with_capacity(sig.inputs().len());
    for &t in sig.inputs() {
        let k = ffi_kind_of(tcx, env, t).ok()?;
        if k == ir::FfiKind::Void {
            return None; // ZST 不可作 cif 参数
        }
        args.push(k);
    }
    let ret = ffi_kind_of(tcx, env, sig.output()).ok()?;
    Some(ir::ForeignSig {
        args,
        ret,
        fixed: None,
        thunk_args: vec![],
    })
}

/// 类型 → libffi 直通类别（标量与指针；ZST=Void 仅返回位；聚合不支持）。
pub(crate) fn ffi_kind_of<'tcx>(
    tcx: TyCtxt<'tcx>,
    env: TypingEnv<'tcx>,
    ty: rustc_middle::ty::Ty<'tcx>,
) -> Result<ir::FfiKind, String> {
    use rustc_abi::{BackendRepr, Float, Integer, Primitive};
    let layout = tcx
        .layout_of(env.as_query_input(ty))
        .map_err(|e| format!("layout 失败: {e}"))?;
    if layout.is_zst() {
        return Ok(ir::FfiKind::Void);
    }
    if let BackendRepr::Scalar(s) = layout.backend_repr {
        return Ok(match s.primitive() {
            Primitive::Int(Integer::I8, true) => ir::FfiKind::I8,
            Primitive::Int(Integer::I16, true) => ir::FfiKind::I16,
            Primitive::Int(Integer::I32, true) => ir::FfiKind::I32,
            Primitive::Int(Integer::I64, true) => ir::FfiKind::I64,
            Primitive::Int(Integer::I8, false) => ir::FfiKind::U8,
            Primitive::Int(Integer::I16, false) => ir::FfiKind::U16,
            Primitive::Int(Integer::I32, false) => ir::FfiKind::U32,
            Primitive::Int(Integer::I64, false) => ir::FfiKind::U64,
            Primitive::Float(Float::F32) => ir::FfiKind::F32,
            Primitive::Float(Float::F64) => ir::FfiKind::F64,
            Primitive::Pointer(_) => ir::FfiKind::Ptr,
            other => return Err(format!("标量 {other:?} 不支持")),
        });
    }
    Err("非标量（按值聚合）".into())
}

/// ①引擎原语表：codegen 会为 allocator shim 生成的符号清单（tier-0/Miri 同款来源，
/// 符号是 mangled 的——`mangle_internal_symbol`）。Special = 默认分配器（引擎接管）；
/// 非 special（自定义 #[global_allocator] 的 __rust_* → 用户 __rg_* 转发、
/// __rust_alloc_error_handler）暂不注册 → 走 ③ Trap 诊断。
fn engine_builtins(tcx: TyCtxt<'_>) -> FxHashMap<Symbol, ir::Builtin> {
    use rustc_ast::expand::allocator::{self, SpecialAllocatorMethod as S};
    use rustc_symbol_mangling::mangle_internal_symbol;

    let mut out = FxHashMap::default();
    if let Some(kind) = tcx.allocator_kind(()) {
        for method in rustc_codegen_ssa::base::allocator_shim_contents(tcx, kind) {
            let Some(special) = method.special else {
                continue;
            };
            let b = match special {
                S::Alloc => ir::Builtin::RustAlloc,
                S::Dealloc => ir::Builtin::RustDealloc,
                S::Realloc => ir::Builtin::RustRealloc,
                S::AllocZeroed => ir::Builtin::RustAllocZeroed,
            };
            let sym = mangle_internal_symbol(tcx, &allocator::global_fn_name(method.name));
            out.insert(Symbol::intern(&sym), b);
        }
    }
    let sentinel =
        mangle_internal_symbol(tcx, rustc_ast::expand::allocator::NO_ALLOC_SHIM_IS_UNSTABLE);
    out.insert(Symbol::intern(&sentinel), ir::Builtin::NoAllocShim);
    // unwind 原语（M4.2）：panic_unwind 照常解释，引擎在平台 unwinder 符号层接管
    out.insert(
        Symbol::intern("_Unwind_RaiseException"),
        ir::Builtin::UnwindRaise,
    );
    // 快路径直通（panic 链高频；其余 foreign 走通用 dlsym+libffi 道）
    out.insert(Symbol::intern("getenv"), ir::Builtin::HostGetenv);
    out.insert(Symbol::intern("write"), ir::Builtin::HostWrite);
    out.insert(Symbol::intern("strlen"), ir::Builtin::HostStrlen);
    out.insert(Symbol::intern("abort"), ir::Builtin::HostAbort);
    // fork（D8f）：builtin 守卫 guest 线程数后直调 libc::fork。
    out.insert(Symbol::intern("fork"), ir::Builtin::HostFork);
    // atexit 家族（D8g）：glibc 不导出 `atexit` 供 guest dlsym → builtin 接管。
    out.insert(Symbol::intern("atexit"), ir::Builtin::HostAtexit);
    out.insert(Symbol::intern("__cxa_atexit"), ir::Builtin::HostCxaAtexit);
    out.insert(Symbol::intern("on_exit"), ir::Builtin::HostOnExit);
    out.insert(Symbol::intern("syscall"), ir::Builtin::HostSyscall);
    // signal/sigaction 的 handler 藏在整数/结构体中，不能由通用 FFI fn-ptr 参数
    // thunk 化；而且 signal trampoline 必须异步信号安全，普通 libffi closure 不满足。
    // 明确 Trap，直到有专用实现。其余旧 StubZero 项改走 dlsym+libffi；
    // atexit/dl_iterate_phdr 的显式 fn-ptr 参数可由 M4.4 thunk 工厂处理。
    out.insert(Symbol::intern("signal"), ir::Builtin::HostSignal);
    out.insert(Symbol::intern("sigaction"), ir::Builtin::HostSigaction);
    // 宿主 unwinder 从 libffi/解释器的 native stack 取回 IP，无法代表
    // guest 的冻结函数条目。回调 thunk 只解决调用方向，不会翻译栈帧；
    // 所以在 guest-frame/IP 映射完成前必须明确拒绝，不能返回貌似成功
    // 的宿主 backtrace。
    // `_Unwind_RaiseException` / `_Unwind_DeleteException` 上面有 guest 专用语义；
    // 其余 libgcc context/stack API 若直通，看到的只会是宿主解释器帧。
    // 整组显式 deny，避免从 GetIPInfo/CFA/LSDA 等旁路重新引入静默错值。
    // 仍拒绝的 unwinder context/state API：直通看到的只是宿主解释器帧，无 guest
    // 语义。整组显式 deny，避免从 CFA/LSDA/SetGR 等旁路重新引入静默错值。
    for name in [
        "_Unwind_Find_FDE",
        "_Unwind_ForcedUnwind",
        "_Unwind_GetDataRelBase",
        "_Unwind_GetGR",
        "_Unwind_GetLanguageSpecificData",
        "_Unwind_GetRegionStart",
        "_Unwind_GetTextRelBase",
        "_Unwind_Resume",
        "_Unwind_Resume_or_Rethrow",
        "_Unwind_SetGR",
        "_Unwind_SetIP",
    ] {
        out.insert(
            Symbol::intern(name),
            ir::Builtin::Unsupported(ir::StaticStr(name)),
        );
    }
    // backtrace 影子帧（D8e）：这四个由 Ctx 影子帧栈诚实回答（IP=合成 fn token）。
    out.insert(
        Symbol::intern("_Unwind_Backtrace"),
        ir::Builtin::UnwindBacktrace,
    );
    out.insert(Symbol::intern("_Unwind_GetIP"), ir::Builtin::UnwindGetIp);
    out.insert(
        Symbol::intern("_Unwind_GetIPInfo"),
        ir::Builtin::UnwindGetIpInfo,
    );
    out.insert(
        Symbol::intern("_Unwind_FindEnclosingFunction"),
        ir::Builtin::UnwindFindEnclosing,
    );
    // GetCFA：backtrace 用作帧的 sp 身份（去重/相等）。合成帧无真 CFA，返回该帧
    // synth IP 作唯一 sp 替身（每帧不同即满足身份用途）。
    out.insert(Symbol::intern("_Unwind_GetCFA"), ir::Builtin::UnwindGetIp);
    out.insert(
        Symbol::intern("_Unwind_DeleteException"),
        ir::Builtin::UnwindDeleteException,
    );
    out.insert(
        Symbol::intern("llvm.x86.sse2.pause"),
        ir::Builtin::CpuHintNop,
    );
    out.insert(
        Symbol::intern("llvm.x86.avx.vzeroupper"),
        ir::Builtin::CpuHintNop,
    );
    out.insert(
        Symbol::intern("llvm.x86.addcarry.64"),
        ir::Builtin::AddCarry64,
    );
    out.insert(
        Symbol::intern("llvm.x86.subborrow.64"),
        ir::Builtin::SubBorrow64,
    );
    out.insert(Symbol::intern("llvm.x86.xgetbv"), ir::Builtin::Xgetbv);
    out.insert(
        Symbol::intern("llvm.x86.ssse3.pshuf.b.128"),
        ir::Builtin::X86Pshufb128,
    );
    out.insert(
        Symbol::intern("llvm.x86.avx2.pshuf.b"),
        ir::Builtin::X86Pshufb256,
    );
    out.insert(
        Symbol::intern("llvm.x86.sha256msg1"),
        ir::Builtin::X86Sha256Msg1,
    );
    out.insert(
        Symbol::intern("llvm.x86.sha256msg2"),
        ir::Builtin::X86Sha256Msg2,
    );
    out.insert(
        Symbol::intern("llvm.x86.sha256rnds2"),
        ir::Builtin::X86Sha256Rnds2,
    );
    out.insert(
        Symbol::intern("llvm.x86.sse2.psad.bw"),
        ir::Builtin::X86PsadBw128,
    );
    out.insert(
        Symbol::intern("llvm.x86.avx2.psad.bw"),
        ir::Builtin::X86PsadBw256,
    );
    out.insert(
        Symbol::intern("llvm.x86.pclmulqdq"),
        ir::Builtin::X86Pclmulqdq,
    );
    out.insert(
        Symbol::intern("llvm.x86.aesni.aesenc"),
        ir::Builtin::X86AesEnc,
    );
    out.insert(
        Symbol::intern("llvm.x86.aesni.aesenclast"),
        ir::Builtin::X86AesEncLast,
    );
    out.insert(
        Symbol::intern("llvm.x86.aesni.aesdec"),
        ir::Builtin::X86AesDec,
    );
    out.insert(
        Symbol::intern("llvm.x86.aesni.aesdeclast"),
        ir::Builtin::X86AesDecLast,
    );
    out.insert(
        Symbol::intern("llvm.x86.aesni.aesimc"),
        ir::Builtin::X86AesImc,
    );
    out.insert(
        Symbol::intern("llvm.x86.aesni.aeskeygenassist"),
        ir::Builtin::X86AesKeygenAssist,
    );
    out.insert(
        Symbol::intern("llvm.x86.sse42.crc32.32.8"),
        ir::Builtin::X86Crc32U8,
    );
    out.insert(
        Symbol::intern("llvm.x86.sse42.crc32.32.16"),
        ir::Builtin::X86Crc32U16,
    );
    out.insert(
        Symbol::intern("llvm.x86.sse42.crc32.32.32"),
        ir::Builtin::X86Crc32U32,
    );
    out.insert(
        Symbol::intern("llvm.x86.sse42.crc32.64.64"),
        ir::Builtin::X86Crc32U64,
    );
    out.insert(
        Symbol::intern("llvm.x86.avx2.permd"),
        ir::Builtin::X86Permd256,
    );
    out.insert(
        Symbol::intern("llvm.x86.avx2.gather.q.pd.256"),
        ir::Builtin::X86GatherQPd256,
    );
    out.insert(
        Symbol::intern("llvm.x86.avx2.gather.d.pd.256"),
        ir::Builtin::X86GatherDPd256,
    );
    out.insert(
        Symbol::intern("llvm.x86.avx512.vpmadd52l.uq.128"),
        ir::Builtin::X86Pmadd52Lo128,
    );
    out.insert(
        Symbol::intern("llvm.x86.avx512.vpmadd52h.uq.128"),
        ir::Builtin::X86Pmadd52Hi128,
    );
    out.insert(
        Symbol::intern("llvm.x86.avx512.vpmadd52l.uq.256"),
        ir::Builtin::X86Pmadd52Lo256,
    );
    out.insert(
        Symbol::intern("llvm.x86.avx512.vpmadd52h.uq.256"),
        ir::Builtin::X86Pmadd52Hi256,
    );
    out.insert(
        Symbol::intern("llvm.x86.avx512.vpmadd52l.uq.512"),
        ir::Builtin::X86Pmadd52Lo512,
    );
    out.insert(
        Symbol::intern("llvm.x86.avx512.vpmadd52h.uq.512"),
        ir::Builtin::X86Pmadd52Hi512,
    );
    out.insert(
        Symbol::intern("llvm.x86.ssse3.pmadd.ub.sw.128"),
        ir::Builtin::X86PmaddUbSw128,
    );
    out.insert(
        Symbol::intern("llvm.x86.avx2.pmadd.ub.sw"),
        ir::Builtin::X86PmaddUbSw256,
    );
    out.insert(
        Symbol::intern("llvm.x86.sse2.pmadd.wd"),
        ir::Builtin::X86PmaddWd128,
    );
    out.insert(
        Symbol::intern("llvm.x86.avx2.pmadd.wd"),
        ir::Builtin::X86PmaddWd256,
    );
    out
}

// ===== purity 测量探针（S3′b 裁定前置调研；`MIRVM_PURITY_STATS=1` 门控）=====
// 分类口径（与 deps-image 各方案的切分一一对应）：
// - Local：定义性 DefId 属 LOCAL_CRATE（bin 自身代码，含本地闭包的 shim）
// - Tainted：非本地定义，但泛型参数或 shim 携带类型提及 LOCAL_CRATE——bin 泛型在
//   依赖里的实例化（如 `serde_json::to_string::<Task>`）；纯化聚合方案（A2）归 delta
// - Pure：其余（bin 无关；A2 的 deps-image 候选，含 std/dep 中底座未覆盖者）
// 计时 = 每 instance `lower_instance` 墙钟累加（含嵌套的 alloc 物化）；env 未设时
// 零开销。

#[derive(Default)]
struct PurityStats {
    local: (u64, u128),
    tainted: (u64, u128),
    pure: (u64, u128),
    /// pure 集按 crate 分解（deps-image 内容的来源分布）
    pure_crates: FxHashMap<Symbol, (u64, u128)>,
    /// tainted 实例逐条（符号, ns）——打印 top 用；量小（预期数百）
    tainted_insts: Vec<(Box<str>, u128)>,
}

enum Purity {
    Local,
    Tainted,
    Pure,
}

impl Purity {
    /// image 类 = Pure（bin 无关实例，deps-image 候选）；Local/Tainted = delta 类。
    fn is_image(&self) -> bool {
        matches!(self, Purity::Pure)
    }
}

impl PurityStats {
    fn record(&mut self, tcx: TyCtxt<'_>, inst: Instance<'_>, sym: &str, ns: u128) {
        let (cls, extra) = match classify_purity(inst) {
            Purity::Local => (&mut self.local, None),
            Purity::Tainted => {
                self.tainted_insts.push((sym.into(), ns));
                (&mut self.tainted, None)
            }
            Purity::Pure => {
                let krate = tcx.crate_name(inst.def_id().krate);
                (&mut self.pure, Some(krate))
            }
        };
        cls.0 += 1;
        cls.1 += ns;
        if let Some(krate) = extra {
            let e = self.pure_crates.entry(krate).or_default();
            e.0 += 1;
            e.1 += ns;
        }
    }

    fn dump(&self) {
        fn ms(ns: u128) -> String {
            format!("{:.1}", ns as f64 / 1e6)
        }
        let (l, t, p) = (self.local, self.tainted, self.pure);
        eprintln!("[purity] local:   {} inst, {} ms", l.0, ms(l.1));
        eprintln!("[purity] tainted: {} inst, {} ms", t.0, ms(t.1));
        eprintln!("[purity] pure:    {} inst, {} ms", p.0, ms(p.1));
        eprintln!(
            "[purity] A2 每编辑重降 = local+tainted = {} inst, {} ms（总降低 {} inst, {} ms）",
            l.0 + t.0,
            ms(l.1 + t.1),
            l.0 + t.0 + p.0,
            ms(l.1 + t.1 + p.1)
        );
        let mut crates: Vec<_> = self.pure_crates.iter().collect();
        crates.sort_by_key(|(_, v)| std::cmp::Reverse(v.0));
        for (k, (n, ns)) in crates.iter().take(12) {
            eprintln!("[purity]   pure crate {k}: {n} inst, {} ms", ms(*ns));
        }
        let mut t: Vec<_> = self.tainted_insts.iter().collect();
        t.sort_by_key(|(_, ns)| std::cmp::Reverse(*ns));
        for (sym, ns) in t.iter().take(10) {
            eprintln!("[purity]   tainted top: {} ms  {sym}", ms(*ns));
        }
    }
}

/// instance 的 purity 分类（口径见 PurityStats 头注）。
fn classify_purity(inst: Instance<'_>) -> Purity {
    use rustc_hir::def_id::LOCAL_CRATE;
    use rustc_middle::ty::ShimKind;
    // 定义性 DefId：任一为本地 ⇒ 这是 bin 自己的代码（本地闭包的 ClosureOnce 等）。
    let local_def = match inst.def {
        InstanceKind::Item(d) | InstanceKind::Intrinsic(d) | InstanceKind::Virtual(d, _) => {
            d.krate == LOCAL_CRATE
        }
        InstanceKind::Shim(shim) => match shim {
            ShimKind::VTable(d)
            | ShimKind::Reify(d, _)
            | ShimKind::ThreadLocal(d)
            | ShimKind::FnPtr(d, _)
            | ShimKind::Clone(d, _)
            | ShimKind::FnPtrAddr(d, _)
            | ShimKind::AsyncDropGlueCtor(d, _)
            | ShimKind::AsyncDropGlue(d, _)
            | ShimKind::DropGlue(d, _)
            | ShimKind::FutureDropPoll(d, _, _)
            | ShimKind::ConstructCoroutineInClosure {
                coroutine_closure_def_id: d,
                ..
            } => d.krate == LOCAL_CRATE,
            ShimKind::ClosureOnce {
                call_once, closure, ..
            } => call_once.krate == LOCAL_CRATE || closure.krate == LOCAL_CRATE,
        },
    };
    if local_def {
        return Purity::Local;
    }
    // shim 额外携带的类型（不进 args 的）。
    let shim_tys: &[rustc_middle::ty::Ty<'_>] = match inst.def {
        InstanceKind::Shim(ShimKind::FnPtr(_, t))
        | InstanceKind::Shim(ShimKind::Clone(_, t))
        | InstanceKind::Shim(ShimKind::FnPtrAddr(_, t))
        | InstanceKind::Shim(ShimKind::AsyncDropGlueCtor(_, t))
        | InstanceKind::Shim(ShimKind::AsyncDropGlue(_, t)) => &[t],
        InstanceKind::Shim(ShimKind::FutureDropPoll(_, t1, t2)) => &[t1, t2],
        InstanceKind::Shim(ShimKind::DropGlue(_, Some(t))) => &[t],
        _ => &[],
    };
    let tainted = inst.args.iter().any(|a| a.walk().any(arg_mentions_local))
        || shim_tys.iter().any(|&t| t.walk().any(arg_mentions_local));
    if tainted {
        Purity::Tainted
    } else {
        Purity::Pure
    }
}

/// 顶层提及 LOCAL_CRATE 的 def？（配合 walk() 的深遍历覆盖一切嵌套位）
fn arg_mentions_local(arg: rustc_middle::ty::GenericArg<'_>) -> bool {
    use rustc_hir::def_id::LOCAL_CRATE;
    use rustc_middle::ty::TyKind;
    let Some(t) = arg.as_type() else { return false };
    let did = match t.kind() {
        TyKind::Adt(def, _) => Some(def.did()),
        &TyKind::FnDef(d, _)
        | &TyKind::Closure(d, _)
        | &TyKind::Coroutine(d, _)
        | &TyKind::CoroutineClosure(d, _)
        | &TyKind::CoroutineWitness(d, _)
        | &TyKind::Foreign(d) => Some(d),
        TyKind::Alias(_, at) => Some(match at.kind {
            rustc_middle::ty::AliasTyKind::Projection { def_id }
            | rustc_middle::ty::AliasTyKind::Inherent { def_id }
            | rustc_middle::ty::AliasTyKind::Opaque { def_id }
            | rustc_middle::ty::AliasTyKind::Free { def_id } => def_id,
        }),
        _ => None,
    };
    if did.is_some_and(|d| d.krate == LOCAL_CRATE) {
        return true;
    }
    // walk 不下钻 trait 对象的谓词 DefId（rustc_type_ir walk.rs Dynamic 分支只推 args）
    if let TyKind::Dynamic(preds, ..) = t.kind() {
        return preds
            .principal()
            .is_some_and(|p| p.skip_binder().def_id.krate == LOCAL_CRATE)
            || preds.auto_traits().any(|d| d.krate == LOCAL_CRATE);
    }
    false
}

/// 整程序降低：种子收集 → worklist 闭包降低 → exports 表 + main 启动计划。
/// argv **不在此布置**（M6 片2）：它是运行期输入，由 `Module::finalize_entry_argv`
/// 在每次运行（冷/热同路）于快照语义之后终结化。
/// 底座导出素材（S4：底座构建会话随模块一起产出，程序会话不用）。
pub struct BaseExports {
    pub fn_entry_syms: Vec<(Box<str>, u64)>,
    pub static_syms: Vec<(Box<str>, u64)>,
    pub tls_syms: Vec<(Box<str>, ir::TlsId)>,
}

/// 程序会话降低：base 在场时按 symbol_name 复用底座（fn/static/TLS），
/// 产出 delta 模块（fn/TLS/asm id 从底座计数起编；absorb 合并后运行）。
/// A2（s3b-a2-design，`MIRVM_DEPS_IMAGE=1` 且底座在场时）：split lower——bin
/// 无关实例分轨成 deps-image（SplitImage，样条 k=0 域），delta 只含 bin 附着物。
pub fn lower_program(
    tcx: TyCtxt<'_>,
    stack: &crate::baseimage::ImageStack,
    split: bool,
) -> (ir::Module, Option<SplitImage>) {
    // A2 v1：split 判定（启用/旁路/本会话已装载/底座在场 Q2）由调用方（cli）给出
    let (module, _, split_image) = lower_inner(tcx, stack, FrozenArena::new(), false, false, split);
    (module, split_image)
}

/// 底座构建会话降低（合成空 main）：栈空、冻结区落底座域，导出 sym 索引。
/// 排除 LOCAL_CRATE——合成 crate 的本地项（空 main + shim）符号名带本地
/// disambiguator，不属"sysroot 面"、不与真实程序相撞。
pub fn lower_for_base_build(tcx: TyCtxt<'_>) -> (ir::Module, BaseExports) {
    let empty = crate::baseimage::ImageStack::empty();
    let (module, exports, _) = lower_inner(
        tcx,
        &empty,
        FrozenArena::new_base_image(),
        true,
        true,
        false,
    );
    (module, exports.expect("image 构建模式必有导出素材"))
}

/// 依赖 image 构建会话降低（S3′b）：栈 = 栈下已装 image 链，冻结区落第 k 样条域，
/// 导出 sym 索引。该 crate mono 集减栈下已有 = 本 image 内容（偏移合并同底座）。
/// **不排除 LOCAL_CRATE**——LOCAL_CRATE 正是要成像的依赖 crate 本身，其符号名跨
/// 程序稳定（同版本依赖 = 同符号名，这正是复用的前提）。
pub fn lower_for_image_build(
    tcx: TyCtxt<'_>,
    stack: &crate::baseimage::ImageStack,
    k: usize,
) -> (ir::Module, BaseExports) {
    let (module, exports, _) =
        lower_inner(tcx, stack, FrozenArena::new_image(k), true, false, false);
    (module, exports.expect("image 构建模式必有导出素材"))
}

/// A2 rebase（s3b-a2-design §4.2）：split lower 收尾，把标签/双空间 id 统一成绝对 id。
/// fn/TLS/asm 同构：`TAG|j` → `first + j`；untagged d（≥ first）→ `d + image_count`；
/// 底座 id（< first）不动。触及字段 = 设计 §9 盘点的 6 处 + ids/tls_ids 两表。
struct Rebase {
    first_fn: u32,
    image_fns: u32,
    first_tls: u32,
    image_tls: u32,
    first_asm: u32,
    image_asm: u32,
}

impl Rebase {
    fn fn_id(&self, id: u32) -> u32 {
        if id & IMAGE_TAG != 0 {
            self.first_fn + (id & !IMAGE_TAG)
        } else if id >= self.first_fn {
            id + self.image_fns
        } else {
            id
        }
    }
    fn tls_id(&self, id: u32) -> u32 {
        if id & IMAGE_TAG != 0 {
            self.first_tls + (id & !IMAGE_TAG)
        } else if id >= self.first_tls {
            id + self.image_tls
        } else {
            id
        }
    }
    fn asm_id(&self, id: u32) -> u32 {
        if id & IMAGE_TAG != 0 {
            self.first_asm + (id & !IMAGE_TAG)
        } else if id >= self.first_asm {
            id + self.image_asm
        } else {
            id
        }
    }

    /// 单函数体重映射。op 级字段只有 3 处（设计 §9 实证）：Call.callee /
    /// InlineAsm.stub / Rvalue::TlsRef。**编译期穷尽**（or-pattern 全枚举，新变体
    /// = 非穷尽编译错误——防"新增携带 id 的 op 被遗忘"的静默错值）。
    fn body(&self, b: &mut ir::FuncBody) {
        for block in &mut b.blocks {
            for stmt in &mut block.stmts {
                match stmt {
                    ir::Stmt::Assign { dst: _, rv } => {
                        if let ir::Rvalue::TlsRef(id) = rv {
                            *id = self.tls_id(*id);
                        }
                    }
                    ir::Stmt::AssignOverflow { .. }
                    | ir::Stmt::Copy { .. }
                    | ir::Stmt::RepeatScalar { .. }
                    | ir::Stmt::AtomicStore { .. }
                    | ir::Stmt::VolatileLoad { .. }
                    | ir::Stmt::VolatileStore { .. }
                    | ir::Stmt::AtomicCxchg { .. }
                    | ir::Stmt::AtomicRmw { .. }
                    | ir::Stmt::MemCopy { .. }
                    | ir::Stmt::MemSet { .. }
                    | ir::Stmt::SimdBin { .. }
                    | ir::Stmt::SimdUn { .. }
                    | ir::Stmt::SimdFma { .. }
                    | ir::Stmt::SimdFunnel { .. }
                    | ir::Stmt::SimdCast { .. }
                    | ir::Stmt::SimdSelect { .. }
                    | ir::Stmt::SimdSelectBitmask { .. }
                    | ir::Stmt::SimdGather { .. }
                    | ir::Stmt::SimdScatter { .. }
                    | ir::Stmt::SimdMaskedLoad { .. }
                    | ir::Stmt::SimdMaskedStore { .. }
                    | ir::Stmt::SimdExtractDyn { .. }
                    | ir::Stmt::SimdInsertDyn { .. }
                    | ir::Stmt::SimdArithOffset { .. }
                    | ir::Stmt::SimdSplat { .. }
                    | ir::Stmt::Bin128 { .. }
                    | ir::Stmt::Sat128 { .. }
                    | ir::Stmt::Wide128ToFloat { .. }
                    | ir::Stmt::FloatToWide128 { .. }
                    | ir::Stmt::Bit128 { .. }
                    | ir::Stmt::Bit128Count { .. }
                    | ir::Stmt::F128Bin { .. }
                    | ir::Stmt::F128MathBin { .. }
                    | ir::Stmt::F128Un { .. }
                    | ir::Stmt::F128Fma { .. }
                    | ir::Stmt::F128FromScalar { .. }
                    | ir::Stmt::F128ToScalar { .. }
                    | ir::Stmt::F128FromWideInt { .. }
                    | ir::Stmt::F128ToWideInt { .. }
                    | ir::Stmt::NicheDiscr128 { .. }
                    | ir::Stmt::Trap(_)
                    | ir::Stmt::Nop
                    | ir::Stmt::Fence { .. }
                    | ir::Stmt::RepeatBytes { .. } => {}
                }
            }
            match &mut block.term {
                ir::Terminator::Call { callee, .. } => *callee = self.fn_id(*callee),
                ir::Terminator::InlineAsm { stub, .. } => *stub = self.asm_id(*stub),
                ir::Terminator::Goto(_)
                | ir::Terminator::SwitchInt { .. }
                | ir::Terminator::CallBuiltin { .. }
                | ir::Terminator::CallForeign { .. }
                | ir::Terminator::CallIndirect { .. }
                | ir::Terminator::Return
                | ir::Terminator::Unreachable
                | ir::Terminator::Resume
                | ir::Terminator::TerminateAbort
                | ir::Terminator::Trap(_) => {}
            }
        }
    }
}

/// 降低单个 instance（worklist 循环体）：trap-stub 全覆盖 + purity 探针记账。
fn lower_one<'tcx>(
    tcx: TyCtxt<'tcx>,
    typing_env: TypingEnv<'tcx>,
    linker: &mut Linker<'tcx>,
    purity: &mut Option<PurityStats>,
    inst: Instance<'tcx>,
) -> ir::FuncBody {
    let sym = tcx.symbol_name(inst).name.to_owned();
    let started = purity.as_ref().map(|_| std::time::Instant::now());
    let body = func::lower_instance(tcx, typing_env, inst, linker)
        .unwrap_or_else(|reason| func::trap_body(&sym, &reason));
    if let (Some(p), Some(t0)) = (purity.as_mut(), started) {
        p.record(tcx, inst, &sym, t0.elapsed().as_nanos());
    }
    body
}

fn lower_inner(
    tcx: TyCtxt<'_>,
    stack: &crate::baseimage::ImageStack,
    frozen: FrozenArena,
    emit_exports: bool,
    exclude_local: bool,
    split: bool,
) -> (ir::Module, Option<BaseExports>, Option<SplitImage>) {
    let typing_env = TypingEnv::fully_monomorphized();
    let mut linker = Linker::new(tcx, stack, frozen);
    if split {
        linker.activate_split();
    }

    // 静态归档 / global_asm+naked 的 `.so` 在排干 worklist 前物化并
    // RTLD_NOW|RTLD_GLOBAL 加载：extern fn 被当作值取址（fn-ptr）时，
    // fn_entry_addr 需在降低期 dlsym 其真符号地址（native 链接器语义的直译）。
    // 顺序敏感：reject_symbol_ambiguity 依赖"我方尚未 dlopen"的 RTLD_DEFAULT
    // 状态，故全模块只此一处物化（装配段复用清单，不再二次审计）；运行期
    // FfiState::ensure_libs 的重复 dlopen 是幂等 refcount。失败响亮终止。
    let required_native_libs: Vec<Box<str>> = {
        let mut v = crate::native_archive::materialize_static_libraries(tcx)
            .unwrap_or_else(|reason| panic!("Static native library 装载失败: {reason}"));
        if let Some(so) = global_asm::materialize(tcx)
            .unwrap_or_else(|reason| panic!("global_asm/naked 物化失败: {reason}"))
        {
            v.push(so);
        }
        for so in &v {
            let cpath = std::ffi::CString::new(&**so).expect("原生库路径不含 NUL");
            // dlerror 是线程局部的粘滞状态；先清空，再在失败后立即复制诊断。
            unsafe { libc::dlerror() };
            let h = unsafe { libc::dlopen(cpath.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
            if h.is_null() {
                let err = unsafe { libc::dlerror() };
                let detail = if err.is_null() {
                    "dlerror 未提供详情".to_string()
                } else {
                    unsafe { std::ffi::CStr::from_ptr(err) }
                        .to_string_lossy()
                        .into_owned()
                };
                panic!("必需原生库 `{so}` 降低期 dlopen 失败: {detail}");
            }
            // 句柄有意不 dlclose（与运行期 FfiState 同：随进程生命周期）。
            // hidden 符号 .symtab 兜底表（口径同 FfiState：只收不进 .dynsym 的
            // 符号）。基址或解析失败不建表——dlsym 可见面不受影响，hidden 符号
            // 由取址路径的既有诊断兜底（宁缺勿滥：错基址表会静默解到野地址）。
            if let Some(bias) = crate::elfsym::load_bias(h)
                && let Ok(syms) = crate::elfsym::hidden_symtab_values(so)
            {
                linker.archive_fallbacks.push((bias, syms));
            }
        }
        v
    };

    // 元数据 Dylib 预载（corpus 批5 openssl 实锤）：cargo 把 `-sys` build.rs 的
    // rustc-link-lib 只写 rlib 元数据（bin 的 rustc 命令行无 -l/-L；rustc 链接期
    // 自己从元数据补）。native 语义里这些库恒进最终链接；我们的 fn-ptr 烘焙
    // （降低期 dlsym 全域）与运行期 CallForeign 都需要它们先在全局域可见——std
    // 自带的 m/dl/pthread/rt/util/gcc_s 亦同源（#[link] 属性落在 libstd）。
    // Static 走上方 archive 通道；Framework/wasm 不在本切片。
    let sess = tcx.sess;
    let mut dylib_names: Vec<Box<str>> = Vec::new();
    for cnum in std::iter::once(rustc_hir::def_id::LOCAL_CRATE).chain(tcx.used_crates(()).iter().copied()) {
        if cnum != rustc_hir::def_id::LOCAL_CRATE && tcx.crate_dep_kind(cnum).macros_only() {
            continue;
        }
        for lib in tcx.native_libraries(cnum) {
            // 系统动态链接类（SONAME 预载）= Dylib/RawDylib + Unspecified（bare
            // `-l ssl`，rustc_hir 注释：Dylib 为默认）+ Static{bundle:false}
            // （对象不进 rlib、链接期按系统库解析——libc 的 m/dl/pthread/rt/util
            // 即此形）。Static{bundle:None|Some(true)} 才是整档进 rlib 的真
            // 静态归档（上方 archive 通道）；Framework/LinkArg/Wasm 不在本切片。
            let system_dylib = matches!(
                lib.kind,
                rustc_hir::attrs::NativeLibKind::Dylib { .. }
                    | rustc_hir::attrs::NativeLibKind::RawDylib { .. }
                    | rustc_hir::attrs::NativeLibKind::Unspecified
            ) || matches!(
                lib.kind,
                rustc_hir::attrs::NativeLibKind::Static {
                    bundle: Some(false),
                    ..
                }
            );
            if !system_dylib {
                continue;
            }
            if let Some(cfg) = &lib.cfg
                && !rustc_attr_parsing::eval_config_entry(sess, cfg).as_bool()
            {
                continue;
            }
            let name: Box<str> = lib.name.as_str().into();
            if !dylib_names.contains(&name) {
                dylib_names.push(name);
            }
        }
    }
    for lib in &sess.opts.libs {
        if matches!(lib.kind, rustc_hir::attrs::NativeLibKind::Static { .. }) {
            continue;
        }
        let name: Box<str> = lib.name.as_str().into();
        if !dylib_names.contains(&name) {
            dylib_names.push(name);
        }
    }
    let dylib_candidates = soname_candidates(&dylib_names);
    // 尽力预载（缺失者留待真引用处的既有响亮诊断）；句柄随进程生命周期。
    for cand in &dylib_candidates {
        let Ok(cpath) = std::ffi::CString::new(&**cand) else {
            continue;
        };
        unsafe { libc::dlerror() };
        let h = unsafe { libc::dlopen(cpath.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
        let _ = h; // 有意不 dlclose（与 required 清单同）
    }

    // 种子 = mono collector 集（D1：与 native codegen 同一起点，正确性白拿）
    for inst in collect::collect(tcx) {
        linker.func_id(inst);
    }

    // main 启动计划（cg_ssa create_entry_fn 同构）：
    // lang_start::<main_ret>(main fn-ptr, argc, argv, sigpipe) -> isize
    let entry = tcx.entry_fn(()).map(|(main_def, entry_ty)| {
        let rustc_session::config::EntryFnType::Main { sigpipe } = entry_ty;
        let main_inst = Instance::mono(tcx, main_def);
        // main 是本地 Rust fn，必非 foreign——取址路径不会失败
        let main_addr = linker
            .fn_entry_addr(main_inst)
            .expect("main fn 条目地址（本地 fn，非 foreign）");
        let main_ret = tcx
            .fn_sig(main_def)
            .no_bound_vars()
            .expect("main 无晚绑定区域")
            .output()
            .no_bound_vars()
            .expect("main 返回无晚绑定");
        let start_def = tcx.require_lang_item(rustc_hir::LangItem::Start, rustc_span::DUMMY_SP);
        let start_inst = Instance::expect_resolve(
            tcx,
            typing_env,
            start_def,
            tcx.mk_args(&[main_ret.into()]),
            rustc_span::DUMMY_SP,
        );
        let lang_start = linker.func_id(start_inst);
        // argc/argv 置零占位：finalize_entry_argv 每次运行回填（运行期输入不进快照）
        ir::EntryPlan {
            lang_start,
            main_addr,
            argc: 0,
            argv_ptr: 0,
            sigpipe,
        }
    });

    let mut module = ir::Module::default();
    let mut funcs: Vec<Option<ir::FuncBody>> = Vec::new();
    // S4：delta 模块的 funcs 向量按本地位序存放（absorb 时 base++delta 拼接后，
    // 位置 = delta_first_fn + 本地位序 = 字节码里的绝对 FuncId）
    let first = linker.delta_first_fn;
    let mut purity = std::env::var_os("MIRVM_PURITY_STATS")
        .is_some_and(|v| !v.is_empty())
        .then(PurityStats::default);
    if linker.split.is_some() {
        // A2 split：不动点轮替排干双队列（image 体只发现 image 类——purity 向下
        // 封闭；delta 体两类都发现）。current_image 决定 arena 路由（§4.3）。
        loop {
            let mut progressed = false;
            while let Some((id, inst)) = linker
                .split
                .as_mut()
                .expect("split")
                .image_queue
                .pop_front()
            {
                progressed = true;
                linker.split.as_mut().expect("split").current_image = true;
                let body = lower_one(tcx, typing_env, &mut linker, &mut purity, inst);
                let j = (id & !IMAGE_TAG) as usize;
                linker.split.as_mut().expect("split").image_funcs[j] = Some(body);
                module.exports.insert(tcx.symbol_name(inst).name.into(), id);
            }
            while let Some((id, inst)) = linker.queue.pop_front() {
                progressed = true;
                linker.split.as_mut().expect("split").current_image = false;
                let body = lower_one(tcx, typing_env, &mut linker, &mut purity, inst);
                let slot = (id - first) as usize;
                if funcs.len() <= slot {
                    funcs.resize_with(slot + 1, || None);
                }
                funcs[slot] = Some(body);
                module.exports.insert(tcx.symbol_name(inst).name.into(), id);
            }
            if !progressed {
                break;
            }
        }
    } else {
        while let Some((id, inst)) = linker.queue.pop_front() {
            let body = lower_one(tcx, typing_env, &mut linker, &mut purity, inst);
            let slot = (id - first) as usize;
            if funcs.len() <= slot {
                funcs.resize_with(slot + 1, || None);
            }
            funcs[slot] = Some(body);
            module.exports.insert(tcx.symbol_name(inst).name.into(), id);
        }
    }
    if let Some(p) = &purity {
        p.dump();
    }

    // ===== A2 split：rebase + 双模块装配 =====
    let mut split_image = None;
    if let Some(mut s) = linker.split.take() {
        let image_fns = s.image_funcs.len() as u32;
        let image_tls = s.image_tls_slots.len() as u32;
        let image_asm = s.image_asm_sites.len() as u32;
        let rb = Rebase {
            first_fn: first,
            image_fns,
            first_tls: linker.delta_first_tls,
            image_tls,
            first_asm: linker.delta_first_asm,
            image_asm,
        };
        // A2 自检（§5.3②）：image 实例逐条复查 purity——任何漏判都是错值级
        for inst in &s.image_insts {
            assert!(
                classify_purity(*inst).is_image(),
                "A2 自检失败：image 实例复查非 pure（分类器状态错误）"
            );
        }
        // 函数体 + 两表 + entry plan 重映射
        for b in s.image_funcs.iter_mut().flatten() {
            rb.body(b);
        }
        for b in funcs.iter_mut().flatten() {
            rb.body(b);
        }
        for v in module.exports.values_mut() {
            *v = rb.fn_id(*v);
        }
        for v in linker.fn_addrs.values_mut() {
            *v = rb.fn_id(*v);
        }
        for v in linker.ids.values_mut() {
            *v = rb.fn_id(*v);
        }
        for v in linker.tls_ids.values_mut() {
            *v = rb.tls_id(*v);
        }
        let mut entry = entry;
        if let Some(e) = entry.as_mut() {
            e.lang_start = rb.fn_id(e.lang_start);
        }
        let entry = entry;
        module.entry = entry;

        // exports 按值域分拆（设计 §9：底座 id < first 恒留 delta 侧）
        let image_lo = first;
        let image_hi = first + image_fns;
        let in_image = |id: &ir::FuncId| *id >= image_lo && *id < image_hi;
        let image_exports: std::collections::HashMap<Box<str>, ir::FuncId> = module
            .exports
            .iter()
            .filter(|(_, id)| in_image(id))
            .map(|(s, id)| (s.clone(), *id))
            .collect();
        module.exports.retain(|_, id| !in_image(id));
        // fn_addrs 按【地址域】分拆：条目物理上在 image 冻结区（image_fn_entries 的
        // 值集 = image 类 + S4 补建的全部条目）就随 image 走。按值域分会把补建条目
        // （底座值域 < first、条目在 image 区）留在建者 delta 侧——消费方装载该
        // image 后，其静态里烘焙的补建地址在运行期反查表无登记（absorb_stack 只并
        // image.fn_addrs，消费方自己的 fn_entry_addr 复用分支也不登记），间接调用
        // abort「不是已知 fn 条目」（corpus 批1 撞出的缓存污染实锤根因；负对照
        // edit_rand v2-v6 五连崩 0x6a0000001630/core::fmt::write）。
        let image_entry_addrs: std::collections::HashSet<u64> =
            s.image_fn_entries.values().copied().collect();
        let image_fn_addrs: std::collections::HashMap<u64, ir::FuncId> = linker
            .fn_addrs
            .iter()
            .filter(|(a, _)| image_entry_addrs.contains(a))
            .map(|(a, id)| (*a, *id))
            .collect();
        module.fn_addrs = linker
            .fn_addrs
            .iter()
            .filter(|(a, _)| !image_entry_addrs.contains(a))
            .map(|(a, id)| (*a, *id))
            .collect();
        let image_module = ir::Module {
            exports: image_exports,
            fn_addrs: image_fn_addrs,
            funcs: s
                .image_funcs
                .into_iter()
                .map(|f| f.expect("image 队列耗尽时每个 id 必有产出"))
                .collect(),
            tls: s.image_tls_slots,
            asm_sites: s.image_asm_sites,
            frozen: Some(s.image_frozen),
            // 会话级单表（depsimage 判据③的语义本意）：image 上下文的 extern
            // static/fn 取址会把宿主地址烤进 image 字节码/冻结区（purity 分类器
            // 看不见裸地址）——非空即不写盘，防跨进程回放野指针；内存态上栈
            // 同进程有效，由 absorb 合并回 delta 保 L2 诚实。
            foreign_static_syms: linker.foreign_static_syms.clone(),
            ..Default::default()
        };
        // image 导出素材（装载方零 tcx 依赖，BaseExports 同构）：fn 条目/static/TLS
        // 三索引只含 image 类。fn 条目以 image 区条目表为准（含底座命中但在 image
        // 区补建者——"总量恰一份"的单一身份在装载端可复现）。
        let fn_entry_syms = s
            .image_fn_entries
            .iter()
            .map(|(inst, &addr)| (Box::from(tcx.symbol_name(*inst).name), addr))
            .collect();
        let static_syms = linker
            .static_defs
            .iter()
            .filter(|(def_id, _)| def_id.krate != rustc_hir::def_id::LOCAL_CRATE)
            .map(|&(def_id, addr)| {
                let sym = tcx.symbol_name(Instance::mono(tcx, def_id)).name;
                (Box::from(sym), addr)
            })
            .collect();
        let tls_first = rb.first_tls;
        let tls_syms = linker
            .tls_ids
            .iter()
            .filter(|(_, id)| **id >= tls_first && **id < tls_first + image_tls)
            .map(|(&def_id, &id)| {
                let sym = tcx.symbol_name(Instance::mono(tcx, def_id)).name;
                (Box::from(sym), id)
            })
            .collect();
        split_image = Some(SplitImage {
            module: image_module,
            fn_entry_syms,
            static_syms,
            tls_syms,
        });
        // delta 侧 tls_slots/asm_sites 本就只有 delta 槽（image 槽在 Split 字段里，
        // 已随 SplitImage 移出），无需再动。
    }
    module.funcs = funcs
        .into_iter()
        .map(|f| f.expect("队列耗尽时每个 FuncId 必有产出"))
        .collect();

    // 入口别名（--vm-stats 从程序入口做可达分析用）
    if let Some((entry_def, _)) = tcx.entry_fn(())
        && let Some(&id) = linker.ids.get(&Instance::mono(tcx, entry_def))
    {
        module.exports.insert("@entry".into(), id);
    }
    // dylib dlopen 候选（运行期 FfiState ensure_libs 的 optional 类）：与降低期
    // 预载同一清单（元数据 + CLI 合并、ldconfig 扩展的版本项）——one build 口径。
    module.native_libs = dylib_candidates.clone();
    // CLI `-l` 额外补 search-path 限定形态（tier-0 旧契约保留；Static 只走上方
    // 经过验证的必需 archive 路径，不能伪装成可选 `.so` 候选）。
    for lib in &sess.opts.libs {
        if matches!(lib.kind, rustc_hir::attrs::NativeLibKind::Static { .. }) {
            continue;
        }
        let name = lib.name.as_str();
        for d in sess.opts.search_paths.iter().map(|sp| &sp.dir) {
            let p: Box<str> = d.join(format!("lib{name}.so")).display().to_string().into();
            if !module.native_libs.contains(&p) {
                module.native_libs.push(p);
            }
        }
    }
    // 上游 crate build.rs 的 Static native libraries（M5.1 D2）+ global_asm/naked
    // 物化（M5.2 D8h）：清单已在排干 worklist 前物化并 RTLD_GLOBAL 加载
    // （fn-ptr 取址的降低期 dlsym 依赖；单点物化保 reject_symbol_ambiguity 的
    // "尚未 dlopen" 前提），此处只移交 Module。
    module.required_native_libs = required_native_libs;
    // asm-stub 批量物化（M5.0）：全部 wrapper cc 汇编 + dlopen + dlsym → 真地址表。
    // 配方留在 Module（M6 片2）：L2 warm 路径以 asm_sites 幂等重物化。
    // A2 split：image 站点随 SplitImage 走（absorb 时合并重物化，与 L2 warm 同契约）。
    module.asm_sites = std::mem::take(&mut linker.asm_sites);
    module.asm_stub_addrs = asm::materialize(&module.asm_sites);
    // S4 底座导出素材（构建模式）：sym 索引在此一次算清，装载方零 tcx 依赖。
    // 合成 crate 的本地项（空 main 及其 shim）不入索引——其符号名带本地
    // disambiguator 不会与真实程序相撞，但索引的语义是"sysroot 面"，如实排除。
    let base_exports = emit_exports.then(|| {
        use rustc_hir::def_id::LOCAL_CRATE;
        let keep = |krate| !exclude_local || krate != LOCAL_CRATE;
        BaseExports {
            fn_entry_syms: linker
                .fn_entries
                .iter()
                .filter(|(inst, _)| keep(inst.def_id().krate))
                .map(|(inst, &addr)| (Box::from(tcx.symbol_name(*inst).name), addr))
                .collect(),
            static_syms: linker
                .static_defs
                .iter()
                .filter(|(def_id, _)| keep(def_id.krate))
                .map(|&(def_id, addr)| {
                    let sym = tcx.symbol_name(Instance::mono(tcx, def_id)).name;
                    (Box::from(sym), addr)
                })
                .collect(),
            tls_syms: linker
                .tls_ids
                .iter()
                .filter(|(def_id, _)| keep(def_id.krate))
                .map(|(&def_id, &id)| {
                    let sym = tcx.symbol_name(Instance::mono(tcx, def_id)).name;
                    (Box::from(sym), id)
                })
                .collect(),
        }
    });

    // 冻结区与 fn 条目反查表移交执行相
    module.frozen = Some(linker.frozen);
    if split_image.is_none() {
        module.fn_addrs = linker.fn_addrs.into_iter().collect();
    }
    module.tls = linker.tls_slots;
    module.foreign_static_syms = linker.foreign_static_syms;
    if split_image.is_none() {
        module.entry = entry;
    }
    (module, base_exports, split_image)
}

#[cfg(test)]
mod tests {
    use super::{IMAGE_TAG, Rebase};
    use crate::vm::engine::ir;

    /// first=100、image 5 个的 rebase 基准（fn/TLS/asm 各自独立空间同构）
    fn rb() -> Rebase {
        Rebase {
            first_fn: 100,
            image_fns: 5,
            first_tls: 20,
            image_tls: 3,
            first_asm: 7,
            image_asm: 2,
        }
    }

    #[test]
    fn rebase_fn_id_three_ranges() {
        let rb = rb();
        // 底座 id（< first）不动
        assert_eq!(rb.fn_id(0), 0);
        assert_eq!(rb.fn_id(99), 99);
        // delta untagged（≥ first）统一 +image_fns
        assert_eq!(rb.fn_id(100), 105);
        assert_eq!(rb.fn_id(137), 142);
        // image 标签（TAG|j）→ first + j
        assert_eq!(rb.fn_id(IMAGE_TAG | 0), 100);
        assert_eq!(rb.fn_id(IMAGE_TAG | 4), 104);
        // 三空间同构：TLS/ASM 同形（各自 first/count）
        assert_eq!(rb.tls_id(19), 19);
        assert_eq!(rb.tls_id(20), 23);
        assert_eq!(rb.tls_id(IMAGE_TAG | 2), 22);
        assert_eq!(rb.asm_id(6), 6);
        assert_eq!(rb.asm_id(7), 9);
        assert_eq!(rb.asm_id(IMAGE_TAG | 1), 8);
        // 标签位绝不残留进执行相
        for id in [0, 99, 100, 137, IMAGE_TAG | 0, IMAGE_TAG | 4] {
            assert_eq!(rb.fn_id(id) & IMAGE_TAG, 0);
        }
    }

    /// 构造最小 body：一个 block，term 任选，返回后可加 stmt
    fn body_with(term: ir::Terminator, stmts: Vec<ir::Stmt>) -> ir::FuncBody {
        ir::FuncBody {
            frame_size: 0,
            frame_align: 1,
            ret: ir::RetAbi::Zst,
            params: Vec::new(),
            caller_loc_off: None,
            blocks: vec![ir::Block { stmts, term }],
            name: "t".into(),
        }
    }

    #[test]
    fn rebase_body_remaps_only_id_carrying_ops() {
        let rb = rb();
        // Call.callee：三区间各自重映射
        let mut b = body_with(
            ir::Terminator::Call {
                callee: IMAGE_TAG | 3,
                args: vec![],
                ret: ir::RetDest::Ignore,
                target: 0,
                unwind: ir::UnwindAction::Continue,
            },
            vec![ir::Stmt::Assign {
                dst: ir::ScalarPlace::Slot(ir::Slot {
                    off: 0,
                    width: ir::Width::W64,
                }),
                rv: ir::Rvalue::TlsRef(IMAGE_TAG | 1),
            }],
        );
        rb.body(&mut b);
        let ir::Terminator::Call { callee, .. } = &b.blocks[0].term else {
            panic!("Call 不变体");
        };
        assert_eq!(*callee, 103);
        let ir::Stmt::Assign {
            rv: ir::Rvalue::TlsRef(id),
            ..
        } = &b.blocks[0].stmts[0]
        else {
            panic!("TlsRef 不变体");
        };
        assert_eq!(*id, 21);

        // InlineAsm.stub 重映射；CallIndirect（无 id 字段）与其他语句不动
        let mut b2 = body_with(
            ir::Terminator::InlineAsm {
                stub: 8,
                buf_size: 0,
                ins: vec![],
                outs: vec![],
                target: 0,
            },
            vec![ir::Stmt::Nop],
        );
        rb.body(&mut b2);
        let ir::Terminator::InlineAsm { stub, .. } = &b2.blocks[0].term else {
            panic!("InlineAsm 不变体");
        };
        assert_eq!(*stub, 10); // untagged ≥ first_asm(7) → +image_asm(2)

        // CallBuiltin / Trap / Goto 等不携带 id 的终止子保持原样
        let mut b3 = body_with(
            ir::Terminator::CallBuiltin {
                builtin: ir::Builtin::HostAbort,
                args: vec![],
                ret: ir::RetDest::Ignore,
                target: 0,
                unwind: ir::UnwindAction::Continue,
            },
            vec![],
        );
        rb.body(&mut b3);
        assert!(matches!(
            b3.blocks[0].term,
            ir::Terminator::CallBuiltin {
                builtin: ir::Builtin::HostAbort,
                ..
            }
        ));
    }
}

/// dylib dlopen 候选 SONAME 清单（按序去重）：dev 符号链 `lib{name}.so` →
/// `ldconfig -p` 的版本项绝对路径（`lib{name}.so.N` 带点锚前缀，libssl.so.3 类；
/// ldconfig 缺失/无命中则仅靠符号链）。cargo 的 rlib 元数据 -l 与 CLI -l 共用。
fn soname_candidates(names: &[Box<str>]) -> Vec<Box<str>> {
    let mut out: Vec<Box<str>> = Vec::new();
    let mut push = |c: String| {
        let c: Box<str> = c.into();
        if !out.contains(&c) {
            out.push(c);
        }
    };
    let ldconfig = std::process::Command::new("ldconfig")
        .arg("-p")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
    for name in names {
        push(format!("lib{name}.so"));
        let prefix = format!("lib{name}.so.");
        if let Some(db) = &ldconfig {
            for line in db.lines() {
                let Some((soname, path)) = line.rsplit_once(" => ") else {
                    continue;
                };
                if soname.trim_start().starts_with(&prefix) {
                    push(path.trim().to_string());
                }
            }
        }
    }
    out
}
