//! Trap 债务统计（调研仪器，纯 IR 分析）：M4 各期开工前用它拿真实债务表，
//! 防止凭直觉排工。`mirvm run --engine vm --vm-stats <file.rs>`。

use std::collections::{HashMap, HashSet, VecDeque};

use super::ir::{Module, Terminator};

/// 聚合键：诊断串截断（Debug 载荷会让原因逐条唯一，按前缀归类）。
fn group_key(reason: &str) -> String {
    let cut = reason.char_indices().nth(40).map(|(i, _)| i).unwrap_or(reason.len());
    reason[..cut].to_string()
}

pub fn report(module: &Module) -> String {
    let mut out = String::new();
    let total = module.funcs.len();
    let mut funcs_with_trap = 0usize;
    let mut histogram: HashMap<String, usize> = HashMap::new();

    for f in &module.funcs {
        let mut has = false;
        for b in &f.blocks {
            if let Terminator::Trap(r) = &b.term {
                has = true;
                *histogram.entry(group_key(r)).or_default() += 1;
            }
        }
        if has {
            funcs_with_trap += 1;
        }
    }

    out.push_str(&format!(
        "instance 总数 {total}，含 Trap 的 {funcs_with_trap}（{:.1}%）\n",
        funcs_with_trap as f64 / total.max(1) as f64 * 100.0
    ));

    let mut hist: Vec<(String, usize)> = histogram.into_iter().collect();
    hist.sort_by(|a, b| b.1.cmp(&a.1));
    out.push_str("\n== Trap 原因 TOP20（按块计数）==\n");
    for (reason, n) in hist.iter().take(20) {
        out.push_str(&format!("{n:6}  {reason}\n"));
    }

    // 裸名导出（no_mangle）：从它出发的可达 Trap 分析
    out.push_str("\n== 各导出函数的可达 Trap（BFS 经 Call 边）==\n");
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
        let mut reasons: Vec<String> = Vec::new();
        let mut reason_seen: HashSet<String> = HashSet::new();
        while let Some(fid) = queue.pop_front() {
            if !seen.insert(fid) {
                continue;
            }
            let f = &module.funcs[fid as usize];
            for b in &f.blocks {
                match &b.term {
                    Terminator::Trap(r) => {
                        let k = group_key(r);
                        if reason_seen.insert(k.clone()) {
                            reasons.push(format!("{k}  [{}]", f.name));
                        }
                    }
                    Terminator::Call { callee, .. } => queue.push_back(*callee),
                    _ => {}
                }
            }
        }
        if reasons.is_empty() {
            out.push_str(&format!("  {name}: ✅ 可达路径 trap-free（{} fn 可达）\n", seen.len()));
        } else {
            out.push_str(&format!(
                "  {name}: {} 类可达 Trap（{} fn 可达），前 6：\n",
                reasons.len(),
                seen.len()
            ));
            for r in reasons.iter().take(6) {
                out.push_str(&format!("      - {r}\n"));
            }
        }
    }
    out
}
