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

use std::collections::{BTreeMap, BTreeSet};
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
    /// `[package] build = "custom.rs"` 的自定义 build script 路径；
    /// None = 缺省 <root>/build.rs（切③ build.rs 调度用）。
    pub build_script_path: Option<PathBuf>,
    /// `[package] links`（-sys 链接键；native 库名推导与 build.rs 调度用）。
    pub links: Option<String>,
    pub default_run: Option<String>,
    /// CARGO_PKG_* 编译期 env 全集（缺键 = 空串，cargo 同契约；pkg_env_map 计算）。
    pub pkg_env: BTreeMap<String, String>,
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
    // 以下均为 CARGO_PKG_* env 原料（pkg_env_map 消费；缺键 = 空串，cargo 同）
    authors: Option<Vec<String>>,
    description: Option<String>,
    homepage: Option<String>,
    repository: Option<String>,
    license: Option<String>,
    #[serde(rename = "license-file")]
    license_file: Option<toml::Value>,
    readme: Option<toml::Value>,
    #[serde(rename = "rust-version")]
    rust_version: Option<toml::Value>,
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
    #[serde(rename = "rust-version")]
    rust_version: Option<String>,
}

#[derive(serde::Deserialize, Default)]
struct RawLib {
    name: Option<String>,
    path: Option<String>,
    // 两种拼写都收：连字符是 cargo 文档形态（用户手写），下划线是新版
    // cargo 归一化产物形态（derive_arbitrary 1.3.2 实锤；cargo 双侧受理）
    #[serde(rename = "proc-macro", alias = "proc_macro")]
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
        // CARGO_PKG_* env 全集（cargo 契约：编译期 env! 可读；缺键 = 空串）。
        // readme = true 归约为 "README.md"（cargo 同）；license-file 只收字符串形。
        let rust_version = match pkg.rust_version {
            Some(toml::Value::String(v)) => Some(v),
            Some(toml::Value::Table(t)) if t.get("workspace").is_some() => {
                ws_pkg.and_then(|w| w.rust_version.clone())
            }
            _ => None,
        };
        let readme = match &pkg.readme {
            Some(toml::Value::String(s)) => Some(s.clone()),
            Some(toml::Value::Boolean(true)) => Some("README.md".to_string()),
            _ => None,
        };
        let license_file = match &pkg.license_file {
            Some(toml::Value::String(s)) => Some(s.clone()),
            _ => None,
        };
        let pkg_env = pkg_env_map(
            &name,
            &version,
            pkg.authors.as_deref(),
            pkg.description.as_deref(),
            pkg.homepage.as_deref(),
            pkg.repository.as_deref(),
            pkg.license.as_deref(),
            license_file.as_deref(),
            readme.as_deref(),
            rust_version.as_deref(),
        );
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
        // cargo 语义：`build = false` 是显式关闭 build script（cfg-if 实锤——
        // 键在场 ≠ 有 build.rs）；字符串形 = 自定义路径；缺省 = 根下 build.rs 实存。
        let has_build_script = pkg.links.is_some()
            || match &pkg.build {
                Some(toml::Value::Boolean(false)) => false,
                Some(_) => true,
                None => root.join("build.rs").is_file(),
            };
        let build_script_path = match &pkg.build {
            Some(toml::Value::String(p)) => Some(root.join(p)),
            _ => None,
        };

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
            build_script_path,
            links: pkg.links,
            default_run: pkg.default_run,
            pkg_env,
        })
    }

    /// frontmatter 伪包：脚本 stem 为包名，依赖段原文喂给同一解析。
    /// bin 名带哈希短缀（与旧物化口径一致，消 target 目录碰撞，cli.rs 旧例）。
    pub fn from_frontmatter(
        stem: &str,
        manifest_text: &str,
        body_path: &Path,
    ) -> Result<Self, MErr> {
        let root = body_path.parent().unwrap_or(Path::new("."));
        Self::from_frontmatter_at(stem, manifest_text, root, body_path)
    }

    /// from_frontmatter 的 root 显式版（D15 P3 切⑤d）：脚本缓存布局与
    /// cargo 腿物化项目同形（cli.rs materialize_script：Cargo.toml 在
    /// <cache>、正文在 <cache>/src/main.rs）——root=<cache> 保证
    /// CARGO_MANIFEST_DIR 与 cargo 腿一致，bin 路径 = <cache>/src/main.rs
    /// 保证 remap 后 file!() = "src/main.rs"（redb_kv/gix_pure 实锤）。
    pub fn from_frontmatter_at(
        stem: &str,
        manifest_text: &str,
        root: &Path,
        body_path: &Path,
    ) -> Result<Self, MErr> {
        let pseudo = format!(
            "[package]\nname = \"{stem}\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\
             [[bin]]\nname = \"{stem}\"\npath = \"{}\"\n{manifest_text}",
            body_path.display()
        );
        Self::parse(&pseudo, root)
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

    /// `--check-cfg cfg(feature, values(...))` 的合法值表（cargo bin 侧同口径）：
    /// [features] 表键 ∪ 隐式 optional 依赖键（optional dep 键未被任何 feature
    /// 值里的 `dep:key` 点名时，存在同名隐式 feature——与 resolve.rs expand_node
    /// 的 hidden 规则同一条）。
    pub fn check_cfg_feature_values(&self) -> BTreeSet<String> {
        let mut out: BTreeSet<String> = self.features.keys().cloned().collect();
        let hidden: BTreeSet<String> = self
            .features
            .values()
            .flatten()
            .filter_map(|v| match v {
                FeatureValue::DepActivation(d) => Some(d.clone()),
                _ => None,
            })
            .collect();
        for d in &self.deps {
            if d.optional && !hidden.contains(&d.key) {
                out.insert(d.key.clone());
            }
        }
        out
    }
}

/// CARGO_PKG_* env 全集（cargo 编译期 env 契约，env!/option_env! 可读）。
/// 缺键 = 空串（cargo 就是设空串）；VERSION_MAJOR/MINOR/PATCH/PRE 由 semver 拆开
/// （PRE = pre 段字符串，无 pre = 空）；AUTHORS 数组以 ":" 连。
/// manifest.rs（根/path 包）与 resolve.rs（registry 包最小读取）两边同调这一份。
// 平铺参数 = cargo 的平铺 env 键集一一对应（D15 切① 简报钉死的签名）；
// 包成 struct 反而失去与 manifest 键的目视对应
#[allow(clippy::too_many_arguments)]
pub fn pkg_env_map(
    name: &str,
    version: &semver::Version,
    authors: Option<&[String]>,
    description: Option<&str>,
    homepage: Option<&str>,
    repository: Option<&str>,
    license: Option<&str>,
    license_file: Option<&str>,
    readme: Option<&str>,
    rust_version: Option<&str>,
) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    let mut put = |k: &str, v: &str| {
        m.insert(k.to_string(), v.to_string());
    };
    put("CARGO_PKG_NAME", name);
    put("CARGO_PKG_VERSION", &version.to_string());
    put("CARGO_PKG_VERSION_MAJOR", &version.major.to_string());
    put("CARGO_PKG_VERSION_MINOR", &version.minor.to_string());
    put("CARGO_PKG_VERSION_PATCH", &version.patch.to_string());
    put("CARGO_PKG_VERSION_PRE", version.pre.as_str());
    put("CARGO_PKG_AUTHORS", &authors.unwrap_or(&[]).join(":"));
    put("CARGO_PKG_DESCRIPTION", description.unwrap_or(""));
    put("CARGO_PKG_HOMEPAGE", homepage.unwrap_or(""));
    put("CARGO_PKG_LICENSE", license.unwrap_or(""));
    put("CARGO_PKG_LICENSE_FILE", license_file.unwrap_or(""));
    put("CARGO_PKG_README", readme.unwrap_or(""));
    put("CARGO_PKG_REPOSITORY", repository.unwrap_or(""));
    put("CARGO_PKG_RUST_VERSION", rust_version.unwrap_or(""));
    m
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

/// 暴露给 buildrs.rs 的 CARGO_CFG_* 映射（切③）。
pub(crate) fn host_cfg_atoms() -> &'static std::collections::BTreeSet<String> {
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
    fn lib_proc_macro_accepts_both_spellings() {
        // 两种拼写都收（resolve.rs registry 最小读取同款纪律：连字符 =
        // cargo 文档形态，下划线 = 新版归一化产物形态）
        let d = tmpdir("libpm");
        std::fs::create_dir_all(d.join("src")).unwrap();
        std::fs::write(d.join("src/lib.rs"), "").unwrap();
        for key in ["proc-macro", "proc_macro"] {
            let m = PackageManifest::parse(
                &format!("[package]\nname=\"d\"\nversion=\"0.1.0\"\n[lib]\n{key} = true\n"),
                &d,
            )
            .unwrap();
            let pm = m.targets.iter().any(|t| {
                matches!(
                    t,
                    Target::Lib {
                        proc_macro: true,
                        ..
                    }
                )
            });
            assert!(pm, "拼写 {key} 必须识别");
        }
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
    fn build_eq_false_disables_build_script_detection() {
        // cargo 语义：`build = false` 显式关闭（cfg-if 实锤——键在场 ≠ 有 build.rs）；
        // 即便根下躺着 build.rs 也不算（cargo 同）
        let d = tmpdir("buildfalse");
        std::fs::create_dir_all(d.join("src")).unwrap();
        std::fs::write(d.join("src/main.rs"), "fn main(){}").unwrap();
        std::fs::write(d.join("build.rs"), "fn main(){}").unwrap();
        let m = PackageManifest::parse(
            "[package]\nname = \"d\"\nversion = \"0.1.0\"\nbuild = false\n",
            &d,
        )
        .unwrap();
        assert!(!m.has_build_script);
        let m = PackageManifest::parse(
            "[package]\nname = \"d\"\nversion = \"0.1.0\"\nbuild = \"custom.rs\"\n",
            &d,
        )
        .unwrap();
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

    #[test]
    fn frontmatter_at_decouples_root_and_body_path() {
        // cargo 腿物化布局同形（切⑤d redb_kv/gix_pure 实锤）：root=<cache>
        // （CARGO_MANIFEST_DIR 口径）而正文在 <cache>/src/main.rs
        // （remap 后 file!() = "src/main.rs"）。
        let d = tmpdir("frontmatterat");
        let body = d.join("src/main.rs");
        std::fs::create_dir_all(body.parent().unwrap()).unwrap();
        std::fs::write(&body, "fn main(){}").unwrap();
        let m = PackageManifest::from_frontmatter_at(
            "c_demo",
            "[dependencies]\nserde_json = \"1\"\n",
            &d,
            &body,
        )
        .unwrap();
        assert_eq!(m.root, d);
        assert_eq!(m.runnable_bin().unwrap().1, body.as_path());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn pkg_env_splits_version_and_blanks_missing_keys() {
        let m = PackageManifest::parse(
            r#"
[package]
name = "demo"
version = "1.2.3-rc.1"
edition = "2021"
authors = ["Alice <a@x>", "Bob"]
description = "演示包"
readme = true
license = "MIT"
"#,
            Path::new("/tmp/x"),
        )
        .unwrap();
        let e = &m.pkg_env;
        assert_eq!(e["CARGO_PKG_NAME"], "demo");
        assert_eq!(e["CARGO_PKG_VERSION"], "1.2.3-rc.1");
        assert_eq!(e["CARGO_PKG_VERSION_MAJOR"], "1");
        assert_eq!(e["CARGO_PKG_VERSION_MINOR"], "2");
        assert_eq!(e["CARGO_PKG_VERSION_PATCH"], "3");
        assert_eq!(e["CARGO_PKG_VERSION_PRE"], "rc.1");
        assert_eq!(e["CARGO_PKG_AUTHORS"], "Alice <a@x>:Bob");
        assert_eq!(e["CARGO_PKG_DESCRIPTION"], "演示包");
        assert_eq!(e["CARGO_PKG_README"], "README.md");
        assert_eq!(e["CARGO_PKG_LICENSE"], "MIT");
        // 缺键 = 空串（cargo 同契约），但键必须在场
        for k in [
            "CARGO_PKG_HOMEPAGE",
            "CARGO_PKG_REPOSITORY",
            "CARGO_PKG_LICENSE_FILE",
            "CARGO_PKG_RUST_VERSION",
        ] {
            assert_eq!(e.get(k).map(String::as_str), Some(""), "{k} 应为空串");
        }
        // 无 pre 段的版本 PRE = 空串
        let e2 = pkg_env_map(
            "d",
            &semver::Version::new(0, 1, 0),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );
        assert_eq!(e2["CARGO_PKG_VERSION_PRE"], "");
        assert_eq!(e2["CARGO_PKG_AUTHORS"], "");
    }

    #[test]
    fn check_cfg_values_include_implicit_optional_and_exclude_dep_shadowed() {
        let m = PackageManifest::parse(
            "[package]\nname = \"d\"\nversion = \"0.1.0\"\n\
             [dependencies]\nserde = { version = \"1\", optional = true }\n\
             itertools = { version = \"0.14\", optional = true }\n\
             plain = \"1\"\n\
             [features]\ndefault = [\"dep:serde\"]\nextra = []\n",
            Path::new("/tmp/x"),
        )
        .unwrap();
        let vals = m.check_cfg_feature_values();
        // [features] 表键在场
        assert!(vals.contains("default"));
        assert!(vals.contains("extra"));
        // 未被 dep: 点名的 optional 依赖 → 同名隐式 feature
        assert!(vals.contains("itertools"));
        // 被 dep:serde 遮蔽 → 无同名隐式 feature
        assert!(!vals.contains("serde"));
        // 非 optional 依赖永不进值表
        assert!(!vals.contains("plain"));
    }
}
