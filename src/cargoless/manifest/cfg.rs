use super::{MErr, unsupported};

// ---------- cfg expressions (parse / validate / host evaluation) ----------

/// A cfg expression AST.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CfgExpr {
    /// (key, value); a bare atom (unix/windows) has an empty value.
    Atom(String, String),
    Any(Vec<CfgExpr>),
    All(Vec<CfgExpr>),
    Not(Box<CfgExpr>),
}

/// Parse `cfg(...)` into an AST (either the `cfg(...)` wrapper or a bare expression).
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
                .ok_or_else(|| format!("unbalanced parentheses in cfg expression: {s}"))?;
            let parts = split_top_level(rest)?;
            let exprs = parts
                .iter()
                .map(|p| parse_cfg_inner(p.trim()))
                .collect::<Result<Vec<_>, _>>()?;
            if op == "not(" && exprs.len() != 1 {
                return Err(format!("cfg not() takes exactly one argument: {s}"));
            }
            return Ok(make(exprs));
        }
    }
    let (key, val) = match s.split_once('=') {
        Some((k, v)) => (k.trim().to_string(), v.trim().trim_matches('"').to_string()),
        None => (s.trim().to_string(), String::new()),
    };
    if key.is_empty() {
        return Err(format!("invalid cfg atom form: {s}"));
    }
    Ok(CfgExpr::Atom(key, val))
}

/// Parse-time validation (a typo must be loud; `cfg(feature=..)` in a target
/// dependency table is a surface cargo forbids, so it is rejected loudly by name).
pub(super) fn validate_cfg_expr(expr: &str) -> Result<(), MErr> {
    fn walk(e: &CfgExpr) -> Result<(), MErr> {
        match e {
            CfgExpr::Atom(k, _) if k == "feature" => Err(unsupported(
                "cfg(feature=..) in a target dependency table (a surface cargo forbids)",
            )),
            CfgExpr::Atom(_, _) => Ok(()),
            CfgExpr::Any(vs) | CfgExpr::All(vs) => vs.iter().try_for_each(walk),
            CfgExpr::Not(e) => walk(e),
        }
    }
    walk(&parse_cfg(expr)?)
}

/// Host platform atom set = the verbatim line set of
/// `rustc --print cfg --target <host>` (the same source cargo matches platforms
/// against, so an unknown/custom key such as rustix_use_libc naturally evaluates to
/// false, and target_feature is covered exactly by rustc's list). Cached in-process
/// with a OnceLock.
static HOST_CFG_ATOMS: std::sync::OnceLock<std::collections::BTreeSet<String>> =
    std::sync::OnceLock::new();

/// The CARGO_CFG_* mapping exposed to buildrs.rs.
pub(crate) fn host_cfg_atoms() -> &'static std::collections::BTreeSet<String> {
    HOST_CFG_ATOMS.get_or_init(|| {
        let rustc = std::path::PathBuf::from(env!("MIRVM_DEFAULT_SYSROOT")).join("bin/rustc");
        let out = std::process::Command::new(rustc)
            .args(["--print", "cfg", "--target", env!("MIRVM_HOST")])
            .output()
            .expect("rustc --print cfg failed");
        let text = String::from_utf8(out.stdout).expect("rustc --print cfg output is not UTF-8");
        text.lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect()
    })
}

/// Host evaluation (used when the host build graph is assembled; must not be called
/// during version resolution -- that is the union over all platforms).
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
                    return Err(format!("unbalanced parentheses in cfg expression: {s}"));
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
        return Err(format!("unbalanced parentheses in cfg expression: {s}"));
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    Ok(out)
}
