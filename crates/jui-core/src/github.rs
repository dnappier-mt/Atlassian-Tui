//! Thin async wrappers around the `gh` CLI. We deliberately shell out instead
//! of hitting the GitHub REST API ourselves so we inherit the user's existing
//! `gh auth` credentials.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use tokio::process::Command;

/// One issue-level comment on a GitHub PR. Tied back to its Jira ticket via
/// `ticket_key` (the daemon resolves the key from the PR's branch name when
/// caching).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrComment {
    pub ticket_key: String,
    pub pr_url: String,
    pub pr_number: u64,
    pub repo: String,
    pub author: String,
    pub created: String,
    pub body: String,
}

/// One PR summary as returned by `gh search prs ... --json` or
/// `gh api notifications`. Just the bits jui needs for surfacing in the
/// inbound section.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrSummary {
    pub number: u64,
    pub title: String,
    pub url: String,
    /// Branch on the head fork (e.g. `proj-123-foo`). Used to recover the
    /// Jira ticket key via `scm::extract_ticket_key`.
    pub head_branch: String,
    /// `<owner>/<repo>` — needed for follow-up `gh -R` calls.
    pub repo: String,
    pub author: Option<String>,
}

/// Result of a successful `gh pr create`.
#[derive(Debug, Clone)]
pub struct CreatedPr {
    pub url: String,
    pub number: u64,
}

/// `<owner>/<repo>` for the GitHub remote of the worktree at `path`. Returns
/// an error if the directory has no GitHub remote or `gh` isn't on PATH.
pub async fn repo_slug(path: &Path) -> Result<String> {
    let out = Command::new("gh")
        .args(["repo", "view", "--json", "nameWithOwner", "-q", ".nameWithOwner"])
        .current_dir(path)
        .output()
        .await
        .context("running gh repo view")?;
    if !out.status.success() {
        return Err(anyhow!(
            "gh repo view failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Currently checked-out branch in `path`.
pub async fn current_branch(path: &Path) -> Result<String> {
    let out = Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(path)
        .output()
        .await
        .context("running git branch --show-current")?;
    if !out.status.success() {
        return Err(anyhow!(
            "git branch --show-current failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { return Err(anyhow!("not on a branch (detached HEAD?)")); }
    Ok(s)
}

/// `git push -u origin <branch>`. Returns the captured stderr on failure (gh's
/// useful messages tend to land there).
pub async fn push_branch(path: &Path, branch: &str) -> Result<()> {
    let out = Command::new("git")
        .args(["push", "-u", "origin", branch])
        .current_dir(path)
        .output()
        .await
        .context("running git push")?;
    if !out.status.success() {
        return Err(anyhow!(
            "git push failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// `gh pr create --base <base> --head <head> --title <t> --body <b>`. `gh`
/// figures out fork→upstream when run inside a fork clone.
pub async fn create_pr(
    path: &Path,
    base: &str,
    head: &str,
    title: &str,
    body: &str,
) -> Result<CreatedPr> {
    let out = Command::new("gh")
        .args([
            "pr", "create",
            "--base", base,
            "--head", head,
            "--title", title,
            "--body", body,
        ])
        .current_dir(path)
        .output()
        .await
        .context("running gh pr create")?;
    if !out.status.success() {
        return Err(anyhow!(
            "gh pr create failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    // gh prints the URL on its own line. Pull the last URL-looking line.
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let url = stdout
        .lines()
        .rev()
        .find(|l| l.starts_with("https://"))
        .map(|s| s.trim().to_string())
        .ok_or_else(|| anyhow!("gh pr create succeeded but no URL in output:\n{stdout}"))?;
    let number = url
        .rsplit('/')
        .next()
        .and_then(|s| s.parse::<u64>().ok())
        .ok_or_else(|| anyhow!("could not parse PR number from URL: {url}"))?;
    Ok(CreatedPr { url, number })
}

/// `gh pr edit <num> --add-reviewer <handle>` (run from inside the worktree
/// so `gh` infers the repo).
pub async fn add_reviewer(path: &Path, pr_number: u64, gh_handle: &str) -> Result<()> {
    let out = Command::new("gh")
        .args([
            "pr", "edit",
            &pr_number.to_string(),
            "--add-reviewer", gh_handle,
        ])
        .current_dir(path)
        .output()
        .await
        .context("running gh pr edit --add-reviewer")?;
    if !out.status.success() {
        return Err(anyhow!(
            "gh pr edit failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// Open PRs where the current user is a requested reviewer.
pub async fn search_review_requested() -> Result<Vec<PrSummary>> {
    let out = Command::new("gh")
        .args([
            "search", "prs",
            "--review-requested", "@me",
            "--state", "open",
            "--limit", "50",
            "--json", "number,title,url,repository,author",
        ])
        .output()
        .await
        .context("running gh search prs --review-requested")?;
    if !out.status.success() {
        return Err(anyhow!(
            "gh search prs failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    // Search response doesn't include headRefName — we have to follow up per PR.
    #[derive(Deserialize)]
    struct Hit {
        number: u64,
        title: String,
        url: String,
        repository: HitRepo,
        author: Option<HitAuthor>,
    }
    #[derive(Deserialize)]
    struct HitRepo {
        #[serde(rename = "nameWithOwner")]
        name_with_owner: String,
    }
    #[derive(Deserialize)]
    struct HitAuthor { login: Option<String> }
    let hits: Vec<Hit> = serde_json::from_slice(&out.stdout)
        .context("parsing gh search prs JSON")?;
    let mut prs = Vec::with_capacity(hits.len());
    for h in hits {
        let head = pr_head_branch(&h.repository.name_with_owner, h.number).await.unwrap_or_default();
        prs.push(PrSummary {
            number: h.number,
            title: h.title,
            url: h.url,
            head_branch: head,
            repo: h.repository.name_with_owner,
            author: h.author.and_then(|a| a.login),
        });
    }
    Ok(prs)
}

async fn pr_head_branch(repo: &str, number: u64) -> Result<String> {
    let (head, _state) = pr_head_and_state(repo, number).await?;
    Ok(head)
}

/// Returns (head branch, state). State is "OPEN" / "CLOSED" / "MERGED".
async fn pr_head_and_state(repo: &str, number: u64) -> Result<(String, String)> {
    let out = Command::new("gh")
        .args([
            "pr", "view", &number.to_string(),
            "-R", repo,
            "--json", "headRefName,state",
        ])
        .output()
        .await
        .context("running gh pr view")?;
    if !out.status.success() {
        return Err(anyhow!(
            "gh pr view failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    #[derive(Deserialize)]
    struct V { #[serde(rename = "headRefName")] head: String, state: String }
    let v: V = serde_json::from_slice(&out.stdout)
        .context("parsing gh pr view JSON")?;
    Ok((v.head, v.state))
}

/// `gh api notifications` filtered to PR-shaped entries the user might care
/// about. We complement `search_review_requested` because notifications also
/// catch @-mentions in PR comments, not just review requests.
pub async fn notifications() -> Result<Vec<PrSummary>> {
    let out = Command::new("gh")
        .args([
            "api", "notifications",
            "--paginate",
            "-q", "[.[] | select(.subject.type==\"PullRequest\")]",
        ])
        .output()
        .await
        .context("running gh api notifications")?;
    if !out.status.success() {
        return Err(anyhow!(
            "gh api notifications failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    #[derive(Deserialize)]
    struct Note {
        subject: Subject,
        repository: NoteRepo,
    }
    #[derive(Deserialize)]
    struct Subject { title: String, url: String }
    #[derive(Deserialize)]
    struct NoteRepo {
        #[serde(rename = "full_name")]
        full_name: String,
    }
    // Each `--paginate` page comes back as its own array; jq's `[.[] |...]`
    // wraps each. Concatenate them.
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut prs = Vec::new();
    for chunk in stdout.split("][").map(|s| s.trim().to_string()) {
        if chunk.is_empty() { continue; }
        let chunk = if chunk.starts_with('[') && chunk.ends_with(']') {
            chunk
        } else if chunk.starts_with('[') {
            format!("{chunk}]")
        } else if chunk.ends_with(']') {
            format!("[{chunk}")
        } else {
            format!("[{chunk}]")
        };
        let notes: Vec<Note> = match serde_json::from_str(&chunk) {
            Ok(n) => n,
            Err(_) => continue,
        };
        for n in notes {
            // Notification URL is the API form (.../pulls/123); convert to /pull/.
            let pr_number = n
                .subject
                .url
                .rsplit('/')
                .next()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            if pr_number == 0 { continue; }
            let (head, state) = match pr_head_and_state(&n.repository.full_name, pr_number).await {
                Ok(v) => v,
                Err(_) => continue,
            };
            // Skip closed / merged PRs — user only cares about active reviews.
            if !state.eq_ignore_ascii_case("OPEN") { continue; }
            let url = format!("https://github.com/{}/pull/{}", n.repository.full_name, pr_number);
            prs.push(PrSummary {
                number: pr_number,
                title: n.subject.title,
                url,
                head_branch: head,
                repo: n.repository.full_name,
                author: None,
            });
        }
    }
    Ok(prs)
}

/// Issue-level comments on a PR (the conversation thread, not per-line review
/// comments). Returns oldest-first.
pub async fn pr_comments(repo: &str, number: u64) -> Result<Vec<(String, String, String)>> {
    let path = format!("repos/{}/issues/{}/comments?per_page=100", repo, number);
    let out = Command::new("gh")
        .args(["api", "--paginate", &path])
        .output()
        .await
        .context("running gh api repos/.../comments")?;
    if !out.status.success() {
        return Err(anyhow!(
            "gh api comments failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    #[derive(Deserialize)]
    struct GhComment { user: GhUser, created_at: String, body: String }
    #[derive(Deserialize)]
    struct GhUser { login: String }
    // `--paginate` concatenates JSON arrays; split on `][` like in notifications().
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut all: Vec<(String, String, String)> = Vec::new();
    for chunk in stdout.split("][").map(|s| s.trim().to_string()) {
        if chunk.is_empty() { continue; }
        let chunk = if chunk.starts_with('[') && chunk.ends_with(']') {
            chunk
        } else if chunk.starts_with('[') {
            format!("{chunk}]")
        } else if chunk.ends_with(']') {
            format!("[{chunk}")
        } else {
            format!("[{chunk}]")
        };
        let comments: Vec<GhComment> = match serde_json::from_str(&chunk) {
            Ok(v) => v,
            Err(_) => continue,
        };
        for c in comments {
            all.push((c.user.login, c.created_at, c.body));
        }
    }
    Ok(all)
}

/// The current user's GitHub login (`gh api user -q .login`).
pub async fn whoami() -> Result<String> {
    let out = Command::new("gh")
        .args(["api", "user", "-q", ".login"])
        .output()
        .await
        .context("running gh api user")?;
    if !out.status.success() {
        return Err(anyhow!(
            "gh api user failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}
