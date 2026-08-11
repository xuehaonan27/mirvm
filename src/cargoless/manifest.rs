//! `cargoless/manifest.rs` —— Cargo.toml 解析与模型（D15 P1，设计档 §3.1）。
//!
//! 子集边界（超出即响亮拒绝并记名，不静默吞掉）：
//! - `[package]`（name/version/edition/autobins/default-run/links）
//! - `[lib]` / `[[bin]]` / `[[test]]` / `[[example]]` 与 Cargo 自动发现
//! - `[dependencies]` / `[build-dependencies]` / `[dev-dependencies]`：version req、features、
//!   optional、default-features、path、git（默认分支/branch/tag/rev）；私有 registry
//!   仍属 P5，响亮拒绝
//! - `[features]` 三形态：`"foo"`（特性或隐式可选依赖）、`"dep:foo"`（显式
//!   依赖激活）、`"foo?/bar"`（弱激活）
//! - `[profile.*]`：只取 debug-assertions / overflow-checks / opt-level
//!   （语义钉死：前两枚进 MIR 语义，设计档 §6 的 jiff 判例）
//! - `target.'cfg()'.dependencies` 平台求值：目标平台原子（target_os/
//!   target_arch/target_family/unix/target_vendor/target_env/target_abi/
//!   target_pointer_width/target_endian）+ any/all/not 组合；
//!   `cfg(feature=..)` 不属于平台求值（cargo 同）；`cfg(target_feature=..)`
//!   响亮拒绝（归 P5）
//! - `[workspace]`：`workspace.rs` 先发现 resolver=1/2/3 多包图并物化
//!   workspace.package/workspace.dependencies/root profile；本文件只解析物化后的包
//! - doctest 仍不做；测试与 bench 目标由 `mirvm test` 消费。

// 模型中仍有只被部分命令消费的字段，暂按模块边界保留。
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Cargo 的全局依赖求解规则版本。依赖包里自己的值会被顶层 package/workspace
/// 覆盖，但仍需保存在 manifest 模型中，供单包作为顶层运行时使用。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolverVersion {
    V1,
    V2,
    V3,
}

impl ResolverVersion {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "1" => Ok(Self::V1),
            "2" => Ok(Self::V2),
            "3" => Ok(Self::V3),
            other => Err(format!("resolver 必须是 1、2 或 3，实际为 `{other}`")),
        }
    }

    pub fn inferred(edition: &str) -> Self {
        match edition {
            "2021" => Self::V2,
            "2024" => Self::V3,
            _ => Self::V1,
        }
    }
}

/// Cargo 对依赖声明的最低 Rust 版本不兼容时采用的选择策略。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IncompatibleRustVersions {
    /// 不改变通常的“选择最高版本”顺序。
    Allow,
    /// 优先选择兼容版本；一个都没有时仍退回最高的不兼容版本。
    Fallback,
}

impl IncompatibleRustVersions {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "allow" => Ok(Self::Allow),
            "fallback" => Ok(Self::Fallback),
            other => Err(format!(
                "resolver.incompatible-rust-versions 只接受 `allow` 或 `fallback`，实际为 `{other}`"
            )),
        }
    }
}

/// Cargo 的 rust-version 允许 1、2 或 3 段裸数字，不接受 semver 运算符、
/// prerelease 或 build metadata。内部补齐到三段，便于稳定比较。
pub fn parse_rust_version(value: &str, field: &str) -> Result<semver::Version, String> {
    let parts: Vec<&str> = value.split('.').collect();
    if parts.is_empty()
        || parts.len() > 3
        || parts
            .iter()
            .any(|part| part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return Err(format!(
            "{field} 必须是 1、2 或 3 段裸版本号，实际为 `{value}`"
        ));
    }
    let normalized = match parts.len() {
        1 => format!("{}.0.0", parts[0]),
        2 => format!("{}.{}.0", parts[0], parts[1]),
        3 => value.to_string(),
        _ => unreachable!(),
    };
    semver::Version::parse(&normalized).map_err(|error| format!("{field} 非法 `{value}`: {error}"))
}

/// mirvm 实际内嵌 rustc 对应的版本。使用构建时 sysroot 中的 rustc，避免 PATH
/// 上另一个工具链影响依赖选择。
pub fn current_rust_version() -> Result<semver::Version, String> {
    static VERSION: std::sync::OnceLock<semver::Version> = std::sync::OnceLock::new();
    if let Some(version) = VERSION.get() {
        return Ok(version.clone());
    }
    let rustc = PathBuf::from(env!("MIRVM_DEFAULT_SYSROOT")).join("bin/rustc");
    let command = std::process::Command::new(&rustc);
    let mut version = rustc_version::VersionMeta::for_command(command)
        .map_err(|error| format!("读取 {} 版本失败: {error}", rustc.display()))?
        .semver;
    version.pre = semver::Prerelease::EMPTY;
    version.build = semver::BuildMetadata::EMPTY;
    let _ = VERSION.set(version.clone());
    Ok(version)
}

/// 依赖来源。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DepSource {
    /// registry 依赖：semver 需求与 manifest 中指定的仓库。
    Registry(semver::VersionReq, RegistryReference),
    /// 本地路径依赖（已绝对化）。
    Path(PathBuf),
    /// Git 仓库依赖；可变引用会在获取阶段解析成精确 commit，lock 只记录精确结果。
    Git(GitSpec),
}

/// Manifest 中的 registry 写法。名称和显式 index 到依赖求解开始时才通过
/// Cargo config 归约成 lock/source 使用的稳定 URL。
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RegistryReference {
    CratesIo,
    Named(String),
    Index(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitReference {
    DefaultBranch,
    Branch(String),
    Tag(String),
    Rev(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitSpec {
    pub url: String,
    pub reference: GitReference,
    pub version: semver::VersionReq,
}

impl GitSpec {
    /// Cargo.lock 中 `#<commit>` 之前的 source id。
    pub fn source_id(&self) -> String {
        let query = match &self.reference {
            GitReference::DefaultBranch => None,
            GitReference::Branch(value) => Some(("branch", value.as_str())),
            GitReference::Tag(value) => Some(("tag", value.as_str())),
            GitReference::Rev(value) => Some(("rev", value.as_str())),
        };
        match query {
            Some((key, value)) => format!("git+{}?{key}={}", self.url, percent_encode(value)),
            None => format!("git+{}", self.url),
        }
    }
}

fn percent_encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// 依赖种类（dev-deps 不建）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DepKind {
    Normal,
    Build,
    /// 只在根包作为测试对象时进入构建图；不传播 path/registry 依赖自己的 dev 边。
    Dev,
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

#[derive(Clone, Debug)]
pub struct PatchDecl {
    pub registry: RegistryReference,
    pub dependency: DepDecl,
}

#[derive(Clone, Debug)]
pub struct ReplaceDecl {
    pub package: String,
    pub version: semver::Version,
    pub source: Option<String>,
    pub dependency: DepDecl,
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

/// Cargo 目标种类。build script 仍由 package.build/links 单独建模。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum TargetKind {
    Lib,
    Bin,
    Test,
    Example,
    Bench,
}

/// 一个可编译目标及其影响测试选择/编译方式的 manifest 属性。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub kind: TargetKind,
    pub name: String,
    pub path: PathBuf,
    pub proc_macro: bool,
    /// Cargo 默认 `test` 选择是否包含本目标。
    pub test: bool,
    /// true = rustc `--test` 注入 libtest；false = 保留目标自己的 main。
    pub harness: bool,
    pub doctest: bool,
    pub required_features: Vec<String>,
}

impl Target {
    pub fn is_lib(&self) -> bool {
        self.kind == TargetKind::Lib
    }

    pub fn is_bin(&self) -> bool {
        self.kind == TargetKind::Bin
    }
}

/// profile 语义旗（只取影响 MIR 语义的 + 照传的 opt-level）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProfileFlags {
    pub debug_assertions: bool,
    pub overflow_checks: bool,
    pub opt_level: OptLevel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OptLevel {
    O0,
    O1,
    O2,
    O3,
    Os,
    Oz,
}

impl OptLevel {
    pub fn is_zero(self) -> bool {
        self == Self::O0
    }
}

impl std::fmt::Display for OptLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::O0 => "0",
            Self::O1 => "1",
            Self::O2 => "2",
            Self::O3 => "3",
            Self::Os => "s",
            Self::Oz => "z",
        })
    }
}

impl Default for ProfileFlags {
    /// cargo dev profile 等价（设计档 §6 语义钉）。
    fn default() -> Self {
        Self {
            debug_assertions: true,
            overflow_checks: true,
            opt_level: OptLevel::O0,
        }
    }
}

/// 一个包的完整模型。
#[derive(Clone, Debug)]
pub struct PackageManifest {
    pub name: String,
    pub version: semver::Version,
    pub edition: String,
    /// 顶层 package/workspace 采用的全局 resolver；path/registry 依赖自身的值不生效。
    pub resolver: ResolverVersion,
    /// 本包声明的最低 Rust 版本。
    pub rust_version: Option<semver::Version>,
    /// resolver 3 的工作区比较基准。工作区会写入所有成员最低值；单包为自身
    /// rust-version，缺席时由求解器使用当前 rustc。
    pub resolver_rust_version: Option<semver::Version>,
    /// `--ignore-rust-version` 同时关闭候选偏好与编译器版本拒绝。
    pub ignore_rust_version: bool,
    pub root: PathBuf,
    /// Cargo.lock 所属目录。单包等于 root；workspace 成员指向 workspace 根。
    pub lock_root: PathBuf,
    pub targets: Vec<Target>,
    pub deps: Vec<DepDecl>,
    /// 只有顶层 package/workspace 根的覆盖生效；workspace 发现层会把根值物化给成员。
    pub patches: Vec<PatchDecl>,
    pub replacements: Vec<ReplaceDecl>,
    pub features: BTreeMap<String, Vec<FeatureValue>>,
    /// CLI 对这个测试根显式请求的 feature；依赖边的 feature 仍由 resolver 传播。
    pub requested_features: BTreeSet<String>,
    /// CLI 的 `dependency/feature` 请求；向依赖边传播，不是根包 cfg feature。
    pub dependency_features: BTreeMap<String, BTreeSet<String>>,
    pub default_features_enabled: bool,
    pub profile: ProfileFlags,
    pub test_profile: ProfileFlags,
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
    /// Cargo `[lints]` 归约出的 rustc 参数。先放按 priority 排序的 level 参数，
    /// 再放 `unexpected_cfgs` 的 `--check-cfg` 两槽参数。
    pub rustc_lint_flags: Vec<String>,
    /// 非 registry 包在 Cargo.lock 中的 source。根/普通 path 为 None；Git 包为
    /// `git+URL?...#commit`。
    pub lock_source: Option<String>,
    /// Git checkout 根，用于让仓库内 path 依赖继承相同 Git source。
    pub git_checkout_root: Option<PathBuf>,
}

// ---------- serde 原料（宽松，未知键忽略，已知不支持的键后置校验）----------

#[derive(serde::Deserialize, Default)]
struct RawManifest {
    package: Option<RawPackage>,
    workspace: Option<RawWorkspace>,
    patch: Option<toml::Value>,
    replace: Option<toml::Value>,
    lints: Option<toml::Value>,
    lib: Option<RawLib>,
    bin: Option<Vec<RawBin>>,
    test: Option<Vec<RawTarget>>,
    example: Option<Vec<RawTarget>>,
    bench: Option<Vec<RawTarget>>,
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
    resolver: Option<String>,
    autobins: Option<bool>,
    autoexamples: Option<bool>,
    autotests: Option<bool>,
    autobenches: Option<bool>,
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
    resolver: Option<String>,
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
    test: Option<bool>,
    harness: Option<bool>,
    doctest: Option<bool>,
    #[serde(rename = "required-features")]
    required_features: Option<Vec<String>>,
}

#[derive(serde::Deserialize, Default)]
struct RawBin {
    name: Option<String>,
    path: Option<String>,
    test: Option<bool>,
    harness: Option<bool>,
    #[serde(rename = "required-features")]
    required_features: Option<Vec<String>>,
}

#[derive(serde::Deserialize, Default)]
struct RawTarget {
    name: Option<String>,
    path: Option<String>,
    test: Option<bool>,
    harness: Option<bool>,
    #[serde(rename = "required-features")]
    required_features: Option<Vec<String>>,
}

#[derive(serde::Deserialize, Default)]
struct RawProfiles {
    dev: Option<RawProfile>,
    test: Option<RawProfile>,
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
    #[serde(rename = "dev-dependencies")]
    dev_dependencies: Option<BTreeMap<String, toml::Value>>,
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
        let rustc_lint_flags = parse_lints(raw.lints.as_ref())?;
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
        if !matches!(edition.as_str(), "2015" | "2018" | "2021" | "2024") {
            return Err(format!("package.edition 不支持 `{edition}`"));
        }
        let resolver = raw
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.resolver.as_deref())
            .or(pkg.resolver.as_deref())
            .map(ResolverVersion::parse)
            .transpose()?
            .unwrap_or_else(|| ResolverVersion::inferred(&edition));
        // CARGO_PKG_* env 全集（cargo 契约：编译期 env! 可读；缺键 = 空串）。
        // readme = true 归约为 "README.md"（cargo 同）；license-file 只收字符串形。
        let rust_version_text = match pkg.rust_version {
            Some(toml::Value::String(v)) => Some(v),
            Some(toml::Value::Table(t)) if t.get("workspace").is_some() => {
                ws_pkg.and_then(|w| w.rust_version.clone())
            }
            Some(_) => return Err("package.rust-version 必须是字符串或 workspace 继承".into()),
            None => None,
        };
        let rust_version = rust_version_text
            .as_deref()
            .map(|version| parse_rust_version(version, "package.rust-version"))
            .transpose()?;
        if let Some(version) = &rust_version {
            let minimum = match edition.as_str() {
                "2015" => semver::Version::new(1, 0, 0),
                "2018" => semver::Version::new(1, 31, 0),
                "2021" => semver::Version::new(1, 56, 0),
                "2024" => semver::Version::new(1, 85, 0),
                _ => unreachable!(),
            };
            if version < &minimum {
                return Err(format!(
                    "package.rust-version {} 与 edition {edition} 所需的 Rust {minimum} 不兼容",
                    rust_version_text.as_deref().unwrap_or("")
                ));
            }
        }
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
            rust_version_text.as_deref(),
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
        parse_dep_table(&raw.dev_dependencies, DepKind::Dev, root, None, &mut deps)?;
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
            parse_dep_table(
                &tdeps.dev_dependencies,
                DepKind::Dev,
                root,
                Some(cfg_expr.clone()),
                &mut deps,
            )?;
        }
        let patches = parse_patches(raw.patch.as_ref(), root)?;
        let replacements = parse_replacements(raw.replace.as_ref(), root)?;

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
        let autoexamples = pkg.autoexamples.unwrap_or(true);
        let autotests = pkg.autotests.unwrap_or(true);
        let autobenches = pkg.autobenches.unwrap_or(true);
        let targets = discover_targets(
            raw.lib.as_ref(),
            raw.bin.as_ref(),
            raw.test.as_ref(),
            raw.example.as_ref(),
            raw.bench.as_ref(),
            autobins,
            autotests,
            autoexamples,
            autobenches,
            &name,
            root,
        )?;
        let (profile, test_profile) = profiles_from(raw.profile)?;
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
            resolver,
            rust_version: rust_version.clone(),
            resolver_rust_version: rust_version,
            ignore_rust_version: false,
            root: root.to_path_buf(),
            lock_root: root.to_path_buf(),
            targets,
            deps,
            patches,
            replacements,
            features,
            requested_features: BTreeSet::new(),
            dependency_features: BTreeMap::new(),
            default_features_enabled: true,
            profile,
            test_profile,
            has_build_script,
            build_script_path,
            links: pkg.links,
            default_run: pkg.default_run,
            pkg_env,
            rustc_lint_flags,
            lock_source: None,
            git_checkout_root: None,
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
        self.runnable_bin_opt(None)
    }

    /// 带 `--bin` 选择的 bin 选定（D15 P4 切⑥b，cargo run --bin 语义）：
    /// Some(name) = 按名精确选（不在 [[bin]] 中 = 响亮报错并列出可选名单，
    /// cargo 同文案）；None = 走 runnable_bin 的 default-run > 唯一 > 拒绝链。
    pub fn runnable_bin_opt(&self, sel: Option<&str>) -> Result<(&str, &Path), MErr> {
        let bins: Vec<_> = self
            .targets
            .iter()
            .filter(|t| t.is_bin())
            .map(|t| (t.name.as_str(), t.path.as_path()))
            .collect();
        if let Some(want) = sel {
            if let Some(b) = bins.iter().find(|(n, _)| *n == want) {
                return Ok(*b);
            }
            return Err(format!(
                "没有名为 `{want}` 的 bin 目标（可用：{}）",
                bins.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", ")
            ));
        }
        if let Some(dr) = &self.default_run {
            if let Some(b) = bins.iter().find(|(n, _)| *n == dr) {
                return Ok(*b);
            }
            return Err(format!("default-run={dr} 在 [[bin]] 中不存在"));
        }
        match bins.len() {
            0 => Err(format!("包 {} 没有 bin 目标", self.name)),
            1 => Ok(bins[0]),
            _ => Err(format!(
                "多 bin 目标（{}）——请用 --bin 选定或 default-run 钉选",
                bins.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", ")
            )),
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

#[derive(Debug)]
struct ParsedLint {
    priority: i64,
    flag: String,
    check_cfg: Vec<String>,
}

/// Cargo `[lints]` 到 rustc argv 的归约。Cargo 保留同 priority 项的清单顺序；
/// TOML map 启用 preserve_order，因此稳定排序只按 priority 即可复现。
pub(super) fn parse_lints(value: Option<&toml::Value>) -> Result<Vec<String>, MErr> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let tools = value
        .as_table()
        .ok_or_else(|| "[lints] 必须是表".to_string())?;
    if tools.contains_key("workspace") {
        return Err(unsupported(
            "[lints] workspace 继承（应先由 workspace 层物化）",
        ));
    }

    let mut parsed = Vec::new();
    for (tool, lints) in tools {
        let lints = lints
            .as_table()
            .ok_or_else(|| format!("[lints.{tool}] 必须是表"))?;
        for (name, spec) in lints {
            let (level, priority, check_cfg) = match spec {
                toml::Value::String(level) => (level.as_str(), 0, Vec::new()),
                toml::Value::Table(table) => {
                    if let Some(key) = table
                        .keys()
                        .find(|key| !matches!(key.as_str(), "level" | "priority" | "check-cfg"))
                    {
                        return Err(format!("lints.{tool}.{name} 含 Cargo 不认识的键 `{key}`"));
                    }
                    let level = table
                        .get("level")
                        .and_then(toml::Value::as_str)
                        .ok_or_else(|| format!("lints.{tool}.{name}.level 必须是字符串"))?;
                    let priority = match table.get("priority") {
                        Some(value) => value
                            .as_integer()
                            .ok_or_else(|| format!("lints.{tool}.{name}.priority 必须是整数"))?,
                        None => 0,
                    };
                    let check_cfg = match table.get("check-cfg") {
                        Some(value) => {
                            if tool != "rust" || name != "unexpected_cfgs" {
                                return Err(
                                    "check-cfg 只允许写在 lints.rust.unexpected_cfgs".to_string()
                                );
                            }
                            value
                                .as_array()
                                .ok_or_else(|| {
                                    "lints.rust.unexpected_cfgs.check-cfg 必须是字符串数组"
                                        .to_string()
                                })?
                                .iter()
                                .map(|item| {
                                    item.as_str().map(str::to_string).ok_or_else(|| {
                                        "lints.rust.unexpected_cfgs.check-cfg 包含非字符串成员"
                                            .to_string()
                                    })
                                })
                                .collect::<Result<Vec<_>, _>>()?
                        }
                        None => Vec::new(),
                    };
                    (level, priority, check_cfg)
                }
                _ => {
                    return Err(format!("lints.{tool}.{name} 必须是 level 字符串或配置表"));
                }
            };
            if !matches!(level, "allow" | "warn" | "deny" | "forbid") {
                return Err(format!(
                    "lints.{tool}.{name}.level 只接受 allow/warn/deny/forbid，实际为 `{level}`"
                ));
            }
            let qualified = if tool == "rust" {
                name.clone()
            } else {
                format!("{tool}::{name}")
            };
            parsed.push(ParsedLint {
                priority,
                flag: format!("--{level}={qualified}"),
                check_cfg,
            });
        }
    }
    parsed.sort_by_key(|lint| lint.priority);
    let mut flags = parsed
        .iter()
        .map(|lint| lint.flag.clone())
        .collect::<Vec<_>>();
    for check_cfg in parsed.into_iter().flat_map(|lint| lint.check_cfg) {
        flags.push("--check-cfg".into());
        flags.push(check_cfg);
    }
    Ok(flags)
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

fn parse_patches(value: Option<&toml::Value>, root: &Path) -> Result<Vec<PatchDecl>, MErr> {
    let Some(registries) = value else {
        return Ok(Vec::new());
    };
    let registries = registries
        .as_table()
        .ok_or_else(|| "[patch] 必须是 registry 表".to_string())?;
    let mut out = Vec::new();
    for (registry, entries) in registries {
        let entries = entries
            .as_table()
            .ok_or_else(|| format!("[patch.{registry}] 必须是依赖表"))?;
        let table = Some(
            entries
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        );
        let mut dependencies = Vec::new();
        parse_dep_table(&table, DepKind::Normal, root, None, &mut dependencies)?;
        let registry = if registry == "crates-io" {
            RegistryReference::CratesIo
        } else if registry.contains("://") || registry.starts_with("sparse+") {
            RegistryReference::Index(registry.clone())
        } else {
            RegistryReference::Named(registry.clone())
        };
        for dependency in dependencies {
            if matches!(
                dependency.source,
                DepSource::Registry(_, RegistryReference::CratesIo)
            ) {
                return Err(format!(
                    "[patch] 包 {} 没有声明 path/git/其他 registry 来源",
                    dependency.package
                ));
            }
            out.push(PatchDecl {
                registry: registry.clone(),
                dependency,
            });
        }
    }
    Ok(out)
}

fn parse_replacements(value: Option<&toml::Value>, root: &Path) -> Result<Vec<ReplaceDecl>, MErr> {
    let Some(entries) = value else {
        return Ok(Vec::new());
    };
    let entries = entries
        .as_table()
        .ok_or_else(|| "[replace] 必须是 package ID 表".to_string())?;
    let mut out = Vec::new();
    for (package_id, replacement) in entries {
        let (source, package, version) = parse_replace_package_id(package_id)?;
        let table = Some(BTreeMap::from([(package.clone(), replacement.clone())]));
        let mut parsed = Vec::new();
        parse_dep_table(&table, DepKind::Normal, root, None, &mut parsed)?;
        let dependency = parsed.pop().unwrap();
        if matches!(
            dependency.source,
            DepSource::Registry(_, RegistryReference::CratesIo)
        ) {
            return Err(format!(
                "[replace] `{package_id}` 没有声明 path/git/其他 registry 来源"
            ));
        }
        out.push(ReplaceDecl {
            package,
            version,
            source,
            dependency,
        });
    }
    Ok(out)
}

fn parse_replace_package_id(
    package_id: &str,
) -> Result<(Option<String>, String, semver::Version), MErr> {
    let (head, version) = package_id
        .rsplit_once(':')
        .ok_or_else(|| format!("[replace] package ID `{package_id}` 缺 `:version`"))?;
    let version = semver::Version::parse(version)
        .map_err(|error| format!("[replace] package ID `{package_id}` 版本非法: {error}"))?;
    let (source, package) = match head.rsplit_once('#') {
        Some((source, package)) => (Some(source.to_string()), package.to_string()),
        None => (None, head.to_string()),
    };
    if package.is_empty() {
        return Err(format!("[replace] package ID `{package_id}` 包名为空"));
    }
    Ok((source, package, version))
}

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
        let mut git_url: Option<String> = None;
        let mut branch: Option<String> = None;
        let mut tag: Option<String> = None;
        let mut rev: Option<String> = None;
        let mut registry: Option<RegistryReference> = None;
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
                        "git" => git_url = Some(v.as_str().ok_or("git 非字符串")?.to_string()),
                        "branch" => branch = Some(v.as_str().ok_or("branch 非字符串")?.to_string()),
                        "tag" => tag = Some(v.as_str().ok_or("tag 非字符串")?.to_string()),
                        "rev" => rev = Some(v.as_str().ok_or("rev 非字符串")?.to_string()),
                        "registry" => {
                            registry = Some(RegistryReference::Named(
                                v.as_str().ok_or("registry 非字符串")?.to_string(),
                            ));
                        }
                        "registry-index" => {
                            registry = Some(RegistryReference::Index(
                                v.as_str().ok_or("registry-index 非字符串")?.to_string(),
                            ));
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
        let req = semver::VersionReq::parse(&req_str)
            .map_err(|e| format!("依赖 {key} version req 非法 {req_str}: {e}"))?;
        let selectors = [branch.is_some(), tag.is_some(), rev.is_some()]
            .into_iter()
            .filter(|present| *present)
            .count();
        if selectors > 1 {
            return Err(format!("依赖 {key} 的 branch/tag/rev 只能指定一个"));
        }
        if git_url.is_none() && selectors != 0 {
            return Err(format!("依赖 {key} 指定 branch/tag/rev 但没有 git URL"));
        }
        if [branch.as_deref(), tag.as_deref(), rev.as_deref()]
            .into_iter()
            .flatten()
            .any(str::is_empty)
        {
            return Err(format!("依赖 {key} 的 branch/tag/rev 不能为空"));
        }
        if git_url.is_some() && source.is_some() {
            return Err(format!("依赖 {key} 不能同时指定 git 与 path"));
        }
        if registry.is_some() && (git_url.is_some() || source.is_some()) {
            return Err(format!("依赖 {key} 不能同时指定 registry 与 git/path"));
        }
        let source = match (source, git_url) {
            (Some(path), None) => path,
            (None, Some(url)) => {
                if url.is_empty() || url.starts_with('-') {
                    return Err(format!("依赖 {key} 的 git URL 非法：`{url}`"));
                }
                if url.contains('?') || url.contains('#') {
                    return Err(format!(
                        "依赖 {key} 的 git URL 不能自带 query/fragment；请使用 branch/tag/rev"
                    ));
                }
                let reference = if let Some(value) = branch {
                    GitReference::Branch(value)
                } else if let Some(value) = tag {
                    GitReference::Tag(value)
                } else if let Some(value) = rev {
                    GitReference::Rev(value)
                } else {
                    GitReference::DefaultBranch
                };
                DepSource::Git(GitSpec {
                    url,
                    reference,
                    version: req,
                })
            }
            (None, None) => {
                DepSource::Registry(req, registry.unwrap_or(RegistryReference::CratesIo))
            }
            (Some(_), Some(_)) => unreachable!(),
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

#[allow(clippy::too_many_arguments)]
fn discover_targets(
    lib: Option<&RawLib>,
    bin: Option<&Vec<RawBin>>,
    tests: Option<&Vec<RawTarget>>,
    examples: Option<&Vec<RawTarget>>,
    benches: Option<&Vec<RawTarget>>,
    autobins: bool,
    autotests: bool,
    autoexamples: bool,
    autobenches: bool,
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
        out.push(Target {
            kind: TargetKind::Lib,
            name,
            path,
            proc_macro,
            test: lib.and_then(|l| l.test).unwrap_or(true),
            harness: lib.and_then(|l| l.harness).unwrap_or(true),
            doctest: lib.and_then(|l| l.doctest).unwrap_or(true),
            required_features: lib
                .and_then(|l| l.required_features.clone())
                .unwrap_or_default(),
        });
    }
    // bin：显式条目覆盖同名自动目标；autobins=true 时其他目标仍自动发现。
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
            Some(Target {
                kind: TargetKind::Bin,
                name,
                path,
                proc_macro: false,
                test: b.test.unwrap_or(true),
                harness: b.harness.unwrap_or(true),
                doctest: false,
                required_features: b.required_features.clone().unwrap_or_default(),
            })
        })
        .collect();
    let explicit_bin_names: BTreeSet<String> = explicit.iter().map(|t| t.name.clone()).collect();
    out.extend(explicit);
    if autobins {
        let main = root.join("src/main.rs");
        if main.is_file() && !explicit_bin_names.contains(pkg_name) {
            out.push(Target {
                kind: TargetKind::Bin,
                name: pkg_name.to_string(),
                path: main,
                proc_macro: false,
                test: true,
                harness: true,
                doctest: false,
                required_features: Vec::new(),
            });
        }
        let bin_dir = root.join("src/bin");
        if let Ok(rd) = std::fs::read_dir(&bin_dir) {
            let mut extra: Vec<Target> = rd
                .flatten()
                .filter_map(|e| {
                    let p = e.path();
                    let (name, path) = if p.extension().is_some_and(|x| x == "rs") {
                        (p.file_stem()?.to_string_lossy().into_owned(), p.clone())
                    } else if p.is_dir() && p.join("main.rs").is_file() {
                        (
                            p.file_name()?.to_string_lossy().into_owned(),
                            p.join("main.rs"),
                        )
                    } else {
                        return None;
                    };
                    Some(Target {
                        kind: TargetKind::Bin,
                        name,
                        path,
                        proc_macro: false,
                        test: true,
                        harness: true,
                        doctest: false,
                        required_features: Vec::new(),
                    })
                })
                .collect();
            extra.retain(|target| !explicit_bin_names.contains(&target.name));
            extra.sort_by(|a, b| a.name.cmp(&b.name));
            out.extend(extra);
        }
    }

    out.extend(discover_file_targets(
        tests,
        autotests,
        TargetKind::Test,
        &root.join("tests"),
    ));
    out.extend(discover_file_targets(
        examples,
        autoexamples,
        TargetKind::Example,
        &root.join("examples"),
    ));
    out.extend(discover_file_targets(
        benches,
        autobenches,
        TargetKind::Bench,
        &root.join("benches"),
    ));
    Ok(out)
}

/// Cargo 的 tests/examples 自动发现：`dir/name.rs` 与 `dir/name/main.rs` 各是一目标；
/// 子目录里的其他 `.rs` 是模块，不应被误当成独立目标。显式条目只覆盖同名自动目标。
fn discover_file_targets(
    explicit: Option<&Vec<RawTarget>>,
    auto: bool,
    kind: TargetKind,
    dir: &Path,
) -> Vec<Target> {
    let mut out: Vec<Target> = explicit
        .into_iter()
        .flatten()
        .filter_map(|row| {
            let path = row
                .path
                .as_ref()
                .and_then(|p| dir.parent().map(|root| root.join(p)))
                .or_else(|| {
                    let name = row.name.as_ref()?;
                    let flat = dir.join(format!("{name}.rs"));
                    flat.is_file().then_some(flat).or_else(|| {
                        let main = dir.join(name).join("main.rs");
                        main.is_file().then_some(main)
                    })
                })?;
            let name = row.name.clone().or_else(|| {
                path.parent()
                    .filter(|parent| parent.parent() == Some(dir))
                    .and_then(Path::file_name)
                    .or_else(|| path.file_stem())
                    .map(|n| n.to_string_lossy().into_owned())
            })?;
            Some(Target {
                kind,
                name,
                path,
                proc_macro: false,
                test: row.test.unwrap_or(kind != TargetKind::Example),
                harness: row.harness.unwrap_or(true),
                doctest: false,
                required_features: row.required_features.clone().unwrap_or_default(),
            })
        })
        .collect();
    if !auto {
        return out;
    }
    let explicit_names: BTreeSet<String> = out.iter().map(|target| target.name.clone()).collect();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    let mut automatic: Vec<_> = rd
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let (name, path) = if path.extension().is_some_and(|x| x == "rs") {
                (path.file_stem()?.to_string_lossy().into_owned(), path)
            } else if path.is_dir() && path.join("main.rs").is_file() {
                (
                    path.file_name()?.to_string_lossy().into_owned(),
                    path.join("main.rs"),
                )
            } else {
                return None;
            };
            Some(Target {
                kind,
                name,
                path,
                proc_macro: false,
                test: kind != TargetKind::Example,
                harness: true,
                doctest: false,
                required_features: Vec::new(),
            })
        })
        .collect();
    automatic.retain(|target| !explicit_names.contains(&target.name));
    automatic.sort_by(|a, b| a.name.cmp(&b.name));
    out.extend(automatic);
    out
}

// ---------- profile ----------

fn profiles_from(raw: Option<RawProfiles>) -> Result<(ProfileFlags, ProfileFlags), MErr> {
    let Some(raw) = raw else {
        let dev = ProfileFlags::default();
        return Ok((dev, dev));
    };
    let dev = apply_profile(ProfileFlags::default(), raw.dev, "dev")?;
    let test = apply_profile(dev, raw.test, "test")?;
    Ok((dev, test))
}

fn apply_profile(
    mut profile: ProfileFlags,
    raw: Option<RawProfile>,
    name: &str,
) -> Result<ProfileFlags, MErr> {
    let Some(raw) = raw else {
        return Ok(profile);
    };
    if let Some(v) = raw.debug_assertions {
        profile.debug_assertions = v;
    }
    if let Some(v) = raw.overflow_checks {
        profile.overflow_checks = v;
    }
    if let Some(value) = raw.opt_level {
        profile.opt_level = match value {
            toml::Value::Integer(0) => OptLevel::O0,
            toml::Value::Integer(1) => OptLevel::O1,
            toml::Value::Integer(2) => OptLevel::O2,
            toml::Value::Integer(3) => OptLevel::O3,
            toml::Value::String(v) if v == "s" => OptLevel::Os,
            toml::Value::String(v) if v == "z" => OptLevel::Oz,
            other => {
                return Err(format!(
                    "profile.{name}.opt-level 非 Cargo 支持值（只接受 0/1/2/3/\"s\"/\"z\"）：{other}"
                ));
            }
        };
    }
    Ok(profile)
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
            DepSource::Registry(
                semver::VersionReq::parse("0.14").unwrap(),
                RegistryReference::CratesIo,
            )
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
    fn parses_git_dependencies_and_cargo_source_ids() {
        let manifest = PackageManifest::parse(
            "[package]\nname='d'\nversion='0.1.0'\n[dependencies]\n\
             default = { package='a', git='https://example.test/repo', version='^1' }\n\
             branch = { package='b', git='ssh://example.test/repo', branch='topic/one', features=['x'] }\n\
             tag = { package='c', git='file:///tmp/repo', tag='release 1' }\n\
             revision = { package='e', git='https://example.test/e', rev='abc123' }\n",
            Path::new("/tmp/x"),
        )
        .unwrap();
        let source = |key: &str| {
            let DepSource::Git(spec) = &manifest
                .deps
                .iter()
                .find(|dep| dep.key == key)
                .unwrap()
                .source
            else {
                panic!("{key} 应为 Git 依赖")
            };
            spec.clone()
        };
        let default = source("default");
        assert_eq!(default.source_id(), "git+https://example.test/repo");
        assert_eq!(default.version.to_string(), "^1");
        assert_eq!(
            source("branch").source_id(),
            "git+ssh://example.test/repo?branch=topic%2Fone"
        );
        assert_eq!(
            source("tag").source_id(),
            "git+file:///tmp/repo?tag=release%201"
        );
        assert_eq!(
            source("revision").source_id(),
            "git+https://example.test/e?rev=abc123"
        );
    }

    #[test]
    fn rejects_invalid_git_and_workspace_inherited_deps_loudly() {
        for (dependency, needle) in [
            (
                "foo = { git='https://x', branch='main', tag='v1' }",
                "只能指定一个",
            ),
            ("foo = { branch='main' }", "没有 git URL"),
            (
                "foo = { git='https://x', path='../x' }",
                "同时指定 git 与 path",
            ),
            ("foo = { git='https://x', rev='' }", "不能为空"),
            ("foo = { git='--upload-pack=bad' }", "git URL 非法"),
            ("foo = { git='https://x?a=b' }", "不能自带 query/fragment"),
        ] {
            let err = PackageManifest::parse(
                &format!("[package]\nname='d'\nversion='0.1.0'\n[dependencies]\n{dependency}\n"),
                Path::new("/tmp/x"),
            )
            .unwrap_err();
            assert!(err.contains(needle), "{dependency}: {err}");
        }
        let err = PackageManifest::parse(
            "[package]\nname=\"d\"\nversion=\"0.1.0\"\n[dependencies]\nfoo.workspace = true",
            Path::new("/tmp/x"),
        )
        .unwrap_err();
        assert!(err.contains("P5"), "{err}");
    }

    #[test]
    fn parses_patch_and_replace_sources() {
        let manifest = PackageManifest::parse(
            "[package]\nname='d'\nversion='0.1.0'\n\
             [patch.crates-io]\nfoo = { path = '../foo' }\n\
             [replace]\n'bar:1.2.3' = { git = 'https://example.test/bar', rev = 'abc' }\n",
            Path::new("/tmp/x"),
        )
        .unwrap();
        assert_eq!(manifest.patches.len(), 1);
        assert_eq!(manifest.patches[0].registry, RegistryReference::CratesIo);
        assert_eq!(manifest.patches[0].dependency.package, "foo");
        assert_eq!(
            manifest.patches[0].dependency.source,
            DepSource::Path(PathBuf::from("/tmp/x/../foo"))
        );
        assert_eq!(manifest.replacements.len(), 1);
        assert_eq!(manifest.replacements[0].package, "bar");
        assert_eq!(
            manifest.replacements[0].version,
            semver::Version::new(1, 2, 3)
        );
        assert!(matches!(
            manifest.replacements[0].dependency.source,
            DepSource::Git(_)
        ));
    }

    #[test]
    fn parses_lints_in_cargo_priority_order() {
        let manifest = PackageManifest::parse(
            "[package]\nname='d'\nversion='0.1.0'\n\
             [lints.rust]\nwarnings={level='allow',priority=-1}\n\
             unused={level='allow',priority=0}\n\
             dead_code={level='deny',priority=0}\n\
             unexpected_cfgs={level='warn',priority=1,check-cfg=['cfg(bootstrap)']}\n\
             unsafe_code={level='forbid',priority=2}\n\
             [lints.clippy]\npedantic={level='warn',priority=-2}\n",
            Path::new("/tmp/x"),
        )
        .unwrap();
        assert_eq!(
            manifest.rustc_lint_flags,
            [
                "--warn=clippy::pedantic",
                "--allow=warnings",
                "--allow=unused",
                "--deny=dead_code",
                "--warn=unexpected_cfgs",
                "--forbid=unsafe_code",
                "--check-cfg",
                "cfg(bootstrap)",
            ]
        );
    }

    #[test]
    fn rejects_invalid_lint_shapes_loudly() {
        for (body, want) in [
            ("unsafe_code='force-warn'", "allow/warn/deny/forbid"),
            (
                "unsafe_code={level='warn',check-cfg=['cfg(x)']}",
                "unexpected_cfgs",
            ),
            ("unsafe_code={level='warn',priority='high'}", "必须是整数"),
        ] {
            let error = PackageManifest::parse(
                &format!("[package]\nname='d'\nversion='0.1.0'\n[lints.rust]\n{body}\n"),
                Path::new("/tmp/x"),
            )
            .unwrap_err();
            assert!(error.contains(want), "{error}");
        }
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
            .filter(|t| t.is_bin())
            .map(|t| t.name.as_str())
            .collect();
        assert!(m.targets.iter().any(Target::is_lib));
        assert_eq!(bins, ["d", "extra"]);
        // 多 bin 时 runnable_bin 响亮拒绝（可选 --bin 或 default-run 解；
        // 切⑥b 起不再是 P5 文案）；default-run 钉选可解
        let err = m.runnable_bin().unwrap_err();
        assert!(err.contains("--bin"), "{err}");
        let m2 = PackageManifest::parse(
            "[package]\nname=\"d\"\nversion=\"0.1.0\"\ndefault-run=\"extra\"\n",
            &d,
        )
        .unwrap();
        assert_eq!(m2.runnable_bin().unwrap().0, "extra");
        // 切⑥b：--bin 按名选定；不在名单 = 响亮报错并列出可选
        assert_eq!(m.runnable_bin_opt(Some("extra")).unwrap().0, "extra");
        let err = m.runnable_bin_opt(Some("nope")).unwrap_err();
        assert!(err.contains("nope") && err.contains("extra"), "{err}");
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
            let pm = m.targets.iter().any(|t| t.is_lib() && t.proc_macro);
            assert!(pm, "拼写 {key} 必须识别");
        }
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn parses_dev_dependencies_test_profile_and_test_targets() {
        let d = tmpdir("test-targets");
        std::fs::create_dir_all(d.join("src")).unwrap();
        std::fs::create_dir_all(d.join("tests/nested")).unwrap();
        std::fs::create_dir_all(d.join("examples")).unwrap();
        std::fs::write(d.join("src/lib.rs"), "").unwrap();
        std::fs::write(d.join("tests/api.rs"), "").unwrap();
        std::fs::write(d.join("tests/required.rs"), "").unwrap();
        std::fs::write(d.join("tests/nested/main.rs"), "").unwrap();
        std::fs::write(d.join("tests/nested/helper.rs"), "").unwrap();
        std::fs::write(d.join("examples/demo.rs"), "fn main(){}").unwrap();
        let m = PackageManifest::parse(
            "[package]\nname=\"d\"\nversion=\"0.1.0\"\n\
             [dev-dependencies]\nhelper=\"1\"\n\
             [profile.dev]\nopt-level=1\n\
             [profile.test]\nopt-level=\"z\"\ndebug-assertions=false\n\
             [[test]]\nname=\"required\"\npath=\"tests/required.rs\"\n",
            &d,
        )
        .unwrap();
        assert_eq!(
            m.deps.iter().find(|d| d.key == "helper").unwrap().kind,
            DepKind::Dev
        );
        assert_eq!(m.profile.opt_level, OptLevel::O1);
        assert_eq!(m.test_profile.opt_level, OptLevel::Oz);
        assert!(!m.test_profile.debug_assertions);
        let targets: Vec<_> = m
            .targets
            .iter()
            .map(|t| (t.kind, t.name.as_str()))
            .collect();
        assert!(targets.contains(&(TargetKind::Test, "api")));
        assert!(targets.contains(&(TargetKind::Test, "nested")));
        assert!(targets.contains(&(TargetKind::Test, "required")));
        assert!(targets.contains(&(TargetKind::Example, "demo")));
        assert!(
            !m.targets
                .iter()
                .find(|target| target.kind == TargetKind::Example && target.name == "demo")
                .unwrap()
                .test
        );
        assert!(!targets.contains(&(TargetKind::Test, "helper")));
        std::fs::remove_dir_all(d).unwrap();
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

    #[test]
    fn resolver_and_rust_version_follow_cargo_manifest_rules() {
        let directory = tmpdir("resolver-rust-version");
        std::fs::create_dir_all(directory.join("src")).unwrap();
        std::fs::write(directory.join("src/main.rs"), "fn main() {}\n").unwrap();

        let implicit = PackageManifest::parse(
            "[package]\nname='implicit'\nversion='0.1.0'\nedition='2024'\nrust-version='1.85'\n",
            &directory,
        )
        .unwrap();
        assert_eq!(implicit.resolver, ResolverVersion::V3);
        assert_eq!(implicit.rust_version, Some(semver::Version::new(1, 85, 0)));

        let explicit = PackageManifest::parse(
            "[package]\nname='explicit'\nversion='0.1.0'\nedition='2021'\nresolver='3'\nrust-version='1.62'\n",
            &directory,
        )
        .unwrap();
        assert_eq!(explicit.resolver, ResolverVersion::V3);
        assert_eq!(explicit.rust_version, Some(semver::Version::new(1, 62, 0)));

        let error = PackageManifest::parse(
            "[package]\nname='too-old'\nversion='0.1.0'\nedition='2024'\nrust-version='1.62'\n",
            &directory,
        )
        .unwrap_err();
        assert!(error.contains("1.85.0"), "{error}");
        assert!(
            PackageManifest::parse(
                "[package]\nname='bad'\nversion='0.1.0'\nedition='2021'\nrust-version='>=1.70'\n",
                &directory,
            )
            .is_err()
        );
        std::fs::remove_dir_all(directory).unwrap();
    }
}
