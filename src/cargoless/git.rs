//! Git dependency fetching and exact-commit checkout.
//!
//! Mutable branch/tag/default-HEAD references are resolved only during fresh
//! resolution; an existing lock always uses its `#<commit>` directly and never
//! re-resolves over the network. Fetching shells out to the system Git and reuses
//! the user's credential helper, SSH agent and known_hosts; mirvm keeps no second
//! credential list.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::manifest::{GitReference, GitSpec, PackageManifest};
use super::workspace::WorkspaceManifest;

pub struct GitStore {
    root: PathBuf,
    offline: bool,
}

impl GitStore {
    pub fn open(root: PathBuf, offline: bool) -> Result<Self, String> {
        std::fs::create_dir_all(root.join("db"))
            .map_err(|error| format!("failed to create Git store {}: {error}", root.display()))?;
        std::fs::create_dir_all(root.join("checkouts"))
            .map_err(|error| format!("failed to create Git store {}: {error}", root.display()))?;
        Ok(Self { root, offline })
    }

    pub fn ensure_package(
        &self,
        spec: &GitSpec,
        package: &str,
        locked_source: Option<&str>,
    ) -> Result<PackageManifest, String> {
        let source_id = spec.source_id();
        let key = repo_key(&spec.url);
        let db = self.root.join("db").join(&key);
        let precise = match locked_source {
            Some(source) => parse_locked_source(source, &source_id)?,
            None => self.resolve_fresh(spec, &db)?,
        };
        if locked_source.is_some() && !git_object_exists(&db, &precise)? {
            if self.offline {
                return Err(format!(
                    "MIRVM_OFFLINE: locked commit {precise} of Git dependency {package} \
                     is not in the local cache"
                ));
            }
            self.ensure_db(spec, &db)?;
            if !git_object_exists(&db, &precise)? {
                fetch_precise(&db, &spec.url, &precise)?;
            }
        }
        let checkout = self.ensure_checkout(&db, &key, &precise)?;
        let package_dir = find_package_dir(&checkout, package)?;
        let workspace = WorkspaceManifest::read(&package_dir).map_err(|error| {
            format!(
                "failed to resolve workspace of Git dependency {package} ({}#{precise}): {error}",
                spec.url
            )
        })?;
        let mut manifest = workspace
            .members
            .into_iter()
            .find(|member| member.name == package && member.root == package_dir)
            .ok_or_else(|| {
                format!(
                    "found {package} in the Git repository but not in the \
                                    materialized workspace"
                )
            })?;
        if !spec.version.matches(&manifest.version) {
            return Err(format!(
                "package.version={} of Git dependency {package} does not satisfy the \
                 manifest version requirement {}",
                manifest.version, spec.version
            ));
        }
        manifest.lock_source = Some(format!("{source_id}#{precise}"));
        manifest.git_checkout_root = Some(checkout);
        Ok(manifest)
    }

    fn resolve_fresh(&self, spec: &GitSpec, db: &Path) -> Result<String, String> {
        if !db.is_dir() {
            if self.offline {
                return Err(format!(
                    "MIRVM_OFFLINE: no local cache for Git repository {} \
                     (resolve it once online first)",
                    spec.url
                ));
            }
            self.ensure_db(spec, db)?;
        } else {
            validate_db_origin(db, &spec.url)?;
        }
        if self.offline {
            return resolve_cached_reference(db, &spec.reference);
        }
        let reference = match &spec.reference {
            GitReference::DefaultBranch => {
                let output = git_output(
                    Command::new("git").args(["ls-remote", "--symref", &spec.url, "HEAD"]),
                    "read Git default branch",
                )?;
                output
                    .lines()
                    .find_map(|line| line.strip_prefix("ref: ")?.strip_suffix("\tHEAD"))
                    .ok_or_else(|| {
                        format!(
                            "Git repository {} has no resolvable default branch",
                            spec.url
                        )
                    })?
                    .to_string()
            }
            GitReference::Branch(branch) => format!("refs/heads/{branch}"),
            GitReference::Tag(tag) => format!("refs/tags/{tag}"),
            GitReference::Rev(rev) => {
                git_ok(
                    Command::new("git").arg("-C").arg(db).args([
                        "fetch",
                        "--force",
                        "origin",
                        "+refs/heads/*:refs/heads/*",
                        "+refs/tags/*:refs/tags/*",
                    ]),
                    "update Git branches and tags",
                )?;
                if let Ok(precise) = rev_parse(db, &format!("{rev}^{{commit}}")) {
                    return Ok(precise);
                }
                git_ok(
                    Command::new("git")
                        .arg("-C")
                        .arg(db)
                        .args(["fetch", "--force", "origin", rev]),
                    &format!("fetch Git rev {rev}"),
                )?;
                return rev_parse(db, "FETCH_HEAD^{commit}");
            }
        };
        let refspec = format!("+{reference}:{reference}");
        git_ok(
            Command::new("git")
                .arg("-C")
                .arg(db)
                .args(["fetch", "--force", "origin", &refspec]),
            &format!("update Git reference {reference}"),
        )?;
        if matches!(&spec.reference, GitReference::DefaultBranch) {
            git_ok(
                Command::new("git")
                    .arg("-C")
                    .arg(db)
                    .args(["symbolic-ref", "HEAD", &reference]),
                "update Git cache default branch",
            )?;
        }
        rev_parse(db, &format!("{reference}^{{commit}}"))
    }

    fn ensure_db(&self, spec: &GitSpec, db: &Path) -> Result<(), String> {
        if db.is_dir() {
            return validate_db_origin(db, &spec.url);
        }
        let parent = db
            .parent()
            .ok_or_else(|| "Git db path has no parent directory".to_string())?;
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        git_ok(
            Command::new("git")
                .args(["clone", "--mirror", "--no-checkout", &spec.url])
                .arg(db),
            &format!("clone Git repository {}", spec.url),
        )?;
        validate_db_origin(db, &spec.url)
    }

    fn ensure_checkout(&self, db: &Path, key: &str, precise: &str) -> Result<PathBuf, String> {
        let dir = self.root.join("checkouts").join(key).join(precise);
        if dir.is_dir() {
            validate_checkout(&dir, precise)?;
            return Ok(dir);
        }
        let parent = dir
            .parent()
            .ok_or_else(|| "Git checkout path has no parent directory".to_string())?;
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        let temp = parent.join(format!(".tmp-{}-{}", std::process::id(), precise));
        if temp.exists() {
            std::fs::remove_dir_all(&temp)
                .map_err(|error| format!("failed to clean up temporary Git checkout: {error}"))?;
        }
        let result = (|| {
            git_ok(
                Command::new("git")
                    .args(["clone", "--no-checkout", "--shared"])
                    .arg(db)
                    .arg(&temp),
                "create Git checkout",
            )?;
            git_ok(
                Command::new("git")
                    .arg("-C")
                    .arg(&temp)
                    .args(["checkout", "--detach", precise]),
                &format!("checkout Git commit {precise}"),
            )?;
            update_submodules(&temp, self.offline)?;
            std::fs::rename(&temp, &dir).map_err(|error| {
                format!("failed to publish Git checkout {}: {error}", dir.display())
            })?;
            validate_checkout(&dir, precise)
        })();
        if result.is_err() && temp.exists() {
            let _ = std::fs::remove_dir_all(&temp);
        }
        result.map(|_| dir)
    }
}

fn repo_key(url: &str) -> String {
    let stem = url
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("repo")
        .trim_end_matches(".git");
    let stem: String = stem
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect();
    format!("{stem}-{:016x}", crate::lower::asm::fnv1a(url.as_bytes()))
}

fn parse_locked_source(source: &str, source_id: &str) -> Result<String, String> {
    let precise = source
        .strip_prefix(source_id)
        .and_then(|rest| rest.strip_prefix('#'))
        .ok_or_else(|| {
            format!(
                "Cargo.lock Git source does not match the manifest: \
                 expected {source_id}#<commit>, got {source}"
            )
        })?;
    if precise.len() < 40 || !precise.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!("invalid Cargo.lock Git commit: {precise}"));
    }
    Ok(precise.to_ascii_lowercase())
}

fn resolve_cached_reference(db: &Path, reference: &GitReference) -> Result<String, String> {
    let rev = match reference {
        GitReference::DefaultBranch => "HEAD^{commit}".to_string(),
        GitReference::Branch(branch) => format!("refs/heads/{branch}^{{commit}}"),
        GitReference::Tag(tag) => format!("refs/tags/{tag}^{{commit}}"),
        GitReference::Rev(rev) => format!("{rev}^{{commit}}"),
    };
    rev_parse(db, &rev)
        .map_err(|error| format!("MIRVM_OFFLINE: Git reference {rev} is not in the cache: {error}"))
}

fn fetch_precise(db: &Path, url: &str, precise: &str) -> Result<(), String> {
    git_ok(
        Command::new("git")
            .arg("-C")
            .arg(db)
            .args(["fetch", "--force", url, precise]),
        &format!("fetch locked Git commit {precise}"),
    )
}

fn git_object_exists(db: &Path, precise: &str) -> Result<bool, String> {
    if !db.is_dir() {
        return Ok(false);
    }
    Ok(Command::new("git")
        .arg("-C")
        .arg(db)
        .args(["cat-file", "-e", &format!("{precise}^{{commit}}")])
        .env("GIT_TERMINAL_PROMPT", "0")
        .status()
        .map_err(|error| format!("failed to run git cat-file: {error}"))?
        .success())
}

fn validate_db_origin(db: &Path, expected: &str) -> Result<(), String> {
    let actual = git_output(
        Command::new("git")
            .arg("-C")
            .arg(db)
            .args(["config", "--get", "remote.origin.url"]),
        "read Git cache origin",
    )?;
    if actual != expected {
        return Err(format!(
            "Git cache identity mismatch {}: expected origin {expected}, got {actual}",
            db.display()
        ));
    }
    Ok(())
}

fn update_submodules(checkout: &Path, offline: bool) -> Result<(), String> {
    if !checkout.join(".gitmodules").is_file() {
        return Ok(());
    }
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(checkout)
        .args(["submodule", "update", "--init", "--recursive"]);
    if offline {
        command.arg("--no-fetch");
    }
    git_ok(
        &mut command,
        if offline {
            "MIRVM_OFFLINE: initialize Git submodules from the existing cache"
        } else {
            "initialize Git submodules"
        },
    )
}

fn validate_checkout(dir: &Path, precise: &str) -> Result<(), String> {
    let actual = rev_parse(dir, "HEAD^{commit}")?;
    if actual != precise {
        return Err(format!(
            "Git checkout identity corrupt {}: expected {precise}, got {actual}",
            dir.display()
        ));
    }
    let status = git_output(
        Command::new("git").arg("-C").arg(dir).args([
            "status",
            "--porcelain",
            "--untracked-files=all",
        ]),
        "check Git checkout integrity",
    )?;
    if !status.is_empty() {
        return Err(format!(
            "Git checkout content was modified {}:\n{}",
            dir.display(),
            status
        ));
    }
    if dir.join(".gitmodules").is_file() {
        let submodules = git_output(
            Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["submodule", "status", "--recursive"]),
            "check Git submodule integrity",
        )?;
        if submodules
            .lines()
            .any(|line| line.starts_with(['-', '+', 'U']))
        {
            return Err(format!(
                "Git checkout has uninitialized or mismatched submodules {}:\n{submodules}",
                dir.display()
            ));
        }
    }
    Ok(())
}

fn rev_parse(repo: &Path, rev: &str) -> Result<String, String> {
    git_output(
        Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", "--verify", rev]),
        &format!("resolve Git revision {rev}"),
    )
}

fn git_ok(command: &mut Command, action: &str) -> Result<(), String> {
    let output = command
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|error| format!("{action}: failed to start git: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "{action} failed (exit={}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

fn git_output(command: &mut Command, action: &str) -> Result<String, String> {
    let output = command
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|error| format!("{action}: failed to start git: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "{action} failed (exit={}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn find_package_dir(checkout: &Path, package: &str) -> Result<PathBuf, String> {
    let mut matches = Vec::new();
    let mut stack = vec![checkout.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir)
            .map_err(|error| format!("failed to scan Git checkout {}: {error}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|error| error.to_string())?;
            let path = entry.path();
            if entry
                .file_type()
                .map_err(|error| error.to_string())?
                .is_dir()
            {
                if matches!(entry.file_name().to_str(), Some(".git" | "target")) {
                    continue;
                }
                stack.push(path);
                continue;
            }
            if entry.file_name() != "Cargo.toml" {
                continue;
            }
            let text = std::fs::read_to_string(&path)
                .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
            let value: toml::Value = toml::from_str(&text)
                .map_err(|error| format!("failed to parse {}: {error}", path.display()))?;
            if value
                .get("package")
                .and_then(|package| package.get("name"))
                .and_then(toml::Value::as_str)
                == Some(package)
            {
                matches.push(dir.clone());
            }
        }
    }
    match matches.len() {
        0 => Err(format!(
            "package `{package}` not found in Git repository {}",
            checkout.display()
        )),
        1 => std::fs::canonicalize(&matches[0]).map_err(|error| error.to_string()),
        _ => Err(format!(
            "multiple packages named `{package}` in the Git repository: {}",
            matches
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use semver::VersionReq;

    fn test_root(tag: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("mirvm-cargoless-git-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn write(path: impl AsRef<Path>, contents: &str) {
        let path = path.as_ref();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn git(repo: &Path, args: &[&str]) -> String {
        git_output(
            Command::new("git").arg("-C").arg(repo).args(args),
            &format!("test git {}", args.join(" ")),
        )
        .unwrap()
    }

    fn spec(url: &str, reference: GitReference) -> GitSpec {
        GitSpec {
            url: url.to_string(),
            reference,
            version: VersionReq::parse("^1").unwrap(),
        }
    }

    #[test]
    fn resolves_selectors_locks_commits_and_rejects_tampering() {
        let root = test_root("contract");
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git_ok(
            Command::new("git").args(["init", "-b", "main"]).arg(&repo),
            "init test Git repository",
        )
        .unwrap();
        git(&repo, &["config", "user.name", "mirvm test"]);
        git(&repo, &["config", "user.email", "mirvm@example.invalid"]);
        write(
            repo.join("Cargo.toml"),
            "[workspace]\nmembers=['core','helper']\nresolver='2'\n\
             [workspace.package]\nedition='2021'\n",
        );
        write(
            repo.join("core/Cargo.toml"),
            "[package]\nname='git-core'\nversion='1.2.3'\nedition.workspace=true\n\
             [dependencies]\ngit-helper={path='../helper'}\n",
        );
        write(
            repo.join("core/src/lib.rs"),
            "pub fn value() -> usize { git_helper::value() }\n",
        );
        write(
            repo.join("helper/Cargo.toml"),
            "[package]\nname='git-helper'\nversion='0.4.0'\nedition.workspace=true\n",
        );
        write(
            repo.join("helper/src/lib.rs"),
            "pub fn value() -> usize { 1 }\n",
        );
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "first"]);
        let first = git(&repo, &["rev-parse", "HEAD"]);
        git(&repo, &["tag", "v1"]);

        let url = format!("file://{}", repo.display());
        let branch = spec(&url, GitReference::Branch("main".to_string()));
        let store = GitStore::open(root.join("store"), false).unwrap();
        let locked_manifest = store.ensure_package(&branch, "git-core", None).unwrap();
        let locked_source = locked_manifest.lock_source.clone().unwrap();
        assert!(locked_source.ends_with(&first));
        assert_eq!(
            locked_manifest.edition, "2021",
            "workspace inheritance must be materialized"
        );

        write(
            repo.join("helper/src/lib.rs"),
            "pub fn value() -> usize { 2 }\n",
        );
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "second"]);
        let second = git(&repo, &["rev-parse", "HEAD"]);

        let still_locked = store
            .ensure_package(&branch, "git-core", Some(&locked_source))
            .unwrap();
        assert!(still_locked.lock_source.unwrap().ends_with(&first));
        let fresh = store.ensure_package(&branch, "git-core", None).unwrap();
        assert!(fresh.lock_source.unwrap().ends_with(&second));

        let default = spec(&url, GitReference::DefaultBranch);
        assert!(
            store
                .ensure_package(&default, "git-core", None)
                .unwrap()
                .lock_source
                .unwrap()
                .ends_with(&second)
        );
        let tag = spec(&url, GitReference::Tag("v1".to_string()));
        assert!(
            store
                .ensure_package(&tag, "git-core", None)
                .unwrap()
                .lock_source
                .unwrap()
                .ends_with(&first)
        );
        let revision = spec(&url, GitReference::Rev(first[..12].to_string()));
        assert!(
            store
                .ensure_package(&revision, "git-core", None)
                .unwrap()
                .lock_source
                .unwrap()
                .ends_with(&first)
        );

        let offline = GitStore::open(root.join("store"), true).unwrap();
        offline
            .ensure_package(&branch, "git-core", Some(&locked_source))
            .unwrap();
        let cold = GitStore::open(root.join("cold"), true).unwrap();
        let error = cold
            .ensure_package(&branch, "git-core", Some(&locked_source))
            .unwrap_err();
        assert!(error.contains("not in the local cache"), "{error}");

        write(
            locked_manifest.root.join("src/lib.rs"),
            "pub fn value() -> usize { 99 }\n",
        );
        let error = store
            .ensure_package(&branch, "git-core", Some(&locked_source))
            .unwrap_err();
        assert!(error.contains("content was modified"), "{error}");
        let _ = std::fs::remove_dir_all(root);
    }
}
