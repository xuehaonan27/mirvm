//! Trap 债务统计（调研仪器，纯 IR 分析）：M4 各期开工前用它拿真实债务表，
//! 防止凭直觉排工。`mirvm run --engine vm --vm-stats <file.rs>`。
//!
//! 可达分析盲点修复（与 lower 配套）：语句失败 = `Stmt::Trap` 占位、终止子照常降低
//! （Call 边保住）；非标量参函数 = 入口 Trap 语句、体照常降低——BFS 现在看得到全下游。
//! 残余盲点：间接调用（fn ptr）与整函数 trap_body（layout 失败/intrinsic）仍无出边。

use std::collections::{HashMap, HashSet, VecDeque};

use super::ir::{Module, Stmt, Terminator};

/// 聚合键：诊断串截断（Debug 载荷会让原因逐条唯一，按前缀归类）。
fn group_key(reason: &str) -> String {
    let cut = reason
        .char_indices()
        .nth(40)
        .map(|(i, _)| i)
        .unwrap_or(reason.len());
    reason[..cut].to_string()
}

/// 分期归属（按诊断串标签）。
fn phase_of(reason: &str) -> &'static str {
    if reason.starts_with("foreign") {
        "foreign(os::种子)"
    } else if reason.contains("M4.1+") {
        "M4.1+"
    } else if reason.contains("M4.1") {
        "M4.1"
    } else if reason.contains("M4.2") {
        "M4.2"
    } else if reason.contains("M4.3") {
        "M4.3"
    } else if reason.contains("M4.4") {
        "M4.4"
    } else if reason.contains("M4.x") {
        "M4.x"
    } else {
        "未标注"
    }
}

/// 遍历一个函数体的全部 Trap 原因（语句级 + 终止子级）。
fn traps_of(f: &super::ir::FuncBody) -> impl Iterator<Item = &str> {
    f.blocks.iter().flat_map(|b| {
        let stmt_traps = b.stmts.iter().filter_map(|s| match s {
            Stmt::Trap(r) => Some(&**r),
            _ => None,
        });
        let term_trap = match &b.term {
            Terminator::Trap(r) => Some(&**r),
            _ => None,
        };
        stmt_traps.chain(term_trap)
    })
}

pub fn report(module: &Module) -> String {
    let mut out = String::new();
    let total = module.funcs.len();
    let mut funcs_with_trap = 0usize;
    let mut histogram: HashMap<String, usize> = HashMap::new();
    let mut phases: HashMap<&'static str, usize> = HashMap::new();

    for f in &module.funcs {
        let mut has = false;
        for r in traps_of(f) {
            has = true;
            *histogram.entry(group_key(r)).or_default() += 1;
            *phases.entry(phase_of(r)).or_default() += 1;
        }
        if has {
            funcs_with_trap += 1;
        }
    }

    out.push_str(&format!(
        "instance 总数 {total}，含 Trap 的 {funcs_with_trap}（{:.1}%）\n",
        funcs_with_trap as f64 / total.max(1) as f64 * 100.0
    ));

    out.push_str("\n== 分期债务余额（Trap 计数）==\n");
    let mut ph: Vec<(&str, usize)> = phases.into_iter().collect();
    ph.sort_by_key(|a| std::cmp::Reverse(a.1));
    for (p, n) in &ph {
        out.push_str(&format!("{n:6}  {p}\n"));
    }

    let mut hist: Vec<(String, usize)> = histogram.into_iter().collect();
    hist.sort_by_key(|a| std::cmp::Reverse(a.1));
    out.push_str("\n== Trap 原因 TOP25 ==\n");
    for (reason, n) in hist.iter().take(25) {
        out.push_str(&format!("{n:6}  {reason}\n"));
    }

    // foreign 全清单（os:: 注册表种子，M4.3 开工调研的直接输入；不截断、不进 TOP 竞争）
    let mut fm: HashMap<&str, usize> = HashMap::new();
    for f in &module.funcs {
        for r in traps_of(f) {
            if r.starts_with("foreign") {
                *fm.entry(r).or_default() += 1;
            }
        }
    }
    if !fm.is_empty() {
        let mut foreigns: Vec<(&str, usize)> = fm.into_iter().collect();
        foreigns.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        out.push_str("\n== foreign 符号全清单（os:: 种子）==\n");
        for (reason, n) in &foreigns {
            out.push_str(&format!("{n:6}  {reason}\n"));
        }
    }

    // 裸名导出（no_mangle / @entry）：可达 Trap 分析（BFS 经 Call 边）
    out.push_str("\n== 各导出/入口的可达 Trap（BFS 经 Call 边）==\n");
    let mut exports: Vec<(&str, u32)> = module
        .exports
        .iter()
        .filter(|(name, _)| !name.starts_with("_ZN") && !name.starts_with("_R"))
        .map(|(n, id)| (&**n, *id))
        .collect();
    exports.sort();
    for (name, id) in exports {
        let mut seen: HashSet<u32> = HashSet::new();
        let mut queue = VecDeque::from([id]);
        let mut reason_hist: HashMap<String, usize> = HashMap::new();
        let mut reach_phases: HashMap<&'static str, usize> = HashMap::new();
        while let Some(fid) = queue.pop_front() {
            if !seen.insert(fid) {
                continue;
            }
            let f = &module.funcs[fid as usize];
            for r in traps_of(f) {
                *reason_hist.entry(group_key(r)).or_default() += 1;
                *reach_phases.entry(phase_of(r)).or_default() += 1;
            }
            for b in &f.blocks {
                if let Terminator::Call { callee, .. } = &b.term {
                    queue.push_back(*callee);
                }
            }
        }
        if reason_hist.is_empty() {
            out.push_str(&format!(
                "  {name}: ✅ 可达路径 trap-free（{} fn 可达）\n",
                seen.len()
            ));
        } else {
            let mut ph: Vec<(&str, usize)> = reach_phases.into_iter().collect();
            ph.sort_by_key(|a| std::cmp::Reverse(a.1));
            let ph_str: Vec<String> = ph.iter().map(|(p, n)| format!("{p}:{n}")).collect();
            out.push_str(&format!(
                "  {name}: {} 类可达 Trap，{} fn 可达 | {}\n",
                reason_hist.len(),
                seen.len(),
                ph_str.join(" ")
            ));
            let mut rs: Vec<(String, usize)> = reason_hist.into_iter().collect();
            rs.sort_by_key(|a| std::cmp::Reverse(a.1));
            for (r, n) in rs.iter().take(6) {
                out.push_str(&format!("      {n:5}  {r}\n"));
            }
        }
    }
    out
}
