//! `cargoless/schedule.rs` —— 拓扑排序 + 指纹 + 每 crate rustc 参数计算
//! （D15 P2 切①/②，设计档 §3.6）。
//!
//! 产物布局：`cache_dir()/target/cargoless/<MIRVM_HOST>/debug/{deps,host-deps}`
//! ——target 产物（deps）与 host 产物（host-deps：proc-macro 闭包，真 rustc
//! 真 codegen）分目录，同名 fp 不撞；与 cargo 路径的 `target/mirvm` 双轨并存
//! （P2→P4 迁移期两条路径互不踩产物）。
//! 产物命名 `lib<lib_name>-<fp>.{rmeta,rlib,so}`：fp 是本文件的自定方案（cargo 的
//! -C metadata 算法不稳定不追，设计档 §3.6——cargo 已退场，内部一致即可）。

use std::collections::{BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

use super::manifest::{PackageManifest, ProfileFlags};
use super::resolve::{ResolvePlan, Unit};

/// 产物布局（见文件头）。
pub struct Layout {
    /// target 产物目录（__cless-dep，-Zno-codegen rlib）。
    pub deps: PathBuf,
    /// host 产物目录（proc-macro 及其依赖闭包，真 rustc 真 codegen）。
    pub host_deps: PathBuf,
}

impl Layout {
    pub fn new() -> Self {
        let base = crate::sysroot::cache_dir()
            .join("target/cargoless")
            .join(env!("MIRVM_HOST"))
            .join("debug");
        Self {
            deps: base.join("deps"),
            host_deps: base.join("host-deps"),
        }
    }
}

/// Kahn 拓扑序：依赖先于依赖者。返回 units 下标序。
pub fn topo_order(plan: &ResolvePlan) -> Result<Vec<usize>, String> {
    let n = plan.units.len();
    let mut indeg = vec![0usize; n];
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); n]; // 反向边：dep → 依赖者
    for (i, u) in plan.units.iter().enumerate() {
        for d in &u.deps {
            indeg[i] += 1;
            dependents[d.unit].push(i);
        }
    }
    let mut queue: VecDeque<usize> = (0..n).filter(|&i| indeg[i] == 0).collect();
    let mut order = Vec::with_capacity(n);
    while let Some(i) = queue.pop_front() {
        order.push(i);
        for &j in &dependents[i] {
            indeg[j] -= 1;
            if indeg[j] == 0 {
                queue.push_back(j);
            }
        }
    }
    if order.len() != n {
        return Err("内部不一致：编译单元依赖图有环（cargo 解析图本应为 DAG）".into());
    }
    Ok(order)
}

/// host 闭包（切②）：从每个 proc-macro unit 沿 dep 边 BFS 的可达集（含
/// proc-macro 自身）。Build 类 unit 已被 strip_build_units 剥掉，闭包内全是
/// Normal 边。闭包单元用真 rustc 真 codegen 编成 host 产物。
pub fn host_closure(plan: &ResolvePlan) -> BTreeSet<usize> {
    let mut set = BTreeSet::new();
    let mut stack: Vec<usize> = plan
        .units
        .iter()
        .enumerate()
        .filter(|(_, u)| u.proc_macro)
        .map(|(i, _)| i)
        .collect();
    while let Some(i) = stack.pop() {
        if set.insert(i) {
            stack.extend(plan.units[i].deps.iter().map(|d| d.unit));
        }
    }
    set
}

/// target 编译集（切②）：从根边出发 BFS；proc-macro unit 对 target 是叶子
/// ——不进集、也不钻入其内部（它的依赖是 host 依赖，不是根的 target 依赖；
/// cargo 也不为 proc-macro crate 产 target rlib）。与 host 闭包可相交：
/// 同一 unit 同时被 bin 与 proc-macro 用时两侧都编，双份产物分目录互不影响。
pub fn target_units(plan: &ResolvePlan) -> BTreeSet<usize> {
    let mut set = BTreeSet::new();
    let mut stack: Vec<usize> = plan.root_deps.iter().map(|d| d.unit).collect();
    while let Some(i) = stack.pop() {
        if plan.units[i].proc_macro {
            continue;
        }
        if set.insert(i) {
            stack.extend(plan.units[i].deps.iter().map(|d| d.unit));
        }
    }
    set
}

/// 每 unit 的内容指纹（按 topo 序算——dep 的 fp 先于依赖者产出）。
/// fp(unit) = fnv1a(BUILD_ID, package, version, edition, 排序后 features,
/// profile 三员, sysroot_stamp, 源 stamp, **排序后各 dep 的 fp**)。
/// 最后这枚必须含：depsimage pre-key 的「传递闭包变更 ⇒ 直接依赖产物盖戳变」
/// 不变量靠它传播——传递 dep 的 fp 变 ⇒ 直接 dep 的 fp 变 ⇒ 其产物文件名变
/// ⇒ bin 的 --extern 盖戳变（depsimage.rs 头注同款语义；钉死，勿删）。
/// 无 dep 源变化但 lock 版本集变化时，版本字段已覆盖。
pub fn fingerprints(
    plan: &ResolvePlan,
    profile: &ProfileFlags,
    sysroot_stamp: &str,
) -> Result<Vec<String>, String> {
    let order = topo_order(plan)?;
    let mut fps: Vec<Option<String>> = vec![None; plan.units.len()];
    for &ix in &order {
        let u = &plan.units[ix];
        let mut dep_fps: Vec<String> = u
            .deps
            .iter()
            .map(|d| fps[d.unit].clone().expect("topo 序保证 dep fp 先算"))
            .collect();
        dep_fps.sort();
        let src_stamp = source_stamp(u)?;
        let mut key = String::from(env!("MIRVM_BUILD_ID"));
        let mut put = |s: &str| {
            key.push('\u{1f}');
            key.push_str(s);
        };
        put(&u.package);
        put(&u.version.to_string());
        put(&u.edition);
        for f in &u.features {
            put(f); // BTreeSet 迭代即字典序
        }
        put(if profile.debug_assertions {
            "da1"
        } else {
            "da0"
        });
        put(if profile.overflow_checks {
            "oc1"
        } else {
            "oc0"
        });
        put(&profile.opt_level.to_string());
        put(sysroot_stamp);
        put(&src_stamp);
        for d in &dep_fps {
            put(d);
        }
        fps[ix] = Some(format!("{:016x}", crate::lower::asm::fnv1a(key.as_bytes())));
    }
    Ok(fps.into_iter().map(|f| f.expect("全序已填")).collect())
}

/// 源 stamp：registry 单元 = 字面 "registry"（源按 cksum 不可变不盖戳——.crate
/// 解包树的 mtime 是解包时刻，盖了只会制造无谓重建；版本号已覆盖内容）；
/// path 单元 = 递归遍历 source_dir（排除 target/ 与 .git/）全部文件的
/// (相对路径, len, mtime_ns) 排序后折叠。
fn source_stamp(u: &Unit) -> Result<String, String> {
    if u.from_registry {
        return Ok("registry".to_string());
    }
    let mut rows: Vec<String> = Vec::new();
    let mut stack = vec![u.source_dir.clone()];
    while let Some(dir) = stack.pop() {
        let rd = std::fs::read_dir(&dir).map_err(|e| {
            format!(
                "path 依赖 {} 源目录读取失败 {}: {e}",
                u.package,
                dir.display()
            )
        })?;
        for ent in rd {
            let ent =
                ent.map_err(|e| format!("path 依赖 {} 源目录条目读取失败: {e}", u.package))?;
            let p = ent.path();
            if p.is_dir() {
                if ent.file_name() == "target" || ent.file_name() == ".git" {
                    continue;
                }
                stack.push(p);
            } else if p.is_file() {
                let md = std::fs::metadata(&p).map_err(|e| {
                    format!(
                        "path 依赖 {} 源文件 stat 失败 {}: {e}",
                        u.package,
                        p.display()
                    )
                })?;
                let rel = p.strip_prefix(&u.source_dir).unwrap_or(&p);
                let mtime_ns = md
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                rows.push(format!("{}:{}:{}", rel.display(), md.len(), mtime_ns));
            }
        }
    }
    rows.sort();
    Ok(rows.join("\u{1e}"))
}

/// profile 三旗（cargo dev profile 语义钉，设计档 §6 的 jiff 判例：
/// debug-assertions/overflow-checks 进 MIR 语义，错配 = 对拍漂移）。
fn push_profile_flags(a: &mut Vec<String>, p: &ProfileFlags) {
    let yn = |b: bool| if b { "yes" } else { "no" };
    a.push("-C".into());
    a.push(format!("debug-assertions={}", yn(p.debug_assertions)));
    a.push("-C".into());
    a.push(format!("overflow-checks={}", yn(p.overflow_checks)));
    if p.opt_level != 0 {
        a.push("-C".into());
        a.push(format!("opt-level={}", p.opt_level));
    }
}

/// 真 rustc 绝对路径：编译期烘焙的默认 sysroot 自带（manifest.rs
/// host_cfg_atoms 同款取法）。host 侧编译只信它——PATH 上的 rustc 可能是
/// 别的工具链；proc-macro dylib 与解释会话的编译器版本必须严格一致
/// （cargo_shim.rs wrapper 段同款纪律）。
fn real_rustc() -> String {
    PathBuf::from(env!("MIRVM_DEFAULT_SYSROOT"))
        .join("bin/rustc")
        .display()
        .to_string()
}

/// 一条 dep 边的 --extern 目标路径分派：dep 是 proc-macro → host-deps 的
/// dylib（消费方编译期 dlopen 展开宏）；否则指本侧目录的 ext 产物。
fn extern_path(layout: &Layout, dir: &Path, du: &Unit, fp: &str, ext: &str) -> String {
    if du.proc_macro {
        format!(
            "{}/lib{}-{}{}",
            layout.host_deps.display(),
            du.lib_name,
            fp,
            std::env::consts::DLL_SUFFIX
        )
    } else {
        format!("{}/lib{}-{}.{}", dir.display(), du.lib_name, fp, ext)
    }
}

/// 一个 dep unit 的 rustc 参数（driver 起 `__cless-dep` 子进程喂
/// cli::run_dep_compiler；形态对齐 cargo 对 target 依赖的调用 +
/// cargo_shim.rs wrapper 段的 MIR sysroot/-Z 注入）。
/// argv0 = "mirvm-cless-rustc"（driver 起子进程时剥掉补真名）。
pub fn dep_rustc_args(
    plan: &ResolvePlan,
    unit_ix: usize,
    profile: &ProfileFlags,
    fps: &[String],
    sysroot: &Path,
    layout: &Layout,
) -> Vec<String> {
    let u = &plan.units[unit_ix];
    let fp = &fps[unit_ix];
    let deps = layout.deps.display();
    let mut a: Vec<String> = vec!["mirvm-cless-rustc".into()];
    a.push("--crate-name".into());
    a.push(u.lib_name.clone());
    a.push(format!("--edition={}", u.edition));
    a.push(u.lib_path.display().to_string());
    a.push("--crate-type=lib".into());
    a.push("--emit=dep-info,metadata,link".into());
    a.push("-C".into());
    a.push("embed-bitcode=no".into());
    a.push("-C".into());
    a.push("debuginfo=2".into());
    for f in &u.features {
        a.push("--cfg".into());
        a.push(format!("feature=\"{f}\""));
    }
    if u.from_registry {
        // registry 代码不归用户改，lint 全哑（cargo 同）；path 依赖照常告警
        a.push("--cap-lints".into());
        a.push("allow".into());
    }
    a.push("--check-cfg".into());
    a.push("cfg(docsrs,test)".into());
    push_profile_flags(&mut a, profile);
    a.push("-C".into());
    a.push(format!("metadata={fp}"));
    // extra-filename 必须作为 -C 的下一个独立 argv 槽——run_dep_compiler 按
    // 窗口（两个独立槽）抠它定 rlib 主名
    a.push("-C".into());
    a.push(format!("extra-filename=-{fp}"));
    a.push("--out-dir".into());
    a.push(deps.to_string());
    a.push("-L".into());
    a.push(format!("dependency={deps}"));
    for d in &u.deps {
        let du = &plan.units[d.unit];
        // proc-macro 边指 host-deps 的 dylib；普通边照旧 target .rmeta
        a.push("--extern".into());
        a.push(format!(
            "{}={}",
            d.key.replace('-', "_"),
            extern_path(layout, &layout.deps, du, &fps[d.unit], "rmeta")
        ));
    }
    a.push("--sysroot".into());
    a.push(sysroot.display().to_string());
    a.push("-Zalways-encode-mir".into());
    a.push("-Zno-codegen".into());
    a
}

/// bin（根 crate）会话参数——走既有 MirvmCallbacks 降低通道（after_analysis
/// 停，Compilation::Stop，零产物）：**不**加 -Z 旗、--out-dir、-C metadata
/// （与 cargo 路径 runner 段的 bin 会话同形态）。
pub fn bin_rustc_args(
    manifest: &PackageManifest,
    plan: &ResolvePlan,
    fps: &[String],
    sysroot: &Path,
    layout: &Layout,
    bin_name: &str,
    bin_path: &Path,
) -> Vec<String> {
    let deps = layout.deps.display();
    let mut a: Vec<String> = vec!["mirvm".into()];
    a.push(bin_path.display().to_string());
    a.push("--crate-name".into());
    a.push(bin_name.replace('-', "_"));
    a.push(format!("--edition={}", manifest.edition));
    a.push("--crate-type=bin".into());
    a.push("-C".into());
    a.push("embed-bitcode=no".into());
    a.push("-C".into());
    a.push("debuginfo=2".into());
    for f in &plan.root_features {
        a.push("--cfg".into());
        a.push(format!("feature=\"{f}\""));
    }
    a.push("--check-cfg".into());
    a.push("cfg(docsrs,test)".into());
    let values = manifest.check_cfg_feature_values();
    let vals = values
        .iter()
        .map(|v| format!("\"{v}\""))
        .collect::<Vec<_>>()
        .join(",");
    a.push("--check-cfg".into());
    a.push(format!("cfg(feature, values({vals}))"));
    push_profile_flags(&mut a, &manifest.profile);
    for d in &plan.root_deps {
        let du = &plan.units[d.unit];
        // bin 侧 --extern 用 .rlib（对齐 cargo 的最终 crate 调用形态）；
        // proc-macro 根边指 host-deps 的 dylib
        a.push("--extern".into());
        a.push(format!(
            "{}={}",
            d.key.replace('-', "_"),
            extern_path(layout, &layout.deps, du, &fps[d.unit], "rlib")
        ));
    }
    a.push("-L".into());
    a.push(format!("dependency={deps}"));
    a.push("--sysroot".into());
    a.push(sysroot.display().to_string());
    a
}

/// host 闭包普通单元的真 rustc 参数（切②，proc-macro2 实锤形态）：
/// `--crate-type lib --emit=dep-info,metadata,link -C embed-bitcode=no`
/// （**无 debuginfo、无 prefer-dynamic**）真 codegen 产 host rlib；
/// dep 边指 host-deps 的 .rmeta（proc-macro 边指 .so）。
/// **不带 --sysroot**（真 rustc 用自家 sysroot）、不带任何 -Z。
/// argv0 = 真 rustc 绝对路径（driver 直接 spawn，不经 __cless-dep）。
pub fn host_rustc_args(
    plan: &ResolvePlan,
    unit_ix: usize,
    profile: &ProfileFlags,
    fps: &[String],
    layout: &Layout,
) -> Vec<String> {
    let u = &plan.units[unit_ix];
    let fp = &fps[unit_ix];
    let host = layout.host_deps.display();
    let mut a: Vec<String> = vec![real_rustc()];
    a.push("--crate-name".into());
    a.push(u.lib_name.clone());
    a.push(format!("--edition={}", u.edition));
    a.push(u.lib_path.display().to_string());
    a.push("--crate-type=lib".into());
    a.push("--emit=dep-info,metadata,link".into());
    a.push("-C".into());
    a.push("embed-bitcode=no".into());
    for f in &u.features {
        a.push("--cfg".into());
        a.push(format!("feature=\"{f}\""));
    }
    if u.from_registry {
        // registry 代码不归用户改，lint 全哑（cargo 同）；path 依赖照常告警
        a.push("--cap-lints".into());
        a.push("allow".into());
    }
    a.push("--check-cfg".into());
    a.push("cfg(docsrs,test)".into());
    push_profile_flags(&mut a, profile);
    a.push("-C".into());
    a.push(format!("metadata={fp}"));
    a.push("-C".into());
    a.push(format!("extra-filename=-{fp}"));
    a.push("--out-dir".into());
    a.push(host.to_string());
    a.push("-L".into());
    a.push(format!("dependency={host}"));
    for d in &u.deps {
        let du = &plan.units[d.unit];
        a.push("--extern".into());
        a.push(format!(
            "{}={}",
            d.key.replace('-', "_"),
            extern_path(layout, &layout.host_deps, du, &fps[d.unit], "rmeta")
        ));
    }
    a
}

/// proc-macro crate 本体的真 rustc 参数（切②，serde_derive 实锤五钉）：
/// `--crate-type proc-macro --emit=dep-info,link -C prefer-dynamic
/// -C embed-bitcode=no`（**无 debuginfo**）+ 末尾裸 `--extern proc_macro`
/// （编译器内建桥 crate）；dep 边指 host-deps 的 .rlib（**真链接**进 dylib）。
/// 不带 --sysroot/-Z；argv0 = 真 rustc 绝对路径。
pub fn proc_macro_rustc_args(
    plan: &ResolvePlan,
    unit_ix: usize,
    profile: &ProfileFlags,
    fps: &[String],
    layout: &Layout,
) -> Vec<String> {
    let u = &plan.units[unit_ix];
    let fp = &fps[unit_ix];
    let host = layout.host_deps.display();
    let mut a: Vec<String> = vec![real_rustc()];
    a.push("--crate-name".into());
    a.push(u.lib_name.clone());
    a.push(format!("--edition={}", u.edition));
    a.push(u.lib_path.display().to_string());
    a.push("--crate-type=proc-macro".into());
    a.push("--emit=dep-info,link".into());
    a.push("-C".into());
    a.push("prefer-dynamic".into());
    a.push("-C".into());
    a.push("embed-bitcode=no".into());
    for f in &u.features {
        a.push("--cfg".into());
        a.push(format!("feature=\"{f}\""));
    }
    if u.from_registry {
        a.push("--cap-lints".into());
        a.push("allow".into());
    }
    a.push("--check-cfg".into());
    a.push("cfg(docsrs,test)".into());
    push_profile_flags(&mut a, profile);
    a.push("-C".into());
    a.push(format!("metadata={fp}"));
    a.push("-C".into());
    a.push(format!("extra-filename=-{fp}"));
    a.push("--out-dir".into());
    a.push(host.to_string());
    a.push("-L".into());
    a.push(format!("dependency={host}"));
    for d in &u.deps {
        let du = &plan.units[d.unit];
        a.push("--extern".into());
        a.push(format!(
            "{}={}",
            d.key.replace('-', "_"),
            extern_path(layout, &layout.host_deps, du, &fps[d.unit], "rlib")
        ));
    }
    // 末尾裸 --extern proc_macro：编译器内建桥，从真 rustc 自家 sysroot 解析
    a.push("--extern".into());
    a.push("proc_macro".into());
    a
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cargoless::lockfile::Lockfile;
    use crate::cargoless::resolve::{UnitClass, UnitDep};
    use semver::Version;
    use std::collections::{BTreeMap, BTreeSet};

    fn unit(
        name: &str,
        version: &str,
        from_registry: bool,
        features: &[&str],
        deps: Vec<UnitDep>,
    ) -> Unit {
        Unit {
            package: name.to_string(),
            lib_name: name.replace('-', "_"),
            version: Version::parse(version).unwrap(),
            source_dir: PathBuf::from(format!("/tmp/{name}")),
            from_registry,
            class: UnitClass::Normal,
            features: features
                .iter()
                .map(|f| f.to_string())
                .collect::<BTreeSet<_>>(),
            proc_macro: false,
            has_build_script: false,
            links: None,
            deps,
            edition: "2021".to_string(),
            lib_path: PathBuf::from(format!("/tmp/{name}/src/lib.rs")),
            pkg_env: BTreeMap::new(),
        }
    }

    fn plan_with(units: Vec<Unit>, root_deps: Vec<UnitDep>) -> ResolvePlan {
        ResolvePlan {
            root_name: "demo".to_string(),
            root_version: Version::new(0, 1, 0),
            root_dir: PathBuf::from("/tmp/demo"),
            root_features: BTreeSet::new(),
            units,
            root_deps,
            version_map: BTreeMap::new(),
            lock: Lockfile::default(),
        }
    }

    fn diamond_plan() -> ResolvePlan {
        // b、c 依赖 a；根依赖 b、c
        let a = unit("a", "1.0.0", true, &["std"], vec![]);
        let b = unit(
            "b",
            "1.0.0",
            true,
            &[],
            vec![UnitDep {
                key: "a".into(),
                unit: 0,
                class: UnitClass::Normal,
            }],
        );
        let c = unit(
            "c",
            "1.0.0",
            true,
            &[],
            vec![UnitDep {
                key: "a".into(),
                unit: 0,
                class: UnitClass::Normal,
            }],
        );
        plan_with(
            vec![a, b, c],
            vec![
                UnitDep {
                    key: "b".into(),
                    unit: 1,
                    class: UnitClass::Normal,
                },
                UnitDep {
                    key: "c".into(),
                    unit: 2,
                    class: UnitClass::Normal,
                },
            ],
        )
    }

    fn layout() -> Layout {
        Layout {
            deps: PathBuf::from("/tmp/cless/deps"),
            host_deps: PathBuf::from("/tmp/cless/host-deps"),
        }
    }

    #[test]
    fn topo_order_puts_deps_before_dependents() {
        let plan = diamond_plan();
        let order = topo_order(&plan).unwrap();
        let pos = |i| order.iter().position(|&x| x == i).unwrap();
        assert!(pos(0) < pos(1), "a 必须先于 b: {order:?}");
        assert!(pos(0) < pos(2), "a 必须先于 c: {order:?}");
        assert_eq!(order.len(), 3);
    }

    #[test]
    fn fingerprint_propagates_transitive_dep_change() {
        let plan = diamond_plan();
        let fps0 = fingerprints(&plan, &ProfileFlags::default(), "stamp0").unwrap();
        // b 自身字段一字不动，只改其传递 dep a 的 feature 集 ⇒ b 的 fp 必须变
        // （depsimage pre-key「传递闭包变更 ⇒ 直接依赖产物盖戳变」不变量）
        let mut plan2 = diamond_plan();
        plan2.units[0].features.insert("alloc".to_string());
        let fps1 = fingerprints(&plan2, &ProfileFlags::default(), "stamp0").unwrap();
        assert_ne!(fps0[0], fps1[0], "a 自身 fp 应变");
        assert_ne!(fps0[1], fps1[1], "a 变 ⇒ b 的 fp 必须变（传递传播）");
        assert_ne!(fps0[2], fps1[2], "a 变 ⇒ c 的 fp 必须变（传递传播）");
        // sysroot_stamp 与 profile 也进 fp
        let fps2 = fingerprints(&plan, &ProfileFlags::default(), "stamp1").unwrap();
        assert_ne!(fps0[0], fps2[0], "sysroot_stamp 进 fp");
        let relaxed = ProfileFlags {
            debug_assertions: false,
            overflow_checks: false,
            opt_level: 2,
        };
        let fps3 = fingerprints(&plan, &relaxed, "stamp0").unwrap();
        assert_ne!(fps0[0], fps3[0], "profile 三员进 fp");
    }

    #[test]
    fn dep_args_carry_key_flags_and_window_shaped_extra_filename() {
        let plan = diamond_plan();
        let fps = fingerprints(&plan, &ProfileFlags::default(), "s").unwrap();
        let lo = layout();
        let a = dep_rustc_args(
            &plan,
            1,
            &ProfileFlags::default(),
            &fps,
            Path::new("/sys"),
            &lo,
        );
        assert_eq!(a[0], "mirvm-cless-rustc");
        assert!(a.windows(2).any(|w| w[0] == "--crate-name" && w[1] == "b"));
        assert!(a.iter().any(|x| x == "--edition=2021"));
        assert!(a.iter().any(|x| x == "--crate-type=lib"));
        assert!(a.iter().any(|x| x == "--emit=dep-info,metadata,link"));
        // registry 单元 cap-lints；feature --cfg 两槽形态
        assert!(
            a.windows(2)
                .any(|w| w[0] == "--cap-lints" && w[1] == "allow")
        );
        assert!(
            a.windows(2)
                .any(|w| w[0] == "--check-cfg" && w[1] == "cfg(docsrs,test)")
        );
        // extra-filename 必须是 -C 的下一个独立 argv 槽（run_dep_compiler 窗口抠取）
        let want = format!("extra-filename=-{}", fps[1]);
        assert!(
            a.windows(2).any(|w| w[0] == "-C" && w[1] == want),
            "缺 -C/extra-filename 窗口: {a:?}"
        );
        assert!(
            a.windows(2)
                .any(|w| w[0] == "-C" && w[1] == format!("metadata={}", fps[1]))
        );
        // --extern 指向 dep 的 .rmeta（两槽 cargo 形态）
        let ext = format!("a=/tmp/cless/deps/liba-{}.rmeta", fps[0]);
        assert!(
            a.windows(2).any(|w| w[0] == "--extern" && w[1] == ext),
            "缺 --extern: {a:?}"
        );
        assert!(a.windows(2).any(|w| w[0] == "--sysroot" && w[1] == "/sys"));
        assert!(a.iter().any(|x| x == "-Zalways-encode-mir"));
        assert!(a.iter().any(|x| x == "-Zno-codegen"));
        // dev profile 默认：debug-assertions/overflow-checks 开，无 opt-level
        assert!(
            a.windows(2)
                .any(|w| w[0] == "-C" && w[1] == "debug-assertions=yes")
        );
        assert!(
            a.windows(2)
                .any(|w| w[0] == "-C" && w[1] == "overflow-checks=yes")
        );
        assert!(!a.iter().any(|x| x.starts_with("opt-level")));
    }

    #[test]
    fn bin_args_use_rlib_and_skip_z_flags() {
        let mut plan = diamond_plan();
        plan.root_features.insert("std".to_string());
        let fps = fingerprints(&plan, &ProfileFlags::default(), "s").unwrap();
        let lo = layout();
        let manifest = PackageManifest::parse(
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
             [dependencies]\nb = \"1\"\n",
            Path::new("/tmp/demo"),
        )
        .unwrap();
        let a = bin_rustc_args(
            &manifest,
            &plan,
            &fps,
            Path::new("/sys"),
            &lo,
            "demo-bin",
            Path::new("/tmp/demo/src/main.rs"),
        );
        assert_eq!(a[0], "mirvm");
        assert!(
            a.windows(2)
                .any(|w| w[0] == "--crate-name" && w[1] == "demo_bin")
        );
        assert!(a.iter().any(|x| x == "--crate-type=bin"));
        assert!(
            a.windows(2)
                .any(|w| w[0] == "--cfg" && w[1] == "feature=\"std\"")
        );
        assert!(
            a.windows(2)
                .any(|w| w[0] == "--check-cfg" && w[1] == "cfg(docsrs,test)")
        );
        assert!(
            a.windows(2)
                .any(|w| w[0] == "--check-cfg" && w[1].starts_with("cfg(feature, values(")),
            "缺 feature 值表 check-cfg: {a:?}"
        );
        // 根边 --extern 用 .rlib；无 -Z 旗、无 --out-dir、无 -C metadata
        let ext = format!("b=/tmp/cless/deps/libb-{}.rlib", fps[1]);
        assert!(a.windows(2).any(|w| w[0] == "--extern" && w[1] == ext));
        assert!(!a.iter().any(|x| x.starts_with("-Z")));
        assert!(!a.iter().any(|x| x == "--out-dir"));
        assert!(
            !a.windows(2)
                .any(|w| w[0] == "-C" && w[1].starts_with("metadata="))
        );
    }

    /// proc-macro 场景（serde 家族形状）：shared 双用（bin 与 my_derive 都
    /// 用）、pm_helper 仅 host、my_derive = proc-macro、uses_pm 是带
    /// proc-macro 边的普通 target dep。
    fn pm_plan() -> ResolvePlan {
        let shared = unit("shared", "1.0.0", true, &[], vec![]);
        let dep = |key: &str, unit: usize| UnitDep {
            key: key.into(),
            unit,
            class: UnitClass::Normal,
        };
        let pm_helper = unit("pm-helper", "1.0.0", true, &[], vec![dep("shared", 0)]);
        let mut my_derive = unit(
            "my-derive",
            "1.0.0",
            true,
            &[],
            vec![dep("pm_helper", 1), dep("shared", 0)],
        );
        my_derive.proc_macro = true;
        let uses_pm = unit(
            "uses-pm",
            "1.0.0",
            true,
            &[],
            vec![dep("my_derive", 2), dep("shared", 0)],
        );
        plan_with(
            vec![shared, pm_helper, my_derive, uses_pm],
            vec![dep("uses_pm", 3), dep("shared", 0), dep("my_derive", 2)],
        )
    }

    #[test]
    fn host_target_partition() {
        let plan = pm_plan();
        let host = host_closure(&plan);
        let target = target_units(&plan);
        assert_eq!(host, BTreeSet::from([0, 1, 2]), "proc-macro 闭包全进 host");
        assert_eq!(
            target,
            BTreeSet::from([0, 3]),
            "proc-macro 本体与其独有依赖（pm_helper）不进 target 集"
        );
        assert!(
            host.contains(&0) && target.contains(&0),
            "双用 unit（shared）两侧都在"
        );
    }

    #[test]
    fn proc_macro_args_five_pins() {
        let plan = pm_plan();
        let fps = fingerprints(&plan, &ProfileFlags::default(), "s").unwrap();
        let lo = layout();
        let a = proc_macro_rustc_args(&plan, 2, &ProfileFlags::default(), &fps, &lo);
        assert!(a[0].ends_with("bin/rustc"), "argv0 = 真 rustc: {}", a[0]);
        assert!(a.iter().any(|x| x == "--crate-type=proc-macro"));
        assert!(a.iter().any(|x| x == "--emit=dep-info,link"));
        assert!(
            a.windows(2)
                .any(|w| w[0] == "-C" && w[1] == "prefer-dynamic")
        );
        assert!(
            a.windows(2)
                .any(|w| w[0] == "-C" && w[1] == "embed-bitcode=no")
        );
        // 无 debuginfo、无 --sysroot、无 -Z
        assert!(
            !a.windows(2)
                .any(|w| w[0] == "-C" && w[1].starts_with("debuginfo"))
        );
        assert!(!a.iter().any(|x| x == "--sysroot"));
        assert!(!a.iter().any(|x| x.starts_with("-Z")));
        // 末尾裸 --extern proc_macro（最后一钉）
        assert_eq!(a.last().unwrap(), "proc_macro");
        assert_eq!(a[a.len() - 2], "--extern");
        // dep 边指 host-deps 的 .rlib（真链接）
        let ext = format!(
            "pm_helper=/tmp/cless/host-deps/libpm_helper-{}.rlib",
            fps[1]
        );
        assert!(
            a.windows(2).any(|w| w[0] == "--extern" && w[1] == ext),
            "缺 --extern: {a:?}"
        );
        // --out-dir 指 host-deps；registry 单元 cap-lints
        assert!(
            a.windows(2)
                .any(|w| w[0] == "--out-dir" && w[1] == "/tmp/cless/host-deps")
        );
        assert!(
            a.windows(2)
                .any(|w| w[0] == "--cap-lints" && w[1] == "allow")
        );
    }

    #[test]
    fn host_rlib_args_shape() {
        let plan = pm_plan();
        let fps = fingerprints(&plan, &ProfileFlags::default(), "s").unwrap();
        let lo = layout();
        let a = host_rustc_args(&plan, 1, &ProfileFlags::default(), &fps, &lo);
        assert!(a[0].ends_with("bin/rustc"), "argv0 = 真 rustc: {}", a[0]);
        assert!(a.iter().any(|x| x == "--crate-type=lib"));
        assert!(a.iter().any(|x| x == "--emit=dep-info,metadata,link"));
        assert!(
            a.windows(2)
                .any(|w| w[0] == "-C" && w[1] == "embed-bitcode=no")
        );
        // 无 prefer-dynamic、无 debuginfo、无 --sysroot、无 -Z
        assert!(!a.iter().any(|x| x == "prefer-dynamic"));
        assert!(
            !a.windows(2)
                .any(|w| w[0] == "-C" && w[1].starts_with("debuginfo"))
        );
        assert!(!a.iter().any(|x| x == "--sysroot"));
        assert!(!a.iter().any(|x| x.starts_with("-Z")));
        // dep 边指 host-deps 的 .rmeta
        let ext = format!("shared=/tmp/cless/host-deps/libshared-{}.rmeta", fps[0]);
        assert!(
            a.windows(2).any(|w| w[0] == "--extern" && w[1] == ext),
            "缺 --extern: {a:?}"
        );
        assert!(
            a.windows(2)
                .any(|w| w[0] == "--out-dir" && w[1] == "/tmp/cless/host-deps")
        );
    }

    #[test]
    fn target_and_bin_proc_macro_edges_point_to_dylib() {
        let plan = pm_plan();
        let fps = fingerprints(&plan, &ProfileFlags::default(), "s").unwrap();
        let lo = layout();
        let so = format!(
            "my_derive=/tmp/cless/host-deps/libmy_derive-{}{}",
            fps[2],
            std::env::consts::DLL_SUFFIX
        );
        // target dep 的 proc-macro 边 → host-deps 的 dylib；普通边照旧 .rmeta
        let a = dep_rustc_args(
            &plan,
            3,
            &ProfileFlags::default(),
            &fps,
            Path::new("/sys"),
            &lo,
        );
        assert!(
            a.windows(2).any(|w| w[0] == "--extern" && w[1] == so),
            "dep 缺 .so --extern: {a:?}"
        );
        let ext = format!("shared=/tmp/cless/deps/libshared-{}.rmeta", fps[0]);
        assert!(a.windows(2).any(|w| w[0] == "--extern" && w[1] == ext));
        // bin 的 proc-macro 根边 → dylib；普通根边照旧 .rlib
        let manifest = PackageManifest::parse(
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
             [dependencies]\nuses-pm = \"1\"\n",
            Path::new("/tmp/demo"),
        )
        .unwrap();
        let a = bin_rustc_args(
            &manifest,
            &plan,
            &fps,
            Path::new("/sys"),
            &lo,
            "demo",
            Path::new("/tmp/demo/src/main.rs"),
        );
        assert!(
            a.windows(2).any(|w| w[0] == "--extern" && w[1] == so),
            "bin 缺 .so --extern: {a:?}"
        );
        let ext = format!("uses_pm=/tmp/cless/deps/libuses_pm-{}.rlib", fps[3]);
        assert!(a.windows(2).any(|w| w[0] == "--extern" && w[1] == ext));
    }
}
