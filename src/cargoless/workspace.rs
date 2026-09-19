//! Cargo workspace discovery, member expansion and inheritance materialization.
//!
//! This layer only reduces workspace semantics to a set of complete `PackageManifest`s;
//! version resolution and rustc scheduling reuse the existing paths, so `test` and `run`
//! do not each re-interpret Cargo.toml.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::manifest::{PackageManifest, ResolverVersion, current_rust_version};

#[derive(Clone, Debug)]
pub struct WorkspaceManifest {
    pub root: PathBuf,
    pub members: Vec<PackageManifest>,
    pub default_members: BTreeSet<PathBuf>,
    pub current_member: Option<PathBuf>,
}

impl WorkspaceManifest {
    pub fn read(input: &Path) -> Result<Self, String> {
        Self::read_inner(input)
    }

    pub(crate) fn read_dependency(input: &Path) -> Result<Self, String> {
        Self::read_inner(input)
    }

    fn read_inner(input: &Path) -> Result<Self, String> {
        let input = std::fs::canonicalize(input)
            .or_else(|_| std::path::absolute(input))
            .map_err(|e| {
                format!(
                    "failed to make project directory absolute {}: {e}",
                    input.display()
                )
            })?;
        let start = if input.file_name().is_some_and(|name| name == "Cargo.toml") {
            input.parent().unwrap_or(Path::new(".")).to_path_buf()
        } else {
            input
        };
        let root = find_workspace_root(&start)?.unwrap_or_else(|| start.clone());
        let root_text = std::fs::read_to_string(root.join("Cargo.toml"))
            .map_err(|e| format!("failed to read {}/Cargo.toml: {e}", root.display()))?;
        let root_value: toml::Value = toml::from_str(&root_text)
            .map_err(|e| format!("failed to parse {}/Cargo.toml: {e}", root.display()))?;
        let Some(workspace) = root_value.get("workspace").and_then(toml::Value::as_table) else {
            let package = PackageManifest::parse(&root_text, &root)?;
            return Ok(Self {
                root: root.clone(),
                default_members: BTreeSet::from([root.clone()]),
                current_member: Some(root.clone()),
                members: vec![package],
            });
        };
        let virtual_root = root_value.get("package").is_none();
        let excludes = string_array(workspace.get("exclude"), "workspace.exclude")?;
        let exclude_dirs = expand_patterns(&root, &excludes, false, "workspace.exclude")?;
        if let Some(excluded) = exclude_dirs
            .iter()
            .filter(|dir| start.starts_with(dir))
            .max_by_key(|dir| dir.components().count())
        {
            let package = PackageManifest::read_dir(excluded)?;
            return Ok(Self {
                root: excluded.clone(),
                default_members: BTreeSet::from([excluded.clone()]),
                current_member: Some(excluded.clone()),
                members: vec![package],
            });
        }
        let member_patterns = string_array(workspace.get("members"), "workspace.members")?;
        let resolver = workspace
            .get("resolver")
            .and_then(toml::Value::as_str)
            .map(str::to_string)
            .or_else(|| inferred_package_resolver(&root_value));
        let resolver = match resolver.as_deref() {
            Some(value) => ResolverVersion::parse(value)?,
            None => ResolverVersion::V1,
        };
        let mut member_dirs = expand_patterns(&root, &member_patterns, true, "workspace.members")?;
        if !virtual_root {
            member_dirs.insert(root.clone());
        }
        member_dirs.retain(|dir| !exclude_dirs.contains(dir));
        if member_dirs.is_empty() && !virtual_root {
            member_dirs.insert(root.clone());
        }
        if member_dirs.is_empty() {
            return Err("workspace has no usable members".into());
        }

        // Cargo automatically makes path dependencies inside the workspace root members;
        // `exclude` blocks them explicitly. Scanning the materialized dependency tables also
        // covers paths inherited through workspace.dependencies.
        loop {
            let mut discovered = BTreeSet::new();
            for dir in &member_dirs {
                let text = std::fs::read_to_string(dir.join("Cargo.toml"))
                    .map_err(|e| format!("failed to read {}/Cargo.toml: {e}", dir.display()))?;
                let value: toml::Value = toml::from_str(&text)
                    .map_err(|e| format!("failed to parse {}/Cargo.toml: {e}", dir.display()))?;
                if dir != &root && value.get("workspace").is_some() {
                    return Err(format!(
                        "workspace member {} declares [workspace] itself; nested workspaces are \
                         not implemented",
                        dir.display()
                    ));
                }
                let materialized = materialize_member(value, &root_value, &root)?;
                for path in dependency_paths(&materialized, dir) {
                    if path.starts_with(&root)
                        && path.join("Cargo.toml").is_file()
                        && !exclude_dirs.contains(&path)
                        && !member_dirs.contains(&path)
                    {
                        discovered.insert(path);
                    }
                }
            }
            if discovered.is_empty() {
                break;
            }
            member_dirs.extend(discovered);
        }

        let mut members = Vec::new();
        for dir in &member_dirs {
            let text = std::fs::read_to_string(dir.join("Cargo.toml"))
                .map_err(|e| format!("failed to read {}/Cargo.toml: {e}", dir.display()))?;
            let value: toml::Value = toml::from_str(&text)
                .map_err(|e| format!("failed to parse {}/Cargo.toml: {e}", dir.display()))?;
            if dir != &root && value.get("workspace").is_some() {
                return Err(format!(
                    "workspace member {} declares [workspace] itself; nested workspaces are \
                     not implemented",
                    dir.display()
                ));
            }
            let materialized = materialize_member(value, &root_value, &root)?;
            let encoded = toml::to_string(&materialized).map_err(|e| {
                format!(
                    "failed to materialize workspace member {}: {e}",
                    dir.display()
                )
            })?;
            let mut package = PackageManifest::parse(&encoded, dir)?;
            // Cargo uses only the top-level workspace resolver; a member's own resolver is ignored.
            package.resolver = resolver;
            package.lock_root = root.clone();
            members.push(package);
        }
        members.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.root.cmp(&b.root)));
        if virtual_root
            && workspace.get("resolver").is_none()
            && let Some((edition, implied_resolver)) = members
                .iter()
                .filter_map(|member| match member.edition.as_str() {
                    "2021" => Some(("2021", "2")),
                    "2024" => Some(("2024", "3")),
                    _ => None,
                })
                .max_by_key(|(_, implied)| *implied)
        {
            eprintln!(
                "warning: virtual workspace defaulting to `resolver = \"1\"` despite one or more workspace members being on edition {edition} which implies `resolver = \"{implied_resolver}\"`\n  |\n  = note: to keep the current resolver, specify `workspace.resolver = \"1\"` in the workspace root's manifest\n  = note: to use the edition {edition} resolver, specify `workspace.resolver = \"{implied_resolver}\"` in the workspace root's manifest\n  = note: for more details see https://doc.rust-lang.org/cargo/reference/resolver.html#resolver-versions"
            );
        }
        if let Some(pair) = members.windows(2).find(|pair| pair[0].name == pair[1].name) {
            return Err(format!(
                "workspace has two packages named `{}` ({} and {}); package selection cannot \
                 disambiguate them reliably",
                pair[0].name,
                pair[0].root.display(),
                pair[1].root.display()
            ));
        }
        // Both resolver 3's fallback ordering and the new lock's format choice use the lowest
        // MSRV of the whole workspace. A member without rust-version counts as the current rustc,
        // matching Cargo's mixed-MSRV workspace heuristic.
        let current_rust = current_rust_version()?;
        let workspace_rust = members
            .iter()
            .map(|member| {
                member
                    .rust_version
                    .clone()
                    .unwrap_or_else(|| current_rust.clone())
            })
            .min()
            .unwrap_or_else(|| current_rust.clone());
        for member in &mut members {
            member.resolver_rust_version = Some(workspace_rust.clone());
        }

        let defaults = string_array(
            workspace.get("default-members"),
            "workspace.default-members",
        )?;
        let default_members = if defaults.is_empty() {
            if virtual_root {
                member_dirs.clone()
            } else {
                BTreeSet::from([root.clone()])
            }
        } else {
            let dirs = expand_patterns(&root, &defaults, true, "workspace.default-members")?;
            if let Some(outside) = dirs.iter().find(|dir| !member_dirs.contains(*dir)) {
                return Err(format!(
                    "workspace.default-members {} is not a workspace member",
                    outside.display()
                ));
            }
            dirs
        };
        let current_member = members
            .iter()
            .filter(|member| start.starts_with(&member.root))
            .max_by_key(|member| member.root.components().count())
            .map(|member| member.root.clone());
        if current_member.is_none() {
            for dir in start.ancestors().take_while(|dir| *dir != root) {
                let manifest = dir.join("Cargo.toml");
                if !manifest.is_file() {
                    continue;
                }
                let text = std::fs::read_to_string(&manifest)
                    .map_err(|e| format!("failed to read {}: {e}", manifest.display()))?;
                let value: toml::Value = toml::from_str(&text)
                    .map_err(|e| format!("failed to parse {}: {e}", manifest.display()))?;
                if value.get("package").is_some() {
                    return Err(format!(
                        "package {} is inside the workspace but is neither a member nor excluded; \
                         Cargo rejects this shape",
                        dir.display()
                    ));
                }
            }
        }

        Ok(Self {
            root,
            members,
            default_members,
            current_member,
        })
    }

    /// The workspace subset of Cargo package ID specs: bare name, `name@version`, and
    /// `path+file:///...#name@version`. Like Cargo, multiple matches require disambiguation.
    pub fn member_by_spec(&self, spec: &str) -> Result<&PackageManifest, String> {
        let parsed = PackageSpec::parse(spec)?;
        let matches: Vec<_> = self
            .members
            .iter()
            .filter(|member| parsed.matches(member))
            .collect();
        match matches.as_slice() {
            [member] => Ok(*member),
            [] => Err(format!(
                "no workspace package matches package spec `{spec}`"
            )),
            _ => Err(format!(
                "package spec `{spec}` matches multiple packages; add a version or a full path package ID"
            )),
        }
    }
}

struct PackageSpec {
    name: Option<String>,
    version: Option<semver::VersionReq>,
    path: Option<PathBuf>,
}

impl PackageSpec {
    fn parse(spec: &str) -> Result<Self, String> {
        let (source, fragment) = spec
            .split_once('#')
            .map_or((None, spec), |(source, fragment)| (Some(source), fragment));
        let (name, version) = match fragment.rsplit_once('@') {
            Some((name, version)) if !name.is_empty() && !version.is_empty() => {
                let requirement =
                    semver::VersionReq::parse(&format!("={version}")).map_err(|error| {
                        format!("invalid version in package spec `{spec}`: {error}")
                    })?;
                (Some(name.to_string()), Some(requirement))
            }
            _ if !fragment.is_empty() => (Some(fragment.to_string()), None),
            _ => (None, None),
        };
        let path = source
            .map(|source| {
                let encoded = source
                    .strip_prefix("path+file://")
                    .or_else(|| source.strip_prefix("file://"))
                    .ok_or_else(|| {
                        format!("workspace package spec `{spec}` source is not path+file")
                    })?;
                percent_decode(encoded).map(PathBuf::from)
            })
            .transpose()?;
        Ok(Self {
            name,
            version,
            path,
        })
    }

    fn matches(&self, member: &PackageManifest) -> bool {
        self.name.as_ref().is_none_or(|name| name == &member.name)
            && self
                .version
                .as_ref()
                .is_none_or(|version| version.matches(&member.version))
            && self.path.as_ref().is_none_or(|path| {
                std::fs::canonicalize(path)
                    .or_else(|_| std::path::absolute(path))
                    .is_ok_and(|path| path == member.root)
            })
    }
}

fn percent_decode(input: &str) -> Result<String, String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' {
            let hex = bytes
                .get(at + 1..at + 3)
                .ok_or_else(|| format!("incomplete percent escape in file URL: `{input}`"))?;
            let text = std::str::from_utf8(hex).map_err(|error| error.to_string())?;
            out.push(
                u8::from_str_radix(text, 16)
                    .map_err(|_| format!("invalid percent escape in file URL: `%{text}`"))?,
            );
            at += 3;
        } else {
            out.push(bytes[at]);
            at += 1;
        }
    }
    String::from_utf8(out).map_err(|error| format!("file URL is not UTF-8: {error}"))
}

fn inferred_package_resolver(root: &toml::Value) -> Option<String> {
    let package = root.get("package")?.as_table()?;
    let edition = match package.get("edition")? {
        toml::Value::String(edition) => edition.as_str(),
        toml::Value::Table(table)
            if table
                .get("workspace")
                .and_then(toml::Value::as_bool)
                .unwrap_or(false) =>
        {
            root.get("workspace")?
                .get("package")?
                .get("edition")?
                .as_str()?
        }
        _ => return None,
    };
    match edition {
        "2021" => Some("2".to_string()),
        "2024" => Some("3".to_string()),
        _ => None,
    }
}

fn find_workspace_root(start: &Path) -> Result<Option<PathBuf>, String> {
    for dir in start.ancestors() {
        let file = dir.join("Cargo.toml");
        if !file.is_file() {
            continue;
        }
        let text = std::fs::read_to_string(&file)
            .map_err(|e| format!("failed to read {}: {e}", file.display()))?;
        let value: toml::Value = toml::from_str(&text)
            .map_err(|e| format!("failed to parse {}: {e}", file.display()))?;
        if value.get("workspace").is_some() {
            return Ok(Some(dir.to_path_buf()));
        }
    }
    Ok(None)
}

fn string_array(value: Option<&toml::Value>, field: &str) -> Result<Vec<String>, String> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    value
        .as_array()
        .ok_or_else(|| format!("{field} must be an array of strings"))?
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("{field} contains a non-string member"))
        })
        .collect()
}

fn expand_patterns(
    root: &Path,
    patterns: &[String],
    require_match: bool,
    field: &str,
) -> Result<BTreeSet<PathBuf>, String> {
    let mut out = BTreeSet::new();
    for pattern in patterns {
        if Path::new(pattern).is_absolute() {
            return Err(format!(
                "workspace member pattern `{pattern}` must be relative to the workspace root"
            ));
        }
        let parts: Vec<&str> = pattern.split('/').filter(|part| !part.is_empty()).collect();
        let mut matches = BTreeSet::new();
        expand_pattern_at(root, &parts, 0, &mut matches)?;
        if require_match && matches.is_empty() {
            return Err(format!(
                "{field} pattern `{pattern}` matches no package containing Cargo.toml"
            ));
        }
        out.extend(matches);
    }
    Ok(out)
}

fn expand_pattern_at(
    dir: &Path,
    parts: &[&str],
    at: usize,
    out: &mut BTreeSet<PathBuf>,
) -> Result<(), String> {
    if at == parts.len() {
        if dir.join("Cargo.toml").is_file() {
            out.insert(
                std::fs::canonicalize(dir)
                    .or_else(|_| std::path::absolute(dir))
                    .map_err(|e| e.to_string())?,
            );
        }
        return Ok(());
    }
    let part = parts[at];
    if part == "**" {
        // `**` may consume zero or any number of segments.
        expand_pattern_at(dir, parts, at + 1, out)?;
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Ok(());
        };
        for entry in entries {
            let entry =
                entry.map_err(|e| format!("failed to read workspace member directory: {e}"))?;
            if entry.file_type().map_err(|e| e.to_string())?.is_dir() {
                expand_pattern_at(&entry.path(), parts, at, out)?;
            }
        }
        return Ok(());
    }
    if !part.contains(['*', '?', '[', '\\']) {
        return expand_pattern_at(&dir.join(part), parts, at + 1, out);
    }
    let entries = std::fs::read_dir(dir).map_err(|e| {
        format!(
            "failed to read {} while expanding workspace member patterns: {e}",
            dir.display()
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("failed to read workspace member directory: {e}"))?;
        if entry.file_type().map_err(|e| e.to_string())?.is_dir()
            && wildcard_match(part, &entry.file_name().to_string_lossy())?
        {
            expand_pattern_at(&entry.path(), parts, at + 1, out)?;
        }
    }
    Ok(())
}

fn wildcard_match(pattern: &str, text: &str) -> Result<bool, String> {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let mut memo = std::collections::BTreeMap::new();
    wildcard_match_at(&pattern, &text, 0, 0, &mut memo)
}

fn wildcard_match_at(
    pattern: &[char],
    text: &[char],
    pi: usize,
    ti: usize,
    memo: &mut std::collections::BTreeMap<(usize, usize), bool>,
) -> Result<bool, String> {
    if let Some(result) = memo.get(&(pi, ti)) {
        return Ok(*result);
    }
    let result = match pattern.get(pi) {
        None => ti == text.len(),
        Some('*') => {
            wildcard_match_at(pattern, text, pi + 1, ti, memo)?
                || (ti < text.len() && wildcard_match_at(pattern, text, pi, ti + 1, memo)?)
        }
        Some('?') => ti < text.len() && wildcard_match_at(pattern, text, pi + 1, ti + 1, memo)?,
        Some('\\') => {
            let literal = pattern
                .get(pi + 1)
                .ok_or_else(|| "workspace glob may not end with a backslash".to_string())?;
            ti < text.len()
                && text[ti] == *literal
                && wildcard_match_at(pattern, text, pi + 2, ti + 1, memo)?
        }
        Some('[') => {
            let (end, matched) = match_character_class(pattern, pi, text.get(ti).copied())?;
            matched && wildcard_match_at(pattern, text, end + 1, ti + 1, memo)?
        }
        Some(literal) => {
            ti < text.len()
                && text[ti] == *literal
                && wildcard_match_at(pattern, text, pi + 1, ti + 1, memo)?
        }
    };
    memo.insert((pi, ti), result);
    Ok(result)
}

fn match_character_class(
    pattern: &[char],
    start: usize,
    candidate: Option<char>,
) -> Result<(usize, bool), String> {
    let mut at = start + 1;
    let negated = matches!(pattern.get(at), Some('!') | Some('^'));
    if negated {
        at += 1;
    }
    let content_start = at;
    let mut matched = false;
    while let Some(&current) = pattern.get(at) {
        if current == ']' && at > content_start {
            return Ok((at, candidate.is_some() && (matched != negated)));
        }
        let (current, consumed) = if current == '\\' {
            (
                *pattern.get(at + 1).ok_or_else(|| {
                    "incomplete escape in workspace glob character class".to_string()
                })?,
                2,
            )
        } else {
            (current, 1)
        };
        if pattern.get(at + consumed) == Some(&'-') {
            let end = *pattern
                .get(at + consumed + 1)
                .ok_or_else(|| "incomplete character range in workspace glob".to_string())?;
            matched |= candidate.is_some_and(|value| current <= value && value <= end);
            at += consumed + 2;
        } else {
            matched |= candidate == Some(current);
            at += consumed;
        }
    }
    Err("workspace glob character class is missing `]`".into())
}

fn materialize_member(
    mut member: toml::Value,
    workspace_root: &toml::Value,
    root: &Path,
) -> Result<toml::Value, String> {
    let member_table = member
        .as_table_mut()
        .ok_or_else(|| "member Cargo.toml top level is not a table".to_string())?;
    let workspace = workspace_root
        .get("workspace")
        .and_then(toml::Value::as_table)
        .ok_or_else(|| "workspace root is missing [workspace]".to_string())?;

    if let Some(package) = member_table
        .get_mut("package")
        .and_then(toml::Value::as_table_mut)
    {
        let inherited = workspace.get("package").and_then(toml::Value::as_table);
        let keys: Vec<String> = package.keys().cloned().collect();
        for key in keys {
            let inherited_value = package.get(&key).and_then(toml::Value::as_table);
            let use_workspace = inherited_value
                .and_then(|table| table.get("workspace"))
                .and_then(toml::Value::as_bool)
                .unwrap_or(false);
            if use_workspace {
                const INHERITABLE: &[&str] = &[
                    "authors",
                    "categories",
                    "description",
                    "documentation",
                    "edition",
                    "exclude",
                    "homepage",
                    "include",
                    "keywords",
                    "license",
                    "license-file",
                    "publish",
                    "readme",
                    "repository",
                    "rust-version",
                    "version",
                ];
                if !INHERITABLE.contains(&key.as_str()) {
                    return Err(format!(
                        "package.{key} cannot be inherited from workspace.package"
                    ));
                }
                if inherited_value.is_some_and(|table| table.len() != 1) {
                    return Err(format!(
                        "package.{key}.workspace=true cannot be combined with other sub-keys"
                    ));
                }
                let value = inherited
                    .and_then(|table| table.get(&key))
                    .cloned()
                    .ok_or_else(|| {
                        format!("package.{key} inherits workspace but the root has no such field")
                    })?;
                package.insert(key, value);
            }
        }
    }

    if let Some(lints) = member_table
        .get("lints")
        .and_then(toml::Value::as_table)
        .filter(|lints| {
            lints
                .get("workspace")
                .and_then(toml::Value::as_bool)
                .unwrap_or(false)
        })
    {
        if lints.len() != 1 {
            return Err(
                "[lints] workspace=true cannot be declared together with other lints".into(),
            );
        }
        let inherited = workspace.get("lints").cloned().ok_or_else(|| {
            "[lints] inherits workspace but the root has no [workspace.lints]".to_string()
        })?;
        member_table.insert("lints".into(), inherited);
    }

    for dep_table_name in ["dependencies", "build-dependencies", "dev-dependencies"] {
        if let Some(deps) = member_table
            .get_mut(dep_table_name)
            .and_then(toml::Value::as_table_mut)
        {
            materialize_dependency_table(deps, dep_table_name, workspace, root)?;
        }
    }
    if let Some(targets) = member_table
        .get_mut("target")
        .and_then(toml::Value::as_table_mut)
    {
        for (cfg, target) in targets.iter_mut() {
            let Some(target) = target.as_table_mut() else {
                continue;
            };
            for dep_table_name in ["dependencies", "build-dependencies", "dev-dependencies"] {
                if let Some(deps) = target
                    .get_mut(dep_table_name)
                    .and_then(toml::Value::as_table_mut)
                {
                    materialize_dependency_table(
                        deps,
                        &format!("target.{cfg}.{dep_table_name}"),
                        workspace,
                        root,
                    )?;
                }
            }
        }
    }

    // Cargo reads only the workspace root's profile; a member's own profile has no effect.
    if let Some(profile) = workspace_root.get("profile").cloned() {
        member_table.insert("profile".into(), profile);
    } else {
        member_table.remove("profile");
    }
    // Cargo reads only the workspace root's patch/replace; same-named tables in members are ignored.
    for key in ["patch", "replace"] {
        match workspace_root.get(key).cloned() {
            Some(mut value) => {
                absolutize_override_paths(key, &mut value, root);
                member_table.insert(key.into(), value);
            }
            None => {
                member_table.remove(key);
            }
        }
    }
    member_table.remove("workspace");
    Ok(member)
}

fn absolutize_override_paths(kind: &str, value: &mut toml::Value, root: &Path) {
    let Some(table) = value.as_table_mut() else {
        return;
    };
    if kind == "patch" {
        for registry in table
            .iter_mut()
            .filter_map(|(_, value)| value.as_table_mut())
        {
            for (_, dependency) in registry.iter_mut() {
                absolutize_dependency_path(dependency, root);
            }
        }
    } else {
        for (_, dependency) in table.iter_mut() {
            absolutize_dependency_path(dependency, root);
        }
    }
}

fn absolutize_dependency_path(dependency: &mut toml::Value, root: &Path) {
    let Some(table) = dependency.as_table_mut() else {
        return;
    };
    let Some(path) = table.get("path").and_then(toml::Value::as_str) else {
        return;
    };
    if !Path::new(path).is_absolute() {
        table.insert(
            "path".into(),
            toml::Value::String(root.join(path).display().to_string()),
        );
    }
}

fn materialize_dependency_table(
    deps: &mut toml::map::Map<String, toml::Value>,
    field: &str,
    workspace: &toml::map::Map<String, toml::Value>,
    root: &Path,
) -> Result<(), String> {
    let names: Vec<String> = deps.keys().cloned().collect();
    for name in names {
        let Some(local) = deps.get(&name).cloned() else {
            continue;
        };
        let Some(local_table) = local.as_table() else {
            continue;
        };
        if !local_table
            .get("workspace")
            .and_then(toml::Value::as_bool)
            .unwrap_or(false)
        {
            continue;
        }
        let base = workspace
            .get("dependencies")
            .and_then(toml::Value::as_table)
            .and_then(|table| table.get(&name))
            .cloned()
            .ok_or_else(|| {
                format!("{field}.{name} inherits workspace but the root has no such dependency")
            })?;
        deps.insert(name, merge_dependency(base, local, root)?);
    }
    Ok(())
}

fn dependency_paths(manifest: &toml::Value, manifest_dir: &Path) -> BTreeSet<PathBuf> {
    let mut out = BTreeSet::new();
    let Some(root) = manifest.as_table() else {
        return out;
    };
    let mut collect = |table: Option<&toml::Value>| {
        for value in table.and_then(toml::Value::as_table).into_iter().flatten() {
            if let Some(path) = value
                .1
                .as_table()
                .and_then(|spec| spec.get("path"))
                .and_then(toml::Value::as_str)
            {
                let path = Path::new(path);
                let absolute = if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    manifest_dir.join(path)
                };
                out.insert(
                    std::fs::canonicalize(&absolute)
                        .unwrap_or_else(|_| std::path::absolute(&absolute).unwrap_or(absolute)),
                );
            }
        }
    };
    for name in ["dependencies", "build-dependencies", "dev-dependencies"] {
        collect(root.get(name));
    }
    if let Some(targets) = root.get("target").and_then(toml::Value::as_table) {
        for target in targets.values().filter_map(toml::Value::as_table) {
            for name in ["dependencies", "build-dependencies", "dev-dependencies"] {
                collect(target.get(name));
            }
        }
    }
    out
}

fn merge_dependency(
    base: toml::Value,
    local: toml::Value,
    root: &Path,
) -> Result<toml::Value, String> {
    let mut base = match base {
        toml::Value::String(version) => {
            let mut table = toml::map::Map::new();
            table.insert("version".into(), toml::Value::String(version));
            table
        }
        toml::Value::Table(table) => table,
        _ => return Err("workspace dependency must be a string or a table".into()),
    };
    if base.contains_key("optional") {
        return Err(
            "workspace.dependencies cannot declare optional; the member dependency must".into(),
        );
    }
    if let Some(path) = base.get("path").and_then(toml::Value::as_str) {
        base.insert(
            "path".into(),
            toml::Value::String(root.join(path).display().to_string()),
        );
    }
    let local = local
        .as_table()
        .ok_or_else(|| "a workspace=true dependency must be a table".to_string())?;
    if let Some(key) = local
        .keys()
        .find(|key| !matches!(key.as_str(), "workspace" | "features" | "optional"))
    {
        return Err(format!(
            "an inherited workspace dependency cannot declare `{key}` beyond features/optional"
        ));
    }
    let mut features = match base.remove("features") {
        Some(value) => value
            .as_array()
            .cloned()
            .ok_or_else(|| "workspace dependency features must be an array".to_string())?,
        None => Vec::new(),
    };
    if let Some(extra) = local.get("features") {
        features.extend(
            extra
                .as_array()
                .ok_or_else(|| "member dependency features must be an array".to_string())?
                .iter()
                .cloned(),
        );
    }
    if !features.is_empty() {
        let mut seen = BTreeSet::new();
        features.retain(|value| seen.insert(value.to_string()));
        base.insert("features".into(), toml::Value::Array(features));
    }
    for (key, value) in local {
        if key != "workspace" && key != "features" {
            base.insert(key.clone(), value.clone());
        }
    }
    Ok(toml::Value::Table(base))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_segments_match() {
        assert!(wildcard_match("member-?", "member-a").unwrap());
        assert!(wildcard_match("[ab]*", "alpha").unwrap());
        assert!(wildcard_match("[!ab]*", "charlie").unwrap());
        assert!(!wildcard_match("foo-*", "bar-foo").unwrap());
    }

    #[test]
    fn resolver_one_is_materialized_for_roots_and_path_dependencies() {
        let root = std::env::temp_dir().join(format!(
            "mirvm-workspace-resolver-one-dependency-{}",
            std::process::id()
        ));
        let member = root.join("member");
        std::fs::create_dir_all(&member).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nresolver='1'\nmembers=['member']\n",
        )
        .unwrap();
        std::fs::write(
            member.join("Cargo.toml"),
            "[package]\nname='member'\nversion='0.1.0'\nedition='2021'\n",
        )
        .unwrap();

        let root_workspace = WorkspaceManifest::read(&member).unwrap();
        assert_eq!(root_workspace.members[0].resolver, ResolverVersion::V1);
        let dependency = WorkspaceManifest::read_dependency(&member).unwrap();
        let package = dependency
            .members
            .into_iter()
            .find(|package| package.root == member)
            .unwrap();
        assert_eq!(package.resolver, ResolverVersion::V1);
    }

    #[test]
    fn missing_workspace_resolver_uses_cargo_resolver_one_default() {
        let root = std::env::temp_dir().join(format!(
            "mirvm-workspace-implicit-resolver-one-{}",
            std::process::id()
        ));
        let member = root.join("member");
        std::fs::create_dir_all(&member).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[workspace]\nmembers=['member']\n").unwrap();
        std::fs::write(
            member.join("Cargo.toml"),
            "[package]\nname='member'\nversion='0.1.0'\nedition='2018'\n",
        )
        .unwrap();

        let workspace = WorkspaceManifest::read(&root).unwrap();
        assert_eq!(workspace.members[0].resolver, ResolverVersion::V1);
    }

    #[test]
    fn reads_contract_workspace_from_root_and_member() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data/fixtures/cargoless/workspace-contract");
        let workspace = WorkspaceManifest::read(&root).unwrap();
        assert_eq!(
            workspace.members.len(),
            4,
            "members={:?}",
            workspace
                .members
                .iter()
                .map(|member| &member.name)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            workspace
                .members
                .iter()
                .find(|member| workspace.default_members.contains(&member.root))
                .unwrap()
                .name,
            "workspace-app"
        );
        assert!(
            workspace
                .members
                .iter()
                .all(|member| member.lock_root == root)
        );

        let from_member = WorkspaceManifest::read(&root.join("tool")).unwrap();
        let current = from_member.current_member.unwrap();
        assert_eq!(
            from_member
                .members
                .iter()
                .find(|member| member.root == current)
                .unwrap()
                .name,
            "workspace-tool"
        );

        let excluded = WorkspaceManifest::read(&root.join("test-helper")).unwrap();
        assert_eq!(excluded.root, root.join("test-helper"));
        assert_eq!(excluded.members.len(), 1);
        assert_eq!(excluded.members[0].name, "test-helper");
        assert_eq!(excluded.default_members, BTreeSet::from([excluded.root]));
    }

    #[test]
    fn rejects_unmatched_member_and_default_member_patterns() {
        let root = std::env::temp_dir().join(format!(
            "mirvm-workspace-unmatched-patterns-{}",
            std::process::id()
        ));
        let member = root.join("member");
        std::fs::create_dir_all(&member).unwrap();
        std::fs::write(
            member.join("Cargo.toml"),
            "[package]\nname='member'\nversion='0.1.0'\nedition='2024'\n",
        )
        .unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nresolver='2'\nmembers=['member', 'missing']\n",
        )
        .unwrap();
        let error = WorkspaceManifest::read(&root).unwrap_err();
        assert!(error.contains("workspace.members"), "{error}");
        assert!(error.contains("missing"), "{error}");

        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nresolver='2'\nmembers=['member']\ndefault-members=['missing']\n",
        )
        .unwrap();
        let error = WorkspaceManifest::read(&root).unwrap_err();
        assert!(error.contains("workspace.default-members"), "{error}");
        assert!(error.contains("missing"), "{error}");

        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nresolver='2'\nmembers=['member']\n",
        )
        .unwrap();
        let unlisted = root.join("unlisted");
        std::fs::create_dir_all(&unlisted).unwrap();
        std::fs::write(
            unlisted.join("Cargo.toml"),
            "[package]\nname='unlisted'\nversion='0.1.0'\nedition='2024'\n",
        )
        .unwrap();
        let error = WorkspaceManifest::read(&unlisted).unwrap_err();
        assert!(error.contains("neither a member nor excluded"), "{error}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn edition_2024_workspace_infers_resolver_three() {
        let root =
            std::env::temp_dir().join(format!("mirvm-workspace-resolver-{}", std::process::id()));
        let member = root.join("member");
        std::fs::create_dir_all(member.join("src")).unwrap();
        std::fs::write(member.join("src/lib.rs"), "pub fn value() -> u8 { 1 }\n").unwrap();
        std::fs::write(
            member.join("Cargo.toml"),
            "[package]\nname='member'\nversion='0.1.0'\nedition='2024'\n",
        )
        .unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname='root'\nversion='0.1.0'\nedition='2024'\n\
             [workspace]\nmembers=['member']\n",
        )
        .unwrap();
        let workspace = WorkspaceManifest::read(&root).unwrap();
        assert!(
            workspace
                .members
                .iter()
                .all(|member| member.resolver == ResolverVersion::V3)
        );

        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname='root'\nversion='0.1.0'\nedition='2024'\n\
             [workspace]\nresolver='2'\nmembers=['member']\n",
        )
        .unwrap();
        let workspace = WorkspaceManifest::read(&root).unwrap();
        assert!(
            workspace
                .members
                .iter()
                .all(|member| member.resolver == ResolverVersion::V2)
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resolver_three_uses_lowest_workspace_rust_version() {
        let root = std::env::temp_dir().join(format!(
            "mirvm-workspace-rust-version-{}",
            std::process::id()
        ));
        for (name, rust_version) in [("old", Some("1.85")), ("current", None)] {
            let member = root.join(name);
            std::fs::create_dir_all(member.join("src")).unwrap();
            std::fs::write(member.join("src/lib.rs"), "pub fn value() {}\n").unwrap();
            std::fs::write(
                member.join("Cargo.toml"),
                format!(
                    "[package]\nname='{name}'\nversion='0.1.0'\nedition='2024'\n{}",
                    rust_version
                        .map(|version| format!("rust-version='{version}'\n"))
                        .unwrap_or_default()
                ),
            )
            .unwrap();
        }
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nresolver='3'\nmembers=['old', 'current']\n",
        )
        .unwrap();
        let workspace = WorkspaceManifest::read(&root).unwrap();
        assert!(workspace.members.iter().all(|member| {
            member.resolver_rust_version == Some(semver::Version::new(1, 85, 0))
        }));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn materializes_workspace_package_dependency_and_profile() {
        let root: toml::Value = toml::from_str(
            r#"
                [workspace]
                [workspace.package]
                version = "1.2.3"
                edition = "2024"
                authors = ["A"]
                [workspace.dependencies]
                dep = { path = "dep", default-features = false, features = ["base"] }
                [profile.test]
                opt-level = 1
            "#,
        )
        .unwrap();
        let member: toml::Value = toml::from_str(
            r#"
                [package]
                name = "member"
                version.workspace = true
                edition.workspace = true
                authors.workspace = true
                [dependencies]
                dep = { workspace = true, features = ["extra"] }
            "#,
        )
        .unwrap();
        let dir = std::env::temp_dir().join("mirvm-workspace-materialize-test");
        let value = materialize_member(member, &root, &dir).unwrap();
        let parsed = PackageManifest::parse(&toml::to_string(&value).unwrap(), &dir).unwrap();
        assert_eq!(parsed.version, semver::Version::new(1, 2, 3));
        assert_eq!(parsed.edition, "2024");
        assert_eq!(parsed.pkg_env["CARGO_PKG_AUTHORS"], "A");
        assert_eq!(
            parsed.test_profile.opt_level,
            super::super::manifest::OptLevel::O1
        );
        let dep = parsed.deps.iter().find(|dep| dep.key == "dep").unwrap();
        assert_eq!(dep.features, ["base", "extra"]);
        assert!(
            matches!(&dep.source, super::super::manifest::DepSource::Path(path) if path == &dir.join("dep"))
        );

        let invalid_member: toml::Value = toml::from_str(
            "[package]\nname='member'\nversion='1.0.0'\n\
             [dependencies]\ndep={ workspace=true, default-features=false }\n",
        )
        .unwrap();
        let error = materialize_member(invalid_member, &root, &dir).unwrap_err();
        assert!(error.contains("features/optional"), "{error}");

        let invalid_root: toml::Value = toml::from_str(
            "[workspace]\n[workspace.dependencies]\ndep={ path='dep', optional=true }\n",
        )
        .unwrap();
        let inherited: toml::Value = toml::from_str(
            "[package]\nname='member'\nversion='1.0.0'\n\
             [dependencies]\ndep.workspace=true\n",
        )
        .unwrap();
        let error = materialize_member(inherited, &invalid_root, &dir).unwrap_err();
        assert!(error.contains("cannot declare optional"), "{error}");
    }
}
