//! `cargoless/rustflags.rs` —— rustflags 子集解析（D15 P3 切⑤a）：
//! `CARGO_ENCODED_RUSTFLAGS` / `RUSTFLAGS` 环境变量 + `.cargo/config.toml` 的
//! `build.rustflags` / `target.<triple>.rustflags` / `target.'cfg(all())'.rustflags`。
//!
//! 落点语义（切⑤a 实证，probe_buildrs + `--target x86_64-unknown-linux-gnu`
//! 跑 RUSTFLAGS='--cap-lints allow'，13 条 rustc 调用行逐类核对）：cargo 在
//! 有 --target 时 rustflags 只落 **target 单元**——target dep 与最终 bin
//! （registry target dep 出现两个 --cap-lints allow：内置一枚 + RUSTFLAGS
//! 一枚；path 包 bin 只有 RUSTFLAGS 那一枚）；**host 单元一律不吃**——
//! build.rs 编译、proc-macro 编译、host dep 编译行均无 RUSTFLAGS 痕迹。
//! 本模块只产出 flags 列表；落点纪律在 schedule.rs：dep_rustc_args 在 -Z 旗
//! 之后、bin_rustc_args 在参数串末尾追加（rustc 后旗压前旗，用户旗覆盖
//! 先行旗）。host 侧三个参数函数签名根本不含 rustflags，编译期保证不吃。
//!
//! 边界（记档不冒充闭合）：
//! - HOST_RUSTFLAGS 不实现（host 侧编译一律不吃用户 rustflags）。
//! - config 发现 = 从项目根逐级向上第一个 `.cargo/config.toml`，都没有再查
//!   `$HOME/.cargo/config.toml`；多文件合并不做——最近者胜；无扩展名的
//!   `.cargo/config` 旧形态不读。
//! - `target.'cfg(...)'` 只特判永真的 `cfg(all())`（精确字符串匹配，空白
//!   变体如 `cfg( all() )` 不认）；其余 cfg 表达式不求值，见到即忽略该键
//!   （cargo 会求值并拼接多个匹配 cfg，v1 不做）。
//! - 高优先级全覆盖低优先级，取到即停不拼接（cargo 同）；env 存在即优先，
//!   哪怕空串（空串 = 空 flags 列表，照样压住 config）。

use std::path::{Path, PathBuf};

/// 解析 rustflags：优先级 encoded > env > config（取到即停，不拼接）。
/// `env_get` 注入环境读取（单测不碰真环境，避开并发测试的 env 竞争）；
/// `root` = 项目根（config 发现起点；脚本 = 缓存物化目录）。
pub fn resolve(
    env_get: impl Fn(&str) -> Option<String>,
    root: &Path,
) -> Result<Vec<String>, String> {
    if let Some(v) = env_get("CARGO_ENCODED_RUSTFLAGS") {
        return Ok(split_encoded(&v));
    }
    if let Some(v) = env_get("RUSTFLAGS") {
        return Ok(split_ws(&v));
    }
    let home = env_get("HOME").map(PathBuf::from);
    let Some(cfg) = find_config(root, home.as_deref()) else {
        return Ok(Vec::new());
    };
    parse_config(&cfg)
}

/// drive() 用的真环境入口。
pub fn from_env_and_disk(root: &Path) -> Result<Vec<String>, String> {
    resolve(|k| std::env::var(k).ok(), root)
}

/// CARGO_ENCODED_RUSTFLAGS：\x1f 分隔（空串 = 空列表；空段滤掉无害）。
fn split_encoded(v: &str) -> Vec<String> {
    v.split('\x1f')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// RUSTFLAGS / config 字符串形态：按空白分（cargo 同）。
fn split_ws(v: &str) -> Vec<String> {
    v.split_whitespace().map(str::to_string).collect()
}

/// config 发现：从 root 逐级向上第一个 `.cargo/config.toml`；都没有再查
/// `$HOME/.cargo/config.toml`（root 在 /tmp 这类 $HOME 之外的位置时祖先链
/// 到不了 $HOME，cargo 同样两处都查）。最近者胜，多文件合并不做。
fn find_config(root: &Path, home: Option<&Path>) -> Option<PathBuf> {
    for dir in root.ancestors() {
        let p = dir.join(".cargo/config.toml");
        if p.is_file() {
            return Some(p);
        }
    }
    let p = home?.join(".cargo/config.toml");
    p.is_file().then_some(p)
}

/// 单文件 config 解析：优先级 target.<MIRVM_HOST 精确 triple> 最高，
/// target.'cfg(all())' 次之，build.rustflags 兜底（取到即停）。三处全无 =
/// 空列表（不是错）；值非法（非字符串/字符串数组、toml 语法坏）= 响亮
/// 报错——用户写错不该静默吞。
fn parse_config(path: &Path) -> Result<Vec<String>, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("读取 {} 失败: {e}", path.display()))?;
    let v: toml::Value =
        toml::from_str(&text).map_err(|e| format!("解析 {} 失败: {e}", path.display()))?;
    let target = v.get("target").and_then(toml::Value::as_table);
    let target_hit = |key: &str| -> Option<&toml::Value> {
        target
            .and_then(|t| t.get(key))
            .and_then(|t| t.get("rustflags"))
    };
    if let Some(f) = target_hit(env!("MIRVM_HOST")) {
        return flags_value(f, path);
    }
    if let Some(f) = target_hit("cfg(all())") {
        return flags_value(f, path);
    }
    if let Some(f) = v.get("build").and_then(|b| b.get("rustflags")) {
        return flags_value(f, path);
    }
    Ok(Vec::new())
}

/// rustflags 值：字符串（按空白分）或字符串数组；其他类型响亮报错。
fn flags_value(v: &toml::Value, path: &Path) -> Result<Vec<String>, String> {
    match v {
        toml::Value::String(s) => Ok(split_ws(s)),
        toml::Value::Array(xs) => {
            let mut out = Vec::with_capacity(xs.len());
            for x in xs {
                let Some(s) = x.as_str() else {
                    return Err(format!(
                        "{} 的 rustflags 数组成员必须是字符串: {x}",
                        path.display()
                    ));
                };
                out.push(s.to_string());
            }
            Ok(out)
        }
        _ => Err(format!(
            "{} 的 rustflags 必须是字符串或字符串数组: {v}",
            path.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "mirvm-cargoless-rustflags-test-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 空环境（连 HOME 都没有——避免误读本机真 ~/.cargo/config.toml）。
    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn encoded_beats_env_beats_config() {
        // config 也造上：三者同时在场验证优先级取到即停
        let dir = tmpdir("prio");
        std::fs::create_dir_all(dir.join(".cargo")).unwrap();
        std::fs::write(
            dir.join(".cargo/config.toml"),
            "[build]\nrustflags = [\"--from-config\"]\n",
        )
        .unwrap();
        // encoded > env > config
        let f = resolve(
            |k| match k {
                "CARGO_ENCODED_RUSTFLAGS" => Some("--a\x1f--b".to_string()),
                "RUSTFLAGS" => Some("--from-env".to_string()),
                _ => None,
            },
            &dir,
        )
        .unwrap();
        assert_eq!(f, vec!["--a", "--b"], "encoded 最优先");
        // env > config（encoded 缺席）
        let f = resolve(
            |k| match k {
                "RUSTFLAGS" => Some("--from-env".to_string()),
                _ => None,
            },
            &dir,
        )
        .unwrap();
        assert_eq!(f, vec!["--from-env"], "env 压 config");
        // env 空串也压 config（存在即优先，空 = 空列表）
        let f = resolve(|k| (k == "RUSTFLAGS").then(String::new), &dir).unwrap();
        assert!(f.is_empty(), "env 空串压 config 且产出空列表");
        // 全缺席才落 config
        let f = resolve(no_env, &dir).unwrap();
        assert_eq!(f, vec!["--from-config"]);
    }

    #[test]
    fn two_split_forms() {
        // encoded 按 \x1f 分，段内空白原样保留
        let f = resolve(
            |k| (k == "CARGO_ENCODED_RUSTFLAGS").then(|| "--cfg\x1ffoo bar\x1f".to_string()),
            Path::new("/nonexistent"),
        )
        .unwrap();
        assert_eq!(f, vec!["--cfg", "foo bar"], "空段滤掉、段内空白不切");
        // env 按空白分
        let f = resolve(
            |k| (k == "RUSTFLAGS").then(|| "  --cap-lints  allow\t--cfg x ".to_string()),
            Path::new("/nonexistent"),
        )
        .unwrap();
        assert_eq!(f, vec!["--cap-lints", "allow", "--cfg", "x"]);
    }

    #[test]
    fn config_discovery_walks_up_and_nearest_wins() {
        // 项目根在 tmp/proj/sub/deeper：.cargo/config.toml 造在 tmp/proj，
        // 从 deeper 逐级向上必须找到它
        let dir = tmpdir("walk");
        let proj = dir.join("proj");
        let deeper = proj.join("sub/deeper");
        std::fs::create_dir_all(deeper.join(".cargo")).unwrap();
        std::fs::create_dir_all(proj.join(".cargo")).unwrap();
        std::fs::write(
            proj.join(".cargo/config.toml"),
            "[build]\nrustflags = \"--outer\"\n",
        )
        .unwrap();
        // 更近的 sub/deeper/.cargo/config.toml 优先（最近者胜，不合并）
        std::fs::write(
            deeper.join(".cargo/config.toml"),
            "[build]\nrustflags = \"--inner\"\n",
        )
        .unwrap();
        let f = resolve(no_env, &deeper).unwrap();
        assert_eq!(f, vec!["--inner"], "最近者胜");
        std::fs::remove_file(deeper.join(".cargo/config.toml")).unwrap();
        let f = resolve(no_env, &deeper).unwrap();
        assert_eq!(f, vec!["--outer"], "逐级向上找到祖先的 config");
        // $HOME 兜底：root 与 home 不相交时查 $HOME/.cargo/config.toml
        let home = tmpdir("home");
        std::fs::create_dir_all(home.join(".cargo")).unwrap();
        std::fs::write(
            home.join(".cargo/config.toml"),
            "[build]\nrustflags = \"--from-home\"\n",
        )
        .unwrap();
        let root = tmpdir("elsewhere");
        let f = resolve(|k| (k == "HOME").then(|| home.display().to_string()), &root).unwrap();
        assert_eq!(f, vec!["--from-home"]);
    }

    #[test]
    fn config_precedence_triple_then_cfg_all_then_build() {
        let dir = tmpdir("cfgprio");
        std::fs::create_dir_all(dir.join(".cargo")).unwrap();
        let triple = env!("MIRVM_HOST");
        // 三处同时在场：triple 胜
        std::fs::write(
            dir.join(".cargo/config.toml"),
            format!(
                "[build]\nrustflags = \"--from-build\"\n\
                 [target.'cfg(all())']\nrustflags = \"--from-cfg-all\"\n\
                 [target.'{triple}']\nrustflags = \"--from-triple\"\n"
            ),
        )
        .unwrap();
        let f = resolve(no_env, &dir).unwrap();
        assert_eq!(f, vec!["--from-triple"], "target.<triple> 最优先");
        // triple 缺席：cfg(all()) 胜
        std::fs::write(
            dir.join(".cargo/config.toml"),
            "[build]\nrustflags = \"--from-build\"\n\
             [target.'cfg(all())']\nrustflags = \"--from-cfg-all\"\n",
        )
        .unwrap();
        let f = resolve(no_env, &dir).unwrap();
        assert_eq!(f, vec!["--from-cfg-all"], "cfg(all()) 永真特判压 build");
        // 其他 cfg 表达式不求值，见到即忽略该键（落回 build）
        std::fs::write(
            dir.join(".cargo/config.toml"),
            "[build]\nrustflags = \"--from-build\"\n\
             [target.'cfg(windows)']\nrustflags = \"--from-windows\"\n",
        )
        .unwrap();
        let f = resolve(no_env, &dir).unwrap();
        assert_eq!(f, vec!["--from-build"], "cfg(windows) 不求值，忽略");
        // target 表在但无 rustflags 键 → 落下一级
        std::fs::write(
            dir.join(".cargo/config.toml"),
            "[build]\nrustflags = \"--from-build\"\n[target.'cfg(all())']\nlinker = \"cc\"\n",
        )
        .unwrap();
        let f = resolve(no_env, &dir).unwrap();
        assert_eq!(f, vec!["--from-build"]);
    }

    #[test]
    fn config_value_forms_and_errors() {
        let dir = tmpdir("forms");
        std::fs::create_dir_all(dir.join(".cargo")).unwrap();
        // 字符串形态按空白分
        std::fs::write(
            dir.join(".cargo/config.toml"),
            "[build]\nrustflags = \"--cap-lints allow\"\n",
        )
        .unwrap();
        let f = resolve(no_env, &dir).unwrap();
        assert_eq!(f, vec!["--cap-lints", "allow"]);
        // 数组形态逐条收
        std::fs::write(
            dir.join(".cargo/config.toml"),
            "[build]\nrustflags = [\"--cap-lints\", \"allow\"]\n",
        )
        .unwrap();
        let f = resolve(no_env, &dir).unwrap();
        assert_eq!(f, vec!["--cap-lints", "allow"]);
        // 非法类型响亮报错
        std::fs::write(dir.join(".cargo/config.toml"), "[build]\nrustflags = 42\n").unwrap();
        assert!(resolve(no_env, &dir).is_err(), "整数 rustflags 必须报错");
        // toml 语法坏响亮报错
        std::fs::write(dir.join(".cargo/config.toml"), "[build\n").unwrap();
        assert!(resolve(no_env, &dir).is_err(), "坏 toml 必须报错");
    }
}
