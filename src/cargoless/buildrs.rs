//! `cargoless/buildrs.rs` —— build.rs 全生命周期的「指令 ↔ env ↔ 校验」半
//! （D15 P2 切③，设计档 §3 清单 7；D15 P3 切⑤b 加重跑判定）。编译参数形态
//! 在 schedule.rs（build_script_rustc_args），调度在 driver.rs；本文件只管：
//! 指令解析（cargo::/cargo: 两形）、CARGO_CFG_* 映射、执行 env 构建、
//! DEP_* 传播键规范化、links 互斥校验、-L 传递汇集、**rerun-if 精细增量**
//! （存档写读 + 重跑判定，cargo 同语义）。
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
//!
//! 重跑判定（切⑤b，cargo 同语义；细节见 should_rerun 头注）：未触发重跑
//! 条件 ⇒ 跳过执行，从存档 output.txt 重新 parse_instructions 得 BuildOutput
//! ——指令流零序列化失真，DEP_*/OUT_DIR/warning 等全部可观察产出与重跑
//! 逐字节一致。fp 变（旗/依赖/工具链/源）⇒ 新 fp 目录存档天然缺席 ⇒
//! 重跑——「源变 ⇒ 重跑」这条由指纹先行覆盖，rerun 存档的真正主战场 =
//! **fp 不变时**（连续 run、env 变化、包外 rerun-if-changed 路径）的
//! skip/run 决策；registry 包默认面（未发 rerun-if-changed）永不重跑
//! （源按 cksum 不可变）是最大收益面。

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
    /// cargo::rerun-if-changed=PATH（切⑤b：存档与重跑判定消费；≥1 枚即取代
    /// 默认面——cargo 同：发了就只盯这些路径，不再全树扫描）。
    pub rerun_if_changed: Vec<String>,
    /// cargo::rerun-if-env-changed=VAR（与文件面独立叠加，两面通用）。
    pub rerun_if_env_changed: Vec<String>,
}

/// 指令解析：行首 `cargo::`（新形，1.77+）与 `cargo:`（legacy 单冒号）都吃。
/// 新形未知键忽略（cargo 前向兼容同口径）；legacy 未知键按 metadata 收
/// （cargo 同——老 build.rs 的 `cargo:KEY=VALUE` 就是 links metadata 旧形）。
/// cargo::error → Err（driver 补 crate 名）；rerun-if-* 两形都收进
/// BuildOutput（切⑤b 重跑判定消费，见 should_rerun）。
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
        // 切⑤b：两形都收（cargo 对 legacy rerun-if 同样认账）
        "rerun-if-changed" => out.rerun_if_changed.push(value.to_string()),
        "rerun-if-env-changed" => out.rerun_if_env_changed.push(value.to_string()),
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
    /// 本包 manifest `links` 值（CARGO_MANIFEST_LINKS；无 links 键则不设——
    /// ring 0.17.14 build.rs `env::var("CARGO_MANIFEST_LINKS").unwrap()` 实锤，
    /// cargo 文档：the manifest links value）。
    pub links: Option<&'a str>,
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
    // CARGO_FEATURE_<NAME>=1 逐启用 feature（cargo 同；build.rs 探测 feature
    // 的正典通道——cranelift-codegen 按 CARGO_FEATURE_PULLEY 决定生成
    // pulley_inst_gen.rs 实锤，缺了它 OUT_DIR 产物缺文件、include! 炸）
    for f in ctx.features {
        env.insert(format!("CARGO_FEATURE_{}", envify(f)), "1".to_string());
    }
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
    if let Some(links) = ctx.links {
        put("CARGO_MANIFEST_LINKS", links.to_string());
    }
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

// ---------- 切⑤b：rerun-if 精细增量（存档 ↔ 重跑判定，cargo 同语义） ----------
//
// 存档两份，落 `build/<pkg>-<fp>/`（driver 的 record_dir）：
// - `output.txt`：上次执行的**原始 stdout**。未重跑时重新 parse_instructions
//   得 BuildOutput——指令流零序列化失真（DEP_*/OUT_DIR/warning 回放全等价），
//   不另建序列化格式。
// - `rerun.txt`：重跑条件存档，手写行格式（不引序列化依赖）：
//     首行 `mirvm-bldrs-rerun-v1 changed` | `mirvm-bldrs-rerun-v1 default`
//       ——changed = 发过 ≥1 枚 rerun-if-changed（取代默认面）；default = 未发。
//     changed 面逐路径一行：`P\t<len>\t<mtime_ns>\t<esc(原样路径)>`
//       （存档时刻 stat 失败记 `P\t-\t-\t...`；判定时当前缺席或存档缺席都按
//       变化计——保守，cargo 同口径）。
//     default 面 path/根包一行树快照：`T\t<折叠串>`（source_stamp_dir 同款
//       (路径:len:mtime_ns) 排序 \u{1e} 折叠，占本行剩余全部不再分列）；
//       registry 包源按 cksum 不可变，无 T 行（判定时直接 skip）。
//     rerun-if-env-changed 两面通用逐变量一行：
//       `E0\t<esc(var)>`（当时缺席）/ `E1\t<esc(var)>\t<esc(值)>`。
//   esc：`\`→`\\`、制表符→`\t`、换行→`\n`（路径/env 值含分列符的理论面兜死；
//   非 UTF-8 路径经 display 有损——存档判定同走 display 串，stat 失败按
//   变化计，保守不错过）。
// 存档在**执行成功后**写（失败 driver 已响亮退出，无半存档）；两文件缺一或
// 解析失败 ⇒ 判 run 自愈。

/// 行内转义（见上格式说明）。
fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => o.push_str("\\\\"),
            '\t' => o.push_str("\\t"),
            '\n' => o.push_str("\\n"),
            _ => o.push(c),
        }
    }
    o
}

fn unesc(s: &str) -> Result<String, String> {
    let mut o = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            match it.next() {
                Some('\\') => o.push('\\'),
                Some('t') => o.push('\t'),
                Some('n') => o.push('\n'),
                other => return Err(format!("rerun.txt 坏转义 \\{}", other.unwrap_or('?'))),
            }
        } else {
            o.push(c);
        }
    }
    Ok(o)
}

/// 一个 rerun-if-changed 路径的存档快照 (len, mtime_ns)；None = 存档时刻缺席
/// （判定时当前缺席/存档缺席都按变化计——保守，cargo 同口径）。
type FileStamp = Option<(u64, u128)>;

/// 解析后的 rerun.txt（should_rerun 的判定输入）。
struct RerunRecord {
    /// Some = changed 面：(显示用原样路径, 存档快照)。
    changed_paths: Option<Vec<(String, FileStamp)>>,
    /// default 面 path/根包的树快照（registry 无）。
    tree: Option<String>,
    /// (var, 当时值（None = 当时缺席）)。
    envs: Vec<(String, Option<String>)>,
}

fn parse_record(text: &str) -> Result<RerunRecord, String> {
    let mut lines = text.lines();
    let face = lines.next().ok_or("rerun.txt 空")?;
    let mut rec = RerunRecord {
        changed_paths: match face {
            "mirvm-bldrs-rerun-v1 changed" => Some(Vec::new()),
            "mirvm-bldrs-rerun-v1 default" => None,
            _ => return Err(format!("rerun.txt 首行不识：{face}")),
        },
        tree: None,
        envs: Vec::new(),
    };
    for line in lines {
        if let Some(rest) = line.strip_prefix("P\t") {
            let mut f = rest.splitn(3, '\t');
            let (len, mtime, path) = (
                f.next().unwrap_or(""),
                f.next().unwrap_or(""),
                f.next().ok_or("P 行缺路径列")?,
            );
            let stamp = if len == "-" && mtime == "-" {
                None
            } else {
                Some((
                    len.parse::<u64>()
                        .map_err(|_| format!("P 行 len 坏：{len}"))?,
                    mtime
                        .parse::<u128>()
                        .map_err(|_| format!("P 行 mtime 坏：{mtime}"))?,
                ))
            };
            rec.changed_paths
                .as_mut()
                .ok_or("default 面混入 P 行")?
                .push((unesc(path)?, stamp));
        } else if let Some(stamp) = line.strip_prefix("T\t") {
            rec.tree = Some(stamp.to_string());
        } else if let Some(var) = line.strip_prefix("E0\t") {
            rec.envs.push((unesc(var)?, None));
        } else if let Some(rest) = line.strip_prefix("E1\t") {
            let (var, val) = rest.split_once('\t').ok_or("E1 行缺值列")?;
            rec.envs.push((unesc(var)?, Some(unesc(val)?)));
        } else {
            return Err(format!("rerun.txt 坏行：{line}"));
        }
    }
    Ok(rec)
}

/// (len, mtime_ns) 快照（source_stamp_dir 行内同款取法：mtime 失败记 0）。
fn len_mtime(p: &Path) -> Option<(u64, u128)> {
    let md = std::fs::metadata(p).ok()?;
    let mtime_ns = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    Some((md.len(), mtime_ns))
}

/// rerun-if-changed 的 PATH 解析：绝对路径原样，相对路径拼包根（cargo 同——
/// 相对者相对 CARGO_MANIFEST_DIR）。
fn absolutize(pkg_root: &Path, p: &str) -> PathBuf {
    let path = Path::new(p);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        pkg_root.join(path)
    }
}

/// 执行成功后写存档（output.txt = 原始 stdout；rerun.txt = 条件存档）。
/// `env_get` 注入便于单测；失败由调用方按「下次 no-record 重跑自愈」忽略。
pub fn write_record(
    record_dir: &Path,
    stdout: &str,
    bo: &BuildOutput,
    from_registry: bool,
    pkg: &str,
    pkg_root: &Path,
    env_get: &dyn Fn(&str) -> Option<String>,
) -> Result<(), String> {
    let mut t = String::new();
    if bo.rerun_if_changed.is_empty() {
        t.push_str("mirvm-bldrs-rerun-v1 default\n");
        if !from_registry {
            // 默认面快照与指纹盖戳同折叠（source_stamp_dir 排除 target/.git）
            let stamp = super::schedule::source_stamp_dir(false, pkg_root, pkg)?;
            t.push_str("T\t");
            t.push_str(&stamp);
            t.push('\n');
        }
    } else {
        t.push_str("mirvm-bldrs-rerun-v1 changed\n");
        for p in &bo.rerun_if_changed {
            let stamp = match len_mtime(&absolutize(pkg_root, p)) {
                Some((l, m)) => format!("{l}\t{m}"),
                None => "-\t-".to_string(),
            };
            t.push_str(&format!("P\t{stamp}\t{}\n", esc(p)));
        }
    }
    for v in &bo.rerun_if_env_changed {
        match env_get(v) {
            Some(val) => t.push_str(&format!("E1\t{}\t{}\n", esc(v), esc(&val))),
            None => t.push_str(&format!("E0\t{}\n", esc(v))),
        }
    }
    let w = |name: &str, data: &str| {
        std::fs::write(record_dir.join(name), data)
            .map_err(|e| format!("写 {name} 失败（{}）: {e}", record_dir.display()))
    };
    w("rerun.txt", &t)?;
    w("output.txt", stdout)
}

/// 重跑判定（cargo 同语义；返回 (是否重跑, 原因短语)——原因供
/// MIRVM_DEBUG_BLDRS=1 的 `bldrs run|skip <pkg> <原因>` 观测行）。
/// 规则：
/// 1. 存档两员缺一 ⇒ run（`no-record`；fp 变 ⇒ 新 fp 目录天然走这条，
///    「源/旗/依赖/工具链变 ⇒ 重跑」由指纹先行免费覆盖）。
/// 2. rerun.txt 损坏 ⇒ run（`bad-record`，自愈）。
/// 3. 直接依赖中带 links 的包本次会话重跑了 ⇒ run（`links-dep:<dep>`；
///    DEP_* 输入可能变，cargo 同。只看直接依赖——DEP_* 本就只给直接
///    依赖者；更远的传递由各层自己的判定覆盖）。
/// 4. rerun-if-env-changed=VAR：当前进程值 ≠ 存档值 ⇒ run（`env:<var>`）。
/// 5. 文件面：
///    - changed 面：任一 PATH 的 (len, mtime_ns) ≠ 存档 ⇒ run
///      （`changed-path:<p>`；当前缺席或存档缺席都按变化计——保守，cargo 同）。
///    - default 面：registry ⇒ 永不重跑（`registry-default-skip`，源按 cksum
///      不可变）；path/根包 ⇒ 重算树快照 ≠ 存档 ⇒ run（`default-tree`）。
///
///    全部一致 ⇒ skip（`changed-intact` / `default-tree-intact`）。
pub fn should_rerun(
    record_dir: &Path,
    from_registry: bool,
    pkg: &str,
    pkg_root: &Path,
    dep_links_reran: &[String],
    env_get: &dyn Fn(&str) -> Option<String>,
) -> (bool, String) {
    let rerun_txt = record_dir.join("rerun.txt");
    let text = match std::fs::read_to_string(&rerun_txt) {
        Ok(t) if record_dir.join("output.txt").is_file() => t,
        _ => return (true, "no-record".to_string()),
    };
    let rec = match parse_record(&text) {
        Ok(r) => r,
        Err(_) => return (true, "bad-record".to_string()),
    };
    if let Some(dep) = dep_links_reran.first() {
        return (true, format!("links-dep:{dep}"));
    }
    for (var, old) in &rec.envs {
        if env_get(var) != *old {
            return (true, format!("env:{var}"));
        }
    }
    match &rec.changed_paths {
        Some(paths) => {
            for (disp, old) in paths {
                if len_mtime(&absolutize(pkg_root, disp)) != *old {
                    return (true, format!("changed-path:{disp}"));
                }
            }
            (false, "changed-intact".to_string())
        }
        None => {
            if from_registry {
                return (false, "registry-default-skip".to_string());
            }
            let Some(old) = &rec.tree else {
                // default 面 path 包存档必有 T 行——缺了按损坏自愈
                return (true, "bad-record".to_string());
            };
            match super::schedule::source_stamp_dir(false, pkg_root, pkg) {
                Ok(now) if &now == old => (false, "default-tree-intact".to_string()),
                // 树读不出来也按变化计（保守；fp 阶段已读过同一棵树，几乎到不了这）
                _ => (true, "default-tree".to_string()),
            }
        }
    }
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
        // 切⑤b：rerun-if 两键收进 BuildOutput（重跑判定消费）
        assert_eq!(out.rerun_if_changed, ["build.rs"]);
        assert_eq!(out.rerun_if_env_changed, ["CC"]);
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
    fn build_script_env_marks_each_enabled_feature() {
        // cranelift-codegen 按 CARGO_FEATURE_PULLEY 决定生成 pulley_inst_gen.rs
        // 实锤——逐启用 feature 的 CARGO_FEATURE_<NAME>=1 必须在场
        // （缺了它 OUT_DIR 产物缺文件、include! 炸，corpus smoke 分诊实锤）。
        let pkg_env = BTreeMap::new();
        let features: BTreeSet<String> = ["pulley", "std"].iter().map(|s| s.to_string()).collect();
        let env = build_script_env(&ExecCtx {
            pkg_env: &pkg_env,
            source_dir: Path::new("/tmp/x"),
            features: &features,
            profile: &ProfileFlags::default(),
            out_dir: Path::new("/tmp/x/out"),
            dep_env: BTreeMap::new(),
            links: None,
            ld_dirs: &[],
        });
        assert_eq!(
            env.get("CARGO_FEATURE_PULLEY").map(String::as_str),
            Some("1")
        );
        assert_eq!(env.get("CARGO_FEATURE_STD").map(String::as_str), Some("1"));
        assert!(!env.contains_key("CARGO_FEATURE_NOPE"));
    }

    #[test]
    fn build_script_env_sets_manifest_links_only_with_links() {
        // ring 0.17.14 build.rs `env::var("CARGO_MANIFEST_LINKS").unwrap()`
        // 实锤：有 links 键时必须设，无 links 键时不设（cargo 文档同款）。
        let pkg_env = BTreeMap::new();
        let features = BTreeSet::new();
        let mk = |links: Option<&str>| {
            build_script_env(&ExecCtx {
                pkg_env: &pkg_env,
                source_dir: Path::new("/tmp/x"),
                features: &features,
                profile: &ProfileFlags::default(),
                out_dir: Path::new("/tmp/x/out"),
                dep_env: BTreeMap::new(),
                links,
                ld_dirs: &[],
            })
        };
        assert_eq!(
            mk(Some("ring_core_0_17_14"))
                .get("CARGO_MANIFEST_LINKS")
                .map(String::as_str),
            Some("ring_core_0_17_14")
        );
        assert!(!mk(None).contains_key("CARGO_MANIFEST_LINKS"));
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

    // ---- 切⑤b：重跑判定矩阵 + 存档往返 ----

    /// 每测试一枚独立临时目录（并行不互踩）；返回 (record_dir, pkg_root)。
    fn rerun_tmp(tag: &str) -> (PathBuf, PathBuf) {
        let base =
            std::env::temp_dir().join(format!("mirvm-bldrs-rerun-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let record = base.join("record");
        let root = base.join("pkg");
        std::fs::create_dir_all(&record).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        (record, root)
    }

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k: &str| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    /// changed 面夹具：包内 build.rs + 一枚 env 变量。
    fn changed_bo() -> BuildOutput {
        BuildOutput {
            rerun_if_changed: vec!["build.rs".into()],
            rerun_if_env_changed: vec!["X".into()],
            ..Default::default()
        }
    }

    #[test]
    fn rerun_no_record_runs() {
        let (record, root) = rerun_tmp("no-record");
        let env = env_of(&[]);
        let (run, why) = should_rerun(&record, false, "demo", &root, &[], &env);
        assert!(run && why == "no-record", "{why}");
        // 只有 rerun.txt 没有 output.txt 同样 no-record（两员缺一不可）
        std::fs::write(record.join("rerun.txt"), "mirvm-bldrs-rerun-v1 default\n").unwrap();
        let (run, why) = should_rerun(&record, true, "demo", &root, &[], &env);
        assert!(run && why == "no-record", "{why}");
    }

    #[test]
    fn changed_face_roundtrip_skips() {
        let (record, root) = rerun_tmp("roundtrip");
        std::fs::write(root.join("build.rs"), "fn main(){}").unwrap();
        let env = env_of(&[("X", "1")]);
        write_record(&record, "stdout", &changed_bo(), false, "demo", &root, &env).unwrap();
        // 存档往返：同 env 同文件 ⇒ skip（changed-intact）
        let (run, why) = should_rerun(&record, false, "demo", &root, &[], &env);
        assert!(!run && why == "changed-intact", "{why}");
    }

    #[test]
    fn changed_path_mtime_or_absence_runs() {
        let (record, root) = rerun_tmp("mtime");
        let f = root.join("build.rs");
        std::fs::write(&f, "fn main(){}").unwrap();
        let env = env_of(&[("X", "1")]);
        write_record(&record, "s", &changed_bo(), false, "demo", &root, &env).unwrap();
        // 只动 mtime（len 不变）⇒ run（(len, mtime_ns) 元组比对）
        std::fs::File::options()
            .write(true)
            .open(&f)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1000))
            .unwrap();
        let (run, why) = should_rerun(&record, false, "demo", &root, &[], &env);
        assert!(run && why == "changed-path:build.rs", "{why}");
        // 文件缺席按变化计（保守，cargo 同口径）
        std::fs::remove_file(&f).unwrap();
        let (run, why) = should_rerun(&record, false, "demo", &root, &[], &env);
        assert!(run && why == "changed-path:build.rs", "{why}");
    }

    #[test]
    fn env_value_change_runs() {
        let (record, root) = rerun_tmp("env");
        std::fs::write(root.join("build.rs"), "fn main(){}").unwrap();
        write_record(
            &record,
            "s",
            &changed_bo(),
            false,
            "demo",
            &root,
            &env_of(&[("X", "1")]),
        )
        .unwrap();
        // 值变 ⇒ run；值撤（缺席）⇒ run
        let (run, why) = should_rerun(&record, false, "demo", &root, &[], &env_of(&[("X", "2")]));
        assert!(run && why == "env:X", "{why}");
        let (run, why) = should_rerun(&record, false, "demo", &root, &[], &env_of(&[]));
        assert!(run && why == "env:X", "{why}");
    }

    #[test]
    fn registry_default_face_never_reruns() {
        let (record, root) = rerun_tmp("registry-default");
        // registry 包默认面：pkg_root 连造都不用造（不读树）⇒ skip
        let env = env_of(&[]);
        write_record(
            &record,
            "s",
            &BuildOutput::default(),
            true,
            "libc",
            &root,
            &env,
        )
        .unwrap();
        let (run, why) = should_rerun(&record, true, "libc", &root, &[], &env);
        assert!(!run && why == "registry-default-skip", "{why}");
    }

    #[test]
    fn path_default_face_tree_change_runs() {
        let (record, root) = rerun_tmp("default-tree");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "").unwrap();
        let env = env_of(&[]);
        write_record(
            &record,
            "s",
            &BuildOutput::default(),
            false,
            "demo",
            &root,
            &env,
        )
        .unwrap();
        // 往返 skip（默认面树快照一致）
        let (run, why) = should_rerun(&record, false, "demo", &root, &[], &env);
        assert!(!run && why == "default-tree-intact", "{why}");
        // 包内任意文件变化（新增）⇒ run
        std::fs::write(root.join("src/new.rs"), "").unwrap();
        let (run, why) = should_rerun(&record, false, "demo", &root, &[], &env);
        assert!(run && why == "default-tree", "{why}");
    }

    #[test]
    fn links_dep_rerun_propagates() {
        let (record, root) = rerun_tmp("links-dep");
        std::fs::write(root.join("build.rs"), "fn main(){}").unwrap();
        let env = env_of(&[("X", "1")]);
        write_record(&record, "s", &changed_bo(), false, "demo", &root, &env).unwrap();
        // 直接依赖的 links 包本次重跑了 ⇒ 本包也 run（DEP_* 输入可能变）
        let deps = vec!["bdep".to_string()];
        let (run, why) = should_rerun(&record, false, "demo", &root, &deps, &env);
        assert!(run && why == "links-dep:bdep", "{why}");
    }

    #[test]
    fn corrupt_record_runs() {
        let (record, root) = rerun_tmp("corrupt");
        std::fs::write(root.join("build.rs"), "fn main(){}").unwrap();
        let env = env_of(&[("X", "1")]);
        write_record(&record, "s", &changed_bo(), false, "demo", &root, &env).unwrap();
        std::fs::write(record.join("rerun.txt"), "这不是存档").unwrap();
        let (run, why) = should_rerun(&record, false, "demo", &root, &[], &env);
        assert!(run && why == "bad-record", "{why}");
    }

    #[test]
    fn esc_roundtrip_and_bad_escape() {
        assert_eq!(esc("a\\b\tc\nd"), "a\\\\b\\tc\\nd");
        assert_eq!(unesc(&esc("a\\b\tc\nd")).unwrap(), "a\\b\tc\nd");
        assert!(unesc("孤\\").is_err());
    }
}
