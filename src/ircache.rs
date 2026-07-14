//! L2 post-mono engine-IR 缓存（M6 片2；distribution-design.md D9b/D9c，JVM AppCDS 对映）。
//!
//! 冷路径（miss）：rustc 前端 → lower → **store**（guest 运行前的洁净快照）→ 运行。
//! 热路径（hit）：**lookup** → asm-stub 重物化 → argv 终结化 → 运行——整个 rustc
//! 会话（前端+metadata+mono+lower）被跳过。
//!
//! 键 = fnv(MIRVM_BUILD_ID, rustc_args)；条目头 = 完整 args 回比（哈希碰撞免疫）+
//! 输入清单校验。清单口径与 rustc 自身 dep-info 同构（rustc_interface::passes）：
//! 本地源文件（source_map 非 imported）+ `include!` 追踪文件（sess.file_depinfo）+
//! 全部上游 crate 工件（used_crate_source：含 sysroot std rlib，故 sysroot 变更天然
//! 失配）+ `env!` 依赖（sess.env_depinfo）。文件以 (size, mtime_ns) 校验（cargo 指纹
//! 同保真度；mtime 粒度风险已记 distribution-design §6）。
//!
//! 防静默错值：任何校验不合即 miss（冷路径重建覆写）；冻结区非固定基址即拒绝
//! 序列化/恢复（见 frozen.rs）；required .so 缺失即 miss（自愈而非运行期报错）。
//! `MIRVM_NO_IR_CACHE=1` 全程旁路。

use std::path::{Path, PathBuf};

use rustc_middle::ty::TyCtxt;
use serde::{Deserialize, Serialize};

use crate::vm::engine::ir;

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
struct FileStamp {
    path: String,
    size: u64,
    mtime_ns: u128,
}

#[derive(Serialize, Deserialize)]
struct Header {
    build_id: String,
    args: Vec<String>,
    files: Vec<FileStamp>,
    /// `env!`/`option_env!` 依赖：(名, 编译时值；None = 编译时未设)
    envs: Vec<(String, Option<String>)>,
}

fn disabled() -> bool {
    std::env::var_os("MIRVM_NO_IR_CACHE").is_some_and(|v| !v.is_empty())
}

fn entry_path(rustc_args: &[String]) -> PathBuf {
    let mut key = String::from(env!("MIRVM_BUILD_ID"));
    for a in rustc_args {
        key.push('\u{1f}');
        key.push_str(a);
    }
    let h = crate::lower::asm::fnv1a(key.as_bytes());
    crate::sysroot::cache_dir()
        .join("ir")
        .join(format!("{h:016x}.bin"))
}

fn stamp(path: &str) -> Option<FileStamp> {
    let md = std::fs::metadata(path).ok()?;
    if !md.is_file() {
        return None;
    }
    let mtime_ns = md
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some(FileStamp {
        path: path.to_string(),
        size: md.len(),
        mtime_ns,
    })
}

fn env_matches(name: &str, recorded: &Option<String>) -> bool {
    match (std::env::var(name), recorded) {
        (Ok(cur), Some(rec)) => cur == *rec,
        (Err(std::env::VarError::NotPresent), None) => true,
        _ => false,
    }
}

/// 热路径查找。返回的 Module 已含恢复到固定基址的冻结区；asm_stub_addrs 是
/// 序列化时的陈旧地址，调用方**必须**以 asm_sites 重物化覆写后再执行。
pub fn lookup(rustc_args: &[String]) -> Option<ir::Module> {
    if disabled() {
        return None;
    }
    let data = std::fs::read(entry_path(rustc_args)).ok()?;
    let (header, module_bytes) = postcard::take_from_bytes::<Header>(&data).ok()?;
    if header.build_id != env!("MIRVM_BUILD_ID") || header.args != rustc_args {
        return None; // 键碰撞或跨构建陈账
    }
    if !header
        .files
        .iter()
        .all(|f| stamp(&f.path).as_ref() == Some(f))
    {
        return None;
    }
    if !header.envs.iter().all(|(k, v)| env_matches(k, v)) {
        return None;
    }
    // Module 反序列化内含冻结区固定基址恢复；失败（基址被占等）→ miss
    let module: ir::Module = postcard::from_bytes(module_bytes).ok()?;
    // 加载相物化的 .so（native archive / global_asm）被清理 → miss 走冷路径自愈
    if !module
        .required_native_libs
        .iter()
        .all(|p| Path::new(&**p).is_file())
    {
        return None;
    }
    Some(module)
}

/// 冷路径入账（lower 刚完成、guest 未运行的洁净态）。返回是否真正写入。
pub fn store(tcx: TyCtxt<'_>, rustc_args: &[String], module: &ir::Module) -> bool {
    if disabled() {
        return false;
    }
    // 冻结区不在固定基址（并发抢占/ASLR 冲突）⇒ 快照内嵌地址跨进程无效，不缓存
    if !module.frozen.as_ref().is_some_and(|f| f.at_fixed_base()) {
        return false;
    }
    // 宿主地址直嵌（environ 类 extern static 的 dlsym 真地址已烤进 const/冻结区）
    // ⇒ 跨进程回放 = 野指针（gate 实测 c_process 热路径 SIGSEGV）——诚实不缓存
    if !module.foreign_static_syms.is_empty() {
        return false;
    }

    // 输入清单（rustc dep-info 同构口径）
    let sess = tcx.sess;
    let mut files: Vec<String> = sess
        .source_map()
        .files()
        .iter()
        .filter(|f| !f.is_imported())
        .filter_map(|f| match &f.name {
            rustc_span::FileName::Real(real) => real.local_path().map(|p| p.display().to_string()),
            _ => None,
        })
        .collect();
    files.extend(
        sess.file_depinfo
            .borrow()
            .iter()
            .map(|sym| sym.as_str().to_string()),
    );
    for &cnum in tcx.crates(()) {
        files.extend(
            tcx.used_crate_source(cnum)
                .paths()
                .map(|p| p.display().to_string()),
        );
    }
    files.sort();
    files.dedup();
    let Some(stamps) = files
        .iter()
        .map(|p| stamp(p))
        .collect::<Option<Vec<FileStamp>>>()
    else {
        return false; // 有输入文件无法盖戳（消失/非常规）——宁不缓存
    };
    let envs: Vec<(String, Option<String>)> = sess
        .env_depinfo
        .borrow()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.map(|s| s.as_str().to_string())))
        .collect();

    let header = Header {
        build_id: env!("MIRVM_BUILD_ID").to_string(),
        args: rustc_args.to_vec(),
        files: stamps,
        envs,
    };
    let Ok(mut buf) = postcard::to_stdvec(&header) else {
        return false;
    };
    match postcard::to_stdvec(module) {
        Ok(m) => buf.extend(m),
        Err(_) => return false,
    }

    // 原子发布（asm-stub 工厂同款：临时名写全再 rename，读方绝不见半成品）
    let path = entry_path(rustc_args);
    let Some(dir) = path.parent() else {
        return false;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let tmp = dir.join(format!(
        ".{}.tmp-{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    if std::fs::write(&tmp, &buf).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return false;
    }
    std::fs::rename(&tmp, &path).is_ok()
}

#[cfg(test)]
mod tests {
    use super::{FileStamp, env_matches, stamp};

    #[test]
    fn stamp_detects_content_length_and_mtime_change() {
        let dir = std::env::temp_dir().join(format!("mirvm-ircache-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("input.rs");
        std::fs::write(&f, b"fn main() {}").unwrap();
        let p = f.display().to_string();
        let s0 = stamp(&p).expect("可盖戳");

        // 尺寸变化必失配
        std::fs::write(&f, b"fn main() { let _ = 1; }").unwrap();
        assert_ne!(stamp(&p).as_ref(), Some(&s0));

        // 同尺寸、mtime 后移也必失配（内容同长的改写由 mtime 兜住）
        std::fs::write(&f, b"fn main() {}").unwrap();
        let s1 = stamp(&p).unwrap();
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(7);
        std::fs::File::options()
            .write(true)
            .open(&f)
            .unwrap()
            .set_modified(later)
            .unwrap();
        assert_ne!(stamp(&p).as_ref(), Some(&s1));

        // 消失 = 无戳
        std::fs::remove_file(&f).unwrap();
        assert_eq!(stamp(&p), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn env_dep_matching_covers_set_unset_and_drift() {
        let name = "MIRVM_IRCACHE_TEST_ENV";
        // SAFETY: 单测进程内自有变量
        unsafe { std::env::remove_var(name) };
        assert!(env_matches(name, &None));
        assert!(!env_matches(name, &Some("x".into())));
        unsafe { std::env::set_var(name, "x") };
        assert!(env_matches(name, &Some("x".into())));
        assert!(!env_matches(name, &Some("y".into())));
        assert!(!env_matches(name, &None));
        unsafe { std::env::remove_var(name) };
    }

    #[test]
    fn stamps_compare_structurally() {
        let a = FileStamp {
            path: "a".into(),
            size: 1,
            mtime_ns: 2,
        };
        assert_eq!(
            a,
            FileStamp {
                path: "a".into(),
                size: 1,
                mtime_ns: 2
            }
        );
    }
}
