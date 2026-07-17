//! Linux/ELF 静态原生归档装载：将经过约束检查的 PIC `.a` 物化成可 `dlopen` 的 `.so`。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use rustc_hir::attrs::NativeLibKind;
use rustc_hir::def_id::LOCAL_CRATE;
use rustc_middle::ty::TyCtxt;
use rustc_session::search_paths::PathKind;
use rustc_target::spec::{BinaryFormat, Os};

const CACHE_FORMAT_VERSION: &[u8] = b"mirvm-native-archive-v1";
const LINK_PREFIX: &[&str] = &["-shared", "-Wl,-z,defs", "-Wl,--whole-archive"];
/// 闭包基准 = std 经 `#[link]` 带给 guest 最终链接的系统库集（glibc：m/dl/pthread/
/// rt/util/gcc_s；native 语义里这些恒在场，rustc 的 C 静态归档可直接引用其符号——
/// libsqlite3 的 FTS5 引 libm `log`、pthread 族皆此类，corpus 批3 rusqlite 实锤）。
/// 它们落成产出 .so 的 DT_NEEDED，dlopen 时由宿主环境解析；`-z defs` 对除此之外
/// 的未定义引用（跨归档/guest 符号）继续响亮拒绝，闭包纪律不松动。
const LINK_SUFFIX: &[&str] = &[
    "-Wl,--no-whole-archive",
    "-lm",
    "-ldl",
    "-lpthread",
    "-lrt",
    "-lutil",
    "-lgcc_s",
];
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

/// 收集 crate 图传播的系统动态库名（corpus 批7 c_libgit2 实锤）：`-sys` crate
/// 的 `cargo:rustc-link-lib` 只写 rlib 元数据——native 最终链接行有这些 `-l`，
/// 而元数据驱动方（bin 命令行）没有。静态归档的 C 对象引这些库（libgit2.a 的
/// crc32/deflate → libz-sys 的 `z`）时，闭包链接行必须同样带上——与
/// `system_dylib_preload`（lower 的 RTLD_GLOBAL 预载）同一收集口径，双消费。
/// Static{bundle:None|Some(true)} 是整档进 rlib 的真静态归档（archive 通道，
/// 上方循环处理，此处跳过）；Framework/LinkArg/wasm 不在本切片。
pub(crate) fn system_dylibs(tcx: TyCtxt<'_>) -> Vec<Box<str>> {
    let sess = tcx.sess;
    let mut names: Vec<Box<str>> = Vec::new();
    for cnum in std::iter::once(LOCAL_CRATE).chain(tcx.used_crates(()).iter().copied()) {
        if cnum != LOCAL_CRATE && tcx.crate_dep_kind(cnum).macros_only() {
            continue;
        }
        for lib in tcx.native_libraries(cnum) {
            // 系统动态链接类 = Dylib/RawDylib + Unspecified（bare `-l ssl`，Dylib
            // 为默认）+ Static{bundle:false}（对象不进 rlib、链接期按系统库解析——
            // libc 的 m/dl/pthread/rt/util 即此形）
            let system_dylib = matches!(
                lib.kind,
                NativeLibKind::Dylib { .. } | NativeLibKind::RawDylib { .. } | NativeLibKind::Unspecified
            ) || matches!(
                lib.kind,
                NativeLibKind::Static {
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
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    // CLI `-l` 同口径（search path 形式由调用方另行处理）
    for lib in &sess.opts.libs {
        if matches!(lib.kind, NativeLibKind::Static { .. }) {
            continue;
        }
        let name: Box<str> = lib.name.as_str().into();
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

#[cfg(test)]
fn materialize_in(archive: &Path, cache_dir: &Path) -> Result<PathBuf, String> {
    materialize_for_target_in(archive, cache_dir, env!("MIRVM_HOST"), Path::new("cc"), &[])
}

/// 收集当前 crate graph 的 Static native libraries，并把每个独立归档转换成 `.so`。
///
/// 只实现 Linux/ELF 的受约束垂直切片。每个 archive 独立以 `-z defs` 链接，因此跨归档
/// 依赖、依赖顺序和非 PIC relocation 都会响亮失败；不会猜测一个通用 native link plan。
pub(crate) fn materialize_static_libraries(tcx: TyCtxt<'_>) -> Result<Vec<Box<str>>, String> {
    let sess = tcx.sess;
    let search_dirs: Vec<_> = sess
        .target_filesearch()
        .cli_search_paths(PathKind::Native)
        .map(|path| path.dir.clone())
        .collect();
    let target = sess.opts.target_triple.tuple();
    let cache = crate::sysroot::cache_dir().join("native-archives");
    let mut shared_objects = Vec::<PathBuf>::new();
    // crate 图系统动态库（c_libgit2 实锤：静态归档 C 对象引元数据传播的 `-l`
    // 库符号时，闭包链接行必须同样带上；与 lower 的 RTLD_GLOBAL 预载同清单）
    let extra_libs = system_dylibs(tcx);

    for cnum in std::iter::once(LOCAL_CRATE).chain(tcx.used_crates(()).iter().copied()) {
        if cnum != LOCAL_CRATE && tcx.crate_dep_kind(cnum).macros_only() {
            continue;
        }
        let crate_name = tcx.crate_name(cnum);
        for lib in tcx.native_libraries(cnum) {
            let NativeLibKind::Static { export_symbols, .. } = lib.kind else {
                continue;
            };
            if let Some(cfg) = &lib.cfg
                && !rustc_attr_parsing::eval_config_entry(sess, cfg).as_bool()
            {
                continue;
            }
            if target != env!("MIRVM_HOST")
                || sess.target.os != Os::Linux
                || sess.target.binary_format != BinaryFormat::Elf
            {
                return Err(format!(
                    "crate `{crate_name}` 的 Static native library `{}` 只能由当前 host \
                     Linux/ELF 归档装载切片处理（host: {}, 当前 target: {target}）",
                    lib.name,
                    env!("MIRVM_HOST")
                ));
            }
            if export_symbols.is_some() {
                return Err(format!(
                    "crate `{crate_name}` 的 Static native library `{}` 使用了 \
                     `+/-export-symbols` modifier；M5.1 归档装载尚未定义其 `.so` 等价语义",
                    lib.name
                ));
            }

            let verbatim = lib.verbatim.unwrap_or(false);
            let filename = if let Some(filename) = lib.filename {
                filename.as_str().to_owned()
            } else {
                let (prefix, suffix) = sess.staticlib_components(verbatim);
                format!("{prefix}{}{suffix}", lib.name)
            };
            let archive = find_archive(&filename, &search_dirs).ok_or_else(|| {
                let searched = search_dirs
                    .iter()
                    .map(|dir| dir.join(&filename).display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "找不到 crate `{crate_name}` 的 Static native library `{}`（文件 `{filename}`）；\
                     rustc native search paths: [{}]",
                    lib.name, searched
                )
            })?;
            let so = materialize_for_target_in(&archive, &cache, target, Path::new("cc"), &extra_libs)?;
            if !shared_objects.contains(&so) {
                shared_objects.push(so);
            }
        }
    }
    reject_symbol_ambiguity(&shared_objects)?;
    Ok(shared_objects
        .into_iter()
        .map(|path| path.display().to_string().into())
        .collect())
}

/// 物化期歧义拒绝：归档 **.dynsym 可见**导出符号不得**跨归档**重名（解析依赖
/// 装载顺序，M5.1 拒绝猜测 native linker 顺序）。
///
/// 与 **RTLD_DEFAULT 既有定义**的碰撞此前同列（①），自 dynsym 归档句柄优先
/// 解析后不再拒绝：解析序 ①hidden 兜底表 → ②归档句柄（链接序）→ ③dlsym
/// 全域，guest 链进的对象（hidden 或 dynsym 可见）恒胜宿主进程同名库——
/// native 链接期绑定语义（psm 的 rust_psm_on_stack vs 宿主 librustc_driver
/// 内嵌副本，corpus 批6 c_polars_frame 实锤；zstd-sys 的 ZSTD_* vs libLLVM
/// 内嵌库同族）。已知残余：归档**内部**对碰撞符号的跨引用仍经动态链接器
/// 全局序（mirror 不了，corpus 无此形态——psm 四符号均为 Rust 侧调用、
/// 内部无交叉引用）。
///
/// 不进 .dynsym 的 hidden 符号（.symtab 兜底表承载）刻意不做任何碰撞检查：
/// 解析序上恒先于全域，碰撞本就解析到归档，无歧义可拒。
fn reject_symbol_ambiguity(shared_objects: &[PathBuf]) -> Result<(), String> {
    let mut owners = HashMap::<String, PathBuf>::new();
    for shared_object in shared_objects {
        let output = Command::new("nm")
            .args(["--dynamic", "--defined-only", "--format=posix"])
            .arg(shared_object)
            .output()
            .map_err(|e| {
                format!(
                    "无法检查归档共享库 `{}` 的导出符号（启动 nm 失败）: {e}",
                    shared_object.display()
                )
            })?;
        if !output.status.success() {
            return Err(format!(
                "无法检查归档共享库 `{}` 的导出符号（nm 失败）:\n{}{}",
                shared_object.display(),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        let symbols = String::from_utf8(output.stdout).map_err(|e| {
            format!(
                "归档共享库 `{}` 的 nm 输出不是 UTF-8: {e}",
                shared_object.display()
            )
        })?;
        for symbol in symbols
            .lines()
            .filter_map(|line| line.split_ascii_whitespace().next())
        {
            std::ffi::CString::new(symbol).map_err(|_| {
                format!(
                    "归档共享库 `{}` 导出含 NUL 的非法符号名",
                    shared_object.display()
                )
            })?;
            if let Some(previous) = owners.insert(symbol.to_owned(), shared_object.clone()) {
                return Err(format!(
                    "静态归档导出符号 `{symbol}` 同时来自 `{}` 与 `{}`；运行期 dlsym \
                     解析将依赖装载顺序，M5.1 拒绝猜测 native linker 顺序",
                    previous.display(),
                    shared_object.display()
                ));
            }
        }
    }
    Ok(())
}

fn find_archive(filename: &str, search_dirs: &[PathBuf]) -> Option<PathBuf> {
    search_dirs
        .iter()
        .map(|dir| dir.join(filename))
        .find(|path| path.is_file())
}

fn materialize_for_target_in(
    archive: &Path,
    cache_dir: &Path,
    target: &str,
    cc: &Path,
    extra_libs: &[Box<str>],
) -> Result<PathBuf, String> {
    let bytes = std::fs::read(archive)
        .map_err(|e| format!("读取静态原生归档 `{}` 失败: {e}", archive.display()))?;
    if bytes.starts_with(b"!<thin>\n") {
        return Err(format!(
            "拒绝 thin 静态归档 `{}`：归档字节不包含成员 object，不能作为完整内容哈希缓存键",
            archive.display()
        ));
    }
    if !bytes.starts_with(b"!<arch>\n") {
        return Err(format!("`{}` 不是受支持的 Unix ar 归档", archive.display()));
    }
    reject_initializers(archive)?;
    let cc_identity = compiler_identity(cc)?;
    // extra_libs（crate 图系统动态库 `-l<name>`）同时进缓存键与 cc 链接行——
    // 名单变化必须换缓存槽，旧闭包不得误命中（c_libgit2 修复的键纪律）
    let extra_flags: Vec<String> = extra_libs.iter().map(|n| format!("-l{n}")).collect();
    let link_flags = LINK_PREFIX
        .iter()
        .chain(LINK_SUFFIX)
        .copied()
        .chain(extra_flags.iter().map(|s| s.as_str()))
        .collect::<Vec<_>>()
        .join("\0");
    let hash = content_hash([
        CACHE_FORMAT_VERSION,
        link_flags.as_bytes(),
        target.as_bytes(),
        &cc_identity,
        &bytes,
    ]);
    std::fs::create_dir_all(cache_dir)
        .map_err(|e| format!("创建原生归档缓存目录 `{}` 失败: {e}", cache_dir.display()))?;
    let so = cache_dir.join(format!("{hash}.so"));
    if so.exists() {
        return Ok(so);
    }

    let serial = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
    let tmp = cache_dir.join(format!("{hash}.so.tmp.{}.{serial}", std::process::id()));
    let output = Command::new(cc)
        .args(LINK_PREFIX)
        .arg(archive)
        .args(LINK_SUFFIX)
        .args(&extra_flags)
        .arg("-o")
        .arg(&tmp)
        .output()
        .map_err(|e| format!("启动 cc 转换 `{}` 失败: {e}", archive.display()))?;
    if !output.status.success() {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!(
            "静态原生归档 `{}` 无法安全转换为共享库（要求 ELF PIC、依赖在本归档内闭合）:\n{}{}",
            archive.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    std::fs::rename(&tmp, &so).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("原子发布原生归档缓存 `{}` 失败: {e}", so.display())
    })?;
    Ok(so)
}

fn compiler_identity(cc: &Path) -> Result<Vec<u8>, String> {
    let version = Command::new(cc)
        .arg("--version")
        .output()
        .map_err(|e| format!("无法查询 C 编译器 `{}` 版本: {e}", cc.display()))?;
    if !version.status.success() {
        return Err(format!(
            "查询 C 编译器 `{}` 版本失败: {}",
            cc.display(),
            String::from_utf8_lossy(&version.stderr)
        ));
    }
    let machine = Command::new(cc)
        .arg("-dumpmachine")
        .output()
        .map_err(|e| format!("无法查询 C 编译器 `{}` target: {e}", cc.display()))?;
    if !machine.status.success() {
        return Err(format!(
            "查询 C 编译器 `{}` target 失败: {}",
            cc.display(),
            String::from_utf8_lossy(&machine.stderr)
        ));
    }
    let first_line = version
        .stdout
        .split(|&b| b == b'\n')
        .next()
        .unwrap_or_default();
    let mut identity = first_line.to_vec();
    identity.push(0);
    identity.extend_from_slice(machine.stdout.trim_ascii());
    Ok(identity)
}

fn reject_initializers(archive: &Path) -> Result<(), String> {
    let output = Command::new("readelf")
        .args(["--section-headers", "--wide"])
        .arg(archive)
        .output()
        .map_err(|e| {
            format!(
                "无法检查静态原生归档 `{}` 的 constructor（启动 readelf 失败）: {e}",
                archive.display()
            )
        })?;
    if !output.status.success() {
        return Err(format!(
            "无法检查静态原生归档 `{}` 的 constructor（readelf 失败）:\n{}{}",
            archive.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let sections = String::from_utf8_lossy(&output.stdout);
    let has_lifecycle_section = sections
        .lines()
        .filter_map(readelf_section_name)
        .any(is_lifecycle_section);
    if has_lifecycle_section {
        return Err(format!(
            "拒绝带 constructor/destructor section 的静态原生归档 `{}`：\
             dlopen 生命周期语义尚未定义",
            archive.display()
        ));
    }
    Ok(())
}

fn is_lifecycle_section(name: &str) -> bool {
    if matches!(name, ".init" | ".fini") {
        return true;
    }
    [
        ".preinit_array",
        ".init_array",
        ".fini_array",
        ".ctors",
        ".dtors",
    ]
    .iter()
    .copied()
    .any(|prefix| {
        name == prefix
            || name
                .strip_prefix(prefix)
                .is_some_and(|suffix| suffix.starts_with('.'))
    })
}

fn readelf_section_name(line: &str) -> Option<&str> {
    let line = line.trim_start();
    if !line.starts_with('[') {
        return None;
    }
    let close = line.find(']')?;
    line[close + 1..].split_ascii_whitespace().next()
}

fn content_hash<'a>(parts: impl IntoIterator<Item = &'a [u8]>) -> String {
    let mut left = 0xcbf2_9ce4_8422_2325_u64;
    let mut right = 0x6c62_272e_07bb_0142_u64;
    for part in parts {
        for &byte in part {
            left ^= u64::from(byte);
            left = left.wrapping_mul(0x0000_0100_0000_01b3);
            right ^= u64::from(byte).wrapping_add(left.rotate_left(17));
            right = right.wrapping_mul(0x9e37_79b1_85eb_ca87);
        }
        left ^= 0xff;
        right ^= left.rotate_right(11);
    }
    format!("{left:016x}{right:016x}")
}

#[cfg(test)]
mod tests {
    use std::ffi::{CStr, CString};
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{materialize_for_target_in, materialize_in, reject_symbol_ambiguity};

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let id = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "mirvm-native-archive-test-{name}-{}-{id}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn make_archive(dir: &Path, source: &str) -> PathBuf {
        let source_path = dir.join("probe.c");
        let object_path = dir.join("probe.o");
        let archive_path = dir.join("libprobe.a");
        std::fs::write(&source_path, source).unwrap();
        let cc = Command::new("cc")
            .args(["-fPIC", "-c"])
            .arg(&source_path)
            .arg("-o")
            .arg(&object_path)
            .status()
            .unwrap();
        assert!(cc.success());
        let ar = Command::new("ar")
            .arg("crs")
            .arg(&archive_path)
            .arg(&object_path)
            .status()
            .unwrap();
        assert!(ar.success());
        archive_path
    }

    fn make_thin_archive(dir: &Path, source: &str) -> PathBuf {
        let source_path = dir.join("thin_probe.c");
        let object_path = dir.join("thin_probe.o");
        let archive_path = dir.join("libthin_probe.a");
        std::fs::write(&source_path, source).unwrap();
        let cc = Command::new("cc")
            .args(["-fPIC", "-c"])
            .arg(&source_path)
            .arg("-o")
            .arg(&object_path)
            .status()
            .unwrap();
        assert!(cc.success());
        let ar = Command::new("ar")
            .arg("crsT")
            .arg(&archive_path)
            .arg(&object_path)
            .status()
            .unwrap();
        assert!(ar.success());
        archive_path
    }

    fn make_non_pic_archive(dir: &Path, source: &str) -> PathBuf {
        let source_path = dir.join("non_pic_probe.c");
        let object_path = dir.join("non_pic_probe.o");
        let archive_path = dir.join("libnon_pic_probe.a");
        std::fs::write(&source_path, source).unwrap();
        let cc = Command::new("cc")
            .arg("-c")
            .arg(&source_path)
            .arg("-o")
            .arg(&object_path)
            .status()
            .unwrap();
        assert!(cc.success());
        let ar = Command::new("ar")
            .arg("crs")
            .arg(&archive_path)
            .arg(&object_path)
            .status()
            .unwrap();
        assert!(ar.success());
        archive_path
    }

    fn make_cc_wrapper(dir: &Path, name: &str, version: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo '{version}'; exit 0; fi\nexec cc \"$@\"\n"
            ),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).unwrap();
        path
    }

    #[test]
    fn unreferenced_archive_symbol_is_dlsym_visible() {
        let temp = TempDir::new("tracer");
        let archive = make_archive(
            temp.path(),
            "unsigned long mirvm_archive_probe(void) { return 0x51aUL; }\n",
        );

        let so = materialize_in(&archive, &temp.path().join("cache")).unwrap();
        let c_so = CString::new(so.as_os_str().as_encoded_bytes()).unwrap();
        let handle = unsafe { libc::dlopen(c_so.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        assert!(
            !handle.is_null(),
            "dlopen {} failed: {}",
            so.display(),
            unsafe { CStr::from_ptr(libc::dlerror()) }.to_string_lossy()
        );
        let symbol = c"mirvm_archive_probe";
        let address = unsafe { libc::dlsym(handle, symbol.as_ptr()) };
        assert!(
            !address.is_null(),
            "whole-archive did not export tracer symbol"
        );
        let probe: unsafe extern "C" fn() -> u64 = unsafe { std::mem::transmute(address) };
        assert_eq!(unsafe { probe() }, 0x51a);
        unsafe { libc::dlclose(handle) };
    }

    #[test]
    fn thin_archive_is_rejected_because_its_content_hash_is_incomplete() {
        let temp = TempDir::new("thin");
        let archive = make_thin_archive(
            temp.path(),
            "unsigned long mirvm_thin_probe(void) { return 7UL; }\n",
        );

        let error = materialize_in(&archive, &temp.path().join("cache")).unwrap_err();
        assert!(error.contains("thin"), "unexpected diagnostic: {error}");
    }

    #[test]
    fn archive_constructor_is_rejected_instead_of_running_during_dlopen() {
        let temp = TempDir::new("constructor");
        let archive = make_archive(
            temp.path(),
            "static void boot(void) __attribute__((constructor));\n\
             static void boot(void) {}\n\
             unsigned long mirvm_constructor_probe(void) { return 9UL; }\n",
        );

        let error = materialize_in(&archive, &temp.path().join("cache")).unwrap_err();
        assert!(
            error.contains("constructor"),
            "unexpected diagnostic: {error}"
        );
    }

    #[test]
    fn prioritized_constructor_section_is_also_rejected() {
        let temp = TempDir::new("priority-constructor");
        let archive = make_archive(
            temp.path(),
            "static void boot(void) __attribute__((constructor(101)));\n\
             static void boot(void) {}\n\
             unsigned long mirvm_priority_constructor_probe(void) { return 17UL; }\n",
        );

        let error = materialize_in(&archive, &temp.path().join("cache")).unwrap_err();
        assert!(
            error.contains("constructor"),
            "unexpected diagnostic: {error}"
        );
    }

    #[test]
    fn legacy_elf_init_and_fini_sections_are_rejected() {
        let temp = TempDir::new("legacy-init-fini");
        for section in [".init", ".fini"] {
            let dir = temp.path().join(section.trim_start_matches('.'));
            std::fs::create_dir_all(&dir).unwrap();
            let archive = make_archive(
                &dir,
                &format!(
                    "__attribute__((used, section(\"{section}\"))) \
                     void mirvm_lifecycle_hook(void) {{}}\n"
                ),
            );

            let error = materialize_in(&archive, &dir.join("cache")).unwrap_err();
            assert!(
                error.contains("constructor/destructor"),
                "section {section} was not rejected: {error}"
            );
        }
    }

    #[test]
    fn dot_init_in_archive_path_is_not_mistaken_for_a_section() {
        let temp = TempDir::new("section-parser");
        let dir = temp.path().join("ordinary.init.path");
        std::fs::create_dir_all(&dir).unwrap();
        let archive = make_archive(
            &dir,
            "unsigned long mirvm_not_a_constructor(void) { return 23UL; }\n",
        );

        materialize_in(&archive, &dir.join("cache"))
            .expect("`.init` in a path must not be parsed as an ELF section");
    }

    #[test]
    fn cache_key_separates_target_triples() {
        let temp = TempDir::new("target-key");
        let archive = make_archive(
            temp.path(),
            "unsigned long mirvm_target_key_probe(void) { return 11UL; }\n",
        );
        let cache = temp.path().join("cache");

        let first = materialize_for_target_in(
            &archive,
            &cache,
            "x86_64-unknown-linux-gnu",
            Path::new("cc"),
            &[],
        )
        .unwrap();
        let second = materialize_for_target_in(
            &archive,
            &cache,
            "aarch64-unknown-linux-gnu",
            Path::new("cc"),
            &[],
        )
        .unwrap();
        assert_ne!(first, second, "target triple must participate in cache key");
    }

    #[test]
    fn unresolved_archive_dependency_fails_during_materialization() {
        let temp = TempDir::new("unresolved");
        let archive = make_archive(
            temp.path(),
            "extern unsigned long mirvm_missing_dependency(void);\n\
             unsigned long mirvm_dependency_probe(void) { return mirvm_missing_dependency(); }\n",
        );

        let error = materialize_in(&archive, &temp.path().join("cache")).unwrap_err();
        assert!(error.contains("依赖"), "unexpected diagnostic: {error}");
        assert!(
            error.contains("mirvm_missing_dependency"),
            "linker detail lost: {error}"
        );
    }

    #[test]
    fn non_pic_archive_fails_during_materialization() {
        let temp = TempDir::new("non-pic");
        let archive = make_non_pic_archive(
            temp.path(),
            "unsigned long mirvm_non_pic_global = 13UL;\n\
             unsigned long mirvm_non_pic_probe(void) { return mirvm_non_pic_global; }\n",
        );

        let error = materialize_in(&archive, &temp.path().join("cache")).unwrap_err();
        assert!(error.contains("PIC"), "unexpected diagnostic: {error}");
        assert!(error.contains("relocation"), "linker detail lost: {error}");
    }

    #[test]
    fn cache_key_separates_c_compiler_identities() {
        let temp = TempDir::new("cc-key");
        let archive = make_archive(
            temp.path(),
            "unsigned long mirvm_cc_key_probe(void) { return 17UL; }\n",
        );
        let cc_a = make_cc_wrapper(temp.path(), "cc-a", "mirvm test cc A");
        let cc_b = make_cc_wrapper(temp.path(), "cc-b", "mirvm test cc B");
        let cache = temp.path().join("cache");

        let first =
            materialize_for_target_in(&archive, &cache, "x86_64-unknown-linux-gnu", &cc_a, &[]).unwrap();
        let second =
            materialize_for_target_in(&archive, &cache, "x86_64-unknown-linux-gnu", &cc_b, &[]).unwrap();
        assert_ne!(
            first, second,
            "C compiler identity must participate in cache key"
        );
    }

    #[test]
    fn cache_key_separates_extra_libs() {
        let temp = TempDir::new("extra-libs-key");
        let archive = make_archive(
            temp.path(),
            "unsigned long mirvm_extra_libs_probe(void) { return 29UL; }\n",
        );
        let cache = temp.path().join("cache");
        let none: &[Box<str>] = &[];
        let with_m: &[Box<str>] = &["m".into()];
        let first =
            materialize_for_target_in(&archive, &cache, "x86_64-unknown-linux-gnu", Path::new("cc"), none)
                .unwrap();
        let second =
            materialize_for_target_in(&archive, &cache, "x86_64-unknown-linux-gnu", Path::new("cc"), with_m)
                .unwrap();
        assert_ne!(first, second, "extra libs 名单必须参与缓存键");
    }

    #[test]
    fn duplicate_symbols_across_archives_are_rejected_as_link_order_ambiguity() {
        let temp = TempDir::new("duplicate-symbol");
        let first_dir = temp.path().join("first");
        let second_dir = temp.path().join("second");
        std::fs::create_dir_all(&first_dir).unwrap();
        std::fs::create_dir_all(&second_dir).unwrap();
        let first_archive = make_archive(
            &first_dir,
            "unsigned long mirvm_duplicate_symbol(void) { return 1UL; }\n",
        );
        let second_archive = make_archive(
            &second_dir,
            "unsigned long mirvm_duplicate_symbol(void) { return 2UL; }\n",
        );
        let cache = temp.path().join("cache");
        let first = materialize_for_target_in(
            &first_archive,
            &cache,
            "x86_64-unknown-linux-gnu",
            Path::new("cc"),
            &[],
        )
        .unwrap();
        let second = materialize_for_target_in(
            &second_archive,
            &cache,
            "x86_64-unknown-linux-gnu",
            Path::new("cc"),
            &[],
        )
        .unwrap();

        let error = reject_symbol_ambiguity(&[first, second]).unwrap_err();
        assert!(
            error.contains("mirvm_duplicate_symbol"),
            "unexpected diagnostic: {error}"
        );
        assert!(error.contains("顺序"), "unexpected diagnostic: {error}");
    }

    #[test]
    fn symbol_already_in_process_is_accepted_under_handle_first_resolution() {
        // dynsym 归档句柄优先解析落地后的新语义：归档导出符号与进程既有定义
        // （此处特意用 malloc，进程必有定义）碰撞不再拒绝——归档句柄恒先命中，
        // native 链接期绑定可复现（guest 自己的对象恒胜宿主同名库）。
        let temp = TempDir::new("process-symbol");
        let archive = make_archive(
            temp.path(),
            "void *malloc(unsigned long size) { (void)size; return (void *)0; }\n",
        );
        let shared_object = materialize_in(&archive, &temp.path().join("cache")).unwrap();

        reject_symbol_ambiguity(&[shared_object]).unwrap();
    }
}
