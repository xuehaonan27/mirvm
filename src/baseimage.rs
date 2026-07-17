//! S4：std 预降低底座（base image；s4-base-image-design.md，JVM CDS base archive 对映）。
//!
//! 底座 = "空 main" 合成会话的完整降低产物（lang_start 链/panic/fmt/alloc 机器，
//! ~3000 instance），冻结区落在 BASE_IMAGE_FIXED_ADDR 域，跨程序共享。程序会话
//! （delta）降低时按 **v0 symbol_name** 查底座：函数命中即复用 FuncId（不入队）、
//! 静态命中即复用地址（双份物化 = static mut 精神分裂，不可选）、TLS 命中即复用
//! TlsId。delta 的函数/TLS/asm-stub id 从底座计数起编（**偏移合并**，施工偏离：
//! 取代简报的"FuncId 域位+双函数表"——装载时 base++delta 拼单表，解释器热路径
//! 零改动；偏离记 decision-history）。
//!
//! 键与失效：文件名 = fnv(build_id, sysroot stamp)；装载再验 build_id/stamp 相等。
//! **降低指纹**（lower 烤进字节码的三个会话布尔：ub/overflow/contract checks）
//! 必须在**会话内**验证（cargo runner 可能带自定义 profile 旗标）——失配即弃用
//! 底座走全量冷降低（自愈，无声回退；构建失败同理，日志落 base/build.log）。
//! `MIRVM_NO_BASE_IMAGE=1` 全程旁路（对拍/诊断用）。

use std::path::PathBuf;
use std::process::ExitCode;

use rustc_driver::{Callbacks, Compilation};
use rustc_interface::interface::Compiler;
use rustc_middle::ty::TyCtxt;
use serde::{Deserialize, Serialize};

use crate::vm::engine::ir;

/// 底座文件（v1 = postcard 整包）。
///
/// **字节确定性契约**（验收：连续两建 cmp 一致）：Module 的 exports/fn_addrs 是
/// std HashMap（RandomState 随机种子 ⇒ 迭代序每进程随机），不能随 module 直接落盘
/// ——摘出为**排序 Vec** 字段（module 内清空），装载端重建。其余字段
/// （funcs/tls/asm_sites/冻结区字节）由降低顺序天然确定。
#[derive(Serialize, Deserialize)]
struct BaseFile {
    build_id: String,
    /// sysroot 新鲜度（S1a stamp 同源）
    sysroot_stamp: String,
    /// (ub_checks, overflow_checks, contract_checks)——lower 唯一烤入的会话布尔
    lowering_fp: (bool, bool, bool),
    /// exports/fn_addrs 已清空（见上），由下方排序表重建
    module: ir::Module,
    /// sym → FuncId（= module.exports 的排序形态）
    export_syms: Vec<(Box<str>, ir::FuncId)>,
    /// fn 条目真地址 → FuncId（= module.fn_addrs 的排序形态）
    fn_addr_pairs: Vec<(u64, ir::FuncId)>,
    /// sym → fn 条目真地址（仅被取址过的函数有条目）
    fn_entry_syms: Vec<(Box<str>, u64)>,
    static_syms: Vec<(Box<str>, u64)>,
    tls_syms: Vec<(Box<str>, ir::TlsId)>,
}

/// 装载完成、待程序会话使用的底座。
pub struct BaseImage {
    pub module: ir::Module,
    pub fn_by_sym: std::collections::HashMap<Box<str>, ir::FuncId>,
    pub entry_by_sym: std::collections::HashMap<Box<str>, u64>,
    pub static_by_sym: std::collections::HashMap<Box<str>, u64>,
    pub tls_by_sym: std::collections::HashMap<Box<str>, ir::TlsId>,
    pub lowering_fp: (bool, bool, bool),
    /// 分层缓存键（ircache delta 条目引用；含降低指纹）
    pub key: String,
}

fn disabled() -> bool {
    std::env::var_os("MIRVM_NO_BASE_IMAGE").is_some_and(|v| !v.is_empty())
}

fn base_dir() -> PathBuf {
    crate::sysroot::cache_dir().join("base")
}

/// (底座文件路径, sysroot stamp)。stamp 不可得（sysroot 未建成等）⇒ None（无底座）。
fn locate() -> Option<(PathBuf, String)> {
    let stamp = crate::sysroot::current_stamp_value()?;
    let mut key = String::from(env!("MIRVM_BUILD_ID"));
    key.push('\u{1f}');
    key.push_str(&stamp);
    let h = crate::lower::asm::fnv1a(key.as_bytes());
    Some((base_dir().join(format!("{h:016x}.img")), stamp))
}

fn load(path: &std::path::Path, want_stamp: &str) -> Option<BaseImage> {
    let data = std::fs::read(path).ok()?;
    let f: BaseFile = postcard::from_bytes(&data).ok()?; // 内含冻结区定基恢复；被占 ⇒ miss
    if f.build_id != env!("MIRVM_BUILD_ID") || f.sysroot_stamp != want_stamp {
        return None;
    }
    // 冻结区必须真的落在底座域（防御：文件被换/域被抢都不接受）
    let frozen_ok = f.module.frozen.as_ref().is_some_and(|fr| {
        fr.at_fixed_base() && fr.home() == crate::vm::engine::frozen::BASE_IMAGE_FIXED_ADDR
    });
    if !frozen_ok {
        return None;
    }
    let fp = f.lowering_fp;
    let mut key = String::from(env!("MIRVM_BUILD_ID"));
    key.push('\u{1f}');
    key.push_str(want_stamp);
    key.push('\u{1f}');
    key.push_str(&format!("fp{}{}{}", fp.0 as u8, fp.1 as u8, fp.2 as u8));
    // exports/fn_addrs 从排序表重建（字节确定性契约，见 BaseFile 文档）
    let mut module = f.module;
    module.exports = f.export_syms.iter().cloned().collect();
    module.fn_addrs = f.fn_addr_pairs.iter().copied().collect();
    Some(BaseImage {
        fn_by_sym: f.export_syms.into_iter().collect(),
        entry_by_sym: f.fn_entry_syms.into_iter().collect(),
        static_by_sym: f.static_syms.into_iter().collect(),
        tls_by_sym: f.tls_syms.into_iter().collect(),
        lowering_fp: fp,
        key,
        module,
    })
}

/// 装载或（子进程）构建底座。失败 = None（全量冷降低，静默自愈；构建日志在
/// base/build.log——stderr 参与 native 差分，主路径不得发声）。
fn ensure_base() -> Option<BaseImage> {
    if disabled() {
        return None;
    }
    let (path, stamp) = locate()?;
    if let Some(b) = load(&path, &stamp) {
        return Some(b);
    }
    // 子进程构建：本进程的 rustc 会话唯一性（TRACK_DIAGNOSTIC/全局计数钩）不容
    // 第二个 compiler；exec 自身 ~13ms，一次性。
    let self_exe = std::env::current_exe().ok()?;
    let _ = std::fs::create_dir_all(base_dir());
    let log = std::fs::File::create(base_dir().join("build.log")).ok()?;
    let status = std::process::Command::new(self_exe)
        .arg("__build-base-image")
        .arg(&path)
        .stdout(std::process::Stdio::null())
        .stderr(log)
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }
    load(&path, &stamp)
}

/// image 栈（S3′a，m5.3-design §3.3）：底座 + 依赖 image 链的有序集合（底→顶 =
/// 拓扑序 [std 底座, dep₁, dep₂ …]）。程序会话降低时按 v0 symbol_name 查**并集**
/// 复用，delta 的 fn/TLS/asm id 从栈总量起编，absorb 时 [栈…][delta] 拼单表。
/// 各 image 各占一固定域（底座 0x6800、依赖样条 0x6A00+k·2^34），跨域绝对地址
/// 互指全稳定。空栈（无底座/旁路）= 全量冷降低，行为回到 S4 前。
pub struct ImageStack {
    images: Vec<BaseImage>,
    /// 并集查找（sym → 绝对 id/地址；各 image id 域不相交，union 无歧义）
    fn_by_sym: std::collections::HashMap<Box<str>, ir::FuncId>,
    entry_by_sym: std::collections::HashMap<Box<str>, u64>,
    static_by_sym: std::collections::HashMap<Box<str>, u64>,
    tls_by_sym: std::collections::HashMap<Box<str>, ir::TlsId>,
    /// delta 起编偏移 = 栈内累积量
    total_fns: usize,
    total_tls: usize,
    total_asm: usize,
    /// 降低指纹（全栈一致；构造时按前缀截断分歧，自愈）
    lowering_fp: (bool, bool, bool),
    /// 分层缓存键链（各 image key 以 \x1f 连接；空栈 = None）
    key: Option<String>,
}

impl ImageStack {
    pub fn empty() -> Self {
        ImageStack {
            images: Vec::new(),
            fn_by_sym: Default::default(),
            entry_by_sym: Default::default(),
            static_by_sym: Default::default(),
            tls_by_sym: Default::default(),
            total_fns: 0,
            total_tls: 0,
            total_asm: 0,
            lowering_fp: (false, false, false),
            key: None,
        }
    }

    /// 有序 image 列表 → 栈。降低指纹分歧处**前缀截断**（分歧 image 及其上全部退回
    /// delta——自愈，绝不用错指纹的 image 复用）。构建并集查找 + 累积偏移 + 键链。
    fn from_images(mut images: Vec<BaseImage>) -> Self {
        if images.is_empty() {
            return Self::empty();
        }
        let fp = images[0].lowering_fp;
        if let Some(cut) = images.iter().position(|i| i.lowering_fp != fp) {
            images.truncate(cut);
        }
        if images.is_empty() {
            return Self::empty();
        }
        let mut s = ImageStack::empty();
        s.lowering_fp = fp;
        let mut keys = Vec::with_capacity(images.len());
        for img in &images {
            keys.push(img.key.clone());
            for (k, v) in &img.fn_by_sym {
                s.fn_by_sym.entry(k.clone()).or_insert(*v);
            }
            for (k, v) in &img.entry_by_sym {
                s.entry_by_sym.entry(k.clone()).or_insert(*v);
            }
            for (k, v) in &img.static_by_sym {
                s.static_by_sym.entry(k.clone()).or_insert(*v);
            }
            for (k, v) in &img.tls_by_sym {
                s.tls_by_sym.entry(k.clone()).or_insert(*v);
            }
            s.total_fns += img.module.funcs.len();
            s.total_tls += img.module.tls.len();
            s.total_asm += img.module.asm_sites.len();
        }
        s.key = Some(keys.join("\u{1f}"));
        s.images = images;
        s
    }

    pub fn is_empty(&self) -> bool {
        self.images.is_empty()
    }

    /// 栈底底座（A2 deps-image 装载的 below 键/fp 分层验证用；空栈 = None）
    pub fn base_image(&self) -> Option<&BaseImage> {
        self.images.first()
    }
    pub fn total_fns(&self) -> usize {
        self.total_fns
    }
    pub fn total_tls(&self) -> usize {
        self.total_tls
    }
    pub fn total_asm(&self) -> usize {
        self.total_asm
    }
    pub fn fn_by_sym(&self) -> &std::collections::HashMap<Box<str>, ir::FuncId> {
        &self.fn_by_sym
    }
    pub fn entry_by_sym(&self) -> &std::collections::HashMap<Box<str>, u64> {
        &self.entry_by_sym
    }
    pub fn static_by_sym(&self) -> &std::collections::HashMap<Box<str>, u64> {
        &self.static_by_sym
    }
    pub fn tls_by_sym(&self) -> &std::collections::HashMap<Box<str>, ir::TlsId> {
        &self.tls_by_sym
    }
    pub fn key(&self) -> Option<&str> {
        self.key.as_deref()
    }

    /// 追加一层 image（A2 in-memory split 产物，s3b-a2-design）：并集查找/累积
    /// 偏移/键链增量更新。fp 由调用方保证与栈一致（同会话构建，无需截断）。
    pub fn push(&mut self, img: BaseImage) {
        if self.images.is_empty() {
            self.lowering_fp = img.lowering_fp;
        }
        for (k, v) in &img.fn_by_sym {
            self.fn_by_sym.entry(k.clone()).or_insert(*v);
        }
        for (k, v) in &img.entry_by_sym {
            self.entry_by_sym.entry(k.clone()).or_insert(*v);
        }
        for (k, v) in &img.static_by_sym {
            self.static_by_sym.entry(k.clone()).or_insert(*v);
        }
        for (k, v) in &img.tls_by_sym {
            self.tls_by_sym.entry(k.clone()).or_insert(*v);
        }
        self.total_fns += img.module.funcs.len();
        self.total_tls += img.module.tls.len();
        self.total_asm += img.module.asm_sites.len();
        match &mut self.key {
            Some(k) => {
                k.push('\u{1f}');
                k.push_str(&img.key);
            }
            None => self.key = Some(img.key.clone()),
        }
        self.images.push(img);
    }

    /// 会话降低指纹核对（cli after_analysis）：不匹配 ⇒ 弃整栈走全量降低。
    /// 栈内 fp 已一致（from_images 截断保证），故整栈判定即可。
    pub fn fp_matches(&self, session_fp: (bool, bool, bool)) -> bool {
        self.is_empty() || self.lowering_fp == session_fp
    }
}

/// 装载 image 栈（S3′a）：底座 + 依赖 image 链。本片仅底座（S3′b 装填依赖链）。
pub fn ensure() -> ImageStack {
    let mut images = Vec::new();
    if let Some(b) = ensure_base() {
        images.push(b);
    }
    ImageStack::from_images(images)
}

/// 偏移合并：[栈…] ++ delta 拼单表（delta 的 fn/TLS/asm id 已从栈总量起编；
/// 各 image funcs 按绝对 FuncId 顺序存 ⇒ 顺次拼接位置 = 绝对 id）。合并后重物化
/// asm-stub（image 文件里的 stub 地址是构建进程的活体，必须以配方在本进程幂等重物化
/// ——L2 warm 同款契约）。各 image 冻结区移交 delta.image_frozens 保活。
pub fn absorb_stack(delta: &mut ir::Module, stack: ImageStack) {
    let mut funcs = Vec::with_capacity(stack.total_fns);
    let mut tls = Vec::with_capacity(stack.total_tls);
    let mut sites = Vec::with_capacity(stack.total_asm);
    let mut frozens = Vec::with_capacity(stack.images.len());
    for img in stack.images {
        let mut m = img.module;
        funcs.append(&mut m.funcs);
        tls.append(&mut m.tls);
        sites.append(&mut m.asm_sites);
        for (a, f) in m.fn_addrs {
            delta.fn_addrs.entry(a).or_insert(f);
        }
        for (s, f) in m.exports {
            delta.exports.entry(s).or_insert(f);
        }
        for l in m.native_libs {
            if !delta.native_libs.contains(&l) {
                delta.native_libs.push(l);
            }
        }
        for l in m.required_native_libs {
            if !delta.required_native_libs.contains(&l) {
                delta.required_native_libs.push(l);
            }
        }
        // P2 GOT 随 image 合流（decision-history §7.5c）：sym 按名去重、fixup
        // idx 重编；image 样条域地址固定基稳定，合流后仍指向同一冻结格
        delta.absorb_got(m.foreign_syms, m.got_fixups);
        // P1 条目 stub 随 image 合流（§7.6）：配方与代码域按挂载，启动相按域重建
        if !m.entry_stub_sites.is_empty() || m.entry_stubs.is_mapped() {
            let home = m
                .frozen
                .as_ref()
                .and_then(|f| crate::vm::engine::codearena::code_home_for_frozen(f.home()))
                .expect("P1：image 冻结域非法，stub 代码域不可推");
            delta.image_entry_stubs.push((
                home,
                std::mem::take(&mut m.entry_stub_sites),
                std::mem::take(&mut m.entry_stubs),
            ));
        }
        if let Some(fr) = m.frozen {
            frozens.push(fr);
        }
    }
    funcs.append(&mut delta.funcs);
    delta.funcs = funcs;
    tls.append(&mut delta.tls);
    delta.tls = tls;
    sites.append(&mut delta.asm_sites);
    delta.asm_sites = sites;
    delta.asm_stub_addrs = crate::lower::asm::materialize(&delta.asm_sites);
    // entry = delta 权威
    delta.image_frozens = frozens;
}

// ===== 构建端（`mirvm __build-base-image <path>` 子进程）=====

struct BaseBuildCallbacks {
    out: PathBuf,
    ok: bool,
}

impl Callbacks for BaseBuildCallbacks {
    fn after_analysis<'tcx>(&mut self, _compiler: &Compiler, tcx: TyCtxt<'tcx>) -> Compilation {
        let (mut module, exports) = crate::lower::lower_for_base_build(tcx);
        // 可缓存性判据（L2 store 同构；底座是跨程序共享，更不容妥协）
        let frozen_ok = module.frozen.as_ref().is_some_and(|fr| {
            fr.at_fixed_base() && fr.home() == crate::vm::engine::frozen::BASE_IMAGE_FIXED_ADDR
        });
        if !frozen_ok {
            eprintln!("base-image: 冻结区未落底座固定域，放弃");
            return Compilation::Stop;
        }
        // foreign 符号自 P2 起经 GOT 槽间接（§7.5c：表随快照、启动相重填）——
        // 原「直嵌宿主地址判据③」已退役，不再是写盘障碍。
        // P1：stub 代码域不在固定基址 ⇒ fn-ptr 值域跨进程不稳定，拒写（同规则）
        if !module.entry_stub_sites.is_empty() && !module.entry_stubs.at_fixed_base() {
            eprintln!("base-image: stub 代码域未落固定域，放弃");
            return Compilation::Stop;
        }
        // "@entry" 是 --vm-stats 的程序入口别名；底座作为库使用，不导出合成入口
        module.exports.remove("@entry");

        let sess = tcx.sess;
        let Some(sysroot_stamp) = crate::sysroot::current_stamp_value() else {
            eprintln!("base-image: sysroot stamp 不可得，放弃");
            return Compilation::Stop;
        };
        // 字节确定性：HashMap（RandomState 随机迭代序）摘出为排序 Vec 落盘
        let mut export_syms: Vec<(Box<str>, ir::FuncId)> = module.exports.drain().collect();
        export_syms.sort_unstable();
        let mut fn_addr_pairs: Vec<(u64, ir::FuncId)> = module.fn_addrs.drain().collect();
        fn_addr_pairs.sort_unstable();
        let mut fn_entry_syms = exports.fn_entry_syms;
        fn_entry_syms.sort_unstable();
        let mut static_syms = exports.static_syms;
        static_syms.sort_unstable();
        let mut tls_syms = exports.tls_syms;
        tls_syms.sort_unstable();
        let file = BaseFile {
            build_id: env!("MIRVM_BUILD_ID").to_string(),
            sysroot_stamp,
            lowering_fp: (
                sess.ub_checks(),
                sess.overflow_checks(),
                sess.contract_checks(),
            ),
            module,
            export_syms,
            fn_addr_pairs,
            fn_entry_syms,
            static_syms,
            tls_syms,
        };
        let Ok(bytes) = postcard::to_stdvec(&file) else {
            eprintln!("base-image: 序列化失败（冻结区非定基？），放弃");
            return Compilation::Stop;
        };
        let Some(dir) = self.out.parent() else {
            return Compilation::Stop;
        };
        let _ = std::fs::create_dir_all(dir);
        let tmp = dir.join(format!(
            ".{}.tmp-{}",
            self.out.file_name().unwrap_or_default().to_string_lossy(),
            std::process::id()
        ));
        if std::fs::write(&tmp, &bytes).is_err() || std::fs::rename(&tmp, &self.out).is_err() {
            let _ = std::fs::remove_file(&tmp);
            eprintln!("base-image: 写文件失败，放弃");
            return Compilation::Stop;
        }
        self.ok = true;
        Compilation::Stop
    }
}

/// 子进程入口。argv = [<输出路径>]。
pub fn build_main(mut argv: impl Iterator<Item = String>) -> ExitCode {
    let Some(out) = argv.next() else {
        eprintln!("__build-base-image: 缺少输出路径");
        return ExitCode::from(2);
    };
    let sysroot = match crate::sysroot::ensure_sysroot() {
        Ok(p) => p.display().to_string(),
        Err(e) => {
            eprintln!("__build-base-image: sysroot 不可得: {e}");
            return ExitCode::from(1);
        }
    };
    // 合成空 main：底座种子（s4-base-image-design §4 方案 A——确定性内容）
    let src_dir = base_dir().join("src");
    if std::fs::create_dir_all(&src_dir).is_err() {
        return ExitCode::from(1);
    }
    let src = src_dir.join("empty_main.rs");
    if std::fs::write(&src, "fn main() {}\n").is_err() {
        return ExitCode::from(1);
    }

    let rustc_args = vec![
        "mirvm-base-build".to_string(),
        src.display().to_string(),
        "--edition=2024".to_string(),
        "--crate-type=bin".to_string(),
        "--sysroot".to_string(),
        sysroot,
    ];
    let mut callbacks = BaseBuildCallbacks {
        out: PathBuf::from(out),
        ok: false,
    };
    let code = rustc_driver::catch_with_exit_code(|| {
        rustc_driver::run_compiler(&rustc_args, &mut callbacks)
    });
    if code != ExitCode::SUCCESS || !callbacks.ok {
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}
