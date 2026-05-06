use anyhow::{anyhow, Context, Result};
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
    /// Switched to (or created) the branch.
    GitSwitched { branch: String, created: bool },
    /// Caller is in an SVN repo. Shell needs to export the env var.
    SvnExport { value: String },
    /// No SCM detected — caller can record the association anyway.
    NoScm,
}

#[derive(Debug, thiserror::Error)]
pub enum StartWorkError {
    #[error("git has staged changes; commit or stash them before starting work")]
    StagedChanges,
    #[error("git command failed: {0}")]
    Git(String),
}

pub fn start_work(repo: &ScmRepo, slug: &str) -> Result<StartWorkOutcome> {
    match repo.kind {
        ScmKind::Git => git_start_work(&repo.root, slug),
        ScmKind::Svn => Ok(StartWorkOutcome::SvnExport { value: slug.to_string() }),
        ScmKind::None => Ok(StartWorkOutcome::NoScm),
    }
}

fn git_start_work(root: &Path, slug: &str) -> Result<StartWorkOutcome> {
    if has_staged_changes(root)? {
        return Err(StartWorkError::StagedChanges.into());
    }
    let exists = git_branch_exists(root, slug)?;
    let args: Vec<&str> =
        if exists { vec!["switch", slug] } else { vec!["switch", "-c", slug] };
    let out = Command::new("git").current_dir(root).args(&args).output().context("running git switch")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(StartWorkError::Git(err).into());
    }
    Ok(StartWorkOutcome::GitSwitched { branch: slug.to_string(), created: !exists })
}

fn has_staged_changes(root: &Path) -> Result<bool> {
    let out = Command::new("git")
        .current_dir(root)
        .args(["diff", "--cached", "--name-only"])
        .output()
        .context("running git diff --cached")?;
    if !out.status.success() {
        return Err(anyhow!(
            "git diff --cached failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(!out.stdout.is_empty())
}

fn git_branch_exists(root: &Path, branch: &str) -> Result<bool> {
    let out = Command::new("git")
        .current_dir(root)
        .args(["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")])
        .output()
        .context("running git rev-parse")?;
    Ok(out.status.success())
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
/// `.git` or `.svn` marker. Stops descending into a repo once found.
pub fn find_repos(root: &Path, max_depth: usize) -> Vec<RepoEntry> {
    let mut out = Vec::new();
    walk(root, 0, max_depth, &mut out);
    out
}

fn walk(dir: &Path, depth: usize, max_depth: usize, out: &mut Vec<RepoEntry>) {
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
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
            if name.starts_with('.') { continue; }
            if matches!(
                name,
                "node_modules" | "target" | "dist" | "build" | "venv" | ".cache" | "__pycache__"
            ) { continue; }
        }
        walk(&path, depth + 1, max_depth, out);
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
