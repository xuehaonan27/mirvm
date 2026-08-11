//! `cargoless/schedule.rs` —— 拓扑排序 + 指纹 + 每 crate rustc 参数计算
//! （D15 P2 切①/②/③，设计档 §3.6）。
//!
//! 产物布局：`cache_dir()/target/cargoless/<MIRVM_HOST>/debug/{deps,host-deps,build}`
//! ——target 产物（deps）与 host 产物（host-deps：proc-macro 闭包与 build-deps
//! 闭包，真 rustc 真 codegen）分目录，同名 fp 不撞；build/ 是 build script
//! 族（`build/<pkg>-<fp>/{build_script_build-<fp>,out}`，切③）；与 cargo
//! 路径的 `target/mirvm` 双轨并存（P2→P4 迁移期两条路径互不踩产物）。
//! 产物命名 `lib<lib_name>-<fp>.{rmeta,rlib,so}`：fp 是本文件的自定方案（cargo 的
//! -C metadata 算法不稳定不追，设计档 §3.6——cargo 已退场，内部一致即可）。

use std::collections::{BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};

use super::buildrs::BuildOutput;
use super::manifest::{DepKind, PackageManifest, ProfileFlags};
use super::resolve::{ResolvePlan, Unit, UnitClass};

/// 产物布局（见文件头）。
pub struct Layout {
    /// target 产物目录（__cless-dep，-Zno-codegen rlib）。
    pub deps: PathBuf,
    /// host 产物目录（proc-macro 闭包与 build-deps 闭包，真 rustc 真 codegen）。
    pub host_deps: PathBuf,
    /// build script 族根目录（`build/<pkg>-<fp>/{build_script_build-<fp>,out}`）。
    pub build_root: PathBuf,
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
            build_root: base.join("build"),
        }
    }

    /// 显式布局（D15 P4 切⑥a，sysroot 自管）：deps 指 sysroot lib 平铺目，
    /// host 产物与 build script 族指独立 staging（不进 sysroot lib——
    /// host 二进制与 host rlib 不是 target 产物）。
    pub fn at(deps: PathBuf, host_deps: PathBuf, build_root: PathBuf) -> Self {
        Self {
            deps,
            host_deps,
            build_root,
        }
    }

    /// 一个包的 build script 工作目录（编译产物与 OUT_DIR 都在其下）。
    pub fn build_dir(&self, pkg: &str, fp: &str) -> PathBuf {
        self.build_root.join(format!("{pkg}-{fp}"))
    }
}

/// 依赖图结构：正向 indegree + 反向边表（dep → 依赖者；依赖者按下标升序
/// 入列——构造循环按 unit 下标 0..n 逐边推入）。topo_order 与并行调度
/// （run_scheduler，D15 P3 切⑤c）共用同一构造纪律：同 unit 的多条边
/// （不同 key/class）两侧都重复计，递减次数才配平。
pub fn dep_graph(plan: &ResolvePlan) -> (Vec<Vec<usize>>, Vec<usize>) {
    let n = plan.units.len();
    let mut indeg = vec![0usize; n];
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); n]; // 反向边：dep → 依赖者
    for (i, u) in plan.units.iter().enumerate() {
        for d in &u.deps {
            indeg[i] += 1;
            dependents[d.unit].push(i);
        }
    }
    (dependents, indeg)
}

/// Kahn 拓扑序：依赖先于依赖者。返回 units 下标序。
pub fn topo_order(plan: &ResolvePlan) -> Result<Vec<usize>, String> {
    let n = plan.units.len();
    let (dependents, mut indeg) = dep_graph(plan);
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

/// Kahn 就绪队列并行调度器（D15 P3 切⑤c）：unit 的全部依赖「完成」即
/// 就绪；主线程跑调度循环（独占 indegree/就绪队列/完成表 state），`jobs`
/// 个 worker 线程从通道领 `(下标, M)` 跑 `work`，经通道回传
/// `(下标, Result<T, String>)`；主线程收完成 → `on_done` 吸收 → 依赖者
/// indegree 递减 → 新就绪入队。失败：记**第一枚**错误（按完成到达先后，
/// 非 topo 位次）、停发新活、等在飞 worker 全部汇合后返回 Err。
///
/// - `build_msg(&state, ix) -> M` 与 `on_done(&mut state, ix, T)` 只在主
///   线程跑——完成表（state）不跨线程，零锁；worker 开工所需的依赖侧
///   输入由主线程在**派发时**算好捎进 M（此刻全部依赖必已完成，取值与
///   串行版在 unit 开头算的相等）。
/// - `work` 在 worker 线程跑，须 `Sync`（多 worker 共享同一份闭包与其
///   捕获的只读上下文）。worker 恐慌经 catch_unwind 转成普通失败回传—
///   —否则主线程会把恐慌中的 unit 记在 in_flight 里干等（恐慌 hook 照
///   常向 stderr 打 panic 原文；与旧串行「恐慌即崩」文案有别，但那是
///   内部错误路径，非对拍面）。
/// - jobs=1 时派发序与 topo_order 逐位一致（同 dep_graph 构造纪律 +
///   单在飞 ⇒ 完成序 = 派发序 = Kahn FIFO）——对拍调试锚，钉。
///
/// 成功返回 `Ok(state)`（完成表物归原主）；`Err` = 第一枚错误原文。
/// `indeg` 就地递减消耗（dep_graph 的产物，调用方不再复用）。
pub fn run_scheduler<S, M, T>(
    state: S,
    dependents: &[Vec<usize>],
    indeg: &mut [usize],
    jobs: usize,
    build_msg: impl Fn(&S, usize) -> M,
    work: impl Fn(M) -> Result<T, String> + Sync,
    mut on_done: impl FnMut(&mut S, usize, T),
) -> Result<S, String>
where
    M: Send,
    T: Send,
{
    let n = dependents.len();
    let mut state = state;
    let jobs = jobs.max(1);
    // 就绪队列播种与 topo_order 同纪律：下标升序扫 indeg==0
    let mut ready: VecDeque<usize> = (0..n).filter(|&i| indeg[i] == 0).collect();
    let (work_tx, work_rx) = mpsc::channel::<(usize, M)>();
    let (done_tx, done_rx) = mpsc::channel::<(usize, Result<T, String>)>();
    // std 的 mpsc 是单消费者：worker 组共享一把锁轮询领活（锁内仅一次
    // recv；竞争开销相对一次 rustc 编译可忽略）
    let work_rx = Arc::new(Mutex::new(work_rx));
    let first_error = std::thread::scope(|s| {
        for _ in 0..jobs {
            let rx = Arc::clone(&work_rx);
            let tx = done_tx.clone();
            let work = &work;
            s.spawn(move || {
                loop {
                    let next = {
                        let g = match rx.lock() {
                            Ok(g) => g,
                            Err(_) => break, // 锁中毒（同行恐慌）= 收工
                        };
                        g.recv()
                    };
                    let (ix, m) = match next {
                        Ok(x) => x,
                        Err(_) => break, // 主端挂线 = 收工
                    };
                    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(m)))
                        .unwrap_or_else(|_| {
                            Err(format!("unit {ix} 的编译 worker 恐慌（内部错误）"))
                        });
                    if tx.send((ix, r)).is_err() {
                        break; // 主端已走（按计数收满才走，理论不到）——防御
                    }
                }
            });
        }
        let mut first_error: Option<String> = None;
        let mut workers_dead = false; // 完成侧断流 = worker 全灭（锁中毒）
        let mut in_flight = 0usize;
        let mut finished = 0usize;
        while finished < n {
            // 先发活：就绪非空、在飞未满、无失败（失败即停发新活）
            while first_error.is_none() && in_flight < jobs {
                match ready.pop_front() {
                    Some(ix) => {
                        if work_tx.send((ix, build_msg(&state, ix))).is_err() {
                            workers_dead = true;
                            break;
                        }
                        in_flight += 1;
                    }
                    None => break,
                }
            }
            if in_flight == 0 {
                break;
            }
            let (ix, res) = match done_rx.recv() {
                Ok(x) => x,
                Err(_) => {
                    workers_dead = true;
                    break;
                }
            };
            in_flight -= 1;
            finished += 1;
            match res {
                Ok(t) => {
                    on_done(&mut state, ix, t);
                    for &j in &dependents[ix] {
                        indeg[j] -= 1;
                        if indeg[j] == 0 {
                            ready.push_back(j);
                        }
                    }
                }
                Err(e) => {
                    if first_error.is_none() {
                        first_error = Some(e);
                    }
                }
            }
        }
        drop(work_tx); // 挂线：空闲 worker 收工（scope 尾自动汇合）
        if first_error.is_none() && finished < n {
            first_error = Some(if workers_dead {
                "编译 worker 线程异常终止（内部错误）".to_string()
            } else {
                // 活发不出也收不齐 = 依赖图有环（正常路径到不了：
                // fingerprints 内的 topo_order 会先炸）——兜底响亮
                "内部不一致：编译单元依赖图有环（cargo 解析图本应为 DAG）".to_string()
            });
        }
        first_error
    });
    match first_error {
        None => Ok(state),
        Some(e) => Err(e),
    }
}

/// host 闭包（切②）：从每个 proc-macro unit 沿 dep 边 BFS 的可达集（含
/// proc-macro 自身）。边不分类跟随——proc-macro 的 Build 边（它的
/// build-deps）同样是 host 编译输入。闭包单元用真 rustc 真 codegen 编成
/// host 产物。
/// 根包本身是 proc-macro 时，它的 Normal 依赖也是宿主编译输入；依赖
/// proc-macro 的闭包之外，再从根 Normal 边沿全依赖边扩张。
pub fn host_closure_for_root(plan: &ResolvePlan, root_proc_macro: bool) -> BTreeSet<usize> {
    let mut set = BTreeSet::new();
    let mut stack: Vec<usize> = plan
        .units
        .iter()
        .enumerate()
        .filter(|(_, u)| u.proc_macro)
        .map(|(i, _)| i)
        .collect();
    if root_proc_macro {
        stack.extend(
            plan.root_deps
                .iter()
                .filter(|dep| dep.class == UnitClass::Normal)
                .map(|dep| dep.unit),
        );
    }
    while let Some(i) = stack.pop() {
        if set.insert(i) {
            stack.extend(plan.units[i].deps.iter().map(|d| d.unit));
        }
    }
    set
}

/// build-deps 闭包（切③）：种子 = 每个 has_build_script unit 的 **Build 类
/// 边**（root_has_build 时根的 Build 边也算——根不是 unit，driver 传
/// manifest.has_build_script）；闭包内沿**全部**边扩张（build-dep 的普通
/// 依赖同样是 host 编译输入；build-dep 自己也可以有 build.rs，其
/// build-deps 随全边扩张自然入闭包，topo 序保证它先跑）。闭包单元用真
/// rustc 真 codegen 编成 host 产物（host-deps 目录）。
pub fn build_closure(plan: &ResolvePlan, root_has_build: bool) -> BTreeSet<usize> {
    let mut stack: Vec<usize> = Vec::new();
    for u in &plan.units {
        if u.has_build_script {
            stack.extend(
                u.deps
                    .iter()
                    .filter(|d| d.class == UnitClass::Build)
                    .map(|d| d.unit),
            );
        }
    }
    if root_has_build {
        stack.extend(
            plan.root_deps
                .iter()
                .filter(|d| d.class == UnitClass::Build)
                .map(|d| d.unit),
        );
    }
    let mut set = BTreeSet::new();
    while let Some(i) = stack.pop() {
        if set.insert(i) {
            stack.extend(plan.units[i].deps.iter().map(|d| d.unit));
        }
    }
    set
}

/// target 编译集（切②③）：从根的 Normal 类边出发沿 Normal 边 BFS——
/// Build 边不是代码依赖（切①② 靠 strip_build_units 掩盖，切③ 修正面）；
/// proc-macro unit 对 target 是叶子——不进集、也不钻入其内部（它的依赖是
/// host 依赖，不是根的 target 依赖；cargo 也不为 proc-macro crate 产
/// target rlib）。与 host 闭包可相交：同一 unit 同时被 bin 与 proc-macro
/// 用时两侧都编，双份产物分目录互不影响。
pub fn target_units(plan: &ResolvePlan) -> BTreeSet<usize> {
    let mut set = BTreeSet::new();
    let mut stack: Vec<usize> = plan
        .root_deps
        .iter()
        .filter(|d| d.class == UnitClass::Normal)
        .map(|d| d.unit)
        .collect();
    while let Some(i) = stack.pop() {
        if plan.units[i].proc_macro {
            continue;
        }
        if set.insert(i) {
            stack.extend(
                plan.units[i]
                    .deps
                    .iter()
                    .filter(|d| d.class == UnitClass::Normal)
                    .map(|d| d.unit),
            );
        }
    }
    set
}

/// 每 unit 的内容指纹（按 topo 序算——dep 的 fp 先于依赖者产出）。
/// fp(unit) = fnv1a(BUILD_ID, package, version, edition, 排序后 features,
/// profile 三员, sysroot_stamp, rustflags 逐条, 源 stamp, **排序后各 dep 的 fp**)。
/// 最后这枚必须含：depsimage pre-key 的「传递闭包变更 ⇒ 直接依赖产物盖戳变」
/// 不变量靠它传播——传递 dep 的 fp 变 ⇒ 直接 dep 的 fp 变 ⇒ 其产物文件名变
/// ⇒ bin 的 --extern 盖戳变（depsimage.rs 头注同款语义；钉死，勿删）。
/// 无 dep 源变化但 lock 版本集变化时，版本字段已覆盖。
/// rustflags（D15 P3 切⑤a）逐条按序进全部 unit fp：host 侧不吃 rustflags
/// 但跟随失效无害（v1 从简，边界记档）；顺序有语义（后旗压前旗）不排序。
pub fn fingerprints(
    plan: &ResolvePlan,
    profile: &ProfileFlags,
    sysroot_stamp: &str,
    rustflags: &[String],
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
        for f in rustflags {
            put(f); // 按序：rustflags 顺序有语义（后旗压前旗）
        }
        for flag in &u.rustc_lint_flags {
            put(flag);
        }
        put(&src_stamp);
        for d in &dep_fps {
            put(d);
        }
        fps[ix] = Some(format!("{:016x}", crate::lower::asm::fnv1a(key.as_bytes())));
    }
    Ok(fps.into_iter().map(|f| f.expect("全序已填")).collect())
}

/// 源 stamp：Git 单元 = lock 中的精确 source id（含 commit）；registry 单元 =
/// 字面 "registry"（源按 cksum 不可变不盖戳——.crate 解包树的 mtime 是解包
/// 时刻，盖了只会制造无谓重建；版本号已覆盖内容）；
/// path 单元 = 递归遍历 source_dir（排除 target/ 与 .git/）全部文件的
/// (相对路径, len, mtime_ns) 排序后折叠。根包同规则（root_fingerprint 复用）。
fn source_stamp(u: &Unit) -> Result<String, String> {
    if let Some(source_id) = &u.immutable_source_id {
        return Ok(source_id.clone());
    }
    source_stamp_dir(u.from_registry, &u.source_dir, &u.package)
}

/// pub(super)：buildrs.rs 的 build.rs 重跑判定（D15 P3 切⑤b）default 面
/// 树快照复用同一折叠——存档快照与指纹盖戳同口径，漂移同源。
pub(super) fn source_stamp_dir(
    from_registry: bool,
    source_dir: &Path,
    package: &str,
) -> Result<String, String> {
    if from_registry {
        return Ok("registry".to_string());
    }
    let mut rows: Vec<String> = Vec::new();
    let mut stack = vec![source_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let rd = std::fs::read_dir(&dir).map_err(|e| {
            format!(
                "path 依赖 {} 源目录读取失败 {}: {e}",
                package,
                dir.display()
            )
        })?;
        for ent in rd {
            let ent = ent.map_err(|e| format!("path 依赖 {} 源目录条目读取失败: {e}", package))?;
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
                        package,
                        p.display()
                    )
                })?;
                let rel = p.strip_prefix(source_dir).unwrap_or(&p);
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

/// 根包指纹（切③：根 build script 编译缓存键 + build 目录名；切⑤a 起兼作
/// 根 lib target 的产物名盖戳）。根不是 unit 不在 fingerprints() 里，同配方
/// 单独算：BUILD_ID、包名/版本/edition、排序后 root features、profile 三员、
/// sysroot_stamp、rustflags 逐条（根 lib 是 target 单元吃 rustflags，fp 必
/// 含；根 build script 编译不吃、跟随失效无害）、根源 stamp、排序后全部根
/// 边 dep 的 fp（Build 边在——build script 的 --extern 盖戳随它们变）。
pub fn root_fingerprint(
    manifest: &PackageManifest,
    plan: &ResolvePlan,
    fps: &[String],
    profile: &ProfileFlags,
    sysroot_stamp: &str,
    rustflags: &[String],
) -> Result<String, String> {
    let src_stamp = source_stamp_dir(false, &manifest.root, &manifest.name)?;
    let mut key = String::from(env!("MIRVM_BUILD_ID"));
    let mut put = |s: &str| {
        key.push('\u{1f}');
        key.push_str(s);
    };
    put(&manifest.name);
    put(&manifest.version.to_string());
    put(&manifest.edition);
    for f in &plan.root_features {
        put(f);
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
    for f in rustflags {
        put(f); // 按序（unit fp 同款纪律）
    }
    for flag in &manifest.rustc_lint_flags {
        put(flag);
    }
    put(&src_stamp);
    let mut dep_fps: Vec<&str> = plan
        .root_deps
        .iter()
        .map(|d| fps[d.unit].as_str())
        .collect();
    dep_fps.sort();
    for d in dep_fps {
        put(d);
    }
    Ok(format!("{:016x}", crate::lower::asm::fnv1a(key.as_bytes())))
}

/// profile 三旗（cargo dev profile 语义钉，设计档 §6 的 jiff 判例：
/// debug-assertions/overflow-checks 进 MIR 语义，错配 = 对拍漂移）。
fn push_profile_flags(a: &mut Vec<String>, p: &ProfileFlags) {
    let yn = |b: bool| if b { "yes" } else { "no" };
    a.push("-C".into());
    a.push(format!("debug-assertions={}", yn(p.debug_assertions)));
    a.push("-C".into());
    a.push(format!("overflow-checks={}", yn(p.overflow_checks)));
    if !p.opt_level.is_zero() {
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

/// 本包 BuildOutput 的编译旗追加（E1 实证：-l/cfg/check-cfg/link-arg 只进
/// 本包；-L 本包 + 传递汇集 `searches`）。rustc-env 不进 argv——经 driver
/// 的 cmd.env 注入（连同 OUT_DIR）。旗序对齐 cargo 本包行（-L、-l、
/// link-arg、--cfg、--check-cfg），对拍不比对 argv 但保持形态诚实。
fn append_build_output(a: &mut Vec<String>, bo: Option<&BuildOutput>, searches: &[String]) {
    if let Some(bo) = bo {
        for s in &bo.link_searches {
            a.push("-L".into());
            a.push(s.clone());
        }
        for l in &bo.link_libs {
            a.push("-l".into());
            a.push(l.clone());
        }
        for f in &bo.link_args {
            a.push("-C".into());
            a.push(format!("link-arg={f}"));
        }
        for c in &bo.cfgs {
            a.push("--cfg".into());
            a.push(c.clone());
        }
        for c in &bo.check_cfgs {
            a.push("--check-cfg".into());
            a.push(c.clone());
        }
    }
    for s in searches {
        a.push("-L".into());
        a.push(s.clone());
    }
}

/// 一个 dep unit 的 rustc 参数（driver 起 `__cless-dep` 子进程喂
/// cli::run_dep_compiler；形态对齐 cargo 对 target 依赖的调用 +
/// cargo_shim.rs wrapper 段的 MIR sysroot/-Z 注入）。
/// argv0 = "mirvm-cless-rustc"（driver 起子进程时剥掉补真名）。
/// extern 只吃 **Normal 类边**（Build 边不是代码依赖——切③ 修正面）；
/// `bo` = 本 unit 的 build script 产物，`searches` = 传递 -L 汇集。
/// `rustflags`（D15 P3 切⑤a）追加在参数串**末尾**（-Z 旗之后）：rustc
/// 后旗压前旗，用户旗覆盖先行旗——实证见 rustflags.rs 头注（有 --target
/// 时 rustflags 只落 target 单元；host 侧参数函数签名不含 rustflags）。
// 平铺参数 = 编译配方各槽一一对应（manifest.rs pkg_env_map 同款先例）；
// 包成 struct 反而失去与 argv 段的目视对应
#[allow(clippy::too_many_arguments)]
pub fn dep_rustc_args(
    plan: &ResolvePlan,
    unit_ix: usize,
    profile: &ProfileFlags,
    fps: &[String],
    sysroot: &Path,
    layout: &Layout,
    bo: Option<&BuildOutput>,
    searches: &[String],
    rustflags: &[String],
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
    a.extend(u.rustc_lint_flags.iter().cloned());
    a.push("--check-cfg".into());
    a.push("cfg(docsrs,test)".into());
    let vals = u
        .declared_features
        .iter()
        .map(|v| format!("\"{v}\""))
        .collect::<Vec<_>>()
        .join(",");
    a.push("--check-cfg".into());
    a.push(format!("cfg(feature, values({vals}))"));
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
    // host-deps 也进 -L：经 facade 再导出的 proc-macro（serde → serde_derive
    // 实锤）被 rustc 按 crate hash 在 -L 目里找 .so——cargo 单 deps 目天然
    // 覆盖，我们双目必须都列
    a.push("-L".into());
    a.push(format!("dependency={}", layout.host_deps.display()));
    for d in &u.deps {
        if d.kind != DepKind::Normal {
            continue; // Build 边不是代码依赖（切③ 修正面）
        }
        let du = &plan.units[d.unit];
        // proc-macro 边指 host-deps 的 dylib；普通边照旧 target .rmeta
        a.push("--extern".into());
        a.push(format!(
            "{}={}",
            d.key.replace('-', "_"),
            extern_path(layout, &layout.deps, du, &fps[d.unit], "rmeta")
        ));
    }
    append_build_output(&mut a, bo, searches);
    a.push("--sysroot".into());
    a.push(sysroot.display().to_string());
    a.push("-Zalways-encode-mir".into());
    a.push("-Zno-codegen".into());
    // rustflags 末尾追加：后旗压前旗（--cap-lints allow 须压住 path 依赖的
    // 内置 lint 行为）
    a.extend(rustflags.iter().cloned());
    a
}

/// bin（根 crate）会话参数——走既有 MirvmCallbacks 降低通道（after_analysis
/// 停，Compilation::Stop，零产物）：**不**加 -Z 旗、--out-dir、-C metadata
/// （与 cargo 路径 runner 段的 bin 会话同形态）。
/// extern 只吃 Normal 类根边；`bo` = 根 build script 产物（cfg/check-cfg/
/// link 旗进会话），`searches` = 传递 -L 汇集。
/// `rustflags`（D15 P3 切⑤a）追加在参数串**末尾**（--sysroot 之后）：
/// 后旗压前旗（bin 是 path 包无内置 --cap-lints，RUSTFLAGS 的 --cap-lints
/// allow 在此压住 lint 告警——hexyl/tokei 对拍场景）。
/// `root_lib` = Some((lib_name, lib_fp)) 时补根包 lib target 的 --extern
/// （[lib]+[[bin]] 双 target 时 bin 隐式依赖同名 lib——hexyl 实锤，cargo
/// 实证行里根 lib 与其余 --extern 混排指 .rlib；root_lib_rustc_args 的产物）。
// 平铺参数先例同 dep_rustc_args
#[allow(clippy::too_many_arguments)]
pub fn bin_rustc_args(
    manifest: &PackageManifest,
    plan: &ResolvePlan,
    fps: &[String],
    sysroot: &Path,
    layout: &Layout,
    bin_name: &str,
    bin_path: &Path,
    bo: Option<&BuildOutput>,
    searches: &[String],
    rustflags: &[String],
    root_lib: Option<(&str, &str)>,
) -> Vec<String> {
    let deps = layout.deps.display();
    let mut a: Vec<String> = vec!["mirvm".into()];
    a.push(bin_path.display().to_string());
    a.push("--crate-name".into());
    a.push(bin_name.replace('-', "_"));
    a.push(format!("--edition={}", manifest.edition));
    a.push("--crate-type=bin".into());
    // file!()/panic Location/诊断路径 parity（redb_kv 的 Location Display 与
    // gix_pure 的 panic 位置实锤）：cargo 以 cwd=包根 + 相对路径 src/main.rs
    // 调 rustc，本地包路径在一切输出里都是相对形；我们传绝对路径，用 remap
    // 把 Cargo 编译根（workspace 根；单包时等于包根）前缀重写为空——rustc book：remap 影响 all output including
    // compiler diagnostics（真 rustc 实证：绝对输入 + remap 的 file!() 与
    // 相对输入逐字节同）。registry/path 依赖路径仍绝对（cargo 同），只盖
    // 根包目录。
    a.push(format!(
        "--remap-path-prefix={}/=",
        manifest.lock_root.display()
    ));
    a.push("-C".into());
    a.push("embed-bitcode=no".into());
    a.push("-C".into());
    a.push("debuginfo=2".into());
    for f in &plan.root_features {
        a.push("--cfg".into());
        a.push(format!("feature=\"{f}\""));
    }
    a.extend(manifest.rustc_lint_flags.iter().cloned());
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
        if d.kind != DepKind::Normal {
            continue; // Build 边不是代码依赖（切③ 修正面）
        }
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
    // 根包 lib target 的 --extern。同包根是 proc-macro 时指宿主动态库；
    // 普通 lib 仍指 target rlib。
    if let Some((lib_name, lib_fp)) = root_lib {
        let root_is_proc_macro = manifest
            .targets
            .iter()
            .any(|target| target.is_lib() && target.proc_macro && target.name == lib_name);
        let path = if root_is_proc_macro {
            format!(
                "{}/lib{}-{lib_fp}{}",
                layout.host_deps.display(),
                lib_name.replace('-', "_"),
                std::env::consts::DLL_SUFFIX
            )
        } else {
            format!("{deps}/lib{}-{lib_fp}.rlib", lib_name.replace('-', "_"))
        };
        a.push("--extern".into());
        a.push(format!("{}={path}", lib_name.replace('-', "_")));
    }
    a.push("-L".into());
    a.push(format!("dependency={deps}"));
    // host-deps 也进 -L（facade 再导出 proc-macro 的 .so 查找——serde →
    // serde_derive 实锤；dep_rustc_args 同款注释）
    a.push("-L".into());
    a.push(format!("dependency={}", layout.host_deps.display()));
    append_build_output(&mut a, bo, searches);
    a.push("--sysroot".into());
    a.push(sysroot.display().to_string());
    // rustflags 末尾追加：后旗压前旗（dep_rustc_args 同款纪律）
    a.extend(rustflags.iter().cloned());
    a
}

/// 根包测试目标参数。先复用普通 bin 的 Cargo 对齐公共段，再只改两处：
/// - libtest harness 目标用 `--test`，不再显式 `--crate-type=bin`；
/// - 测试上下文额外看见根 Dev 边。普通根 lib 仍由 root_lib_rustc_args
///   编译且只消费 Normal 边，正好对应 Cargo 的“双编根 lib”。
#[allow(clippy::too_many_arguments)]
pub fn test_target_rustc_args(
    manifest: &PackageManifest,
    plan: &ResolvePlan,
    fps: &[String],
    sysroot: &Path,
    layout: &Layout,
    target_name: &str,
    target_path: &Path,
    harness: bool,
    bo: Option<&BuildOutput>,
    searches: &[String],
    rustflags: &[String],
    root_lib: Option<(&str, &str)>,
) -> Vec<String> {
    let mut args = bin_rustc_args(
        manifest,
        plan,
        fps,
        sysroot,
        layout,
        target_name,
        target_path,
        bo,
        searches,
        rustflags,
        root_lib,
    );
    if harness {
        if let Some(i) = args.iter().position(|a| a == "--crate-type=bin") {
            args.remove(i);
        }
        args.push("--test".into());
    } else {
        // Cargo 的 harness=false 测试仍设置 cfg(test)，但保留用户 main。
        args.push("--cfg".into());
        args.push("test".into());
    }
    if manifest
        .targets
        .iter()
        .any(|target| target.is_lib() && target.proc_macro && target.path == target_path)
    {
        args.push("-C".into());
        args.push("prefer-dynamic".into());
        args.push("--extern".into());
        args.push("proc_macro".into());
    }
    append_root_dev_externs(&mut args, plan, fps, layout);
    args
}

fn append_root_dev_externs(
    args: &mut Vec<String>,
    plan: &ResolvePlan,
    fps: &[String],
    layout: &Layout,
) {
    for d in &plan.root_deps {
        if d.kind != DepKind::Dev {
            continue;
        }
        let unit = &plan.units[d.unit];
        args.push("--extern".into());
        args.push(format!(
            "{}={}",
            d.key.replace('-', "_"),
            extern_path(layout, &layout.deps, unit, &fps[d.unit], "rlib")
        ));
    }
}

/// `cargo test` 默认会编译 examples，并在有 integration test 时编译普通 bins，
/// 但不会执行它们。这里沿用同一参数主体，以 metadata-only rustc 会话完成
/// 解析、类型检查和 mono 收集；`include_dev` 对应 example=true、普通 bin=false。
#[allow(clippy::too_many_arguments)]
pub fn check_root_target_rustc_args(
    manifest: &PackageManifest,
    plan: &ResolvePlan,
    fps: &[String],
    sysroot: &Path,
    layout: &Layout,
    target_name: &str,
    target_path: &Path,
    include_dev: bool,
    bo: Option<&BuildOutput>,
    searches: &[String],
    rustflags: &[String],
    root_lib: Option<(&str, &str)>,
    fp: &str,
) -> Vec<String> {
    let mut args = bin_rustc_args(
        manifest,
        plan,
        fps,
        sysroot,
        layout,
        target_name,
        target_path,
        bo,
        searches,
        rustflags,
        root_lib,
    );
    if include_dev {
        append_root_dev_externs(&mut args, plan, fps, layout);
    }
    args.push("--emit=dep-info,metadata".into());
    args.push("-C".into());
    args.push(format!("metadata={fp}"));
    args.push("-C".into());
    args.push(format!("extra-filename=-{fp}"));
    args.push("--out-dir".into());
    args.push(layout.deps.display().to_string());
    args.push("-Zalways-encode-mir".into());
    args.push("-Zno-codegen".into());
    args
}

/// 根包 lib target 的 rustc 参数（D15 P3 切⑤a full 层迁移面，hexyl 实锤：
/// 根包 [lib]+[[bin]] 双 target 时 bin 隐式依赖同名 lib，cargo 先把根 lib
/// 编成 target rlib 再让 bin --extern 它——cargo 实证行：根 lib =
/// `--crate-type lib --emit=dep-info,metadata,link` + --extern 指 dep 的
/// .rmeta，无 --cap-lints（path 包照常告警））。形态 = dep_rustc_args
/// 作用于根 lib（__cless-dep 通道，-Zno-codegen rlib 进 layout.deps），
/// 差异：源/edition/features 取自 manifest/plan（根不是 unit）；check-cfg
/// feature 值表 = 声明全集 + 隐式 optional（bin 同款）；--extern 吃
/// root_deps 的 Normal 类边；rustflags 末尾追加（根 lib 是 target 单元，
/// 吃 RUSTFLAGS——cargo --target 语义）。`fp` = root_fingerprint（产物名
/// lib<lib_name>-<fp>.{rmeta,rlib}）。
// 平铺参数先例同 dep_rustc_args
#[allow(clippy::too_many_arguments)]
pub fn root_lib_rustc_args(
    manifest: &PackageManifest,
    plan: &ResolvePlan,
    fps: &[String],
    sysroot: &Path,
    layout: &Layout,
    lib_name: &str,
    lib_path: &Path,
    bo: Option<&BuildOutput>,
    searches: &[String],
    rustflags: &[String],
    fp: &str,
) -> Vec<String> {
    let deps = layout.deps.display();
    let mut a: Vec<String> = vec!["mirvm-cless-rustc".into()];
    a.push("--crate-name".into());
    a.push(lib_name.replace('-', "_"));
    a.push(format!("--edition={}", manifest.edition));
    a.push(lib_path.display().to_string());
    a.push("--crate-type=lib".into());
    a.push("--emit=dep-info,metadata,link".into());
    a.push("-C".into());
    a.push("embed-bitcode=no".into());
    a.push("-C".into());
    a.push("debuginfo=2".into());
    for f in &plan.root_features {
        a.push("--cfg".into());
        a.push(format!("feature=\"{f}\""));
    }
    // path 包无 --cap-lints（cargo 同：照常告警）
    a.extend(manifest.rustc_lint_flags.iter().cloned());
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
    a.push("-C".into());
    a.push(format!("metadata={fp}"));
    // extra-filename 窗口纪律同 dep_rustc_args（run_dep_compiler 窗口抠取）
    a.push("-C".into());
    a.push(format!("extra-filename=-{fp}"));
    a.push("--out-dir".into());
    a.push(deps.to_string());
    a.push("-L".into());
    a.push(format!("dependency={deps}"));
    // host-deps 进 -L（facade 再导出 proc-macro 的 .so 查找——dep 同款注释）
    a.push("-L".into());
    a.push(format!("dependency={}", layout.host_deps.display()));
    for d in &plan.root_deps {
        if d.kind != DepKind::Normal {
            continue; // 根普通 lib 不消费 Build/Dev 边
        }
        let du = &plan.units[d.unit];
        // proc-macro 边指 host-deps 的 dylib；普通边照旧 target .rmeta
        a.push("--extern".into());
        a.push(format!(
            "{}={}",
            d.key.replace('-', "_"),
            extern_path(layout, &layout.deps, du, &fps[d.unit], "rmeta")
        ));
    }
    append_build_output(&mut a, bo, searches);
    a.push("--sysroot".into());
    a.push(sysroot.display().to_string());
    a.push("-Zalways-encode-mir".into());
    a.push("-Zno-codegen".into());
    // rustflags 末尾追加：后旗压前旗（dep_rustc_args 同款纪律）
    a.extend(rustflags.iter().cloned());
    a
}

/// 根 proc-macro 的宿主动态库参数。它不能走 VM 的 `-Zno-codegen`
/// 通道：后续 integration test 编译时 rustc 必须真实 dlopen 这个产物。
#[allow(clippy::too_many_arguments)]
pub fn root_proc_macro_rustc_args(
    manifest: &PackageManifest,
    plan: &ResolvePlan,
    fps: &[String],
    layout: &Layout,
    lib_name: &str,
    lib_path: &Path,
    bo: Option<&BuildOutput>,
    searches: &[String],
    rustflags: &[String],
    fp: &str,
) -> Vec<String> {
    let host = layout.host_deps.display();
    let mut a = vec![real_rustc()];
    a.push("--crate-name".into());
    a.push(lib_name.replace('-', "_"));
    a.push(format!("--edition={}", manifest.edition));
    a.push(lib_path.display().to_string());
    a.push("--crate-type=proc-macro".into());
    a.push("--emit=dep-info,link".into());
    a.push("-C".into());
    a.push("prefer-dynamic".into());
    a.push("-C".into());
    a.push("embed-bitcode=no".into());
    for feature in &plan.root_features {
        a.push("--cfg".into());
        a.push(format!("feature=\"{feature}\""));
    }
    a.extend(manifest.rustc_lint_flags.iter().cloned());
    a.push("--check-cfg".into());
    a.push("cfg(docsrs,test)".into());
    let values = manifest.check_cfg_feature_values();
    let vals = values
        .iter()
        .map(|value| format!("\"{value}\""))
        .collect::<Vec<_>>()
        .join(",");
    a.push("--check-cfg".into());
    a.push(format!("cfg(feature, values({vals}))"));
    push_profile_flags(&mut a, &manifest.profile);
    a.push("-C".into());
    a.push(format!("metadata={fp}"));
    a.push("-C".into());
    a.push(format!("extra-filename=-{fp}"));
    a.push("--out-dir".into());
    a.push(host.to_string());
    a.push("-L".into());
    a.push(format!("dependency={host}"));
    for dep in &plan.root_deps {
        if dep.class != UnitClass::Normal {
            continue;
        }
        let unit = &plan.units[dep.unit];
        a.push("--extern".into());
        a.push(format!(
            "{}={}",
            dep.key.replace('-', "_"),
            extern_path(layout, &layout.host_deps, unit, &fps[dep.unit], "rlib")
        ));
    }
    append_build_output(&mut a, bo, searches);
    a.push("--extern".into());
    a.push("proc_macro".into());
    a.extend(rustflags.iter().cloned());
    a
}

/// host 闭包普通单元的真 rustc 参数（切②，proc-macro2 实锤形态）：
/// `--crate-type lib --emit=dep-info,metadata,link -C embed-bitcode=no`
/// （**无 debuginfo、无 prefer-dynamic**）真 codegen 产 host rlib；
/// dep 边指 host-deps 的 .rmeta（proc-macro 边指 .so），**只吃 Normal 类边**
/// （切③ 修正面：Build 边是 build script 的输入，不是本 crate 代码依赖）。
/// **不带 --sysroot**（真 rustc 用自家 sysroot）、不带任何 -Z。
/// argv0 = 真 rustc 绝对路径（driver 直接 spawn，不经 __cless-dep）。
pub fn host_rustc_args(
    plan: &ResolvePlan,
    unit_ix: usize,
    profile: &ProfileFlags,
    fps: &[String],
    layout: &Layout,
    bo: Option<&BuildOutput>,
    searches: &[String],
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
    a.extend(u.rustc_lint_flags.iter().cloned());
    a.push("--check-cfg".into());
    a.push("cfg(docsrs,test)".into());
    let vals = u
        .declared_features
        .iter()
        .map(|v| format!("\"{v}\""))
        .collect::<Vec<_>>()
        .join(",");
    a.push("--check-cfg".into());
    a.push(format!("cfg(feature, values({vals}))"));
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
        if d.class != UnitClass::Normal {
            continue;
        }
        let du = &plan.units[d.unit];
        a.push("--extern".into());
        a.push(format!(
            "{}={}",
            d.key.replace('-', "_"),
            extern_path(layout, &layout.host_deps, du, &fps[d.unit], "rmeta")
        ));
    }
    append_build_output(&mut a, bo, searches);
    a
}

/// proc-macro crate 本体的真 rustc 参数（切②，serde_derive 实锤五钉）：
/// `--crate-type proc-macro --emit=dep-info,link -C prefer-dynamic
/// -C embed-bitcode=no`（**无 debuginfo**）+ 末尾裸 `--extern proc_macro`
/// （编译器内建桥 crate）；dep 边指 host-deps 的 .rlib（**真链接**进 dylib），
/// **只吃 Normal 类边**（切③ 修正面）。
/// 不带 --sysroot/-Z；argv0 = 真 rustc 绝对路径。
pub fn proc_macro_rustc_args(
    plan: &ResolvePlan,
    unit_ix: usize,
    profile: &ProfileFlags,
    fps: &[String],
    layout: &Layout,
    bo: Option<&BuildOutput>,
    searches: &[String],
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
    a.extend(u.rustc_lint_flags.iter().cloned());
    a.push("--check-cfg".into());
    a.push("cfg(docsrs,test)".into());
    let vals = u
        .declared_features
        .iter()
        .map(|v| format!("\"{v}\""))
        .collect::<Vec<_>>()
        .join(",");
    a.push("--check-cfg".into());
    a.push(format!("cfg(feature, values({vals}))"));
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
        if d.class != UnitClass::Normal {
            continue;
        }
        let du = &plan.units[d.unit];
        a.push("--extern".into());
        a.push(format!(
            "{}={}",
            d.key.replace('-', "_"),
            extern_path(layout, &layout.host_deps, du, &fps[d.unit], "rlib")
        ));
    }
    append_build_output(&mut a, bo, searches);
    // 末尾裸 --extern proc_macro：编译器内建桥，从真 rustc 自家 sysroot 解析
    a.push("--extern".into());
    a.push("proc_macro".into());
    a
}

/// build script 编译的真 rustc 参数（切③，E2 实证形态——registry 行）：
/// `--crate-name build_script_build --edition=<e> <build.rs 路径>
/// --crate-type bin --emit=dep-info,link -C embed-bitcode=no`，加 feature
/// cfgs、`--check-cfg cfg(docsrs,test)` 与 `cfg(feature, values(...))`、
/// profile 旗、`-C metadata/extra-filename`、`--out-dir <build/<pkg>-<fp>>`、
/// `-L dependency=<host-deps>`，以及 **Build 类边** --extern 指 host-deps
/// 产物（proc-macro build-dep 指 .so——extern_path 已分派）。
/// registry 加 --cap-lints allow（cap-lints 已兜住 unexpected_cfgs，feature
/// 值表 v1 只填已启用 feature——cargo 是声明全集 + 隐式 optional，path 包
/// build.rs 用未启用 feature 的 cfg 会多一条 unexpected_cfgs，夹具不触，
/// 记档）。无 --sysroot/-Z（真 rustc 自家 sysroot）；无 incremental（cargo
/// 只对 path 包开，属内部优化不复制）。argv0 = 真 rustc 绝对路径。
pub fn build_script_rustc_args(
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
    a.push("build_script_build".into());
    a.push(format!("--edition={}", u.edition));
    a.push(
        u.build_script_path
            .clone()
            .unwrap_or_else(|| u.source_dir.join("build.rs"))
            .display()
            .to_string(),
    );
    a.push("--crate-type=bin".into());
    a.push("--emit=dep-info,link".into());
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
    a.extend(u.rustc_lint_flags.iter().cloned());
    a.push("--check-cfg".into());
    a.push("cfg(docsrs,test)".into());
    let vals = u
        .declared_features
        .iter()
        .map(|v| format!("\"{v}\""))
        .collect::<Vec<_>>()
        .join(",");
    a.push("--check-cfg".into());
    a.push(format!("cfg(feature, values({vals}))"));
    push_profile_flags(&mut a, profile);
    a.push("-C".into());
    a.push(format!("metadata={fp}"));
    a.push("-C".into());
    a.push(format!("extra-filename=-{fp}"));
    a.push("--out-dir".into());
    a.push(layout.build_dir(&u.package, fp).display().to_string());
    a.push("-L".into());
    a.push(format!("dependency={host}"));
    for d in &u.deps {
        if d.kind != DepKind::Build {
            continue; // build script 只吃 Build 类边（build-deps）
        }
        let du = &plan.units[d.unit];
        a.push("--extern".into());
        a.push(format!(
            "{}={}",
            d.key.replace('-', "_"),
            extern_path(layout, &layout.host_deps, du, &fps[d.unit], "rlib")
        ));
    }
    a
}

/// 根包 build script 编译参数（根不是 unit：feature/边表来自 manifest 与
/// plan.root_deps；feature 值表用 manifest.check_cfg_feature_values()——
/// 声明全集 + 隐式 optional，与 cargo 精确一致）。`fp` = root_fingerprint。
pub fn root_build_script_rustc_args(
    manifest: &PackageManifest,
    plan: &ResolvePlan,
    fps: &[String],
    layout: &Layout,
    fp: &str,
) -> Vec<String> {
    let host = layout.host_deps.display();
    let mut a: Vec<String> = vec![real_rustc()];
    a.push("--crate-name".into());
    a.push("build_script_build".into());
    a.push(format!("--edition={}", manifest.edition));
    a.push(
        manifest
            .build_script_path
            .clone()
            .unwrap_or_else(|| manifest.root.join("build.rs"))
            .display()
            .to_string(),
    );
    a.push("--crate-type=bin".into());
    a.push("--emit=dep-info,link".into());
    a.push("-C".into());
    a.push("embed-bitcode=no".into());
    for f in &plan.root_features {
        a.push("--cfg".into());
        a.push(format!("feature=\"{f}\""));
    }
    a.extend(manifest.rustc_lint_flags.iter().cloned());
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
    a.push("-C".into());
    a.push(format!("metadata={fp}"));
    a.push("-C".into());
    a.push(format!("extra-filename=-{fp}"));
    a.push("--out-dir".into());
    a.push(layout.build_dir(&manifest.name, fp).display().to_string());
    a.push("-L".into());
    a.push(format!("dependency={host}"));
    for d in &plan.root_deps {
        if d.class != UnitClass::Build {
            continue;
        }
        let du = &plan.units[d.unit];
        a.push("--extern".into());
        a.push(format!(
            "{}={}",
            d.key.replace('-', "_"),
            extern_path(layout, &layout.host_deps, du, &fps[d.unit], "rlib")
        ));
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cargoless::lockfile::Lockfile;
    use crate::cargoless::manifest::DepKind;
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
            immutable_source_id: None,
            class: UnitClass::Normal,
            features: features
                .iter()
                .map(|f| f.to_string())
                .collect::<BTreeSet<_>>(),
            declared_features: features
                .iter()
                .map(|f| f.to_string())
                .collect::<BTreeSet<_>>(),
            proc_macro: false,
            has_build_script: false,
            build_script_path: None,
            links: None,
            deps,
            edition: "2021".to_string(),
            lib_path: PathBuf::from(format!("/tmp/{name}/src/lib.rs")),
            pkg_env: BTreeMap::new(),
            rustc_lint_flags: Vec::new(),
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
                kind: DepKind::Normal,
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
                kind: DepKind::Normal,
            }],
        );
        plan_with(
            vec![a, b, c],
            vec![
                UnitDep {
                    key: "b".into(),
                    unit: 1,
                    class: UnitClass::Normal,
                    kind: DepKind::Normal,
                },
                UnitDep {
                    key: "c".into(),
                    unit: 2,
                    class: UnitClass::Normal,
                    kind: DepKind::Normal,
                },
            ],
        )
    }

    fn layout() -> Layout {
        Layout {
            deps: PathBuf::from("/tmp/cless/deps"),
            host_deps: PathBuf::from("/tmp/cless/host-deps"),
            build_root: PathBuf::from("/tmp/cless/build"),
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

    // ---- 切⑤c：run_scheduler（Kahn 就绪队列并行调度核）----
    // 测试与 plan 解耦：直写「每 unit 的 dep 下标表」，按 dep_graph 同纪律
    // （下标升序逐边推入，重复边重复计）展开成 (dependents, indeg)。

    fn graph_from_deps(deps: &[&[usize]]) -> (Vec<Vec<usize>>, Vec<usize>) {
        let n = deps.len();
        let mut indeg = vec![0usize; n];
        let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); n];
        for (i, ds) in deps.iter().enumerate() {
            for &d in *ds {
                indeg[i] += 1;
                dependents[d].push(i);
            }
        }
        (dependents, indeg)
    }

    /// 菱形 + 独立点 + 重复边，jobs=4：任何 unit 不会在其依赖前开工
    /// （worker 内核对，违例成 Err 浮出水面），且全 unit 完成入 state。
    #[test]
    fn scheduler_never_starts_before_deps_and_finishes_all() {
        let deps: &[&[usize]] = &[&[], &[0], &[0], &[1, 2], &[], &[0, 0]];
        let n = deps.len();
        let (dependents, mut indeg) = graph_from_deps(deps);
        let done_flags = std::sync::Mutex::new(vec![false; n]);
        let r = run_scheduler(
            Vec::<usize>::new(),
            &dependents,
            &mut indeg,
            4,
            |_s, ix| ix,
            |ix| {
                {
                    let d = done_flags.lock().unwrap();
                    for &dep in deps[ix] {
                        if !d[dep] {
                            return Err(format!("unit {ix} 开工时依赖 {dep} 未完成"));
                        }
                    }
                }
                done_flags.lock().unwrap()[ix] = true;
                Ok(ix)
            },
            |s, ix, _| s.push(ix),
        );
        let mut got = r.unwrap();
        got.sort_unstable();
        assert_eq!(got, (0..n).collect::<Vec<_>>(), "全 unit 完成: {got:?}");
    }

    /// jobs=1 的派发序必须与 Kahn FIFO（topo_order 同纪律）逐位一致——
    /// 对拍调试锚（注释钉在 run_scheduler 头注）。
    #[test]
    fn scheduler_jobs1_matches_kahn_fifo_order() {
        // a←b, a←c；{b,c}←d；e 独立。手工 Kahn FIFO：播种 [0,4] → 0 完成
        // 放 1,2 → 4 → 1 → 2（放 3）→ 3
        let deps: &[&[usize]] = &[&[], &[0], &[0], &[1, 2], &[]];
        let (dependents, mut indeg) = graph_from_deps(deps);
        let done = run_scheduler(
            Vec::<usize>::new(),
            &dependents,
            &mut indeg,
            1,
            |_s, ix| ix,
            Ok,
            |s, ix, _| s.push(ix),
        )
        .unwrap();
        assert_eq!(done, vec![0, 4, 1, 2, 3]);
    }

    /// 失败语义（jobs=1 链 0→1→2，1/2 皆炸）：第一枚错误保留，失败后
    /// 停发新活（2 从不开工）。
    #[test]
    fn scheduler_first_error_wins_and_dispatch_stops() {
        let deps: &[&[usize]] = &[&[], &[0], &[1]];
        let (dependents, mut indeg) = graph_from_deps(deps);
        let started = std::sync::Mutex::new(Vec::new());
        let r = run_scheduler(
            Vec::<usize>::new(),
            &dependents,
            &mut indeg,
            1,
            |_s, ix| ix,
            |ix| {
                started.lock().unwrap().push(ix);
                if ix >= 1 {
                    Err(format!("boom-{ix}"))
                } else {
                    Ok(ix)
                }
            },
            |s, ix, _| s.push(ix),
        );
        assert_eq!(r.unwrap_err(), "boom-1", "第一枚错误保留");
        assert_eq!(*started.lock().unwrap(), vec![0, 1], "失败后停发新活");
    }

    /// 依赖图有环：无活可发也收不齐 → 响亮报错（与 topo_order 同文案）。
    #[test]
    fn scheduler_reports_cycle_loudly() {
        let deps: &[&[usize]] = &[&[1], &[0]]; // 0↔1 环
        let (dependents, mut indeg) = graph_from_deps(deps);
        let r = run_scheduler(
            Vec::<usize>::new(),
            &dependents,
            &mut indeg,
            2,
            |_s, ix| ix,
            Ok,
            |s, ix, _| s.push(ix),
        );
        assert_eq!(
            r.unwrap_err(),
            "内部不一致：编译单元依赖图有环（cargo 解析图本应为 DAG）"
        );
    }

    #[test]
    fn fingerprint_propagates_transitive_dep_change() {
        let plan = diamond_plan();
        let fps0 = fingerprints(&plan, &ProfileFlags::default(), "stamp0", &[]).unwrap();
        // b 自身字段一字不动，只改其传递 dep a 的 feature 集 ⇒ b 的 fp 必须变
        // （depsimage pre-key「传递闭包变更 ⇒ 直接依赖产物盖戳变」不变量）
        let mut plan2 = diamond_plan();
        plan2.units[0].features.insert("alloc".to_string());
        let fps1 = fingerprints(&plan2, &ProfileFlags::default(), "stamp0", &[]).unwrap();
        assert_ne!(fps0[0], fps1[0], "a 自身 fp 应变");
        assert_ne!(fps0[1], fps1[1], "a 变 ⇒ b 的 fp 必须变（传递传播）");
        assert_ne!(fps0[2], fps1[2], "a 变 ⇒ c 的 fp 必须变（传递传播）");
        // sysroot_stamp 与 profile 也进 fp
        let fps2 = fingerprints(&plan, &ProfileFlags::default(), "stamp1", &[]).unwrap();
        assert_ne!(fps0[0], fps2[0], "sysroot_stamp 进 fp");
        let relaxed = ProfileFlags {
            debug_assertions: false,
            overflow_checks: false,
            opt_level: crate::cargoless::manifest::OptLevel::O2,
        };
        let fps3 = fingerprints(&plan, &relaxed, "stamp0", &[]).unwrap();
        assert_ne!(fps0[0], fps3[0], "profile 三员进 fp");
    }

    #[test]
    fn fingerprint_distinguishes_git_commits() {
        let mut first = diamond_plan();
        first.units[0].immutable_source_id = Some(
            "git+https://example.invalid/repo?branch=main#1111111111111111111111111111111111111111"
                .to_string(),
        );
        let mut second = first.clone();
        second.units[0].immutable_source_id = Some(
            "git+https://example.invalid/repo?branch=main#2222222222222222222222222222222222222222"
                .to_string(),
        );
        let first_fps = fingerprints(&first, &ProfileFlags::default(), "stamp", &[]).unwrap();
        let second_fps = fingerprints(&second, &ProfileFlags::default(), "stamp", &[]).unwrap();
        assert_ne!(first_fps[0], second_fps[0], "Git commit 必须进入自身指纹");
        assert_ne!(
            first_fps[1], second_fps[1],
            "Git commit 变化必须沿依赖边传播"
        );
    }

    #[test]
    fn dep_args_carry_key_flags_and_window_shaped_extra_filename() {
        let plan = diamond_plan();
        let fps = fingerprints(&plan, &ProfileFlags::default(), "s", &[]).unwrap();
        let lo = layout();
        let a = dep_rustc_args(
            &plan,
            1,
            &ProfileFlags::default(),
            &fps,
            Path::new("/sys"),
            &lo,
            None,
            &[],
            &[],
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
    fn package_lints_reach_proc_macro_args_and_fingerprint() {
        let mut plain = diamond_plan();
        let plain_fps = fingerprints(&plain, &ProfileFlags::default(), "s", &[]).unwrap();
        plain.units[1].rustc_lint_flags = vec![
            "--warn=unexpected_cfgs".into(),
            "--check-cfg".into(),
            "cfg(bootstrap)".into(),
        ];
        let lint_fps = fingerprints(&plain, &ProfileFlags::default(), "s", &[]).unwrap();
        assert_ne!(plain_fps[1], lint_fps[1], "lint 配置必须进入 unit 指纹");

        let args = proc_macro_rustc_args(
            &plain,
            1,
            &ProfileFlags::default(),
            &lint_fps,
            &layout(),
            None,
            &[],
        );
        assert!(args.iter().any(|arg| arg == "--warn=unexpected_cfgs"));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--check-cfg", "cfg(bootstrap)"])
        );
    }

    #[test]
    fn bin_args_use_rlib_and_skip_z_flags() {
        let mut plan = diamond_plan();
        plan.root_features.insert("std".to_string());
        let fps = fingerprints(&plan, &ProfileFlags::default(), "s", &[]).unwrap();
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
            None,
            &[],
            &[],
            None,
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

    #[test]
    fn test_args_use_libtest_and_add_only_dev_externs_to_test_unit() {
        let normal = unit("normal", "1.0.0", true, &[], vec![]);
        let dev = unit("devonly", "1.0.0", true, &[], vec![]);
        let mut plan = plan_with(
            vec![normal, dev],
            vec![
                UnitDep {
                    key: "normal".into(),
                    unit: 0,
                    class: UnitClass::Normal,
                    kind: DepKind::Normal,
                },
                UnitDep {
                    key: "devonly".into(),
                    unit: 1,
                    class: UnitClass::Normal,
                    kind: DepKind::Dev,
                },
            ],
        );
        plan.root_features.insert("root-feature".into());
        let fps = fingerprints(&plan, &ProfileFlags::default(), "s", &[]).unwrap();
        let manifest = PackageManifest::parse(
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
             [features]\nroot-feature = []\n\
             [dependencies]\nnormal = \"1\"\n\
             [dev-dependencies]\ndevonly = \"1\"\n",
            Path::new("/tmp/demo"),
        )
        .unwrap();
        let lo = layout();
        let args = test_target_rustc_args(
            &manifest,
            &plan,
            &fps,
            Path::new("/sys"),
            &lo,
            "demo",
            Path::new("/tmp/demo/src/lib.rs"),
            true,
            None,
            &[],
            &[],
            None,
        );
        assert!(args.iter().any(|arg| arg == "--test"));
        assert!(!args.iter().any(|arg| arg == "--crate-type=bin"));
        assert!(args.windows(2).any(|w| {
            w[0] == "--extern"
                && w[1] == format!("normal=/tmp/cless/deps/libnormal-{}.rlib", fps[0])
        }));
        assert!(args.windows(2).any(|w| {
            w[0] == "--extern"
                && w[1] == format!("devonly=/tmp/cless/deps/libdevonly-{}.rlib", fps[1])
        }));

        let normal_lib = root_lib_rustc_args(
            &manifest,
            &plan,
            &fps,
            Path::new("/sys"),
            &lo,
            "demo",
            Path::new("/tmp/demo/src/lib.rs"),
            None,
            &[],
            &[],
            "rootfp",
        );
        assert!(!normal_lib.iter().any(|arg| arg.contains("devonly=")));
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
            kind: DepKind::Normal,
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
        let host = host_closure_for_root(&plan, false);
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
        let fps = fingerprints(&plan, &ProfileFlags::default(), "s", &[]).unwrap();
        let lo = layout();
        let a = proc_macro_rustc_args(&plan, 2, &ProfileFlags::default(), &fps, &lo, None, &[]);
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
        let fps = fingerprints(&plan, &ProfileFlags::default(), "s", &[]).unwrap();
        let lo = layout();
        let a = host_rustc_args(&plan, 1, &ProfileFlags::default(), &fps, &lo, None, &[]);
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
        let fps = fingerprints(&plan, &ProfileFlags::default(), "s", &[]).unwrap();
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
            None,
            &[],
            &[],
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
            None,
            &[],
            &[],
            None,
        );
        assert!(
            a.windows(2).any(|w| w[0] == "--extern" && w[1] == so),
            "bin 缺 .so --extern: {a:?}"
        );
        let ext = format!("uses_pm=/tmp/cless/deps/libuses_pm-{}.rlib", fps[3]);
        assert!(a.windows(2).any(|w| w[0] == "--extern" && w[1] == ext));
    }

    /// build.rs 场景（切③ 形状）：bdep = Build 类 build-dep（b 的 build.rs
    /// 用它）；b 有 build.rs；bdep 自己也有 build.rs 与 build-dep（cc0）。
    fn buildrs_plan() -> ResolvePlan {
        let bdep = |key: &str, unit: usize| UnitDep {
            key: key.into(),
            unit,
            class: UnitClass::Build,
            kind: DepKind::Build,
        };
        let ndep = |key: &str, unit: usize| UnitDep {
            key: key.into(),
            unit,
            class: UnitClass::Normal,
            kind: DepKind::Normal,
        };
        let mut cc0 = unit("cc0", "1.0.0", true, &[], vec![]);
        cc0.class = UnitClass::Build;
        let mut bdep_u = unit("bdep", "1.0.0", true, &[], vec![bdep("cc0", 0)]);
        bdep_u.class = UnitClass::Build;
        bdep_u.has_build_script = true;
        bdep_u.links = Some("mylinks".into());
        let mut b = unit("b", "1.0.0", true, &[], vec![bdep("bdep", 1)]);
        b.has_build_script = true;
        plan_with(
            vec![cc0, bdep_u, b],
            vec![
                ndep("b", 2),
                // 根也声明了一个 build-dep（根有 build.rs 时才是种子）
                bdep("bdep", 1),
            ],
        )
    }

    #[test]
    fn build_closure_follows_build_edges_then_all_edges() {
        let plan = buildrs_plan();
        // 根无 build.rs：种子只有 b 的 Build 边 → bdep；闭包内沿全边扩到 cc0
        let set = build_closure(&plan, false);
        assert_eq!(set, BTreeSet::from([1, 0]), "{set:?}");
        // 根有 build.rs：根的 Build 边同样是种子（结果集相同——bdep 共享）
        let set2 = build_closure(&plan, true);
        assert_eq!(set2, BTreeSet::from([1, 0]), "{set2:?}");
        // 根无 build.rs 且无人有 build.rs → 空集（孤儿 build-dep 不编）
        let mut plan2 = buildrs_plan();
        plan2.units[1].has_build_script = false;
        plan2.units[2].has_build_script = false;
        assert!(build_closure(&plan2, false).is_empty());
        // target 集不吃 Build 边：b 在，bdep/cc0 不在
        assert_eq!(target_units(&plan), BTreeSet::from([2]));
    }

    #[test]
    fn build_edges_stay_out_of_code_compiles_but_feed_build_script() {
        let plan = buildrs_plan();
        let fps = fingerprints(&plan, &ProfileFlags::default(), "s", &[]).unwrap();
        let lo = layout();
        // b 的 target 编译：--extern 不吃 Build 边（bdep 不出现）
        let a = dep_rustc_args(
            &plan,
            2,
            &ProfileFlags::default(),
            &fps,
            Path::new("/sys"),
            &lo,
            None,
            &[],
            &[],
        );
        assert!(
            !a.windows(2)
                .any(|w| w[0] == "--extern" && w[1].starts_with("bdep=")),
            "Build 边漏进 lib 参数: {a:?}"
        );
        // b 的 build script 编译：--extern 只吃 Build 边（bdep → host-deps rlib）
        let bs = build_script_rustc_args(&plan, 2, &ProfileFlags::default(), &fps, &lo);
        assert!(bs[0].ends_with("bin/rustc"), "argv0 = 真 rustc: {}", bs[0]);
        assert!(
            bs.windows(2)
                .any(|w| w[0] == "--crate-name" && w[1] == "build_script_build")
        );
        assert!(bs.iter().any(|x| x == "--crate-type=bin"));
        assert!(bs.iter().any(|x| x == "--emit=dep-info,link"));
        assert!(
            bs.windows(2)
                .any(|w| w[0] == "--check-cfg" && w[1] == "cfg(docsrs,test)")
        );
        assert!(
            bs.windows(2)
                .any(|w| w[0] == "--check-cfg" && w[1].starts_with("cfg(feature, values(")),
            "缺 feature 值表 check-cfg: {bs:?}"
        );
        let want_out = format!("/tmp/cless/build/b-{}", fps[2]);
        assert!(
            bs.windows(2)
                .any(|w| w[0] == "--out-dir" && w[1] == want_out),
            "build script --out-dir 形态: {bs:?}"
        );
        let ext = format!("bdep=/tmp/cless/host-deps/libbdep-{}.rlib", fps[1]);
        assert!(
            bs.windows(2).any(|w| w[0] == "--extern" && w[1] == ext),
            "build script 缺 Build 边 --extern: {bs:?}"
        );
        // registry 单元 cap-lints（E2 形态）；默认 build.rs 路径
        assert!(
            bs.windows(2)
                .any(|w| w[0] == "--cap-lints" && w[1] == "allow")
        );
        assert!(bs.iter().any(|x| x == "/tmp/b/build.rs"));
        // bin 会话同样不吃 Build 边
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
            "demo",
            Path::new("/tmp/demo/src/main.rs"),
            None,
            &[],
            &[],
            None,
        );
        assert!(
            !a.windows(2)
                .any(|w| w[0] == "--extern" && w[1].starts_with("bdep=")),
            "Build 边漏进 bin 参数: {a:?}"
        );
    }

    #[test]
    fn build_output_flags_land_on_own_compile_only() {
        let plan = buildrs_plan();
        let fps = fingerprints(&plan, &ProfileFlags::default(), "s", &[]).unwrap();
        let lo = layout();
        let bo = BuildOutput {
            cfgs: vec!["bdep_feat".into()],
            check_cfgs: vec!["cfg(bdep_feat)".into()],
            link_libs: vec!["static=probehelper".into()],
            link_searches: vec!["native=/opt/probe/lib".into()],
            link_args: vec!["-Wl,--x".into()],
            ..Default::default()
        };
        let searches = vec!["native=/opt/transitive".to_string()];
        let a = dep_rustc_args(
            &plan,
            2,
            &ProfileFlags::default(),
            &fps,
            Path::new("/sys"),
            &lo,
            Some(&bo),
            &searches,
            &[],
        );
        // 本包 bo：-L 自身 + -l + link-arg + --cfg + --check-cfg + 汇集 -L
        assert!(
            a.windows(2)
                .any(|w| w[0] == "-L" && w[1] == "native=/opt/probe/lib")
        );
        assert!(
            a.windows(2)
                .any(|w| w[0] == "-l" && w[1] == "static=probehelper")
        );
        assert!(
            a.windows(2)
                .any(|w| w[0] == "-C" && w[1] == "link-arg=-Wl,--x")
        );
        assert!(a.windows(2).any(|w| w[0] == "--cfg" && w[1] == "bdep_feat"));
        assert!(
            a.windows(2)
                .any(|w| w[0] == "--check-cfg" && w[1] == "cfg(bdep_feat)")
        );
        assert!(
            a.windows(2)
                .any(|w| w[0] == "-L" && w[1] == "native=/opt/transitive")
        );
        // 无 bo 时这些旗一律不在（传播面只经显式参数）
        let a0 = dep_rustc_args(
            &plan,
            2,
            &ProfileFlags::default(),
            &fps,
            Path::new("/sys"),
            &lo,
            None,
            &[],
            &[],
        );
        assert!(!a0.iter().any(|x| x == "static=probehelper"));
        assert!(
            !a0.windows(2)
                .any(|w| w[0] == "--cfg" && w[1] == "bdep_feat")
        );
    }

    /// rustflags（D15 P3 切⑤a）：逐条按序进全部 unit fp；target 侧参数
    /// 末尾追加（dep 在 -Z 旗之后、bin 在 --sysroot 之后）。host 侧三个
    /// 参数函数签名不含 rustflags——不吃旗由编译期保证，无需断言。
    #[test]
    fn rustflags_enter_fingerprint_and_target_args_tail() {
        let plan = diamond_plan();
        let rf = vec!["--cap-lints".to_string(), "allow".to_string()];
        let fps0 = fingerprints(&plan, &ProfileFlags::default(), "s", &[]).unwrap();
        let fps1 = fingerprints(&plan, &ProfileFlags::default(), "s", &rf).unwrap();
        assert_ne!(fps0[0], fps1[0], "rustflags 进 unit fp");
        assert_ne!(fps0[1], fps1[1], "rustflags 进 unit fp（传递侧同样变）");
        // 顺序有语义：旗序不同 fp 不同（后旗压前旗，不是集合）
        let rf_rev = vec!["allow".to_string(), "--cap-lints".to_string()];
        let fps2 = fingerprints(&plan, &ProfileFlags::default(), "s", &rf_rev).unwrap();
        assert_ne!(fps1[0], fps2[0], "rustflags 按序进 fp（不排序）");
        // dep：rustflags 在 -Z 旗之后（参数串末尾）
        let lo = layout();
        let a = dep_rustc_args(
            &plan,
            1,
            &ProfileFlags::default(),
            &fps1,
            Path::new("/sys"),
            &lo,
            None,
            &[],
            &rf,
        );
        let zpos = a.iter().rposition(|x| x.starts_with("-Z")).unwrap();
        // registry 单元内置已有一枚 --cap-lints（与 RUSTFLAGS 并存，实证 serde
        // 行同款两枚形态）——rustflags 段的断言必须取**最后一枚**
        let rfpos = a.iter().rposition(|x| x == "--cap-lints").unwrap();
        assert!(rfpos > zpos, "rustflags 必须在 -Z 旗之后: {a:?}");
        assert_eq!(&a[a.len() - 2..], &["--cap-lints", "allow"], "末尾追加");
        // bin：rustflags 在 --sysroot 之后（参数串末尾）
        let manifest = PackageManifest::parse(
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
             [dependencies]\nb = \"1\"\n",
            Path::new("/tmp/demo"),
        )
        .unwrap();
        let a = bin_rustc_args(
            &manifest,
            &plan,
            &fps1,
            Path::new("/sys"),
            &lo,
            "demo",
            Path::new("/tmp/demo/src/main.rs"),
            None,
            &[],
            &rf,
            None,
        );
        let syspos = a.iter().rposition(|x| x == "--sysroot").unwrap();
        let rfpos = a.iter().rposition(|x| x == "--cap-lints").unwrap();
        assert!(rfpos > syspos, "rustflags 必须在 --sysroot 之后: {a:?}");
        assert_eq!(&a[a.len() - 2..], &["--cap-lints", "allow"], "末尾追加");
        // 空 rustflags：参数串与此前形态一字不差（无旗路径零漂移）
        let a_empty = bin_rustc_args(
            &manifest,
            &plan,
            &fps0,
            Path::new("/sys"),
            &lo,
            "demo",
            Path::new("/tmp/demo/src/main.rs"),
            None,
            &[],
            &[],
            None,
        );
        assert!(a_empty.ends_with(&["--sysroot".into(), "/sys".into()]));
    }

    /// 根包 lib target（切⑤a full 层迁移面，hexyl 实锤）：root_lib 参数
    /// 形态 = dep 参数作用于根（__cless-dep/-Z/-C metadata 窗口/.rmeta
    /// --extern），差异钉 = 无 --cap-lints（path 包）、feature 值表 check-cfg
    /// 在场、rustflags 末尾追加；root_fingerprint 随 rustflags 变；bin 会话
    /// 补根 lib --extern 指 .rlib。
    #[test]
    fn root_lib_args_shape_and_bin_extern() {
        let plan = diamond_plan();
        let rf = vec!["--cap-lints".to_string(), "allow".to_string()];
        let fps = fingerprints(&plan, &ProfileFlags::default(), "s", &rf).unwrap();
        let lo = layout();
        // root_fingerprint 对根源目录盖戳（source_stamp_dir）——目录必须实存
        let root = std::env::temp_dir().join(format!(
            "mirvm-cargoless-schedule-test-rootlib-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "").unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main(){}").unwrap();
        let manifest = PackageManifest::parse(
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
             [lib]\nname = \"demo\"\npath = \"src/lib.rs\"\n\
             [dependencies]\nb = \"1\"\n",
            &root,
        )
        .unwrap();
        let rfp0 =
            root_fingerprint(&manifest, &plan, &fps, &ProfileFlags::default(), "s", &[]).unwrap();
        let rfp1 =
            root_fingerprint(&manifest, &plan, &fps, &ProfileFlags::default(), "s", &rf).unwrap();
        assert_ne!(rfp0, rfp1, "rustflags 进 root_fingerprint");
        let a = root_lib_rustc_args(
            &manifest,
            &plan,
            &fps,
            Path::new("/sys"),
            &lo,
            "demo",
            &root.join("src/lib.rs"),
            None,
            &[],
            &rf,
            &rfp1,
        );
        assert_eq!(a[0], "mirvm-cless-rustc");
        assert!(
            a.windows(2)
                .any(|w| w[0] == "--crate-name" && w[1] == "demo")
        );
        let want_src = root.join("src/lib.rs").display().to_string();
        assert!(a.iter().any(|x| x == &want_src));
        assert!(a.iter().any(|x| x == "--crate-type=lib"));
        assert!(a.iter().any(|x| x == "-Zno-codegen"));
        // extra-filename 窗口（run_dep_compiler 抠取形态）
        let want = format!("extra-filename=-{rfp1}");
        assert!(a.windows(2).any(|w| w[0] == "-C" && w[1] == want));
        // feature 值表 check-cfg 在场（bin 同款全集口径）；path 包无 --cap-lints
        // 内置——唯一一枚 --cap-lints 是末尾的 rustflags
        assert!(
            a.windows(2)
                .any(|w| w[0] == "--check-cfg" && w[1].starts_with("cfg(feature, values(")),
            "缺 feature 值表 check-cfg: {a:?}"
        );
        assert_eq!(
            &a[a.len() - 2..],
            &["--cap-lints", "allow"],
            "rustflags 末尾"
        );
        // --extern 吃 root_deps Normal 边指 .rmeta
        let ext = format!("b=/tmp/cless/deps/libb-{}.rmeta", fps[1]);
        assert!(a.windows(2).any(|w| w[0] == "--extern" && w[1] == ext));
        // bin 会话补根 lib --extern（指 .rlib，与根边混排同段）
        let a = bin_rustc_args(
            &manifest,
            &plan,
            &fps,
            Path::new("/sys"),
            &lo,
            "demo",
            Path::new("/tmp/demo/src/main.rs"),
            None,
            &[],
            &rf,
            Some(("demo", &rfp1)),
        );
        let want_ext = format!("demo=/tmp/cless/deps/libdemo-{rfp1}.rlib");
        assert!(
            a.windows(2).any(|w| w[0] == "--extern" && w[1] == want_ext),
            "bin 缺根 lib --extern: {a:?}"
        );
        // 根包目录 remap 在场（file!()/panic Location 相对形 = cargo 的
        // cwd=包根相对调用语义，redb_kv/gix_pure 实锤）
        let want_remap = format!("--remap-path-prefix={}/=", root.display());
        assert!(
            a.iter().any(|x| x == &want_remap),
            "bin 缺根包目录 remap: {a:?}"
        );
        // 无根 lib 时 extern 缺席（老路径零漂移）
        let a0 = bin_rustc_args(
            &manifest,
            &plan,
            &fps,
            Path::new("/sys"),
            &lo,
            "demo",
            Path::new("/tmp/demo/src/main.rs"),
            None,
            &[],
            &rf,
            None,
        );
        assert!(
            !a0.windows(2)
                .any(|w| w[0] == "--extern" && w[1].starts_with("demo=")),
            "无根 lib 时不得有根 lib --extern: {a0:?}"
        );
    }
}
