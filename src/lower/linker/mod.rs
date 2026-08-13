//! Linker（自 lower/mod.rs M4-M5 整搬）：降低期链接器——FuncId 去重集与
//! 待降低队列、冻结区物化、P1 fn 条目、P2 GOT/foreign 槽、FFI 签名、A2
//! split 状态。结构与新构造在 mod.rs；impl 子块按 recon 字段分组带 =
//! entries(P1 条目+FFI 签名)/alloc(冻结区物化)/got(GOT·foreign 槽)/calls(调用解析)。

mod alloc;
mod calls;
mod entries;
mod got;

use super::*;

pub(crate) struct Linker<'tcx> {
    pub(crate) tcx: TyCtxt<'tcx>,
    /// instance → FuncId（去重集；含已降与在队的）
    pub(crate) ids: FxHashMap<Instance<'tcx>, ir::FuncId>,
    /// 待降低队列（FuncId 已分配，体未产出）——split 模式下为 **delta 类**队列
    pub(crate) queue: VecDeque<(ir::FuncId, Instance<'tcx>)>,
    /// A2 split 状态（None = 非 split 路径，行为与 S4/S3′a 完全一致）
    pub(crate) split: Option<Split<'tcx>>,
    /// ①引擎原语表：mangled 符号 → Builtin
    pub(crate) builtins: FxHashMap<Symbol, ir::Builtin>,
    /// ②链接仿真：导出符号名 → (定义 instance, is_weak)（strong 覆盖 weak；惰性构建）
    pub(crate) exports: Option<FxHashMap<Symbol, (Instance<'tcx>, bool)>>,
    /// 冻结区（statics/常量池/fn 条目）——lower 期物化，结束移交 Module
    pub(crate) frozen: FrozenArena,
    /// 已物化的 alloc → 冻结区真地址（去重 + 先分后填破指针环）
    pub(crate) alloc_addrs: FxHashMap<AllocId, u64>,
    /// fn-ptr 条目：instance → 条目真地址（D4 每 instance 一个真地址身份）
    pub(crate) fn_entries: FxHashMap<Instance<'tcx>, u64>,
    /// 反查：条目真地址 → FuncId（间接调用派发用，移交 Module）
    pub(crate) fn_addrs: FxHashMap<u64, ir::FuncId>,
    /// guest TLS：`#[thread_local]` static → 稠密 TlsId + 槽表（M4.4 D3，移交 Module）
    pub(crate) tls_ids: FxHashMap<rustc_hir::def_id::DefId, ir::TlsId>,
    pub(crate) tls_slots: Vec<ir::TlsSlot>,
    /// asm-stub wrapper 文本（M5.0）：AsmStubId → 符号名+GAS 源；lower 结束批量
    /// cc+dlopen 物化。A2 起名字与位序解耦（split 模式最终位序收尾才知）。
    pub(crate) asm_sites: Vec<ir::AsmSite>,
    /// extern fn 被当作值取址（fn-ptr）的条目：instance → dlsym 真地址（D4 条目
    /// 语义的外延）。不进 fn_addrs——执行相反查未命中正是 CallIndirect 的
    /// native_sig libffi 直调通道（M4.4 FFI 反方向之二）的触发条件。
    pub(crate) foreign_fn_entries: FxHashMap<Instance<'tcx>, u64>,
    /// P2 GOT（decision-history §7.5c）delta 侧三表（image 侧在 Split）
    pub(crate) got_syms: Vec<ir::GotSym>,
    pub(crate) got_idx: FxHashMap<Box<str>, u32>,
    pub(crate) got_fixups: Vec<ir::GotFixup>,
    pub(crate) frozen_relocs: Vec<ir::FrozenReloc>,
    /// foreign 分配 → (符号名, weak)：非 weak extern static 与 extern fn 取址两类
    ///（weak extern static 走 foreign_slot 直道不登记本表——E27，2026-07-18）；
    /// 重定位/常量发码经它把"烤值"转"槽位"。
    pub(crate) foreign_alloc_sym: FxHashMap<AllocId, (Box<str>, bool)>,
    /// (符号名, image 上下文) → GOT 槽真地址（槽 = 本侧冻结区普通 8 字节格）
    pub(crate) foreign_slots: std::collections::HashMap<(Box<str>, bool), u64>,
    /// P1 条目可执行化（decision-history §7.6）本域 stub 代码区与配方表（image
    /// 侧在 Split）：instance → stub idx（域由实例类定，与 fn 条目同纪律）
    pub(crate) code_arena: crate::vm::engine::codearena::StubArena,
    pub(crate) entry_stub_sites: Vec<ir::EntryStubSite>,
    pub(crate) entry_stub_ids: FxHashMap<Instance<'tcx>, u32>,
    /// FFI 可派生性缓存（freeze_c_fnptr_sig；None = 保持数据槽条目——Rust ABI /
    /// 聚合按值 / 变参无 native 合法调用面，无盲区内损失）
    pub(crate) entry_sig_cache: FxHashMap<Instance<'tcx>, Option<ir::ForeignSig>>,
    /// 必需归档库的 hidden 符号兜底表（装载基址, 符号→st_value）：只收不进
    /// .dynsym 的符号（-fvisibility=hidden 归档，ring/zstd-sys 一族）；extern
    /// static/fn 取址的降低期解析**先于 dlsym 全域**——native 链接期绑定语义
    /// （归档内定义恒胜全局命名空间；宿主 libLLVM 内嵌 ZSTD_* 静默截胡的实锤，
    /// 见 elfsym 模块头注）。lower_inner 头部随 dlopen 一并构建。
    pub(crate) archive_fallbacks: Vec<(u64, std::collections::HashMap<Box<str>, u64>)>,
    /// 必需归档/系统库的 dlopen 句柄（required_native_libs + dylib_candidates 序
    /// = 链接序同构）：其 .dynsym 可见符号的降低期解析**先于 dlsym 全域**——
    /// native 链接期绑定（guest 自己的对象恒胜宿主进程同名库；psm 的
    /// rust_psm_on_stack vs 宿主 librustc_driver 内嵌副本实锤，corpus 批6
    /// c_polars_frame）。句柄不随进程关闭（与运行期 FfiState 同）。
    pub(crate) archive_handles: Vec<usize>,
    // ===== S4 底座（s4-base-image-design；偏移合并）=====
    /// sym → 底座 FuncId（命中即复用，不入队）。空表 = 无底座/底座构建模式。
    pub(crate) base_fns: FxHashMap<Box<str>, ir::FuncId>,
    /// sym → 底座 fn 条目真地址（仅被取址过的）
    pub(crate) base_fn_entries: FxHashMap<Box<str>, u64>,
    /// sym → 底座 static 真地址（双份物化 = static mut 精神分裂，必须去重）
    pub(crate) base_statics: FxHashMap<Box<str>, u64>,
    /// sym → 底座 TlsId（线程局部身份同理必须去重）
    pub(crate) base_tls: FxHashMap<Box<str>, ir::TlsId>,
    /// delta 的 fn/TLS/asm id 起点 = 底座各表长度（absorb 时 base++delta 拼单表）
    pub(crate) delta_first_fn: ir::FuncId,
    pub(crate) delta_first_tls: ir::TlsId,
    pub(crate) delta_first_asm: ir::AsmStubId,
    /// 下一个待分配 FuncId（不能再用 ids.len()：底座命中也占 ids 条目）
    pub(crate) next_fn: ir::FuncId,
    /// 底座导出素材：本会话物化的非 foreign static（DefId, 冻结区地址）
    pub(crate) static_defs: Vec<(rustc_hir::def_id::DefId, u64)>,
    /// 固定 std 启动链中，外层调用边界和最终执行捕获的 intrinsic 调用点。
    pub(crate) main_catch_site: Option<MainCatchSite<'tcx>>,
}

#[derive(Clone, Copy)]
pub(crate) struct MainCatchSite<'tcx> {
    pub(crate) boundary_caller: Instance<'tcx>,
    pub(crate) boundary_callee: ir::FuncId,
    pub(crate) catcher_caller: Instance<'tcx>,
    pub(crate) catcher_intrinsic: Instance<'tcx>,
}

impl<'tcx> Linker<'tcx> {
    /// S3′a：base-maps = image 栈的并集查找；delta 起编 = 栈累积量。frozen 由调用方
    /// 按目标域构造（程序 delta = new()、底座 = new_base_image()、依赖 image = new_image(k)）。
    /// code_arena = P1 本域 stub 代码区（§7.6，与 frozen 同 k 域，lower_inner 统一推导）。
    pub(super) fn new(
        tcx: TyCtxt<'tcx>,
        stack: &crate::baseimage::ImageStack,
        frozen: FrozenArena,
        code_arena: crate::vm::engine::codearena::StubArena,
    ) -> Self {
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
            foreign_fn_entries: FxHashMap::default(),
            got_syms: Vec::new(),
            got_idx: FxHashMap::default(),
            got_fixups: Vec::new(),
            frozen_relocs: Vec::new(),
            foreign_alloc_sym: FxHashMap::default(),
            foreign_slots: std::collections::HashMap::new(),
            code_arena,
            entry_stub_sites: Vec::new(),
            entry_stub_ids: FxHashMap::default(),
            entry_sig_cache: FxHashMap::default(),
            archive_fallbacks: Vec::new(),
            archive_handles: Vec::new(),
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
            main_catch_site: None,
        }
    }

    /// A2 split 激活（s3b-a2-design §4）：image 冻结区落样条 k=0 域。
    /// 域被占 = 回退动态基址（语义不变；A2-2 写盘阶段会拒序列化自愈）。
    pub(super) fn activate_split(&mut self) {
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
            image_got_syms: Vec::new(),
            image_got_idx: FxHashMap::default(),
            image_got_fixups: Vec::new(),
            image_frozen_relocs: Vec::new(),
            image_code_arena: crate::vm::engine::codearena::StubArena::new_image(0),
            image_stub_sites: Vec::new(),
        });
    }

    /// 预留一个 asm-stub 槽（M5.0），返回 (AsmStubId, 符号名)；文本随后 set_asm_stub 回填。
    /// 分两步是因为 wrapper 名要先于文本生成确定（自引用 .size 指令）。
    /// S4：id 从底座计数起编（wrapper 名跨域唯一白拿）。A2 split 按当前类分轨：
    /// image 类 = 标签 id + `mirvm_asm_xi{j}` 名，delta 类 = 原 id 空间 + `mirvm_asm_xd{k}`
    /// 名（最终位序收尾才知，名字与位序解耦）；非 split 路径沿用位序名不变。
    pub(super) fn reserve_asm_stub(&mut self) -> (ir::AsmStubId, Box<str>) {
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
    pub(super) fn set_asm_stub(&mut self, id: ir::AsmStubId, text: String) {
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
                    template: ir::LinkAddr(template),
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
            template: ir::LinkAddr(template),
            size,
            align: align as u32,
        });
        self.tls_ids.insert(def_id, id);
        Ok(id)
    }
}
