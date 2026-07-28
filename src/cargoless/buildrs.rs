//! `cargoless/buildrs.rs` —— build.rs 全生命周期的「指令 ↔ env ↔ 校验」半
//! （D15 P2 切③，设计档 §3 清单 7）。编译参数形态在 schedule.rs
//! （build_script_rustc_args），调度在 driver.rs；本文件只管：
//! 指令解析（cargo::/cargo: 两形）、CARGO_CFG_* 映射、执行 env 构建、
//! DEP_* 传播键规范化、links 互斥校验、-L 传递汇集。
//!
//! 传播规则全部按切③ 实证钉（/tmp/probe_link，cargo 1.98 逐条验证）：
//! - `-l`（rustc-link-lib）只进**本包**自己的编译行；`-L`（rustc-link-search）
//!   进本包 + 全部传递依赖者；rustc-cfg/check-cfg/rustc-env/link-arg 只进本包。
//! - metadata（cargo::metadata=K=V）经 `DEP_<LINKS>_<K>` env 只给**直接依赖者**
//!   的 build script（传递依赖者看不到）；cargo **不**自动注入 DEP_<LINKS>_ROOT
//!   （那是 -sys crate 自发 metadata=root 的惯例，非 cargo 行为）。
//! - cargo **不**对 rustc-cfg 自动补 --check-cfg（serde 一族是显式发
//!   rustc-check-cfg；probe 的 sysd 行实锤无自动补钉）。
//! - build script 的 warning 只有 path 包才显示（registry 包默认吞，-vv 才见）。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use super::manifest::ProfileFlags;
use super::resolve::{ResolvePlan, UnitClass, UnitDep};

/// 一个 build script 一次执行产出的解析后指令集。
#[derive(Clone, Debug, Default)]
pub struct BuildOutput {
    /// rustc-cfg 原始串（`foo` / `foo="bar"`）→ --cfg 只进本包编译。
    pub cfgs: Vec<String>,
    /// rustc-check-cfg 原始串（`cfg(foo, values("bar"))`）→ --check-cfg 只进本包。
    pub check_cfgs: Vec<String>,
    /// rustc-env（VAR=VALUE）→ 本包编译期 env（env! 可读；经 cmd.env 注入）。
    pub envs: Vec<(String, String)>,
    /// rustc-link-lib 原始 LIB 段（[KIND[:MOD]=]NAME）→ -l 只进本包。
    pub link_libs: Vec<String>,
    /// rustc-link-search 原始 [KIND=]PATH → -L 进本包 + 传递依赖者。
    pub link_searches: Vec<String>,
    /// rustc-link-arg / rustc-link-arg-bins → -C link-arg= 只进本包
    /// （cargo 对无 bin 目标的包发 link-arg-bins 是硬错误；v1 不做该项校验，
    /// 只收集——mirvm 的最终 bin 是会话解释不产生链接，旗标本就惰性）。
    pub link_args: Vec<String>,
    /// cargo::metadata=K=V → DEP_<LINKS>_<K> 给直接依赖者的 build script。
    pub metadata: BTreeMap<String, String>,
    /// cargo::warning=MSG（driver 按 from_registry 门控显示，cargo 同口径）。
    pub warnings: Vec<String>,
}

/// 指令解析：行首 `cargo::`（新形，1.77+）与 `cargo:`（legacy 单冒号）都吃。
/// 新形未知键忽略（cargo 前向兼容同口径）；legacy 未知键按 metadata 收
/// （cargo 同——老 build.rs 的 `cargo:KEY=VALUE` 就是 links metadata 旧形）。
/// cargo::error → Err（driver 补 crate 名）；rerun-if-* v1 忽略（粗指纹
/// 每次都重跑，精细化归 P3，设计档 §5 P2 行）。
pub fn parse_instructions(stdout: &str) -> Result<BuildOutput, String> {
    let mut out = BuildOutput::default();
    for line in stdout.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some(rest) = line.strip_prefix("cargo::") {
            apply(&mut out, rest, false)?;
        } else if let Some(rest) = line.strip_prefix("cargo:") {
            apply(&mut out, rest, true)?;
        }
        // 其余行是 build script 自己的 println!——cargo 同样忽略（-vv 才显示）
    }
    Ok(out)
}

fn apply(out: &mut BuildOutput, instr: &str, legacy: bool) -> Result<(), String> {
    let (key, value) = match instr.split_once('=') {
        Some((k, v)) => (k, v),
        None => (instr, ""),
    };
    match key {
        "rustc-link-lib" => out.link_libs.push(value.to_string()),
        "rustc-link-search" => out.link_searches.push(value.to_string()),
        "rustc-flags" => {
            // 只允许 -l/-L（cargo 同），拆开入两类
            for tok in value.split_whitespace() {
                if let Some(v) = tok.strip_prefix("-l") {
                    out.link_libs.push(v.to_string());
                } else if let Some(v) = tok.strip_prefix("-L") {
                    out.link_searches.push(v.to_string());
                } else {
                    return Err(format!("rustc-flags 只允许 -l/-L 旗（收到 `{tok}`）"));
                }
            }
        }
        "rustc-cfg" => out.cfgs.push(value.to_string()),
        "rustc-check-cfg" => out.check_cfgs.push(value.to_string()),
        "rustc-env" => {
            let Some((k, v)) = value.split_once('=') else {
                return Err(format!("rustc-env 缺 `=`：`{value}`"));
            };
            out.envs.push((k.to_string(), v.to_string()));
        }
        "rustc-link-arg" | "rustc-link-arg-bins" => out.link_args.push(value.to_string()),
        "metadata" => {
            let Some((k, v)) = value.split_once('=') else {
                return Err(format!("metadata 缺 `=`：`{value}`"));
            };
            out.metadata.insert(k.to_string(), v.to_string());
        }
        "warning" => out.warnings.push(value.to_string()),
        "error" => return Err(format!("build script 发了 cargo::error：{value}")),
        // v1 粗指纹每次都重跑，rerun-if 不消费（P3 精细化）
        "rerun-if-changed" | "rerun-if-env-changed" => {}
        _ => {
            if legacy {
                out.metadata.insert(key.to_string(), value.to_string());
            }
        }
    }
    Ok(())
}

/// DEP_* 键规范化（cargo envify 同）：ASCII 字母数字大写，其余一律 `_`
/// （my-links.x → MY_LINKS_X）。
pub fn envify(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// CARGO_CFG_* env 全集（E2 实证：cargo 就是 `rustc --print cfg` 的通用
/// 映射——k="v" 原子按 key 分组、多值逗号连、裸旗 → 空串值；再按 profile
/// 强制 DEBUG_ASSERTIONS/PANIC 两员；FEATURE = 本包启用 feature 逗号连）。
/// 原子集来自 manifest.rs 的 host_cfg_atoms（与 cargo 平台匹配同源）。
pub fn cargo_cfg_env(
    enabled_features: &BTreeSet<String>,
    profile: &ProfileFlags,
) -> BTreeMap<String, String> {
    let atoms = super::manifest::host_cfg_atoms();
    // key → 收集到的非裸值（host_cfg_atoms 是 BTreeSet，字典序即cargo 拼接序）
    let mut grouped: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for atom in atoms {
        if let Some((k, v)) = atom.split_once('=') {
            grouped.entry(k).or_default().push(v.trim_matches('"'));
        } else {
            grouped.entry(atom.as_str()).or_default();
        }
    }
    let mut out = BTreeMap::new();
    for (k, vs) in &grouped {
        out.insert(format!("CARGO_CFG_{}", k.to_uppercase()), vs.join(","));
    }
    // profile 强制两员（cargo 按 profile 钉，不随 --print cfg 原子有无）
    if profile.debug_assertions {
        out.insert("CARGO_CFG_DEBUG_ASSERTIONS".into(), String::new());
    } else {
        out.remove("CARGO_CFG_DEBUG_ASSERTIONS");
    }
    out.insert("CARGO_CFG_PANIC".into(), "unwind".into());
    out.insert(
        "CARGO_CFG_FEATURE".into(),
        enabled_features
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(","),
    );
    out
}

/// 直接依赖的 links metadata → DEP_<LINKS>_<KEY> env（E1(d) 实证：**只给
/// 直接依赖者**，传递依赖者看不到；links 与 metadata 键都过 envify；
/// 无自动 ROOT——cargo 不注入 DEP_<LINKS>_ROOT）。
pub fn dep_metadata_env(
    plan: &ResolvePlan,
    dep_edges: &[UnitDep],
    outputs: &BTreeMap<usize, BuildOutput>,
) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    for d in dep_edges {
        let du = &plan.units[d.unit];
        let Some(links) = &du.links else { continue };
        let Some(bo) = outputs.get(&d.unit) else {
            continue;
        };
        for (k, v) in &bo.metadata {
            env.insert(format!("DEP_{}_{}", envify(links), envify(k)), v.clone());
        }
    }
    env
}

/// build script 执行 env 的全部输入（平铺参数太多，收成上下文结构）。
pub struct ExecCtx<'a> {
    /// CARGO_PKG_* 全集（unit.pkg_env / manifest.pkg_env）。
    pub pkg_env: &'a BTreeMap<String, String>,
    /// 包根（cwd 也用它）。
    pub source_dir: &'a Path,
    /// 本包启用 feature（CARGO_CFG_FEATURE）。
    pub features: &'a BTreeSet<String>,
    pub profile: &'a ProfileFlags,
    /// OUT_DIR = build_root/<pkg>-<fp>/out。
    pub out_dir: &'a Path,
    /// 直接依赖的 DEP_* env（dep_metadata_env 产出）。
    pub dep_env: BTreeMap<String, String>,
    /// LD_LIBRARY_PATH 组成目（host_deps + deps；proc-macro build-dep 的
    /// .so 运行期 dlopen 要能找到）。
    pub ld_dirs: &'a [PathBuf],
}

/// build script 执行 env 全集（E2 实证清单逐条核对，cargo 1.98 同一工具链
/// 实机 dump）。不设 CARGO_MAKEFLAGS：jobserver 不在——cli.rs runner 段同款
/// 处理（串行调度无令牌协议可给）。
pub fn build_script_env(ctx: &ExecCtx) -> BTreeMap<String, String> {
    let mut env = ctx.pkg_env.clone();
    env.extend(cargo_cfg_env(ctx.features, ctx.profile));
    env.extend(ctx.dep_env.iter().map(|(k, v)| (k.clone(), v.clone())));
    let sysroot = PathBuf::from(env!("MIRVM_DEFAULT_SYSROOT"));
    let mut put = |k: &str, v: String| {
        env.insert(k.to_string(), v);
    };
    put("OUT_DIR", ctx.out_dir.display().to_string());
    put("CARGO_MANIFEST_DIR", ctx.source_dir.display().to_string());
    put(
        "CARGO_MANIFEST_PATH",
        ctx.source_dir.join("Cargo.toml").display().to_string(),
    );
    put("HOST", env!("MIRVM_HOST").to_string());
    put("TARGET", env!("MIRVM_HOST").to_string());
    // mirvm 只有 dev profile 一档（ProfileFlags::default 语义钉）
    put("PROFILE", "debug".into());
    put(
        "DEBUG",
        if ctx.profile.opt_level == 0 {
            "true".into()
        } else {
            "false".into()
        },
    );
    put("OPT_LEVEL", ctx.profile.opt_level.to_string());
    put(
        "NUM_JOBS",
        std::thread::available_parallelism()
            .map(|n| n.get().to_string())
            .unwrap_or_else(|_| "1".into()),
    );
    put("RUSTC", sysroot.join("bin/rustc").display().to_string());
    put("RUSTDOC", sysroot.join("bin/rustdoc").display().to_string());
    put("RUST_RECURSION_COUNT", "1".into());
    // 与 cargo 的差：cargo 填真 cargo 路径，我们填 mirvm 当前 exe——
    // build.rs 里 env!("CARGO")/var("CARGO") 可观察到这个差，夹具不触
    let cargo = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "mirvm".into());
    put("CARGO", cargo);
    let cargo_home = std::env::var("CARGO_HOME").unwrap_or_else(|_| {
        std::env::var("HOME")
            .map(|h| format!("{h}/.cargo"))
            .unwrap_or_default()
    });
    put("CARGO_HOME", cargo_home);
    // cargo 形态：<build 目录族>:<deps>:<rustlib lib>:<toolchain lib>；
    // 我们 = host_deps + deps + 工具链两目
    let mut ld: Vec<String> = ctx
        .ld_dirs
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    ld.push(
        sysroot
            .join(format!("lib/rustlib/{}/lib", env!("MIRVM_HOST")))
            .display()
            .to_string(),
    );
    ld.push(sysroot.join("lib").display().to_string());
    put("LD_LIBRARY_PATH", ld.join(":"));
    env
}

/// 同步执行 build script：cwd = 包根；stdin null；stdout 捕获（=指令流）；
/// stderr 捕获，仅失败时随错误回吐（cargo 同）。非零退出响亮报错。
pub fn run_build_script(
    exe: &Path,
    cwd: &Path,
    env: &BTreeMap<String, String>,
) -> Result<String, String> {
    let out = std::process::Command::new(exe)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .envs(env)
        .output()
        .map_err(|e| format!("build script 启动失败 {}: {e}", exe.display()))?;
    if !out.status.success() {
        return Err(format!(
            "build script 退出非零（{}）：\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    String::from_utf8(out.stdout).map_err(|e| format!("build script stdout 非 UTF-8: {e}"))
}

/// links 互斥（cargo 同：同一 links 值至多一个包——防重复符号）。根包与
/// 全部 unit 一起查；同名包的 Normal/Build 双 unit 共享 links 不算冲突
/// （按 (包, 版本) 判重）。
pub fn check_links_unique(
    root: Option<(&str, Option<&str>)>,
    plan: &ResolvePlan,
) -> Result<(), String> {
    let mut seen: BTreeMap<String, (String, String)> = BTreeMap::new();
    let mut note = |links: &str, pkg: &str, ver: &str| -> Result<(), String> {
        match seen.get(links) {
            Some((p, v)) if p == pkg && v == ver => Ok(()),
            Some((p, v)) => Err(format!(
                "links 键冲突：`{links}` 同时被 {p} {v} 与 {pkg} {ver} 声明（cargo 同拒：同一 links 至多一个包）"
            )),
            None => {
                seen.insert(links.to_string(), (pkg.to_string(), ver.to_string()));
                Ok(())
            }
        }
    };
    if let Some((name, Some(links))) = root {
        note(links, name, "（根包）")?;
    }
    for u in &plan.units {
        if let Some(links) = &u.links {
            note(links, &u.package, &u.version.to_string())?;
        }
    }
    Ok(())
}

/// -L 传播汇集（E1(a)(b) 实证：rustc-link-search 进本包 + 全部传递依赖
/// 者）：从 `edges` 的 Normal 类边出发沿 Normal 边 BFS，收集闭包内全部
/// 已执行 BuildOutput 的 link_searches。proc-macro unit 收自身一份但不再
/// 深入（它的依赖是 host 世界的，与 target 链接无关——与 target_units
/// 同一条界）。
pub fn aggregate_link_searches(
    plan: &ResolvePlan,
    edges: &[UnitDep],
    outputs: &BTreeMap<usize, BuildOutput>,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: BTreeSet<usize> = BTreeSet::new();
    let mut stack: Vec<usize> = edges
        .iter()
        .filter(|d| d.class == UnitClass::Normal)
        .map(|d| d.unit)
        .collect();
    while let Some(i) = stack.pop() {
        if !seen.insert(i) {
            continue;
        }
        if let Some(bo) = outputs.get(&i) {
            out.extend(bo.link_searches.iter().cloned());
        }
        if plan.units[i].proc_macro {
            continue;
        }
        stack.extend(
            plan.units[i]
                .deps
                .iter()
                .filter(|d| d.class == UnitClass::Normal)
                .map(|d| d.unit),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_new_form_all_keys() {
        let out = parse_instructions(
            "cargo::rustc-link-lib=static=probehelper\n\
             cargo::rustc-link-search=native=/opt/probe/lib\n\
             cargo::rustc-flags=-lfoo -L/bar\n\
             cargo::rustc-cfg=probe_feat\n\
             cargo::rustc-cfg=has_val=\"x\"\n\
             cargo::rustc-check-cfg=cfg(probe_feat2)\n\
             cargo::rustc-env=ROOT_SEEN=bar\n\
             cargo::rustc-link-arg=-Wl,--x\n\
             cargo::rustc-link-arg-bins=-Wl,--y\n\
             cargo::metadata=foo=bar\n\
             cargo::warning=小心点\n\
             cargo::rerun-if-changed=build.rs\n\
             cargo::rerun-if-env-changed=CC\n\
             cargo::future-new-key=被忽略\n\
             普通输出行当没看见\n",
        )
        .unwrap();
        assert_eq!(out.link_libs, ["static=probehelper", "foo"]);
        assert_eq!(out.link_searches, ["native=/opt/probe/lib", "/bar"]);
        assert_eq!(out.cfgs, ["probe_feat", "has_val=\"x\""]);
        assert_eq!(out.check_cfgs, ["cfg(probe_feat2)"]);
        assert_eq!(out.envs, [("ROOT_SEEN".to_string(), "bar".to_string())]);
        assert_eq!(out.link_args, ["-Wl,--x", "-Wl,--y"]);
        assert_eq!(out.metadata.get("foo").map(String::as_str), Some("bar"));
        assert_eq!(out.warnings, ["小心点"]);
    }

    #[test]
    fn parse_legacy_single_colon_and_unknown_as_metadata() {
        // legacy 单冒号：已知键照常，未知键按 metadata 收（cargo 同口径）
        let out = parse_instructions(
            "cargo:rustc-link-lib=z\n\
             cargo:rustc-cfg=old\n\
             cargo:root=/opt/sys\n\
             cargo:rustc-link-search=/p\r\n",
        )
        .unwrap();
        assert_eq!(out.link_libs, ["z"]);
        assert_eq!(out.cfgs, ["old"]);
        assert_eq!(out.link_searches, ["/p"], "CRLF 尾要剥");
        assert_eq!(
            out.metadata.get("root").map(String::as_str),
            Some("/opt/sys")
        );
    }

    #[test]
    fn parse_error_and_bad_forms_are_loud() {
        let e = parse_instructions("cargo::error=缺 libfoo").unwrap_err();
        assert!(e.contains("缺 libfoo"), "{e}");
        let e = parse_instructions("cargo::rustc-env=NOEQ").unwrap_err();
        assert!(e.contains("rustc-env"), "{e}");
        let e = parse_instructions("cargo::rustc-flags=-O2").unwrap_err();
        assert!(e.contains("-l/-L"), "{e}");
        // 新形未知键静默忽略（前向兼容），不报错
        parse_instructions("cargo::brand-new=1").unwrap();
    }

    #[test]
    fn envify_normalizes_dep_keys() {
        assert_eq!(envify("my-links.x"), "MY_LINKS_X");
        assert_eq!(envify("sysd"), "SYSD");
        assert_eq!(envify("foo_bar"), "FOO_BAR");
    }

    #[test]
    fn cargo_cfg_env_maps_atoms() {
        let feats: BTreeSet<String> = ["derive", "std"].iter().map(|s| s.to_string()).collect();
        let env = cargo_cfg_env(&feats, &ProfileFlags::default());
        // 真机原子（nightly 工具链 x86_64-linux）：多值逗号连、裸旗空串
        assert_eq!(
            env.get("CARGO_CFG_TARGET_ARCH").map(String::as_str),
            Some("x86_64")
        );
        assert_eq!(
            env.get("CARGO_CFG_UNIX").map(String::as_str),
            Some(""),
            "裸旗 unix → 空串值"
        );
        let atomic = env.get("CARGO_CFG_TARGET_HAS_ATOMIC").unwrap();
        assert!(
            atomic.split(',').count() >= 4 && atomic.contains("ptr"),
            "多值逗号连: {atomic}"
        );
        assert_eq!(
            env.get("CARGO_CFG_FEATURE").map(String::as_str),
            Some("derive,std"),
            "feature 逗号连（BTreeSet 字典序）"
        );
        assert_eq!(
            env.get("CARGO_CFG_DEBUG_ASSERTIONS").map(String::as_str),
            Some(""),
            "dev profile → 在场空串"
        );
        assert_eq!(
            env.get("CARGO_CFG_PANIC").map(String::as_str),
            Some("unwind")
        );
        // debug_assertions 跟 profile 走
        let rel = ProfileFlags {
            debug_assertions: false,
            overflow_checks: false,
            opt_level: 2,
        };
        let env2 = cargo_cfg_env(&BTreeSet::new(), &rel);
        assert!(!env2.contains_key("CARGO_CFG_DEBUG_ASSERTIONS"));
        assert_eq!(env2.get("CARGO_CFG_FEATURE").map(String::as_str), Some(""));
    }
}
