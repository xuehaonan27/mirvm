//! `cargoless/manifest.rs` —— Cargo.toml 解析与模型（D15 P1，设计档 §3.1）。
//!
//! 子集边界（超出即响亮拒绝并记名，不静默吞掉）：
//! - `[package]`（name/version/edition/autobins/default-run/links）
//! - `[lib]` / `[[bin]]` / 自动发现（src/lib.rs、src/main.rs、src/bin/*.rs）
//! - `[dependencies]` / `[build-dependencies]`：version req、features、
//!   optional、default-features、path；**git / 私有 registry（registry=/git/
//!   branch/tag/rev 键）P5 范畴，响亮拒绝**
//! - `[features]` 三形态：`"foo"`（特性或隐式可选依赖）、`"dep:foo"`（显式
//!   依赖激活）、`"foo?/bar"`（弱激活）
//! - `[profile.*]`：只取 debug-assertions / overflow-checks / opt-level
//!   （语义钉死：前两枚进 MIR 语义，设计档 §6 的 jiff 判例）
//! - `target.'cfg()'.dependencies` 平台求值：目标平台原子（target_os/
//!   target_arch/target_family/unix/target_vendor/target_env/target_abi/
//!   target_pointer_width/target_endian）+ any/all/not 组合；
//!   `cfg(feature=..)` 不属于平台求值（cargo 同）；`cfg(target_feature=..)`
//!   响亮拒绝（归 P5）
//! - `[workspace]`：单包或"包 + workspace 根"两形态；workspace.package 的
//!   version/edition 继承（向上找根）；**多包成员图与 virtual manifest 归 P5，
//!   响亮拒绝**
//! - 整体不做：dev-dependencies（mirvm 永不跑 test，设计档 §3.5 事先明说）。

// P1 逐切接入中：resolve/registry/audit 后续切片接入后摘除本 allow（设计档 §5）。
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// 依赖来源。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DepSource {
    /// crates.io（或读穿的本地缓存）：semver 需求。
    Registry(semver::VersionReq),
    /// 本地路径依赖（已绝对化）。
    Path(PathBuf),
}

/// 依赖种类（dev-deps 不建）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DepKind {
    Normal,
    Build,
}

/// 一条依赖声明（平台 cfg 表达式随行，不在解析期过滤——版本求解是
/// 全平台并集（cargo lock 语义），过滤只在 host 构建图装配期发生）。
#[derive(Clone, Debug)]
pub struct DepDecl {
    /// manifest 里的键名（feature 引用、--extern 命名用它，除非 rename）。
    pub key: String,
    /// 真实 crate 名（`package = "real"` 改名时是 real，否则 == key）。
    pub package: String,
    pub source: DepSource,
    pub features: Vec<String>,
    pub optional: bool,
    pub default_features: bool,
    pub kind: DepKind,
    /// 来自 `target.'cfg(...)'` 表时的 cfg 表达式（普通表 = None）。
    pub platform_cfg: Option<String>,
}

impl DepDecl {
    /// feature 引用名（cargo 语义：隐式 feature 名 = key）。
    pub fn feature_name(&self) -> &str {
        &self.key
    }
}

/// `[features]` 表一项的值形态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FeatureValue {
    /// `"foo"`：另一 feature，或同名可选依赖的隐式激活。
    Simple(String),
    /// `"dep:foo"`：显式激活可选依赖（不建同名 feature）。
    DepActivation(String),
    /// `"foo/bar"`：强激活——激活 foo 并开其 bar。
    StrongDep { dep: String, feature: String },
    /// `"foo?/bar"`：若 foo 被激活则开其 bar（弱激活，不激活 foo 本身）。
    WeakDep { dep: String, feature: String },
}

/// 编译目标。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Lib {
        name: String,
        path: PathBuf,
        proc_macro: bool,
    },
    Bin {
        name: String,
        path: PathBuf,
    },
}

/// profile 语义旗（只取影响 MIR 语义的 + 照传的 opt-level）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProfileFlags {
    pub debug_assertions: bool,
    pub overflow_checks: bool,
    pub opt_level: u8,
}

impl Default for ProfileFlags {
    /// cargo dev profile 等价（设计档 §6 语义钉）。
    fn default() -> Self {
        Self {
            debug_assertions: true,
            overflow_checks: true,
            opt_level: 0,
        }
    }
}

/// 一个包的完整模型。
#[derive(Clone, Debug)]
pub struct PackageManifest {
    pub name: String,
    pub version: semver::Version,
    pub edition: String,
    pub root: PathBuf,
    pub targets: Vec<Target>,
    pub deps: Vec<DepDecl>,
    pub features: BTreeMap<String, Vec<FeatureValue>>,
    pub profile: ProfileFlags,
    /// 有 build script（build 键 / links 键 / 根下 build.rs 实存）。
    pub has_build_script: bool,
    /// `[package] links`（-sys 链接键；native 库名推导与 build.rs 调度用）。
    pub links: Option<String>,
    pub default_run: Option<String>,
}

// ---------- serde 原料（宽松，未知键忽略，已知不支持的键后置校验）----------

#[derive(serde::Deserialize, Default)]
struct RawManifest {
    package: Option<RawPackage>,
    workspace: Option<RawWorkspace>,
    lib: Option<RawLib>,
    bin: Option<Vec<RawBin>>,
    dependencies: Option<BTreeMap<String, toml::Value>>,
    #[serde(rename = "build-dependencies")]
    build_dependencies: Option<BTreeMap<String, toml::Value>>,
    #[serde(rename = "dev-dependencies")]
    dev_dependencies: Option<BTreeMap<String, toml::Value>>,
    features: Option<BTreeMap<String, Vec<String>>>,
    profile: Option<RawProfiles>,
    target: Option<BTreeMap<String, RawTargetDeps>>,
}

#[derive(serde::Deserialize, Default)]
struct RawPackage {
    name: Option<String>,
    version: Option<toml::Value>,
    edition: Option<toml::Value>,
    autobins: Option<bool>,
    links: Option<String>,
    build: Option<toml::Value>,
    #[serde(rename = "default-run")]
    default_run: Option<String>,
}

#[derive(serde::Deserialize, Default)]
struct RawWorkspace {
    package: Option<RawWorkspacePackage>,
    members: Option<Vec<String>>,
}

#[derive(serde::Deserialize, Default)]
struct RawWorkspacePackage {
    version: Option<String>,
    edition: Option<String>,
}

#[derive(serde::Deserialize, Default)]
struct RawLib {
    name: Option<String>,
    path: Option<String>,
    #[serde(rename = "proc-macro")]
    proc_macro: Option<bool>,
}

#[derive(serde::Deserialize, Default)]
struct RawBin {
    name: Option<String>,
    path: Option<String>,
}

#[derive(serde::Deserialize, Default)]
struct RawProfiles {
    dev: Option<RawProfile>,
    release: Option<RawProfile>,
}

#[derive(serde::Deserialize, Default)]
struct RawProfile {
    #[serde(rename = "debug-assertions")]
    debug_assertions: Option<bool>,
    #[serde(rename = "overflow-checks")]
    overflow_checks: Option<bool>,
    #[serde(rename = "opt-level")]
    opt_level: Option<toml::Value>,
}

#[derive(serde::Deserialize, Default)]
struct RawTargetDeps {
    dependencies: Option<BTreeMap<String, toml::Value>>,
    #[serde(rename = "build-dependencies")]
    build_dependencies: Option<BTreeMap<String, toml::Value>>,
}

// ---------- 错误 ----------

type MErr = String;

fn unsupported(what: impl Into<String>) -> MErr {
    format!(
        "manifest 子集外构造（D15 P5 范畴，响亮拒绝）：{}",
        what.into()
    )
}

// ---------- 公开入口 ----------

impl PackageManifest {
    /// 从项目目录读（目录/Cargo.toml）。
    pub fn read_dir(dir: &Path) -> Result<Self, MErr> {
        let dir = std::path::absolute(dir)
            .map_err(|e| format!("项目目录绝对化失败 {}: {e}", dir.display()))?;
        let file = dir.join("Cargo.toml");
        let text = std::fs::read_to_string(&file)
            .map_err(|e| format!("读取 {} 失败: {e}", file.display()))?;
        Self::parse(&text, &dir)
    }

    /// 从 manifest 文本解析（root = 包根目录）。
    pub fn parse(text: &str, root: &Path) -> Result<Self, MErr> {
        let raw: RawManifest =
            toml::from_str(text).map_err(|e| format!("Cargo.toml 解析失败: {e}"))?;
        if raw.package.is_none() && raw.workspace.is_some() {
            return Err(unsupported("virtual manifest（[workspace] 无 [package]）"));
        }
        let pkg = raw
            .package
            .ok_or_else(|| "manifest 缺 [package]".to_string())?;
        let name = pkg.name.ok_or_else(|| "package.name 缺失".to_string())?;

        // version/edition 支持 workspace 继承（workspace.package.*）
        let ws_pkg = raw.workspace.as_ref().and_then(|w| w.package.as_ref());
        let version = match pkg.version {
            Some(toml::Value::String(v)) => {
                semver::Version::parse(&v).map_err(|e| format!("package.version 非法 {v}: {e}"))?
            }
            Some(toml::Value::Table(t)) if t.get("workspace").is_some() => ws_pkg
                .and_then(|w| w.version.clone())
                .and_then(|v| semver::Version::parse(&v).ok())
                .ok_or_else(|| "package.version 继承 workspace 但根无 version".to_string())?,
            Some(_) => return Err("package.version 形态不支持".into()),
            None => semver::Version::new(0, 0, 0),
        };
        let edition = match pkg.edition {
            Some(toml::Value::String(e)) => e,
            Some(toml::Value::Table(t)) if t.get("workspace").is_some() => ws_pkg
                .and_then(|w| w.edition.clone())
                .ok_or_else(|| "package.edition 继承 workspace 但根无 edition".to_string())?,
            Some(_) => return Err("package.edition 形态不支持".into()),
            None => "2015".to_string(),
        };
        if raw
            .workspace
            .as_ref()
            .and_then(|w| w.members.as_ref())
            .is_some()
            && raw
                .workspace
                .as_ref()
                .and_then(|w| w.package.as_ref())
                .is_none()
        {
            // 有 members 而无 workspace.package：可能是多包根。单包自用 members 少见，
            // P1 不分辨，响亮拒绝对多包图的解析承诺。
            return Err(unsupported(
                "workspace.members 多包图（P5；单包项目可移除此键）",
            ));
        }

        let mut deps = Vec::new();
        parse_dep_table(&raw.dependencies, DepKind::Normal, root, None, &mut deps)?;
        parse_dep_table(
            &raw.build_dependencies,
            DepKind::Build,
            root,
            None,
            &mut deps,
        )?;
        if raw.dev_dependencies.is_some() {
            // 事先明说的不做面：见到即忽略（不拒绝——cargo 项目常带，但我们永不消费）
        }
        // target.'cfg()'.dependencies：表达式随行进模型（全平台并集语义，不过滤）
        for (cfg_expr, tdeps) in raw.target.iter().flatten() {
            // 表达式合法性在此校验（拼写错误要响亮；语义求值在使用期）
            validate_cfg_expr(cfg_expr).map_err(|e| format!("target.{cfg_expr}: {e}"))?;
            parse_dep_table(
                &tdeps.dependencies,
                DepKind::Normal,
                root,
                Some(cfg_expr.clone()),
                &mut deps,
            )?;
            parse_dep_table(
                &tdeps.build_dependencies,
                DepKind::Build,
                root,
                Some(cfg_expr.clone()),
                &mut deps,
            )?;
        }

        let features = raw
            .features
            .as_ref()
            .map(|fs| {
                fs.iter()
                    .map(|(k, vs)| {
                        vs.iter()
                            .map(|v| parse_feature_value(v))
                            .collect::<Result<Vec<_>, _>>()
                            .map(|vals| (k.clone(), vals))
                    })
                    .collect::<Result<BTreeMap<_, _>, _>>()
            })
            .transpose()?
            .unwrap_or_default();

        let autobins = pkg.autobins.unwrap_or(true);
        let targets = discover_targets(raw.lib.as_ref(), raw.bin.as_ref(), autobins, &name, root)?;
        let profile = profile_from(raw.profile);
        let has_build_script =
            pkg.build.is_some() || pkg.links.is_some() || root.join("build.rs").is_file();

        Ok(Self {
            name,
            version,
            edition,
            root: root.to_path_buf(),
            targets,
            deps,
            features,
            profile,
            has_build_script,
            links: pkg.links,
            default_run: pkg.default_run,
        })
    }

    /// frontmatter 伪包：脚本 stem 为包名，依赖段原文喂给同一解析。
    /// bin 名带哈希短缀（与旧物化口径一致，消 target 目录碰撞，cli.rs 旧例）。
    pub fn from_frontmatter(
        stem: &str,
        manifest_text: &str,
        body_path: &Path,
    ) -> Result<Self, MErr> {
        let pseudo = format!(
            "[package]\nname = \"{stem}\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\
             [[bin]]\nname = \"{stem}\"\npath = \"{}\"\n{manifest_text}",
            body_path.display()
        );
        let tmp_root = body_path.parent().unwrap_or(Path::new("."));
        Self::parse(&pseudo, tmp_root)
    }

    /// 选定要跑的 bin（cargo run 语义子集）：default-run > 唯一 bin > 多 bin 响亮拒绝。
    pub fn runnable_bin(&self) -> Result<(&str, &Path), MErr> {
        let bins: Vec<_> = self
            .targets
            .iter()
            .filter_map(|t| match t {
                Target::Bin { name, path } => Some((name.as_str(), path.as_path())),
                _ => None,
            })
            .collect();
        if let Some(dr) = &self.default_run {
            if let Some(b) = bins.iter().find(|(n, _)| *n == dr) {
                return Ok(*b);
            }
            return Err(format!("default-run={dr} 在 [[bin]] 中不存在"));
        }
        match bins.len() {
            0 => Err(format!("包 {} 没有 bin 目标", self.name)),
            1 => Ok(bins[0]),
            _ => Err(unsupported(format!(
                "多 bin 目标（{}）——P5 范畴，请用 default-run 钉选",
                bins.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", ")
            ))),
        }
    }
}

// ---------- 依赖表 ----------

fn parse_dep_table(
    table: &Option<BTreeMap<String, toml::Value>>,
    kind: DepKind,
    root: &Path,
    platform_cfg: Option<String>,
    out: &mut Vec<DepDecl>,
) -> Result<(), MErr> {
    for (key, val) in table.iter().flatten() {
        let mut package = key.clone();
        let mut req_str = "*".to_string();
        let mut source: Option<DepSource> = None;
        let mut features = Vec::new();
        let mut optional = false;
        let mut default_features = true;
        match val {
            toml::Value::String(v) => req_str = v.clone(),
            toml::Value::Table(t) => {
                for (k, v) in t {
                    match k.as_str() {
                        "version" => req_str = v.as_str().ok_or("version 非字符串")?.to_string(),
                        "path" => {
                            let p = v.as_str().ok_or("path 非字符串")?;
                            source = Some(DepSource::Path(root.join(p)));
                        }
                        "package" => package = v.as_str().ok_or("package 非字符串")?.to_string(),
                        "features" => {
                            features = v
                                .as_array()
                                .ok_or("features 非数组")?
                                .iter()
                                .map(|f| {
                                    f.as_str()
                                        .map(str::to_string)
                                        .ok_or("features 元素非字符串")
                                })
                                .collect::<Result<_, _>>()?;
                        }
                        "optional" => optional = v.as_bool().ok_or("optional 非布尔")?,
                        "default-features" => {
                            default_features = v.as_bool().ok_or("default-features 非布尔")?
                        }
                        "git" | "branch" | "tag" | "rev" | "registry" | "registry-index" => {
                            return Err(unsupported(format!(
                                "依赖 {key} 的 {k} 源（git/私有 registry 归 P5）"
                            )));
                        }
                        // 已知无害键：public/private（cargo 新键）、artifact、lib、
                        // workspace（workspace.dependencies 继承——P5，见到响亮拒绝）
                        "workspace" => {
                            return Err(unsupported(format!("依赖 {key} 的 workspace 继承（P5）")));
                        }
                        _ => {} // 未知小键忽略（前向兼容）
                    }
                }
            }
            _ => return Err(format!("依赖 {key} 形态不支持（非字符串非表）")),
        }
        let source = match source {
            Some(p) => p,
            None => DepSource::Registry(
                semver::VersionReq::parse(&req_str)
                    .map_err(|e| format!("依赖 {key} version req 非法 {req_str}: {e}"))?,
            ),
        };
        out.push(DepDecl {
            key: key.clone(),
            package,
            source,
            features,
            optional,
            default_features,
            kind,
            platform_cfg: platform_cfg.clone(),
        });
    }
    Ok(())
}

pub(crate) fn parse_feature_value(v: &str) -> Result<FeatureValue, MErr> {
    if let Some(dep) = v.strip_prefix("dep:") {
        return Ok(FeatureValue::DepActivation(dep.to_string()));
    }
    if let Some((dep, feat)) = v.split_once("?/") {
        return Ok(FeatureValue::WeakDep {
            dep: dep.to_string(),
            feature: feat.to_string(),
        });
    }
    if let Some((dep, feat)) = v.split_once('/') {
        return Ok(FeatureValue::StrongDep {
            dep: dep.to_string(),
            feature: feat.to_string(),
        });
    }
    Ok(FeatureValue::Simple(v.to_string()))
}

// ---------- cfg 表达式（解析 / 校验 / host 求值） ----------

/// cfg 表达式 AST。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CfgExpr {
    /// (key, value)；裸原子（unix/windows）的 value = 空串。
    Atom(String, String),
    Any(Vec<CfgExpr>),
    All(Vec<CfgExpr>),
    Not(Box<CfgExpr>),
}

/// 解析 `cfg(...)` 为 AST（`cfg(...)` 外壳或裸表达式均可）。
pub fn parse_cfg(expr: &str) -> Result<CfgExpr, MErr> {
    let e = expr.trim();
    let inner = e
        .strip_prefix("cfg(")
        .and_then(|s| s.strip_suffix(')'))
        .unwrap_or(e);
    parse_cfg_inner(inner.trim())
}

fn parse_cfg_inner(s: &str) -> Result<CfgExpr, MErr> {
    for (op, make) in [
        ("any(", CfgExpr::Any as fn(Vec<CfgExpr>) -> CfgExpr),
        ("all(", CfgExpr::All),
        ("not(", |v| {
            debug_assert!(v.len() == 1);
            CfgExpr::Not(Box::new(v.into_iter().next().unwrap()))
        }),
    ] {
        if let Some(rest) = s.strip_prefix(op) {
            let rest = rest
                .strip_suffix(')')
                .ok_or_else(|| format!("cfg 表达式括号不配对: {s}"))?;
            let parts = split_top_level(rest)?;
            let exprs = parts
                .iter()
                .map(|p| parse_cfg_inner(p.trim()))
                .collect::<Result<Vec<_>, _>>()?;
            if op == "not(" && exprs.len() != 1 {
                return Err(format!("cfg not() 只收一个参数: {s}"));
            }
            return Ok(make(exprs));
        }
    }
    let (key, val) = match s.split_once('=') {
        Some((k, v)) => (k.trim().to_string(), v.trim().trim_matches('"').to_string()),
        None => (s.trim().to_string(), String::new()),
    };
    if key.is_empty() {
        return Err(format!("cfg 原子形态非法: {s}"));
    }
    Ok(CfgExpr::Atom(key, val))
}

/// 解析期校验（拼写错误响亮；`cfg(feature=..)` 在 target 依赖表属 cargo
/// 禁用面，响亮拒绝记名）。
fn validate_cfg_expr(expr: &str) -> Result<(), MErr> {
    fn walk(e: &CfgExpr) -> Result<(), MErr> {
        match e {
            CfgExpr::Atom(k, _) if k == "feature" => Err(unsupported(
                "cfg(feature=..) 于 target 依赖表（cargo 禁用面）",
            )),
            CfgExpr::Atom(_, _) => Ok(()),
            CfgExpr::Any(vs) | CfgExpr::All(vs) => vs.iter().try_for_each(walk),
            CfgExpr::Not(e) => walk(e),
        }
    }
    walk(&parse_cfg(expr)?)
}

/// host 平台原子集 = `rustc --print cfg --target <host>` 原样行集（cargo
/// 平台匹配同源：未知/自定义键（rustix_use_libc 等）天然求 false；
/// target_feature 由 rustc 列表精确覆盖）。进程内 OnceLock 缓存。
static HOST_CFG_ATOMS: std::sync::OnceLock<std::collections::BTreeSet<String>> =
    std::sync::OnceLock::new();

fn host_cfg_atoms() -> &'static std::collections::BTreeSet<String> {
    HOST_CFG_ATOMS.get_or_init(|| {
        let rustc = std::path::PathBuf::from(env!("MIRVM_DEFAULT_SYSROOT")).join("bin/rustc");
        let out = std::process::Command::new(rustc)
            .args(["--print", "cfg", "--target", env!("MIRVM_HOST")])
            .output()
            .expect("rustc --print cfg 失败");
        let text = String::from_utf8(out.stdout).expect("rustc --print cfg 非 UTF-8");
        text.lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect()
    })
}

/// host 求值（host 构建图装配期用；版本求解期不得调用——那是全平台并集）。
pub fn eval_cfg(expr: &str) -> Result<bool, MErr> {
    let ast = parse_cfg(expr)?;
    let atoms = host_cfg_atoms();
    Ok(eval_ast(&ast, atoms))
}

fn eval_ast(e: &CfgExpr, atoms: &std::collections::BTreeSet<String>) -> bool {
    match e {
        CfgExpr::Atom(key, val) => {
            let needle = if val.is_empty() {
                key.clone()
            } else {
                format!("{key}=\"{val}\"")
            };
            atoms.contains(&needle)
        }
        CfgExpr::Any(vs) => vs.iter().any(|e| eval_ast(e, atoms)),
        CfgExpr::All(vs) => vs.iter().all(|e| eval_ast(e, atoms)),
        CfgExpr::Not(e) => !eval_ast(e, atoms),
    }
}

fn split_top_level(s: &str) -> Result<Vec<String>, MErr> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    for c in s.chars() {
        match c {
            '(' => {
                depth += 1;
                cur.push(c);
            }
            ')' => {
                depth -= 1;
                if depth < 0 {
                    return Err(format!("cfg 表达式括号不配对: {s}"));
                }
                cur.push(c);
            }
            ',' if depth == 0 => {
                out.push(cur.trim().to_string());
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    if depth != 0 {
        return Err(format!("cfg 表达式括号不配对: {s}"));
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    Ok(out)
}

// ---------- target 发现 ----------

fn discover_targets(
    lib: Option<&RawLib>,
    bin: Option<&Vec<RawBin>>,
    autobins: bool,
    pkg_name: &str,
    root: &Path,
) -> Result<Vec<Target>, MErr> {
    let mut out = Vec::new();
    // lib：显式 [lib] 或自动 src/lib.rs
    let lib_path = lib
        .and_then(|l| l.path.clone())
        .map(|p| root.join(&p))
        .filter(|p| p.is_file())
        .or_else(|| {
            let p = root.join("src/lib.rs");
            p.is_file().then_some(p)
        });
    if let Some(path) = lib_path {
        let name = lib
            .and_then(|l| l.name.clone())
            .unwrap_or_else(|| pkg_name.replace('-', "_"));
        let proc_macro = lib.and_then(|l| l.proc_macro).unwrap_or(false);
        out.push(Target::Lib {
            name,
            path,
            proc_macro,
        });
    }
    // bin：显式 [[bin]] 优先；否则自动发现
    let explicit: Vec<Target> = bin
        .into_iter()
        .flatten()
        .filter_map(|b| {
            let name = b.name.clone()?;
            let path = b
                .path
                .clone()
                .map(|p| root.join(&p))
                .or_else(|| {
                    let cand = root.join(format!("src/bin/{name}.rs"));
                    cand.is_file().then_some(cand)
                })
                .or_else(|| {
                    let cand = root.join("src/main.rs");
                    (name == pkg_name && cand.is_file()).then_some(cand)
                })?;
            Some(Target::Bin { name, path })
        })
        .collect();
    if !explicit.is_empty() {
        out.extend(explicit);
        return Ok(out);
    }
    if autobins {
        let main = root.join("src/main.rs");
        if main.is_file() {
            out.push(Target::Bin {
                name: pkg_name.to_string(),
                path: main,
            });
        }
        let bin_dir = root.join("src/bin");
        if let Ok(rd) = std::fs::read_dir(&bin_dir) {
            let mut extra: Vec<Target> = rd
                .flatten()
                .filter_map(|e| {
                    let p = e.path();
                    if p.extension().is_some_and(|x| x == "rs") {
                        p.file_stem().map(|s| s.to_string_lossy().into_owned())
                    } else {
                        None
                    }
                    .map(|name| Target::Bin {
                        name,
                        path: p.clone(),
                    })
                })
                .collect();
            extra.sort_by(|a, b| match (a, b) {
                (Target::Bin { name: an, .. }, Target::Bin { name: bn, .. }) => an.cmp(bn),
                _ => std::cmp::Ordering::Equal,
            });
            out.extend(extra);
        }
    }
    Ok(out)
}

// ---------- profile ----------

fn profile_from(raw: Option<RawProfiles>) -> ProfileFlags {
    let mut pf = ProfileFlags::default();
    if let Some(dev) = raw.and_then(|p| p.dev) {
        if let Some(v) = dev.debug_assertions {
            pf.debug_assertions = v;
        }
        if let Some(v) = dev.overflow_checks {
            pf.overflow_checks = v;
        }
        if let Some(toml::Value::Integer(v)) = dev.opt_level {
            pf.opt_level = v as u8;
        }
    }
    pf
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "mirvm-cargoless-manifest-test-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn parses_string_and_table_deps_with_rename_and_optional() {
        let m = PackageManifest::parse(
            r#"
[package]
name = "demo"
version = "1.2.3"
edition = "2021"

[dependencies]
itertools = "0.14"
renamed = { package = "real-crate", version = "^2.0", optional = true }
no_default = { version = "1", default-features = false, features = ["a", "b"] }
local = { path = "../sibling" }

[build-dependencies]
cc = "1"
"#,
            Path::new("/tmp/x"),
        )
        .unwrap();
        assert_eq!(m.name, "demo");
        assert_eq!(m.version.to_string(), "1.2.3");
        assert_eq!(m.edition, "2021");
        let get = |k: &str| m.deps.iter().find(|d| d.key == k).unwrap();
        assert_eq!(get("itertools").package, "itertools");
        assert_eq!(
            get("itertools").source,
            DepSource::Registry(semver::VersionReq::parse("0.14").unwrap())
        );
        assert_eq!(get("renamed").package, "real-crate");
        assert!(get("renamed").optional);
        assert!(!get("no_default").default_features);
        assert_eq!(get("no_default").features, ["a", "b"]);
        assert_eq!(
            get("local").source,
            DepSource::Path(Path::new("/tmp/x").join("../sibling"))
        );
        assert_eq!(get("cc").kind, DepKind::Build);
    }

    #[test]
    fn rejects_git_and_workspace_inherited_deps_loudly() {
        let err = PackageManifest::parse(
            "[package]\nname=\"d\"\nversion=\"0.1.0\"\n[dependencies]\nfoo = { git = \"https://x\" }",
            Path::new("/tmp/x"),
        )
        .unwrap_err();
        assert!(err.contains("P5"), "{err}");
        let err = PackageManifest::parse(
            "[package]\nname=\"d\"\nversion=\"0.1.0\"\n[dependencies]\nfoo.workspace = true",
            Path::new("/tmp/x"),
        )
        .unwrap_err();
        assert!(err.contains("P5"), "{err}");
    }

    #[test]
    fn parses_three_feature_value_forms() {
        let m = PackageManifest::parse(
            "[package]\nname=\"d\"\nversion=\"0.1.0\"\n\
             [features]\ndefault = [\"std\", \"dep:serde\", \"itertools?/use_alloc\", \"old/strong\"]\n",
            Path::new("/tmp/x"),
        )
        .unwrap();
        let vals = &m.features["default"];
        assert_eq!(vals[0], FeatureValue::Simple("std".into()));
        assert_eq!(vals[1], FeatureValue::DepActivation("serde".into()));
        assert_eq!(
            vals[2],
            FeatureValue::WeakDep {
                dep: "itertools".into(),
                feature: "use_alloc".into()
            }
        );
        assert_eq!(
            vals[3],
            FeatureValue::StrongDep {
                dep: "old".into(),
                feature: "strong".into()
            }
        );
    }

    #[test]
    fn cfg_platform_atoms_evaluate_for_host() {
        assert!(eval_cfg("cfg(target_os=\"linux\")").unwrap());
        assert!(!eval_cfg("cfg(target_os=\"windows\")").unwrap());
        assert!(eval_cfg("cfg(unix)").unwrap());
        assert!(eval_cfg("cfg(any(target_os=\"macos\", target_os=\"linux\"))").unwrap());
        assert!(!eval_cfg("cfg(not(unix))").unwrap());
        assert!(eval_cfg("cfg(all(unix, target_arch=\"x86_64\"))").unwrap());
        assert!(eval_cfg("cfg(target_pointer_width=\"64\")").unwrap());
        assert!(eval_cfg("cfg(target_endian=\"little\")").unwrap());
        // 自定义键（rustix 型）与未知键天然 false（rustc --print cfg 同源）
        assert!(!eval_cfg("cfg(rustix_use_libc)").unwrap());
        assert!(!eval_cfg("cfg(some_custom_key)").unwrap());
        // target_feature 由 rustc 列表精确覆盖（x86_64 基线 = sse/sse2 真、avx2 假）
        assert!(eval_cfg("cfg(target_feature=\"sse2\")").unwrap());
        assert!(!eval_cfg("cfg(target_feature=\"avx512f\")").unwrap());
        // 复杂嵌套（rustix 形态微缩版）
        assert!(eval_cfg(
            "cfg(all(not(rustix_use_libc), target_os=\"linux\", any(target_arch=\"x86_64\", target_arch=\"aarch64\")))"
        )
        .unwrap());
    }

    #[test]
    fn target_specific_dep_tables_carry_cfg_expr_unfiltered() {
        // 全平台并集语义：解析期不过滤，cfg 表达式随行（cargo lock 同语）
        let m = PackageManifest::parse(
            "[package]\nname=\"d\"\nversion=\"0.1.0\"\n\
             [target.'cfg(unix)'.dependencies]\nnix = \"0.29\"\n\
             [target.'cfg(windows)'.dependencies]\nwinapi = \"0.3\"\n",
            Path::new("/tmp/x"),
        )
        .unwrap();
        let nix = m.deps.iter().find(|d| d.key == "nix").unwrap();
        let winapi = m.deps.iter().find(|d| d.key == "winapi").unwrap();
        assert_eq!(nix.platform_cfg.as_deref(), Some("cfg(unix)"));
        assert_eq!(winapi.platform_cfg.as_deref(), Some("cfg(windows)"));
        // 普通表无标记
        assert!(nix.platform_cfg.is_some());
        // host 求值在使用期：unix 真、windows 假
        assert!(eval_cfg(nix.platform_cfg.as_ref().unwrap()).unwrap());
        assert!(!eval_cfg(winapi.platform_cfg.as_ref().unwrap()).unwrap());
    }

    #[test]
    fn autodiscovers_lib_main_and_bin_dir() {
        let d = tmpdir("autodisc");
        std::fs::create_dir_all(d.join("src/bin")).unwrap();
        std::fs::write(d.join("src/lib.rs"), "").unwrap();
        std::fs::write(d.join("src/main.rs"), "fn main(){}").unwrap();
        std::fs::write(d.join("src/bin/extra.rs"), "fn main(){}").unwrap();
        let m = PackageManifest::parse("[package]\nname=\"d\"\nversion=\"0.1.0\"\n", &d).unwrap();
        let bins: Vec<_> = m
            .targets
            .iter()
            .filter_map(|t| match t {
                Target::Bin { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert!(m.targets.iter().any(|t| matches!(t, Target::Lib { .. })));
        assert_eq!(bins, ["d", "extra"]);
        // 多 bin 时 runnable_bin 响亮拒绝；default-run 钉选可解
        assert!(m.runnable_bin().unwrap_err().contains("P5"));
        let m2 = PackageManifest::parse(
            "[package]\nname=\"d\"\nversion=\"0.1.0\"\ndefault-run=\"extra\"\n",
            &d,
        )
        .unwrap();
        assert_eq!(m2.runnable_bin().unwrap().0, "extra");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn build_script_detection_and_workspace_package_inheritance() {
        let d = tmpdir("buildrs");
        std::fs::create_dir_all(d.join("src")).unwrap();
        std::fs::write(d.join("src/main.rs"), "fn main(){}").unwrap();
        std::fs::write(d.join("build.rs"), "fn main(){}").unwrap();
        let m = PackageManifest::parse(
            "[package]\nname=\"d\"\nedition.workspace = true\nversion.workspace = true\n\
             [workspace]\n[workspace.package]\nversion = \"9.9.9\"\nedition = \"2021\"\n",
            &d,
        )
        .unwrap();
        assert_eq!(m.version.to_string(), "9.9.9");
        assert_eq!(m.edition, "2021");
        assert!(m.has_build_script);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn frontmatter_pseudo_package_parses() {
        let d = tmpdir("frontmatter");
        let body = d.join("main.rs");
        std::fs::write(&body, "fn main(){}").unwrap();
        let m = PackageManifest::from_frontmatter(
            "c_demo",
            "[dependencies]\nserde_json = \"1\"\n",
            &body,
        )
        .unwrap();
        assert_eq!(m.name, "c_demo");
        assert_eq!(m.edition, "2024");
        assert_eq!(m.deps.len(), 1);
        assert_eq!(m.runnable_bin().unwrap().0, "c_demo");
        std::fs::remove_dir_all(&d).unwrap();
    }
}
