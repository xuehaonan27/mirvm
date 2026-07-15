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

/// 底座文件（v1 = postcard 整包；6c 换分区布局 + COW 定基映射）。
#[derive(Serialize, Deserialize)]
struct BaseFile {
    build_id: String,
    /// sysroot 新鲜度（S1a stamp 同源）
    sysroot_stamp: String,
    /// (ub_checks, overflow_checks, contract_checks)——lower 唯一烤入的会话布尔
    lowering_fp: (bool, bool, bool),
    module: ir::Module,
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
    Some(BaseImage {
        fn_by_sym: f
            .module
            .exports
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect(),
        entry_by_sym: f.fn_entry_syms.into_iter().collect(),
        static_by_sym: f.static_syms.into_iter().collect(),
        tls_by_sym: f.tls_syms.into_iter().collect(),
        lowering_fp: fp,
        key,
        module: f.module,
    })
}

/// 装载或（子进程）构建底座。一切失败 = None（全量冷降低，静默自愈；构建日志在
/// base/build.log——stderr 参与 native 差分，主路径不得发声）。
pub fn ensure() -> Option<BaseImage> {
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

/// 偏移合并：base ++ delta 拼单表（delta 的 fn/TLS/asm id 已从底座计数起编）。
/// 合并后重物化 asm-stub（底座文件里的 stub 地址是构建进程的活体，必须以配方
/// 在本进程幂等重物化——L2 warm 同款契约）。
pub fn absorb(delta: &mut ir::Module, base: ir::Module) {
    let mut funcs = base.funcs;
    funcs.append(&mut delta.funcs);
    delta.funcs = funcs;

    let mut tls = base.tls;
    tls.append(&mut delta.tls);
    delta.tls = tls;

    let mut sites = base.asm_sites;
    sites.append(&mut delta.asm_sites);
    delta.asm_sites = sites;
    delta.asm_stub_addrs = crate::lower::asm::materialize(&delta.asm_sites);

    for (a, f) in base.fn_addrs {
        delta.fn_addrs.entry(a).or_insert(f);
    }
    for (s, f) in base.exports {
        delta.exports.entry(s).or_insert(f);
    }
    for l in base.native_libs {
        if !delta.native_libs.contains(&l) {
            delta.native_libs.push(l);
        }
    }
    for l in base.required_native_libs {
        if !delta.required_native_libs.contains(&l) {
            delta.required_native_libs.push(l);
        }
    }
    // entry = delta 权威；foreign_static_syms：底座构建期已保证为空
    delta.base_frozen = base.frozen;
}

// ===== 构建端（`mirvm __build-base-image <path>` 子进程）=====

struct BaseBuildCallbacks {
    out: PathBuf,
    ok: bool,
}

impl Callbacks for BaseBuildCallbacks {
    fn after_analysis<'tcx>(&mut self, _compiler: &Compiler, tcx: TyCtxt<'tcx>) -> Compilation {
        let (mut module, exports) = crate::lower::lower_for_base_build(tcx);
        // 可缓存性三判据（L2 store 同款；底座是跨程序共享，更不容妥协）
        let frozen_ok = module.frozen.as_ref().is_some_and(|fr| {
            fr.at_fixed_base() && fr.home() == crate::vm::engine::frozen::BASE_IMAGE_FIXED_ADDR
        });
        if !frozen_ok {
            eprintln!("base-image: 冻结区未落底座固定域，放弃");
            return Compilation::Stop;
        }
        if !module.foreign_static_syms.is_empty() {
            eprintln!(
                "base-image: 空 main 闭包含宿主地址直嵌 {:?}，放弃（违可缓存性判据③）",
                module.foreign_static_syms
            );
            return Compilation::Stop;
        }
        // "@entry" 是 --vm-stats 的程序入口别名；底座作为库使用，不导出合成入口
        module.exports.remove("@entry");

        let sess = tcx.sess;
        let Some(sysroot_stamp) = crate::sysroot::current_stamp_value() else {
            eprintln!("base-image: sysroot stamp 不可得，放弃");
            return Compilation::Stop;
        };
        let file = BaseFile {
            build_id: env!("MIRVM_BUILD_ID").to_string(),
            sysroot_stamp,
            lowering_fp: (
                sess.ub_checks(),
                sess.overflow_checks(),
                sess.contract_checks(),
            ),
            module,
            fn_entry_syms: exports.fn_entry_syms,
            static_syms: exports.static_syms,
            tls_syms: exports.tls_syms,
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
