//! A2 deps-image（s3b-a2-design §3）：bin 无关实例的跨运行缓存——S4 std 底座对
//! registry 依赖闭包的推广。依赖闭包的 lower 产物（"纯化聚合"：只含 purity=Pure
//! 实例）在 bin 的**首次冷跑**里由 split lower 产出并落盘；编辑 bin 重跑时装载
//! 进 image 栈 `[std 底座, deps-image]`，runner 只 lower delta（bin 附着物）。
//!
//! 键（pre-key，**pre-compiler 可算**——L2 热路径在编译会话之前，不可依赖 tcx）：
//! `fnv(MIRVM_BUILD_ID, 底座键, 排序后的 --extern 工件内容盖戳)`。
//! --extern 只含直接依赖（eco 4 个），传递闭包由 **cargo 重建传播**覆盖（任一传递
//! crate 变更 ⇒ 其反向依赖链上的直接依赖被 cargo 重编译 ⇒ 直接 rlib 盖戳变）；
//! 内容摘要兜住同大小且 mtime 被恢复的改写。**不含 bin 源与项目
//! 身份** ⇒ bin 编辑必命中；同 lockfile + 同工具链的项目共享（S3′c）。
//! **A2-3 起默认开启**；`MIRVM_NO_DEPS_IMAGE=1` 全程旁路（双态对拍用）。
//! v1 边界：--extern 为空（无 registry 依赖的纯 std 程序）不产/不用 image——
//! 那是 S4 底座已经覆盖的地盘。
//!
//! 正确性红线（与 chain 时代同一条）：image 的绝对 FuncId/地址只在"装载栈下 ==
//! 构建栈下"（below 恒 = [底座]，键含底座键）时有效；任何校验不合 = 不装载
//! （全量降低自愈，绝不错值）。降低指纹分层验证：image.fp == 底座.fp（装载时）+
//! 底座.fp == 会话.fp（after_analysis fp_matches）⇒ image.fp == 会话.fp。
//! 字节确定性：**不作契约**（与 L2 条目同规则——身份由键/文件名承载，无 cmp 消费方；
//! 底座文件的确定性契约是"跨程序共享底座"专有，见 decision-history §7.3）。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::vm::engine::ir;

/// --extern 工件盖戳清单（path, size, mtime_ns, BLAKE3；排序去重）
type ExternStamps = Vec<(String, u64, u128, [u8; 32])>;

/// deps-image 文件（v1 = postcard 整包；module 的 exports/fn_addrs 保留在 module 内
/// ——无字节确定性契约，免去 BaseFile 的排序 Vec 摘出/重建舞）。
#[derive(Serialize, Deserialize)]
struct DepsFile {
    build_id: String,
    /// below = [底座] 的精确身份（错配装载 = 全盘错值，必须精确相等）
    base_key: String,
    lowering_fp: (bool, bool, bool),
    /// pre-key 素材回比（哈希碰撞免疫）：排序后的 --extern 工件盖戳
    extern_stamps: ExternStamps,
    module: ir::Module,
    fn_entry_syms: Vec<(Box<str>, u64)>,
    static_syms: Vec<(Box<str>, u64)>,
    tls_syms: Vec<(Box<str>, ir::TlsId)>,
}

/// 写入侧的借用形态（ir::Module 非 Clone——FrozenArena 持有 mmap 所有权）。
#[derive(Serialize)]
struct DepsFileRef<'a> {
    build_id: &'a str,
    base_key: &'a str,
    lowering_fp: (bool, bool, bool),
    extern_stamps: &'a [(String, u64, u128, [u8; 32])],
    module: &'a ir::Module,
    fn_entry_syms: &'a [(Box<str>, u64)],
    static_syms: &'a [(Box<str>, u64)],
    tls_syms: &'a [(Box<str>, ir::TlsId)],
}

/// 启用判定（A2-3 起默认开）：唯一旋钮 = 旁路 `MIRVM_NO_DEPS_IMAGE=1`（诊断/对拍
/// 双态用）。A2-2 期的 `MIRVM_DEPS_IMAGE=1` 启用旋钮已退役（残留无害）。
pub fn bypassed() -> bool {
    std::env::var_os("MIRVM_NO_DEPS_IMAGE").is_some_and(|v| !v.is_empty())
}

fn deps_dir() -> PathBuf {
    crate::sysroot::cache_dir().join("deps")
}

/// pre-key 的素材：rustc_args 中 `--extern name=path`（两参形态）与
/// `--extern=name=path`（单参形态）的 path 列表（去重排序）。
/// 无 path 的 --extern（裸名）⇒ None：v1 不产/不用 image（自愈，防静默错值）。
fn extern_paths(rustc_args: &[String]) -> Option<Vec<String>> {
    let mut paths = Vec::new();
    let mut it = rustc_args.iter();
    while let Some(a) = it.next() {
        if a == "--extern" {
            let v = it.next()?;
            let (_, p) = v.split_once('=')?;
            paths.push(p.to_string());
        } else if let Some(v) = a.strip_prefix("--extern=") {
            let (_, p) = v.split_once('=')?;
            paths.push(p.to_string());
        }
    }
    paths.sort();
    paths.dedup();
    Some(paths)
}

/// 内容盖戳；任一文件不可稳定读取 ⇒ None（不产/不用 image）。
fn stamp_externs(paths: &[String]) -> Option<ExternStamps> {
    paths
        .iter()
        .map(|p| {
            let stamped =
                crate::utils::content::file_content_stamp(std::path::Path::new(p)).ok()?;
            Some((p.clone(), stamped.size, stamped.mtime_ns, stamped.digest))
        })
        .collect()
}

/// pre-key = fnv(build_id, 底座键, 排序盖戳)。返回 (key, 盖戳清单)；
/// 任一素材不可得 ⇒ None（调用方按"无 image"处理，全量降低自愈）。
/// **--extern 为空 ⇒ None**（v1 边界：无依赖的纯 std 程序走 S4 底座域，
/// 不为它们建"std 残余共享 image"——那是 S4 已经覆盖的地盘）。
pub fn pre_key(rustc_args: &[String], base_key: &str) -> Option<(String, ExternStamps)> {
    let paths = extern_paths(rustc_args)?;
    if paths.is_empty() {
        return None;
    }
    let stamps = stamp_externs(&paths)?;
    let mut key = String::from(env!("MIRVM_BUILD_ID"));
    key.push('\u{1f}');
    key.push_str(base_key);
    for (p, size, mt, digest) in &stamps {
        key.push('\u{1f}');
        key.push_str(p);
        key.push('\u{1e}');
        key.push_str(&size.to_string());
        key.push('\u{1e}');
        key.push_str(&mt.to_string());
        key.push('\u{1e}');
        key.push_str(&crate::utils::content::digest_hex(digest));
    }
    let h = crate::lower::asm::fnv1a(key.as_bytes());
    if std::env::var_os("MIRVM_A2_DEBUG").is_some() {
        eprintln!("[a2-debug] pre-key={h:016x} externs={paths:?}");
    }
    Some((format!("{h:016x}"), stamps))
}

fn file_path(key: &str) -> PathBuf {
    deps_dir().join(format!("{key}.img"))
}

/// 装载 deps-image（pre-compiler 调用）。`base` = 已在场的底座（键与 fp 分层验证）；
/// 成功 = 待 push 上栈的 BaseImage（其 key = pre-key，供 L2 键链）。
/// 一切不合/失败 = None（全量降低自愈，主路径不发声——stderr 参与 native 差分）。
pub fn try_load(
    rustc_args: &[String],
    base: &crate::baseimage::BaseImage,
) -> Option<crate::baseimage::BaseImage> {
    let (key, stamps) = pre_key(rustc_args, &base.key)?;
    let data = std::fs::read(file_path(&key)).ok()?;
    let f: DepsFile = postcard::from_bytes(&data).ok()?;
    // 精确相等校验：build id、底座键、盖戳清单（碰撞免疫）、降低指纹分层
    if f.build_id != env!("MIRVM_BUILD_ID")
        || f.base_key != base.key
        || f.extern_stamps != stamps
        || f.lowering_fp != base.lowering_fp
    {
        return None;
    }
    // 冻结区必须真的落在样条 k=0 域（防御：文件被换/域被抢都不接受）
    let frozen_ok = f.module.frozen.as_ref().is_some_and(|fr| {
        fr.at_fixed_base() && fr.home() == crate::vm::engine::addrlayout::image_addr(0)
    });
    if !frozen_ok {
        return None;
    }
    crate::vm::engine::verify::module_with_prefix(
        &f.module,
        crate::vm::engine::verify::Prefix {
            funcs: base.module.funcs.len(),
            tls: base.module.tls.len(),
            asm: base.module.asm_sites.len(),
        },
    )
    .ok()?;
    // required .so 被清理 ⇒ miss 走冷路径自愈（ircache 同契约）
    if !f
        .module
        .required_native_libs
        .iter()
        .all(|p| std::path::Path::new(&**p).is_file())
    {
        return None;
    }
    let module = f.module;
    Some(crate::baseimage::BaseImage {
        fn_by_sym: module.exports.clone(),
        entry_by_sym: f.fn_entry_syms.into_iter().collect(),
        static_by_sym: f.static_syms.into_iter().collect(),
        tls_by_sym: f.tls_syms.into_iter().collect(),
        lowering_fp: f.lowering_fp,
        key,
        module,
    })
}

/// split 产物的入账与上栈（A2-2 管线）：可缓存则写盘（原子发布），返回待 push 的
/// BaseImage。写盘失败/不可缓存/pre-key 不可得 = 仅本次无文件（后续运行全量降低
/// 自愈），返回的上栈层键退化为进程唯一占位——L2 键链自然失效，绝不误命中。
pub fn store_and_wrap(
    rustc_args: &[String],
    base_key: &str,
    fp: (bool, bool, bool),
    image: crate::lower::SplitImage,
) -> crate::baseimage::BaseImage {
    let mut bi = image.into_base_image(fp);
    // 可缓存性判据：冻结区必须在样条 k=0 固定域（快照内嵌绝对地址跨进程稳定
    // 的前提）；条目 stub 代码域同规则（P1：fn-ptr 值域 = stub 码址）。foreign
    // 符号自 P2 起经 GOT 槽间接（decision-history §7.5c）：image 侧 GOT 表随
    // 文件走、装载后经启动相重填本进程真值——不再是写盘障碍。
    let cacheable = bi.module.frozen.as_ref().is_some_and(|fr| {
        fr.at_fixed_base() && fr.home() == crate::vm::engine::addrlayout::image_addr(0)
    }) && (bi.module.entry_stub_sites.is_empty()
        || bi.module.entry_stubs.at_fixed_base());
    let keyed = pre_key(rustc_args, base_key);
    if let (true, Some((key, stamps))) = (cacheable, keyed) {
        let mut fn_entry_syms = bi
            .entry_by_sym
            .iter()
            .map(|(s, a)| (s.clone(), *a))
            .collect::<Vec<_>>();
        fn_entry_syms.sort_unstable();
        let mut static_syms = bi
            .static_by_sym
            .iter()
            .map(|(s, a)| (s.clone(), *a))
            .collect::<Vec<_>>();
        static_syms.sort_unstable();
        let mut tls_syms = bi
            .tls_by_sym
            .iter()
            .map(|(s, id)| (s.clone(), *id))
            .collect::<Vec<_>>();
        tls_syms.sort_unstable();
        let file = DepsFileRef {
            build_id: env!("MIRVM_BUILD_ID"),
            base_key,
            lowering_fp: fp,
            extern_stamps: &stamps,
            module: &bi.module,
            fn_entry_syms: &fn_entry_syms,
            static_syms: &static_syms,
            tls_syms: &tls_syms,
        };
        if let Ok(bytes) = postcard::to_stdvec(&file) {
            let path = file_path(&key);
            let written = path.parent().is_some_and(|dir| {
                if std::fs::create_dir_all(dir).is_err() {
                    return false;
                }
                let tmp = dir.join(format!(
                    ".{}.tmp-{}",
                    path.file_name().unwrap_or_default().to_string_lossy(),
                    std::process::id()
                ));
                if std::fs::write(&tmp, &bytes).is_err() || std::fs::rename(&tmp, &path).is_err() {
                    let _ = std::fs::remove_file(&tmp);
                    return false;
                }
                true
            });
            if written {
                bi.key = key;
                return bi;
            }
        }
    }
    // 退化键：进程唯一 ⇒ L2 键链跨运行不误命中（本运行内存 absorb 不受影响）
    bi.key = format!("a2-unstable-{}", std::process::id());
    bi
}

#[cfg(test)]
mod tests {
    /// --extern 两参/单参形态都解析出 path；裸名 --extern ⇒ None（自愈）
    #[test]
    fn extern_paths_parse_both_forms_and_reject_bare_name() {
        let args = vec![
            "mirvm".to_string(),
            "src/main.rs".to_string(),
            "--extern".to_string(),
            "regex=/t/deps/libregex-abc.rlib".to_string(),
            "--extern=serde=/t/deps/libserde-def.rlib".to_string(),
            "--extern".to_string(),
            "rand=/t/deps/librand-123.rlib".to_string(),
            "-C".to_string(),
            "metadata=xyz".to_string(),
        ];
        let paths = super::extern_paths(&args).expect("解析成功");
        assert_eq!(
            paths,
            vec![
                "/t/deps/librand-123.rlib",
                "/t/deps/libregex-abc.rlib",
                "/t/deps/libserde-def.rlib"
            ]
        );
        let bare = vec!["--extern".to_string(), "regex".to_string()];
        assert_eq!(super::extern_paths(&bare), None);
        // 重复 --extern 去重
        let dup = vec![
            "--extern".to_string(),
            "regex=/t/a.rlib".to_string(),
            "--extern".to_string(),
            "regex=/t/a.rlib".to_string(),
        ];
        assert_eq!(super::extern_paths(&dup).unwrap().len(), 1);
    }

    #[test]
    fn pre_key_detects_same_length_content_change_with_restored_mtime() {
        let dir = std::env::temp_dir().join(format!(
            "mirvm-depsimage-content-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let artifact = dir.join("libdep.rlib");
        std::fs::write(&artifact, b"first").unwrap();
        let original_mtime = std::fs::metadata(&artifact).unwrap().modified().unwrap();
        let args = vec![format!("--extern=dep={}", artifact.display())];
        let before = super::pre_key(&args, "base").unwrap().0;

        std::fs::write(&artifact, b"other").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&artifact)
            .unwrap()
            .set_modified(original_mtime)
            .unwrap();
        let after = super::pre_key(&args, "base").unwrap().0;
        assert_ne!(before, after);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
