use anyhow::{Context, Result};
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
            return ScmRepo { kind: ScmKind::Git, root: dir.to_path_buf() };
        }
        if dir.join(".svn").exists() {
            return ScmRepo { kind: ScmKind::Svn, root: dir.to_path_buf() };
        }
        cur = dir.parent();
    }
    ScmRepo { kind: ScmKind::None, root: start.to_path_buf() }
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
    /// Caller is in an SVN repo. Shell needs to export the env var.
    SvnExport { value: String },
    /// No SCM detected — caller can record the association anyway.
    NoScm,
}

#[derive(Debug, thiserror::Error)]
pub enum StartWorkError {
    #[error("git command failed: {0}")]
    Git(String),
}

pub fn start_work(repo: &ScmRepo, key: &str, slug: &str) -> Result<StartWorkOutcome> {
    match repo.kind {
        ScmKind::Git => git_start_work(&repo.root, key, slug),
        ScmKind::Svn => Ok(StartWorkOutcome::SvnExport { value: slug.to_string() }),
        ScmKind::None => Ok(StartWorkOutcome::NoScm),
    }
}

/// Worktree-based start-work:
///   1. Search local branches for one whose name contains the (lowercased) ticket key.
///   2. If a branch is found, attach a worktree to it; otherwise create a new branch
///      named `slug` from HEAD and attach a worktree to it.
///   3. Worktree path is `<repo>/../<repo-name>-worktrees/<slug>`.
fn git_start_work(root: &Path, key: &str, slug: &str) -> Result<StartWorkOutcome> {
    let repo_name = root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("repo")
        .to_string();
    let parent = root.parent().unwrap_or(root);
    let worktrees_root = parent.join(format!("{}-worktrees", repo_name));
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
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if line.to_ascii_lowercase().contains(&needle) {
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
    if !out.status.success() { return None; }
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

/// The conventional jui worktree path for a ticket slug: sibling to the repo
/// at `<repo>/../<repo-name>-worktrees/<slug>`. Returns `None` if `cwd` isn't
/// inside a git repo.
pub fn worktree_path_for_slug(cwd: &Path, slug: &str) -> Option<std::path::PathBuf> {
    let repo = detect(cwd);
    if !matches!(repo.kind, ScmKind::Git) { return None; }
    let repo_name = repo.root.file_name().and_then(|s| s.to_str()).unwrap_or("repo");
    let parent = repo.root.parent().unwrap_or(&repo.root);
    Some(parent.join(format!("{}-worktrees", repo_name)).join(slug))
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
        if !visited.insert(c.clone()) { return; }
    }

    if dir.join(".git").exists() {
        out.push(RepoEntry { path: dir.to_path_buf(), kind: "git".into() });
        return;
    }
    if dir.join(".svn").exists() {
        out.push(RepoEntry { path: dir.to_path_buf(), kind: "svn".into() });
        return;
    }
    if depth >= max_depth {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for entry in rd.filter_map(|e| e.ok()) {
        let path = entry.path();
        // `entry.file_type()` is lstat — symlinks-to-directories return false
        // for is_dir(). `path.is_dir()` follows symlinks via metadata().
        if !path.is_dir() {
            continue;
        }
        if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
            if name.starts_with('.') { continue; }
            if matches!(
                name,
                "node_modules" | "target" | "dist" | "build" | "venv" | ".cache" | "__pycache__"
            ) { continue; }
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
        assert_eq!(extract_ticket_key("feature/PROJ-123-add-login"), Some("PROJ-123".into()));
        assert_eq!(extract_ticket_key("ENG-7"), Some("ENG-7".into()));
        assert_eq!(extract_ticket_key("noticket"), None);
        assert_eq!(extract_ticket_key("A-1"), None); // single-letter prefix rejected
    }
}
