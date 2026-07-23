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
pub(crate) struct Split<'tcx> {
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
    /// P2 GOT（decision-history §7.5c）image 侧三表：符号表/去重/修补点（槽开在
    /// image_frozen；收尾随 image 模块走，absorb 时按名合流进 delta 并重编 idx）
    image_got_syms: Vec<ir::GotSym>,
    image_got_idx: FxHashMap<Box<str>, u32>,
    image_got_fixups: Vec<ir::GotFixup>,
    /// P1（§7.6）image 侧 stub 代码区与配方表（image 类实例的可执行条目恒在
    /// image 域——跨运行稳定域，与 fn 条目同域纪律；收尾随 image 模块走）
    image_code_arena: crate::vm::engine::codearena::StubArena,
    image_stub_sites: Vec<ir::EntryStubSite>,
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
pub(crate) mod linker;
use linker::Linker;
mod builtins;
pub(crate) mod ffi_sig;
mod purity;
mod rebase;
use builtins::engine_builtins;
pub(crate) use ffi_sig::{canonical_link_name, ffi_kind_of, freeze_c_fnptr_sig};
use purity::{PurityStats, classify_purity};
use rebase::Rebase;

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
///
/// 预留口（2026-07-19 用户裁定保留）：全仓当前零调用方——依赖 image 的构建侧
/// 尚未接线（S3′b 只兑现了装载侧 A2 聚合）。未来依赖 image 构建线重启时启用；
/// 勿因「无调用方」再提删除。
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
    // P1（§7.6）：本域 stub 代码区与冻结区同 k 域（frozen.home() 记意向域，
    // 动态回退下推导仍一致；各自的固定基/回退独立判定，缓存门槛两用其判）
    let code_home = crate::vm::engine::addrlayout::code_home_for_frozen(frozen.home())
        .expect("P1：冻结域非法，stub 代码域不可推");
    let mut linker = Linker::new(
        tcx,
        stack,
        frozen,
        crate::vm::engine::codearena::StubArena::new_at(code_home),
    );
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
        let mut v = crate::native_archive::materialize_static_libraries(tcx, &mut linker)
            .unwrap_or_else(|reason| panic!("Static native library 装载失败: {reason}"));
        // C4（decision-history §7.22）：dep crate 的 global_asm 清单（dep 编译期
        // 自 HIR 抽取的 `.mirasm.s` 文本，rlib 旁挂）——按 crate 图序经同一
        // assemble 通道物化装载；pulp LD_ST 表类符号经此进全局域
        for cnum in tcx.used_crates(()) {
            if tcx.crate_dep_kind(*cnum).macros_only() {
                continue;
            }
            for p in tcx.used_crate_source(*cnum).paths() {
                let Some(stem) = p.to_str().and_then(|s| s.strip_suffix(".rlib")) else {
                    continue;
                };
                let manifest = std::path::PathBuf::from(format!("{stem}.mirasm.s"));
                if !manifest.is_file() {
                    continue;
                }
                let text = std::fs::read_to_string(&manifest).unwrap_or_else(|e| {
                    panic!("dep global_asm 清单 `{}` 读取失败: {e}", manifest.display())
                });
                let so = global_asm::assemble(&text).unwrap_or_else(|reason| {
                    panic!(
                        "dep global_asm 清单 `{}` 物化失败: {reason}",
                        manifest.display()
                    )
                });
                v.push(so);
            }
        }
        if let Some(so) = global_asm::materialize(tcx, &mut linker)
            .unwrap_or_else(|reason| panic!("global_asm/naked 物化失败: {reason}"))
        {
            v.push(so);
        }
        for so in &v {
            let cpath = std::ffi::CString::new(&**so).expect("原生库路径不含 NUL");
            let h = crate::os::dll::open(&cpath, crate::os::dll::Mode::Now)
                .unwrap_or_else(|detail| panic!("必需原生库 `{so}` 降低期 dlopen 失败: {detail}"));
            // 句柄有意不 dlclose（与运行期 FfiState 同：随进程生命周期）。
            // 记 required 句柄（dynsym 可见符号的链接序解析，先于全域——
            // native 链接期绑定，psm/rustc_driver 碰撞实锤）。
            linker.archive_handles.push(h);
            // T5：global_asm 物化的 .so 若含 syscall 间接槽则当场重填
            // （系统库无此符号，静默跳过）
            crate::lower::asm::refill_syscall_slot(h);
            // hidden 符号 .symtab 兜底表（口径同 FfiState：只收不进 .dynsym 的
            // 符号）。基址或解析失败不建表——dlsym 可见面不受影响，hidden 符号
            // 由取址路径的既有诊断兜底（宁缺勿滥：错基址表会静默解到野地址）。
            if let Some(bias) = crate::os::dll::load_bias(h)
                && let Ok(syms) = crate::elfsym::hidden_symtab_values(so)
            {
                linker.archive_fallbacks.push((bias as u64, syms));
            }
        }
        v
    };

    let sess = tcx.sess;
    // 元数据 Dylib 预载（corpus 批5 openssl 实锤）：cargo 把 `-sys` build.rs 的
    // rustc-link-lib 只写 rlib 元数据（bin 的 rustc 命令行无 -l/-L；rustc 链接期
    // 自己从元数据补）。native 语义里这些库恒进最终链接；我们的 fn-ptr 烘焙
    // （降低期 dlsym 全域）与运行期 CallForeign 都需要它们先在全局域可见——std
    // 自带的 m/dl/pthread/rt/util/gcc_s 亦同源（#[link] 属性落在 libstd）。
    // Static 走上方 archive 通道；Framework/wasm 不在本切片。
    // 收集口径与 native_archive 闭包链接行共用（c_libgit2 修复，system_dylibs）。
    let dylib_names = crate::native_archive::system_dylibs(tcx);
    let dylib_candidates = soname_candidates(&dylib_names);
    // 尽力预载（缺失者留待真引用处的既有响亮诊断）；句柄随进程生命周期。
    for cand in &dylib_candidates {
        let Ok(cpath) = std::ffi::CString::new(&**cand) else {
            continue;
        };
        let _ = crate::os::dll::open(&cpath, crate::os::dll::Mode::Now);
    }

    // 种子 = mono collector 集（D1：与 native codegen 同一起点，正确性白拿）
    for inst in collect::collect(tcx) {
        linker.func_id(inst);
    }

    // 自定义 #[global_allocator] 的 __rust_* shim（corpus 批7 c_mimalloc 实锤修）：
    // kind=Global 时 HIR 展开器已在本地 crate 生成 __rust_{alloc,dealloc,realloc,
    // alloc_zeroed} 四只转发 fn（rustc_allocator 等 flag 标记，body = 调用户
    // GlobalAlloc）——登记 FuncId 供运行期 interp CallBuiltin(Rust*) 臂统一路由
    // （分配是程序级语义：base/deps image 按 Default 会话烘的臂与用户分配器
    // 不得并存，跨堆 free = mimalloc 元数据 SIGSEGV）。
    let mut custom_alloc_shims: Option<ir::AllocShims> = if let Some(kind) = tcx.allocator_kind(())
        && matches!(kind, rustc_ast::expand::allocator::AllocatorKind::Global)
    {
        use rustc_middle::middle::codegen_fn_attrs::CodegenFnAttrFlags as F;
        let mut found: [Option<ir::FuncId>; 4] = [None; 4];
        for def_id in tcx.hir_crate_items(()).definitions() {
            if tcx.def_kind(def_id) != rustc_hir::def::DefKind::Fn
                || !tcx.def_kind(def_id).has_codegen_attrs()
            {
                continue;
            }
            let flags = tcx.codegen_fn_attrs(def_id).flags;
            for (f, i) in [
                (F::ALLOCATOR, 0),
                (F::DEALLOCATOR, 1),
                (F::REALLOCATOR, 2),
                (F::ALLOCATOR_ZEROED, 3),
            ] {
                if flags.contains(f) {
                    found[i] = Some(linker.func_id(Instance::mono(tcx, def_id.to_def_id())));
                }
            }
        }
        match found {
            [
                Some(alloc),
                Some(dealloc),
                Some(realloc),
                Some(alloc_zeroed),
            ] => Some(ir::AllocShims {
                alloc,
                dealloc,
                realloc,
                alloc_zeroed,
            }),
            // 四件不齐 = 生成面不完整（不应发生；None 落引擎堆既有纪律）
            _ => None,
        }
    } else {
        None
    };

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
        // P1 配方表的 FuncId 同规则重映射（s.image_stub_sites 在下方装配前）
        for site in linker.entry_stub_sites.iter_mut() {
            site.func = rb.fn_id(site.func);
        }
        for site in s.image_stub_sites.iter_mut() {
            site.func = rb.fn_id(site.func);
        }
        // 自定义分配器 shim 的 FuncId 同规则重映射（c_mimalloc ABI 错调根因：
        // 漏映射则运行期路由到移位前的野 FuncId，call_guest 打错函数体）
        if let Some(shims) = custom_alloc_shims.as_mut() {
            shims.alloc = rb.fn_id(shims.alloc);
            shims.dealloc = rb.fn_id(shims.dealloc);
            shims.realloc = rb.fn_id(shims.realloc);
            shims.alloc_zeroed = rb.fn_id(shims.alloc_zeroed);
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
            // P2 GOT image 侧（decision-history §7.5c）：随 image 模块走，
            // 装载/absorb 时按名合流进 delta 并重编 idx
            foreign_syms: s.image_got_syms,
            got_fixups: s.image_got_fixups,
            // P1 image 侧（§7.6）：配方随 image 文件，代码域句柄运行期随域重建
            entry_stub_sites: s.image_stub_sites,
            entry_stubs: s.image_code_arena,
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
    // P2 GOT（decision-history §7.5c）delta 侧（image 侧已随 split_image 走）
    module.foreign_syms = linker.got_syms;
    module.got_fixups = linker.got_fixups;
    // P1（§7.6）本域配方与代码域句柄（image 侧已随 split_image 走）
    module.entry_stub_sites = linker.entry_stub_sites;
    module.entry_stubs = linker.code_arena;
    // 自定义分配器 shim（程序级语义，delta 权威：shim 恒 LOCAL_CRATE——split
    // 与否同此一处，base/deps image 按 Default 烘的臂在运行期经它路由）
    module.custom_alloc_shims = custom_alloc_shims;
    if split_image.is_none() {
        module.entry = entry;
    }
    (module, base_exports, split_image)
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
        assert_eq!(rb.fn_id(IMAGE_TAG), 100);
        assert_eq!(rb.fn_id(IMAGE_TAG | 4), 104);
        // 三空间同构：TLS/ASM 同形（各自 first/count）
        assert_eq!(rb.tls_id(19), 19);
        assert_eq!(rb.tls_id(20), 23);
        assert_eq!(rb.tls_id(IMAGE_TAG | 2), 22);
        assert_eq!(rb.asm_id(6), 6);
        assert_eq!(rb.asm_id(7), 9);
        assert_eq!(rb.asm_id(IMAGE_TAG | 1), 8);
        // 标签位绝不残留进执行相
        for id in [0, 99, 100, 137, IMAGE_TAG, IMAGE_TAG | 4] {
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
