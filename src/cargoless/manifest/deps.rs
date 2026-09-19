use std::collections::BTreeMap;
use std::path::Path;

use super::{
    DepDecl, DepKind, DepSource, FeatureValue, GitReference, GitSpec, MErr, PatchDecl,
    RegistryReference, ReplaceDecl, unsupported,
};

// ---------- dependency tables ----------

pub(super) fn parse_patches(
    value: Option<&toml::Value>,
    root: &Path,
) -> Result<Vec<PatchDecl>, MErr> {
    let Some(registries) = value else {
        return Ok(Vec::new());
    };
    let registries = registries
        .as_table()
        .ok_or_else(|| "[patch] must be a registry table".to_string())?;
    let mut out = Vec::new();
    for (registry, entries) in registries {
        let entries = entries
            .as_table()
            .ok_or_else(|| format!("[patch.{registry}] must be a dependency table"))?;
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
                    "[patch] package {} declares no path/git/other-registry source",
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

pub(super) fn parse_replacements(
    value: Option<&toml::Value>,
    root: &Path,
) -> Result<Vec<ReplaceDecl>, MErr> {
    let Some(entries) = value else {
        return Ok(Vec::new());
    };
    let entries = entries
        .as_table()
        .ok_or_else(|| "[replace] must be a package ID table".to_string())?;
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
                "[replace] `{package_id}` declares no path/git/other-registry source"
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
        .ok_or_else(|| format!("[replace] package ID `{package_id}` lacks `:version`"))?;
    let version = semver::Version::parse(version).map_err(|error| {
        format!("invalid version in [replace] package ID `{package_id}`: {error}")
    })?;
    let (source, package) = match head.rsplit_once('#') {
        Some((source, package)) => (Some(source.to_string()), package.to_string()),
        None => (None, head.to_string()),
    };
    if package.is_empty() {
        return Err(format!(
            "[replace] package ID `{package_id}` has an empty package name"
        ));
    }
    Ok((source, package, version))
}

pub(super) fn parse_dep_table(
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
                        "version" => {
                            req_str = v.as_str().ok_or("version is not a string")?.to_string()
                        }
                        "path" => {
                            let p = v.as_str().ok_or("path is not a string")?;
                            source = Some(DepSource::Path(root.join(p)));
                        }
                        "package" => {
                            package = v.as_str().ok_or("package is not a string")?.to_string()
                        }
                        "features" => {
                            features = v
                                .as_array()
                                .ok_or("features is not an array")?
                                .iter()
                                .map(|f| {
                                    f.as_str()
                                        .map(str::to_string)
                                        .ok_or("features has a non-string element")
                                })
                                .collect::<Result<_, _>>()?;
                        }
                        "optional" => optional = v.as_bool().ok_or("optional is not a bool")?,
                        "default-features" => {
                            default_features =
                                v.as_bool().ok_or("default-features is not a bool")?
                        }
                        "git" => {
                            git_url = Some(v.as_str().ok_or("git is not a string")?.to_string())
                        }
                        "branch" => {
                            branch = Some(v.as_str().ok_or("branch is not a string")?.to_string())
                        }
                        "tag" => tag = Some(v.as_str().ok_or("tag is not a string")?.to_string()),
                        "rev" => rev = Some(v.as_str().ok_or("rev is not a string")?.to_string()),
                        "registry" => {
                            registry = Some(RegistryReference::Named(
                                v.as_str().ok_or("registry is not a string")?.to_string(),
                            ));
                        }
                        "registry-index" => {
                            registry = Some(RegistryReference::Index(
                                v.as_str()
                                    .ok_or("registry-index is not a string")?
                                    .to_string(),
                            ));
                        }
                        // Known harmless keys: public/private (new cargo keys), artifact,
                        // lib, workspace (workspace.dependencies inheritance, rejected
                        // loudly when seen)
                        "workspace" => {
                            return Err(unsupported(format!(
                                "workspace inheritance of dependency {key}"
                            )));
                        }
                        _ => {} // unknown minor keys are ignored (forward compatibility)
                    }
                }
            }
            _ => {
                return Err(format!(
                    "unsupported form of dependency {key} (neither string nor table)"
                ));
            }
        }
        let req = semver::VersionReq::parse(&req_str)
            .map_err(|e| format!("invalid version req {req_str} for dependency {key}: {e}"))?;
        let selectors = [branch.is_some(), tag.is_some(), rev.is_some()]
            .into_iter()
            .filter(|present| *present)
            .count();
        if selectors > 1 {
            return Err(format!(
                "dependency {key} may set only one of branch/tag/rev"
            ));
        }
        if git_url.is_none() && selectors != 0 {
            return Err(format!(
                "dependency {key} sets branch/tag/rev but has no git URL"
            ));
        }
        if [branch.as_deref(), tag.as_deref(), rev.as_deref()]
            .into_iter()
            .flatten()
            .any(str::is_empty)
        {
            return Err(format!(
                "branch/tag/rev of dependency {key} must not be empty"
            ));
        }
        if git_url.is_some() && source.is_some() {
            return Err(format!("dependency {key} may not set both git and path"));
        }
        if registry.is_some() && (git_url.is_some() || source.is_some()) {
            return Err(format!(
                "dependency {key} may not set registry together with git/path"
            ));
        }
        let source = match (source, git_url) {
            (Some(path), None) => path,
            (None, Some(url)) => {
                if url.is_empty() || url.starts_with('-') {
                    return Err(format!("invalid git URL for dependency {key}: `{url}`"));
                }
                if url.contains('?') || url.contains('#') {
                    return Err(format!(
                        "the git URL of dependency {key} may not carry a query/fragment; use branch/tag/rev"
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
