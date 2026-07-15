use anyhow::{Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScmKind {
    Git,
    Svn,
    None,
}

#[derive(Debug, Clone)]
pub struct ScmRepo {
    pub kind: ScmKind,
    pub root: PathBuf,
}

pub fn detect(start: &Path) -> ScmRepo {
    let mut cur = Some(start);
    while let Some(dir) = cur {
        if dir.join(".git").exists() {
            return ScmRepo {
                kind: ScmKind::Git,
                root: dir.to_path_buf(),
            };
        }
        if dir.join(".svn").exists() {
            return ScmRepo {
                kind: ScmKind::Svn,
                root: dir.to_path_buf(),
            };
        }
        cur = dir.parent();
    }
    ScmRepo {
        kind: ScmKind::None,
        root: start.to_path_buf(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartWorkOutcome {
    /// Worktree ready. `branch` is the branch checked out in `path`.
    /// `created_branch` is true when no existing branch matched the ticket key
    /// and a new one was created from HEAD.
    /// `attached_existing_worktree` is true when the worktree path already existed
    /// before the request (we did not run `git worktree add`).
    GitWorktree {
        branch: String,
        path: PathBuf,
        created_branch: bool,
        attached_existing_worktree: bool,
    },
    /// Branch checked out in the main repo (no worktree). `path` is the repo root.
    /// `created_branch` is true when no existing branch matched the ticket key.
    /// `already_on_branch` is true when HEAD was already on the target branch.
    GitBranchInRepo {
        branch: String,
        path: PathBuf,
        created_branch: bool,
        already_on_branch: bool,
    },
    /// Caller is in an SVN repo. Shell needs to export the env var.
    SvnExport { value: String },
    /// No SCM detected — caller can record the association anyway.
    NoScm,
}

/// Where to land the start-work checkout: a separate worktree (default) or the
/// main repo itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkLocation {
    Worktree,
    BranchInRepo,
}

impl Default for WorkLocation {
    fn default() -> Self {
        WorkLocation::Worktree
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StartWorkError {
    #[error("git command failed: {0}")]
    Git(String),
}

pub fn start_work(
    repo: &ScmRepo,
    key: &str,
    slug: &str,
    location: WorkLocation,
) -> Result<StartWorkOutcome> {
    match repo.kind {
        ScmKind::Git => match location {
            WorkLocation::Worktree => git_start_work(&repo.root, key, slug),
            WorkLocation::BranchInRepo => git_start_work_in_repo(&repo.root, key, slug),
        },
        ScmKind::Svn => Ok(StartWorkOutcome::SvnExport {
            value: slug.to_string(),
        }),
        ScmKind::None => Ok(StartWorkOutcome::NoScm),
    }
}

/// Find an already-registered git worktree for `key` below `root`'s repository.
/// Matches either the checked-out branch or the worktree directory name, so it
/// handles both jui-created slugs and manually-created ticket worktrees.
pub fn find_existing_worktree_for_key(
    root: &Path,
    key: &str,
    slug: &str,
) -> Result<Option<(PathBuf, String)>> {
    let out = Command::new("git")
        .current_dir(root)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .context("running git worktree list")?;
    if !out.status.success() {
        return Ok(None);
    }

    let repo_root = detect(root).root.canonicalize().ok();
    let mut path: Option<PathBuf> = None;
    let mut branch: Option<String> = None;
    for line in String::from_utf8_lossy(&out.stdout).lines().chain([""]) {
        if line.is_empty() {
            if let Some(p) = path.take() {
                let b = branch.take().unwrap_or_else(|| slug.to_string());
                let is_main_repo = repo_root
                    .as_ref()
                    .and_then(|root| p.canonicalize().ok().map(|canon| canon == *root))
                    .unwrap_or(false);
                if !is_main_repo && p.exists() && worktree_matches_ticket(&p, &b, key, slug) {
                    return Ok(Some((p, b)));
                }
            }
            branch = None;
            continue;
        }
        if let Some(raw) = line.strip_prefix("worktree ") {
            path = Some(PathBuf::from(raw));
        } else if let Some(raw) = line.strip_prefix("branch ") {
            branch = Some(raw.strip_prefix("refs/heads/").unwrap_or(raw).to_string());
        }
    }
    Ok(None)
}

/// Find a separate registered git worktree whose checked-out branch exactly
/// matches `branch`. The main repository checkout is deliberately excluded.
pub fn find_existing_worktree_for_branch(
    root: &Path,
    branch_name: &str,
) -> Result<Option<(PathBuf, String)>> {
    let out = Command::new("git")
        .current_dir(root)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .context("running git worktree list")?;
    if !out.status.success() {
        return Ok(None);
    }

    let repo_root = detect(root).root.canonicalize().ok();
    let mut path: Option<PathBuf> = None;
    let mut branch: Option<String> = None;
    for line in String::from_utf8_lossy(&out.stdout).lines().chain([""]) {
        if line.is_empty() {
            if let (Some(p), Some(b)) = (path.take(), branch.take()) {
                let is_main_repo = repo_root
                    .as_ref()
                    .and_then(|root| p.canonicalize().ok().map(|canon| canon == *root))
                    .unwrap_or(false);
                if !is_main_repo && p.exists() && b == branch_name {
                    return Ok(Some((p, b)));
                }
            }
            branch = None;
            continue;
        }
        if let Some(raw) = line.strip_prefix("worktree ") {
            path = Some(PathBuf::from(raw));
        } else if let Some(raw) = line.strip_prefix("branch ") {
            branch = Some(raw.strip_prefix("refs/heads/").unwrap_or(raw).to_string());
        }
    }
    Ok(None)
}

fn worktree_matches_ticket(path: &Path, branch: &str, key: &str, slug: &str) -> bool {
    let key = key.to_ascii_lowercase();
    let normalized_key = crate::ticket::normalized_ticket_key(&key);
    let slug = slug.to_ascii_lowercase();
    let branch = branch.to_ascii_lowercase();
    branch.contains(&key)
        || (!normalized_key.is_empty() && branch.contains(&normalized_key))
        || path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|name| {
                let name = name.to_ascii_lowercase();
                name.contains(&key)
                    || (!normalized_key.is_empty() && name.contains(&normalized_key))
                    || name.contains(&slug)
            })
            .unwrap_or(false)
}

/// Branch-in-main-repo start-work:
///   1. Refuse if working tree is dirty (staged or unstaged changes).
///   2. Search local branches for one whose name contains the ticket key.
///   3. If found, `git checkout <branch>`; otherwise `git checkout -b <slug>`.
///   4. Returns the repo root as the launch path.
fn git_start_work_in_repo(root: &Path, key: &str, slug: &str) -> Result<StartWorkOutcome> {
    // Reject dirty trees so we don't silently lose work. Ignore untracked files
    // — build artifacts shouldn't block a branch switch.
    let status = Command::new("git")
        .current_dir(root)
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()
        .context("running git status")?;
    if !status.status.success() {
        let err = String::from_utf8_lossy(&status.stderr).trim().to_string();
        return Err(StartWorkError::Git(err).into());
    }
    if !status.stdout.is_empty() {
        let dirty = String::from_utf8_lossy(&status.stdout).trim().to_string();
        return Err(StartWorkError::Git(format!(
            "working tree has uncommitted changes — stash or commit first:\n{dirty}"
        ))
        .into());
    }

    let existing = find_branch_for_key(root, key)?;
    let current = current_git_branch(root).ok().flatten();
    let (branch, created) = match existing {
        Some(b) => (b, false),
        None => (slug.to_string(), true),
    };

    if current.as_deref() == Some(branch.as_str()) {
        return Ok(StartWorkOutcome::GitBranchInRepo {
            branch,
            path: root.to_path_buf(),
            created_branch: false,
            already_on_branch: true,
        });
    }

    let mut args: Vec<String> = vec!["checkout".into()];
    if created {
        args.push("-b".into());
    }
    args.push(branch.clone());

    let out = Command::new("git")
        .current_dir(root)
        .args(args.iter().map(|s| s.as_str()))
        .output()
        .context("running git checkout")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        // Git's "already used by worktree" error is the most common branch-in-repo
        // failure — surface a clearer message pointing at the worktree.
        let friendly =
            if err.contains("already used by worktree") || err.contains("is already checked out") {
                format!(
                    "branch '{branch}' is already checked out in a worktree — \
                 use 'worktree' mode to reuse it, or `git worktree remove` the old one first. \
                 (git said: {err})"
                )
            } else {
                err
            };
        return Err(StartWorkError::Git(friendly).into());
    }

    Ok(StartWorkOutcome::GitBranchInRepo {
        branch,
        path: root.to_path_buf(),
        created_branch: created,
        already_on_branch: false,
    })
}

/// Worktree-based start-work:
///   1. Search local branches for one whose name contains the (lowercased) ticket key.
///   2. If a branch is found, attach a worktree to it; otherwise create a new branch
///      named `slug` from HEAD and attach a worktree to it.
///   3. Worktree path is `<repo>/worktrees/<slug>`.
fn git_start_work(root: &Path, key: &str, slug: &str) -> Result<StartWorkOutcome> {
    let worktrees_root = root.join("worktrees");
    let path = worktrees_root.join(slug);

    // If the target path already exists, leave it alone — assume previous run.
    if path.exists() {
        let branch = current_git_branch(&path)
            .ok()
            .flatten()
            .unwrap_or_else(|| slug.to_string());
        return Ok(StartWorkOutcome::GitWorktree {
            branch,
            path,
            created_branch: false,
            attached_existing_worktree: true,
        });
    }

    std::fs::create_dir_all(&worktrees_root)
        .with_context(|| format!("creating {}", worktrees_root.display()))?;
    ensure_worktrees_excluded(root)?;

    let existing_branch = find_branch_for_key(root, key)?;
    let (branch, created) = match existing_branch {
        Some(b) => (b, false),
        None => (slug.to_string(), true),
    };

    let mut args: Vec<String> = vec!["worktree".into(), "add".into()];
    if created {
        args.push("-b".into());
        args.push(branch.clone());
    }
    args.push(path.to_string_lossy().into_owned());
    if !created {
        args.push(branch.clone());
    }

    let out = Command::new("git")
        .current_dir(root)
        .args(args.iter().map(|s| s.as_str()))
        .output()
        .context("running git worktree add")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(StartWorkError::Git(err).into());
    }
    Ok(StartWorkOutcome::GitWorktree {
        branch,
        path,
        created_branch: created,
        attached_existing_worktree: false,
    })
}

pub fn ensure_worktrees_excluded(root: &Path) -> Result<()> {
    let exclude = root.join(".git").join("info").join("exclude");
    let existing = std::fs::read_to_string(&exclude).unwrap_or_default();
    if existing.lines().any(|line| line.trim() == "/worktrees/") {
        return Ok(());
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&exclude)
        .with_context(|| format!("opening {}", exclude.display()))?
        .write_all(b"\n/worktrees/\n")
        .with_context(|| format!("writing {}", exclude.display()))?;
    Ok(())
}

/// Search local branches for one whose name contains the (lowercased) ticket key.
/// Returns the first match. Branch comparison is case-insensitive.
pub fn find_branch_for_key(root: &Path, key: &str) -> Result<Option<String>> {
    let out = Command::new("git")
        .current_dir(root)
        .args(["for-each-ref", "--format=%(refname:short)", "refs/heads/"])
        .output()
        .context("running git for-each-ref")?;
    if !out.status.success() {
        return Ok(None);
    }
    let needle = key.to_ascii_lowercase();
    let normalized_needle = crate::ticket::normalized_ticket_key(key);
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let line_lc = line.to_ascii_lowercase();
        if line_lc.contains(&needle)
            || (!normalized_needle.is_empty() && line_lc.contains(&normalized_needle))
        {
            return Ok(Some(line.to_string()));
        }
    }
    Ok(None)
}

/// Remove a worktree at `path`. Equivalent to `git worktree remove <path>`.
/// `force` adds `--force` (needed if working tree has untracked / dirty files).
pub fn git_worktree_remove(root: &Path, path: &Path, force: bool) -> Result<()> {
    let mut args: Vec<&str> = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    let path_str = path.to_string_lossy();
    args.push(&path_str);
    let out = Command::new("git")
        .current_dir(root)
        .args(&args)
        .output()
        .context("running git worktree remove")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(StartWorkError::Git(err).into());
    }
    Ok(())
}

/// Parse `owner/repo` from a GitHub remote URL in any common form.
pub fn parse_github_slug(url: &str) -> Option<String> {
    let s = url.trim();
    let s = s
        .strip_prefix("https://github.com/")
        .or_else(|| s.strip_prefix("http://github.com/"))
        .or_else(|| s.strip_prefix("git@github.com:"))
        .or_else(|| s.strip_prefix("ssh://git@github.com/"))?;
    let s = s.trim_end_matches('/').trim_end_matches(".git");
    if s.split('/').count() == 2 && !s.is_empty() {
        Some(s.to_string())
    } else {
        None
    }
}

/// Read `git remote get-url <name>` for the repo at `path`. Returns the parsed
/// `owner/repo` slug (or `None` if the remote doesn't exist or isn't a GitHub URL).
pub fn gh_slug_for_remote(path: &Path, remote: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["-C", path.to_str()?, "remote", "get-url", remote])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    parse_github_slug(&url)
}

/// Find a local clone whose `origin` or `upstream` remote points to the given
/// `<owner>/<repo>` slug. First exact-match origin, then exact-match upstream,
/// then case-insensitive on either.
pub fn find_clone_for_gh_repo(
    pr_slug: &str,
    candidates: &[std::path::PathBuf],
) -> Option<std::path::PathBuf> {
    let pr_lc = pr_slug.to_ascii_lowercase();
    let mut fallback: Option<std::path::PathBuf> = None;
    for p in candidates {
        for remote in ["origin", "upstream"] {
            if let Some(slug) = gh_slug_for_remote(p, remote) {
                if slug == pr_slug {
                    return Some(p.clone());
                }
                if slug.to_ascii_lowercase() == pr_lc && fallback.is_none() {
                    fallback = Some(p.clone());
                }
            }
        }
    }
    fallback
}

/// The conventional jui worktree path for a ticket slug: under the repo at
/// `<repo>/worktrees/<slug>`. Returns `None` if `cwd` isn't
/// inside a git repo.
pub fn worktree_path_for_slug(cwd: &Path, slug: &str) -> Option<std::path::PathBuf> {
    let repo = detect(cwd);
    if !matches!(repo.kind, ScmKind::Git) {
        return None;
    }
    Some(repo.root.join("worktrees").join(slug))
}

pub fn current_git_branch(root: &Path) -> Result<Option<String>> {
    let out = Command::new("git")
        .current_dir(root)
        .args(["branch", "--show-current"])
        .output()
        .context("running git branch --show-current")?;
    if !out.status.success() {
        return Ok(None);
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Ok(if s.is_empty() { None } else { Some(s) })
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RepoEntry {
    pub path: PathBuf,
    pub kind: String, // "git" or "svn"
}

/// Walk `root` recursively (depth-limited) and return every directory that contains a
/// `.git` or `.svn` marker. Stops descending into a repo once found. Symlinks
/// to directories are followed; cycles are broken via a canonicalized-path
/// visited set so a `~/workspace -> /mnt/workspace` link doesn't infinite-loop.
pub fn find_repos(root: &Path, max_depth: usize) -> Vec<RepoEntry> {
    let mut out = Vec::new();
    let mut visited: std::collections::HashSet<std::path::PathBuf> =
        std::collections::HashSet::new();
    walk(root, 0, max_depth, &mut out, &mut visited);
    out
}

fn walk(
    dir: &Path,
    depth: usize,
    max_depth: usize,
    out: &mut Vec<RepoEntry>,
    visited: &mut std::collections::HashSet<std::path::PathBuf>,
) {
    // Canonicalize for cycle detection. Skip if we've been here under a
    // different name (e.g. via a symlink).
    let canonical = std::fs::canonicalize(dir).ok();
    if let Some(c) = &canonical {
        if !visited.insert(c.clone()) {
            return;
        }
    }

    if dir.join(".git").exists() {
        out.push(RepoEntry {
            path: dir.to_path_buf(),
            kind: "git".into(),
        });
        return;
    }
    if dir.join(".svn").exists() {
        out.push(RepoEntry {
            path: dir.to_path_buf(),
            kind: "svn".into(),
        });
        return;
    }
    if depth >= max_depth {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.filter_map(|e| e.ok()) {
        let path = entry.path();
        // `entry.file_type()` is lstat — symlinks-to-directories return false
        // for is_dir(). `path.is_dir()` follows symlinks via metadata().
        if !path.is_dir() {
            continue;
        }
        if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
            if name.starts_with('.') {
                continue;
            }
            if matches!(
                name,
                "node_modules" | "target" | "dist" | "build" | "venv" | ".cache" | "__pycache__"
            ) {
                continue;
            }
        }
        walk(&path, depth + 1, max_depth, out, visited);
    }
}

/// Pull a Jira-style key (e.g. PROJ-123) out of a string.
pub fn extract_ticket_key(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_uppercase() {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_uppercase() {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b'-' {
            let dash = i;
            i += 1;
            let num_start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if i > num_start && (dash - start) >= 2 {
                return Some(s[start..i].to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_key_from_branch() {
        assert_eq!(extract_ticket_key("proj-123-add-login"), None); // requires uppercase
        assert_eq!(
            extract_ticket_key("feature/PROJ-123-add-login"),
            Some("PROJ-123".into())
        );
        assert_eq!(extract_ticket_key("ENG-7"), Some("ENG-7".into()));
        assert_eq!(extract_ticket_key("noticket"), None);
        assert_eq!(extract_ticket_key("A-1"), None); // single-letter prefix rejected
    }

    #[test]
    fn worktree_matches_normalized_ticket_key() {
        assert!(worktree_matches_ticket(
            Path::new("/tmp/repo/worktrees/proj123-login-auth"),
            "proj123-login-auth",
            "PROJ-123",
            "proj-123-add-login-auth-flow"
        ));
    }
}
