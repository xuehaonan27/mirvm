//! `cargoless/registry.rs` —— crates.io registry 访问层（D15 P1，设计档 §3.4）。
//!
//! 自有 store 布局（根 = `~/.mirvm/registry`，`MIRVM_REGISTRY_DIR` 改址）：
//! ```text
//! index/<reg-key>/<sparse 路径>     # sparse index 缓存（JSON 行文件）
//! cache/<reg-key>/<name>-<version>.crate
//! src/<reg-key>/<name>-<version>/   # .crate 解包树（.cargo-ok 标记完成）
//! ```
//! `<reg-key>` = registry URL 的稳定目录名（读穿侧按 glob `index.crates.io-*`
//! 发现，不硬编码 cargo 的哈希后缀——cargo 后缀算法随版本漂移过）。
//!
//! 读穿顺序（裁定 ②，只读不污染）：自有 src → 自有 cache →
//! `~/.cargo/registry/src` → `~/.cargo/registry/cache` → HTTP
//! （index.crates.io / static.crates.io，ureq 纯 Rust 栈）。
//! `MIRVM_OFFLINE=1`：禁 HTTP，全靠本地缓存，缺席响亮报错。
//!
//! P1 子集：仅 crates.io 主 registry；alt registry 归 P5 响亮拒绝。
//! yanked 语义：lock 在允许（cargo 同）；新解跳过 yanked（resolve.rs 消费此约定）。

// P1 逐切接入中：resolve/audit 后续切片接入后摘除本 allow（设计档 §5）。
#![allow(dead_code)]

use std::io::Read;
use std::path::{Path, PathBuf};

use semver::{Version, VersionReq};

const INDEX_URL: &str = "https://index.crates.io/";
const DL_URL: &str = "https://static.crates.io/crates/";

/// sparse index 里一个版本的元数据（JSON 行一格）。
#[derive(Clone, Debug)]
pub struct IndexVersion {
    pub name: String,
    pub version: Version,
    pub cksum: String,
    pub yanked: bool,
    pub deps: Vec<IndexDep>,
    pub features: std::collections::BTreeMap<String, Vec<String>>,
    pub links: Option<String>,
    pub rust_version: Option<String>,
}

/// index 里的依赖元数据（registry crate 的权威依赖描述——resolve 不读其
/// Cargo.toml，与 cargo 同以 index 为准）。
#[derive(Clone, Debug)]
pub struct IndexDep {
    pub name: String,
    pub req: VersionReq,
    pub features: Vec<String>,
    pub optional: bool,
    pub default_features: bool,
    /// `cfg(...)` 平台表达式（None = 全平台）；求值归 manifest::eval_cfg。
    pub target: Option<String>,
    /// "build" / "dev" / None(normal)。
    pub kind: Option<String>,
    /// `package = "real"` 改名时的真实名。
    pub package: Option<String>,
}

type RErr = String;

fn unsupported(what: impl Into<String>) -> RErr {
    format!(
        "registry 子集外构造（D15 P5 范畴，响亮拒绝）：{}",
        what.into()
    )
}

pub struct Registry {
    root: PathBuf,
    offline: bool,
    agent: ureq::Agent,
}

impl Registry {
    /// 打开自有 store（目录按需创建；offline 取自 MIRVM_OFFLINE）。
    pub fn open() -> Result<Self, RErr> {
        Self::open_at(
            crate::sysroot::cache_dir().join("registry"),
            std::env::var_os("MIRVM_OFFLINE").is_some(),
        )
    }

    pub fn open_at(root: PathBuf, offline: bool) -> Result<Self, RErr> {
        for sub in ["index", "cache", "src"] {
            std::fs::create_dir_all(root.join(sub))
                .map_err(|e| format!("registry store 创建失败 {}: {e}", root.display()))?;
        }
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_secs(60)))
            .build();
        Ok(Self {
            root,
            offline,
            agent: config.into(),
        })
    }

    // ---------- sparse index ----------

    /// sparse 路径规约（cargo 同）：全小写；1→`1/n`，2→`2/n`，3→`3/{c1}/{n}`，
    /// 否则 `{c1c2}/{c3c4}/{n}`。
    pub fn sparse_path(name: &str) -> Result<String, RErr> {
        let n = name.to_ascii_lowercase();
        let bytes = n.as_bytes();
        let path = match bytes.len() {
            0 => return Err("crate 名空".into()),
            1 => format!("1/{n}"),
            2 => format!("2/{n}"),
            3 => format!("3/{}/{n}", &n[..1]),
            _ => format!("{}/{}/{n}", &n[..2], &n[2..4]),
        };
        Ok(path)
    }

    fn index_file(&self, name: &str) -> Result<PathBuf, RErr> {
        Ok(self
            .root
            .join("index/crates.io")
            .join(Self::sparse_path(name)?))
    }

    /// 读 index 条目（缓存命中直接用；否则 HTTP 拉取并落缓存）。
    pub fn index_entry(&self, name: &str) -> Result<Vec<IndexVersion>, RErr> {
        let file = self.index_file(name)?;
        let text = if file.is_file() {
            std::fs::read_to_string(&file)
                .map_err(|e| format!("index 缓存读取失败 {}: {e}", file.display()))?
        } else {
            if self.offline {
                return Err(format!(
                    "MIRVM_OFFLINE：index 无本地缓存 {name}（先在线解析一次或放通网络）"
                ));
            }
            let url = format!("{INDEX_URL}{}", Self::sparse_path(name)?);
            let mut resp = self
                .agent
                .get(&url)
                .call()
                .map_err(|e| format!("sparse index 拉取失败 {url}: {e}"))?;
            let text = resp
                .body_mut()
                .read_to_string()
                .map_err(|e| format!("sparse index 读取失败 {url}: {e}"))?;
            if let Some(parent) = file.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("index 缓存目录创建失败: {e}"))?;
            }
            std::fs::write(&file, &text)
                .map_err(|e| format!("index 缓存落盘失败 {}: {e}", file.display()))?;
            text
        };
        parse_index_lines(&text)
    }

    // ---------- .crate 下载与解包 ----------

    /// 确保 {name}-{version} 的解包源码在场，返回目录（读穿五级链）。
    pub fn ensure_source(
        &self,
        name: &str,
        version: &Version,
        cksum: Option<&str>,
    ) -> Result<PathBuf, RErr> {
        let dir_name = format!("{name}-{version}");
        // ① 自有 src
        let own = self.root.join("src/crates.io").join(&dir_name);
        if own.join(".cargo-ok").is_file() {
            return Ok(own);
        }
        // ③ cargo src（只读复用，不回拷——直接当解析输入）
        if let Some(d) = glob_dirs(&cargo_registry_sub("src"), &dir_name)
            .into_iter()
            .next()
        {
            return Ok(d);
        }
        // ② 自有 cache 的 .crate 文件；④ cargo cache 的 .crate 文件
        let own_crate = self
            .root
            .join("cache/crates.io")
            .join(format!("{dir_name}.crate"));
        let crate_file = if own_crate.is_file() {
            own_crate
        } else {
            let mut found = glob_dirs(&cargo_registry_sub("cache"), &format!("{dir_name}.crate"));
            if found.is_empty() {
                // ⑤ HTTP
                if self.offline {
                    return Err(format!(
                        "MIRVM_OFFLINE：{dir_name} 无本地缓存（自有/读穿均无）"
                    ));
                }
                let bytes = self.download_crate(name, version)?;
                if let Some(parent) = own_crate.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("crate 缓存目录创建失败: {e}"))?;
                }
                std::fs::write(&own_crate, &bytes)
                    .map_err(|e| format!("crate 缓存落盘失败 {}: {e}", own_crate.display()))?;
                found.push(own_crate.clone());
            }
            found.remove(0)
        };
        // 校验 + 解包进自有 src
        let bytes = std::fs::read(&crate_file).map_err(|e| format!("crate 读取失败: {e}"))?;
        verify_cksum(&bytes, cksum, &dir_name)?;
        unpack_crate(&bytes, &own, &dir_name)?;
        std::fs::write(own.join(".cargo-ok"), "ok\n")
            .map_err(|e| format!("cargo-ok 落盘失败: {e}"))?;
        Ok(own)
    }

    fn download_crate(&self, name: &str, version: &Version) -> Result<Vec<u8>, RErr> {
        let url = format!("{DL_URL}{name}/{name}-{version}.crate");
        let mut resp = self
            .agent
            .get(&url)
            .call()
            .map_err(|e| format!("crate 下载失败 {url}: {e}"))?;
        let mut buf = Vec::new();
        resp.body_mut()
            .as_reader()
            .read_to_end(&mut buf)
            .map_err(|e| format!("crate 下载读取失败 {url}: {e}"))?;
        Ok(buf)
    }
}

// ---------- index JSON 行解析 ----------

fn parse_index_lines(text: &str) -> Result<Vec<IndexVersion>, RErr> {
    let mut out = Vec::new();
    for (ln, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value = serde_json::from_str(line)
            .map_err(|e| format!("index JSON 行 {} 解析失败: {e}", ln + 1))?;
        out.push(parse_index_version(&v).map_err(|e| format!("index 行 {}: {e}", ln + 1))?);
    }
    Ok(out)
}

fn parse_index_version(v: &serde_json::Value) -> Result<IndexVersion, RErr> {
    let get_str = |k: &str| v.get(k).and_then(|x| x.as_str());
    let name = get_str("name").ok_or("缺 name")?.to_string();
    let version =
        Version::parse(get_str("vers").ok_or("缺 vers")?).map_err(|e| format!("vers 非法: {e}"))?;
    let cksum = get_str("cksum").ok_or("缺 cksum")?.to_string();
    let yanked = v.get("yanked").and_then(|x| x.as_bool()).unwrap_or(false);
    let links = get_str("links").map(str::to_string);
    let rust_version = get_str("rust_version")
        .or_else(|| get_str("rust_version2"))
        .map(str::to_string);
    let mut features = std::collections::BTreeMap::new();
    for (fk, fv) in v
        .get("features")
        .and_then(|x| x.as_object())
        .into_iter()
        .flatten()
    {
        let vals = fv
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        features.insert(fk.clone(), vals);
    }
    // features2（弱激活 "?/" 扩展表）并入同表（形态由 manifest::parse_feature_value 判）
    for (fk, fv) in v
        .get("features2")
        .and_then(|x| x.as_object())
        .into_iter()
        .flatten()
    {
        let vals = fv
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        features.entry(fk.clone()).or_default().extend(vals);
    }
    let mut deps = Vec::new();
    for d in v
        .get("deps")
        .and_then(|x| x.as_array())
        .into_iter()
        .flatten()
    {
        let dname = d
            .get("name")
            .and_then(|x| x.as_str())
            .ok_or("dep 缺 name")?;
        let req = VersionReq::parse(d.get("req").and_then(|x| x.as_str()).ok_or("dep 缺 req")?)
            .map_err(|e| format!("dep {dname} req 非法: {e}"))?;
        let features = d
            .get("features")
            .and_then(|x| x.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        deps.push(IndexDep {
            name: dname.to_string(),
            req,
            features,
            optional: d.get("optional").and_then(|x| x.as_bool()).unwrap_or(false),
            default_features: d
                .get("default_features")
                .and_then(|x| x.as_bool())
                .unwrap_or(true),
            target: d.get("target").and_then(|x| x.as_str()).map(str::to_string),
            kind: d.get("kind").and_then(|x| x.as_str()).map(str::to_string),
            package: d
                .get("package")
                .and_then(|x| x.as_str())
                .map(str::to_string),
        });
    }
    Ok(IndexVersion {
        name,
        version,
        cksum,
        yanked,
        deps,
        features,
        links,
        rust_version,
    })
}

// ---------- cksum 与解包 ----------

/// registry 协议只接受 sha256（64 hex）——cksum 缺席时跳过校验（仅用于
/// 合成测试；真 registry 路径恒有 cksum，设计档 §3.4 的校验纪律不变）。
fn verify_cksum(bytes: &[u8], cksum: Option<&str>, dir_name: &str) -> Result<(), RErr> {
    let Some(want) = cksum else { return Ok(()) };
    if want.len() != 64 || !want.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("{dir_name} cksum 形态非法（非 64 hex sha256）"));
    }
    let got = sha256_hex(bytes);
    if got != want.to_ascii_lowercase() {
        return Err(format!(
            "{dir_name} sha256 校验失败（want {want} got {got}）——拒绝解包"
        ));
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    // SHA-256 最小实现（避免为校验单点再引依赖树；registry 协议只需要它）
    struct Sha256 {
        state: [u32; 8],
        buf: [u8; 64],
        buf_len: usize,
        total: u64,
    }
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    impl Sha256 {
        fn new() -> Self {
            Self {
                state: [
                    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c,
                    0x1f83d9ab, 0x5be0cd19,
                ],
                buf: [0; 64],
                buf_len: 0,
                total: 0,
            }
        }
        fn block(&mut self, b: &[u8]) {
            let mut w = [0u32; 64];
            for i in 0..16 {
                w[i] = u32::from_be_bytes([b[i * 4], b[i * 4 + 1], b[i * 4 + 2], b[i * 4 + 3]]);
            }
            for i in 16..64 {
                let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
                let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
                w[i] = w[i - 16]
                    .wrapping_add(s0)
                    .wrapping_add(w[i - 7])
                    .wrapping_add(s1);
            }
            let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
            for i in 0..64 {
                let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
                let ch = (e & f) ^ (!e & g);
                let t1 = h
                    .wrapping_add(s1)
                    .wrapping_add(ch)
                    .wrapping_add(K[i])
                    .wrapping_add(w[i]);
                let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
                let maj = (a & b) ^ (a & c) ^ (b & c);
                let t2 = s0.wrapping_add(maj);
                h = g;
                g = f;
                f = e;
                e = d.wrapping_add(t1);
                d = c;
                c = b;
                b = a;
                a = t1.wrapping_add(t2);
            }
            self.state = [
                a.wrapping_add(self.state[0]),
                b.wrapping_add(self.state[1]),
                c.wrapping_add(self.state[2]),
                d.wrapping_add(self.state[3]),
                e.wrapping_add(self.state[4]),
                f.wrapping_add(self.state[5]),
                g.wrapping_add(self.state[6]),
                h.wrapping_add(self.state[7]),
            ];
        }
        fn update(&mut self, mut data: &[u8]) {
            self.total = self.total.wrapping_add(data.len() as u64);
            if self.buf_len > 0 {
                let need = 64 - self.buf_len;
                let take = need.min(data.len());
                self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
                self.buf_len += take;
                data = &data[take..];
                if self.buf_len == 64 {
                    let b = self.buf;
                    self.block(&b);
                    self.buf_len = 0;
                }
            }
            while data.len() >= 64 {
                let (head, tail) = data.split_at(64);
                self.block(head);
                data = tail;
            }
            if !data.is_empty() {
                self.buf[..data.len()].copy_from_slice(data);
                self.buf_len = data.len();
            }
        }
        fn finish(mut self) -> [u8; 32] {
            let bit_len = self.total.wrapping_mul(8);
            self.update(&[0x80]);
            while self.buf_len != 56 {
                self.update(&[0]);
            }
            self.update(&bit_len.to_be_bytes());
            let mut out = [0u8; 32];
            for (i, s) in self.state.iter().enumerate() {
                out[i * 4..i * 4 + 4].copy_from_slice(&s.to_be_bytes());
            }
            out
        }
    }
    let mut h = Sha256::new();
    h.update(bytes);
    h.finish().iter().map(|b| format!("{b:02x}")).collect()
}

/// .crate = tar.gz，内层唯一顶层目录 `{name}-{version}/`（校验并剥掉）。
fn unpack_crate(bytes: &[u8], dest: &Path, dir_name: &str) -> Result<(), RErr> {
    let gz = flate2::read::GzDecoder::new(bytes);
    let mut ar = tar::Archive::new(gz);
    if dest.exists() {
        std::fs::remove_dir_all(dest).map_err(|e| format!("解包目标清理失败: {e}"))?;
    }
    std::fs::create_dir_all(dest).map_err(|e| format!("解包目标创建失败: {e}"))?;
    for entry in ar
        .entries()
        .map_err(|e| format!("crate tar 读取失败: {e}"))?
    {
        let mut entry = entry.map_err(|e| format!("crate tar 条目读取失败: {e}"))?;
        let path = entry
            .path()
            .map_err(|e| format!("crate tar 路径读取失败: {e}"))?
            .into_owned();
        let mut comps = path.components();
        let top = comps
            .next()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .unwrap_or_default();
        if top != dir_name {
            return Err(format!("crate 顶层目录 {top} 与 {dir_name} 不符——拒绝解包"));
        }
        let rel: PathBuf = comps.collect();
        if rel.as_os_str().is_empty() {
            continue;
        }
        // 路径逃逸防护（.. 与绝对路径一律拒绝）
        if rel.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::RootDir
            )
        }) {
            return Err(format!("crate 含逃逸路径 {}——拒绝解包", rel.display()));
        }
        let target = dest.join(&rel);
        if entry.header().entry_type().is_dir() {
            std::fs::create_dir_all(&target).map_err(|e| format!("解包建目录失败: {e}"))?;
        } else {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|e| format!("解包建目录失败: {e}"))?;
            }
            entry
                .unpack(&target)
                .map_err(|e| format!("解包写文件失败 {}: {e}", target.display()))?;
        }
    }
    Ok(())
}

// ---------- 读穿（cargo 缓存只读）----------

fn cargo_registry_sub(kind: &str) -> PathBuf {
    let home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo")))
        .unwrap_or_default();
    home.join("registry").join(kind)
}

/// 在 cargo 的 {kind}/index.crates.io-*/ 下找名为 file_name 的条目（glob 发现，
/// 不硬编码哈希后缀）。
fn glob_dirs(base: &Path, file_name: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(base) else {
        return out;
    };
    for e in rd.flatten() {
        let p = e.path();
        if !p.is_dir() {
            continue;
        }
        let Some(dirname) = p.file_name().map(|s| s.to_string_lossy().into_owned()) else {
            continue;
        };
        if !dirname.starts_with("index.crates.io-") {
            continue;
        }
        let cand = p.join(file_name);
        if cand.is_file() || cand.is_dir() {
            out.push(cand);
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "mirvm-cargoless-registry-test-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn sparse_path_scheme_matches_cargo_rules() {
        assert_eq!(Registry::sparse_path("a").unwrap(), "1/a");
        assert_eq!(Registry::sparse_path("ab").unwrap(), "2/ab");
        assert_eq!(Registry::sparse_path("abc").unwrap(), "3/a/abc");
        assert_eq!(Registry::sparse_path("serde").unwrap(), "se/rd/serde");
        assert_eq!(
            Registry::sparse_path("SerDe_JSON").unwrap(),
            "se/rd/serde_json"
        );
    }

    #[test]
    fn parses_index_json_lines_with_features2_and_deps() {
        let line = r#"{"name":"demo","vers":"1.2.3","deps":[{"name":"d1","req":"^1.0","features":["f"],"optional":true,"default_features":false,"target":"cfg(unix)","kind":"build","package":"real-d1"},{"name":"d2","req":"*","features":[],"optional":false,"default_features":true,"target":null,"kind":"dev"}],"cksum":"abc123","features":{"default":["std"],"std":[]},"features2":{"weak":["d1?/inner"]},"yanked":false,"links":"demo-sys","rust_version":"1.70"}"#;
        let vs = parse_index_lines(&format!("{line}\n")).unwrap();
        assert_eq!(vs.len(), 1);
        let v = &vs[0];
        assert_eq!(v.version.to_string(), "1.2.3");
        assert_eq!(v.cksum, "abc123");
        assert!(!v.yanked);
        assert_eq!(v.links.as_deref(), Some("demo-sys"));
        assert_eq!(v.rust_version.as_deref(), Some("1.70"));
        assert_eq!(v.features["default"], vec!["std"]);
        assert_eq!(v.features["weak"], vec!["d1?/inner"]);
        assert_eq!(v.deps.len(), 2);
        assert!(v.deps[0].optional);
        assert!(!v.deps[0].default_features);
        assert_eq!(v.deps[0].kind.as_deref(), Some("build"));
        assert_eq!(v.deps[0].package.as_deref(), Some("real-d1"));
        assert_eq!(v.deps[0].target.as_deref(), Some("cfg(unix)"));
        assert_eq!(v.deps[1].kind.as_deref(), Some("dev"));
    }

    #[test]
    fn sha256_hex_matches_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"hello world"),
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
        // 多块 + 非整块尾巴：形态校验（分块/收尾路径由上面两向量锁正确性）
        let big: Vec<u8> = (0..200u32).map(|i| (i % 251) as u8).collect();
        let got = sha256_hex(&big);
        assert_eq!(got.len(), 64);
        assert!(got.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[test]
    fn cksum_verify_rejects_tampered_bytes() {
        let good = sha256_hex(b"payload");
        verify_cksum(b"payload", Some(&good), "x-1.0.0").unwrap();
        let err = verify_cksum(b"tampered", Some(&good), "x-1.0.0").unwrap_err();
        assert!(err.contains("校验失败"), "{err}");
    }

    #[test]
    fn unpacks_crate_and_rejects_traversal() {
        let d = tmpdir("unpack");
        // 合成一个 tar.gz：demo-1.0.0/{Cargo.toml, src/lib.rs}
        let mut tar_bytes = Vec::new();
        {
            let mut b = tar::Builder::new(&mut tar_bytes);
            let mut add = |path: &str, content: &[u8]| {
                let mut h = tar::Header::new_gnu();
                h.set_size(content.len() as u64);
                h.set_mode(0o644);
                h.set_cksum();
                b.append_data(&mut h, path, content).unwrap();
            };
            add("demo-1.0.0/Cargo.toml", b"[package]\nname=\"demo\"\n");
            add("demo-1.0.0/src/lib.rs", b"pub fn f() {}\n");
            b.finish().unwrap();
        }
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&tar_bytes).unwrap();
        let crate_bytes = gz.finish().unwrap();

        let dest = d.join("demo-1.0.0");
        unpack_crate(&crate_bytes, &dest, "demo-1.0.0").unwrap();
        assert!(dest.join("Cargo.toml").is_file());
        assert!(dest.join("src/lib.rs").is_file());

        // 顶层目录名不符 → 拒绝
        let err = unpack_crate(&crate_bytes, &d.join("other"), "other-9.9.9").unwrap_err();
        assert!(err.contains("顶层目录"), "{err}");

        // 逃逸路径 → 响亮拒绝（tar crate 在 path() 读取即拒或我方守卫拒，
        // 两层防线任一即不可解包；邪恶文件不得落盘）
        // 手搓含 .. 的恶意 tar（Builder 会拒写，绕过它直接写字节）：
        let mut evil_tar = vec![0u8; 512];
        let evil_name = b"demo-1.0.0/../evil.txt";
        evil_tar[..evil_name.len()].copy_from_slice(evil_name);
        evil_tar[100..108].copy_from_slice(b"0000644\0");
        evil_tar[108..116].copy_from_slice(b"0000000\0");
        evil_tar[116..124].copy_from_slice(b"0000000\0");
        evil_tar[124..136].copy_from_slice(b"00000000004\0"); // size=4 (octal)
        evil_tar[136..148].copy_from_slice(b"00000000000\0");
        evil_tar[148..156].copy_from_slice(b"        "); // cksum 占位空格
        evil_tar[156] = b'0';
        evil_tar[257..263].copy_from_slice(b"ustar\0");
        evil_tar[263..265].copy_from_slice(b"00");
        let sum: u32 = evil_tar[..512].iter().map(|&b| b as u32).sum();
        evil_tar[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        evil_tar.extend_from_slice(b"evil");
        evil_tar.resize(1024, 0);
        evil_tar.extend_from_slice(&[0u8; 1024]);
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&evil_tar).unwrap();
        let evil_bytes = gz.finish().unwrap();
        let dest2 = d.join("demo2-1.0.0");
        assert!(unpack_crate(&evil_bytes, &dest2, "demo2-1.0.0").is_err());
        assert!(!d.join("evil.txt").exists());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn offline_mode_refuses_http_loudly() {
        let d = tmpdir("offline");
        // 无缓存 + offline：index 与 source 都必须响亮（测试不触网）
        let reg = Registry::open_at(d.clone(), true).unwrap();
        let err = reg.index_entry("no-such-crate-mirvm-test").unwrap_err();
        assert!(err.contains("MIRVM_OFFLINE"), "{err}");
        let err = reg
            .ensure_source("no-such", &Version::new(0, 0, 0), None)
            .unwrap_err();
        assert!(err.contains("MIRVM_OFFLINE"), "{err}");
        std::fs::remove_dir_all(&d).unwrap();
    }
}
