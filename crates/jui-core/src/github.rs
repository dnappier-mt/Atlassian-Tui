//! Thin async wrappers around the `gh` CLI. We deliberately shell out instead
//! of hitting the GitHub REST API ourselves so we inherit the user's existing
//! `gh auth` credentials.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use tokio::process::Command;

/// One comment from a GitHub PR. Tied back to its Jira ticket via
/// `ticket_key` (the daemon resolves the key from the PR's branch name when
/// caching). The reply flow routes on `kind`: "issue" (top-level thread),
/// "review" (per-line on the diff — supports threaded replies), or
/// "review_wrapper" (the body of a PR review).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrComment {
    pub ticket_key: String,
    pub pr_url: String,
    pub pr_number: u64,
    pub repo: String,
    pub author: String,
    pub created: String,
    pub body: String,
    /// GitHub's numeric comment id (as a string for IPC simplicity).
    /// Empty when the row was cached pre-migration.
    #[serde(default)]
    pub comment_id: String,
    /// "issue" | "review" | "review_wrapper". Defaulted to "issue" when
    /// loaded from a pre-migration row.
    #[serde(default = "default_pr_comment_kind")]
    pub kind: String,
    /// True for review-thread comments whose thread is resolved on GitHub.
    /// Always false for "issue" and "review_wrapper" kinds — they don't
    /// belong to a thread.
    #[serde(default)]
    pub is_resolved: bool,
    /// Parent comment's id when this row is a threaded reply (only set for
    /// `kind = "review"`). Empty string = top-level. Drives the indent +
    /// `↳` decoration in the PR Comments pane.
    #[serde(default)]
    pub in_reply_to_id: String,
}

fn default_pr_comment_kind() -> String { "issue".to_string() }

/// What the github::pr_* fetchers return before the daemon enriches with
/// ticket_key / pr_url / pr_number / repo. Five fields keep the fetch
/// helpers focused; the daemon promotes these into `PrComment`s.
#[derive(Debug, Clone)]
pub struct FetchedComment {
    pub id: String,
    pub kind: String,
    pub author: String,
    pub created: String,
    pub body: String,
    /// Filled in for `kind = "review"` after the GraphQL thread-state lookup.
    /// Other kinds always carry `false`.
    pub is_resolved: bool,
    /// Parent's REST id for review replies; empty for top-level / non-review.
    pub in_reply_to_id: String,
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

/// `git push -u <remote> <branch>`. Returns the captured stderr on failure
/// (gh's useful messages tend to land there). Caller picks the remote — in
/// fork workflows pushing to `origin` (= upstream) fails with "Write access
/// not granted", so the daemon resolves the user's preferred remote first.
pub async fn push_branch(path: &Path, branch: &str, remote: &str) -> Result<()> {
    let out = Command::new("git")
        .args(["push", "-u", remote, branch])
        .current_dir(path)
        .output()
        .await
        .context("running git push")?;
    if !out.status.success() {
        return Err(anyhow!(
            "git push to {remote} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// `git remote -v` parsed into (name, fetch_url) pairs. The push URL is
/// usually identical; we return fetch since that's what `gh` matches against.
pub async fn list_remotes(path: &Path) -> Result<Vec<(String, String)>> {
    let out = Command::new("git")
        .args(["remote", "-v"])
        .current_dir(path)
        .output()
        .await
        .context("running git remote -v")?;
    if !out.status.success() {
        return Err(anyhow!(
            "git remote -v failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut out_remotes: Vec<(String, String)> = Vec::new();
    for line in stdout.lines() {
        // Each line: "<name>\t<url> (fetch|push)"
        if !line.contains("(fetch)") { continue; }
        let mut parts = line.split_whitespace();
        let Some(name) = parts.next() else { continue };
        let Some(url) = parts.next() else { continue };
        if !out_remotes.iter().any(|(n, _)| n == name) {
            out_remotes.push((name.to_string(), url.to_string()));
        }
    }
    Ok(out_remotes)
}

/// `gh pr create --base <base> --head <head> --title <t> --body <b>`.
///
/// When `head_owner` is `Some` (cross-fork PR), the head ref is rewritten to
/// `<owner>:<branch>` so GitHub knows the branch lives on a fork — without
/// this, `gh` looks up the branch on the *target* repo (upstream) and fails
/// with "No commits between …" because the branch only exists on the fork.
pub async fn create_pr(
    path: &Path,
    base: &str,
    head_branch: &str,
    head_owner: Option<&str>,
    title: &str,
    body: &str,
) -> Result<CreatedPr> {
    let head_ref = match head_owner {
        Some(owner) => format!("{owner}:{head_branch}"),
        None => head_branch.to_string(),
    };
    let out = Command::new("gh")
        .args([
            "pr", "create",
            "--base", base,
            "--head", &head_ref,
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

/// Open PRs the user authored (`--author=@me`). Used to flag Jira tickets
/// where the user has already sent the PR — those rows sort below
/// not-yet-PR'd tickets in the tree.
pub async fn search_authored_open() -> Result<Vec<PrSummary>> {
    run_search(&["--author", "@me"]).await
}

/// Open PRs the user is involved with as a reviewer — covers BOTH:
///   - `--review-requested=@me` (someone tagged you on the PR)
///   - `--reviewed-by=@me` (you already left a review, even if just PENDING)
/// Some workflows assign reviewers via Jira comments rather than GitHub's
/// review-request mechanism — once the user starts a review, this catches it.
pub async fn search_review_requested() -> Result<Vec<PrSummary>> {
    let mut prs = run_search(&["--review-requested", "@me"]).await.unwrap_or_default();
    let extra = run_search(&["--reviewed-by", "@me"]).await.unwrap_or_default();
    // Dedupe by URL.
    let mut seen: std::collections::HashSet<String> =
        prs.iter().map(|p| p.url.clone()).collect();
    for p in extra {
        if seen.insert(p.url.clone()) {
            prs.push(p);
        }
    }
    Ok(prs)
}

/// Run `gh search prs <qualifier> --state open --limit 50 --json ...` and
/// turn the response into PrSummary rows (one extra `gh pr view` per row to
/// recover the head branch).
async fn run_search(qualifier: &[&str]) -> Result<Vec<PrSummary>> {
    let mut args: Vec<&str> = vec!["search", "prs"];
    args.extend_from_slice(qualifier);
    args.extend_from_slice(&[
        "--state", "open",
        "--limit", "50",
        "--json", "number,title,url,repository,author",
    ]);
    let out = Command::new("gh").args(&args).output().await.context("running gh search prs")?;
    if !out.status.success() {
        return Err(anyhow!(
            "gh search prs {qualifier:?} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
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
pub async fn pr_comments(repo: &str, number: u64) -> Result<Vec<FetchedComment>> {
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
    struct GhComment { id: u64, user: GhUser, created_at: String, body: String }
    #[derive(Deserialize)]
    struct GhUser { login: String }
    // `--paginate` concatenates JSON arrays; split on `][` like in notifications().
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut all: Vec<FetchedComment> = Vec::new();
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
            all.push(FetchedComment {
                id: c.id.to_string(),
                kind: "issue".to_string(),
                author: c.user.login,
                created: c.created_at,
                body: c.body,
                is_resolved: false,
                in_reply_to_id: String::new(),
            });
        }
    }
    Ok(all)
}

/// Per-line review comments on a PR (the kind Copilot and human reviewers
/// leave inline on the diff). The body is prefixed with `[path:line] ` so
/// the source location is visible in the flat PR-comments pane.
pub async fn pr_review_comments(repo: &str, number: u64) -> Result<Vec<FetchedComment>> {
    let path = format!("repos/{}/pulls/{}/comments?per_page=100", repo, number);
    let out = Command::new("gh")
        .args(["api", "--paginate", &path])
        .output()
        .await
        .context("running gh api repos/.../pulls/N/comments")?;
    if !out.status.success() {
        return Err(anyhow!(
            "gh api review comments failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    #[derive(Deserialize)]
    struct GhReviewComment {
        id: u64,
        user: GhUser,
        created_at: String,
        body: String,
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        line: Option<u64>,
        #[serde(default)]
        in_reply_to_id: Option<u64>,
    }
    #[derive(Deserialize)]
    struct GhUser { login: String }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut all: Vec<FetchedComment> = Vec::new();
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
        let comments: Vec<GhReviewComment> = match serde_json::from_str(&chunk) {
            Ok(v) => v,
            Err(_) => continue,
        };
        for c in comments {
            let loc = match (c.path.as_deref(), c.line) {
                (Some(p), Some(l)) => format!("[{p}:{l}] "),
                (Some(p), None) => format!("[{p}] "),
                _ => String::new(),
            };
            all.push(FetchedComment {
                id: c.id.to_string(),
                kind: "review".to_string(),
                author: c.user.login,
                created: c.created_at,
                body: format!("{loc}{}", c.body),
                is_resolved: false,
                in_reply_to_id: c.in_reply_to_id.map(|n| n.to_string()).unwrap_or_default(),
            });
        }
    }
    Ok(all)
}

/// Top-level reviews on a PR (the `gh pr review` wrappers). Returns only
/// reviews that carry an actual body — reviews that just record APPROVED /
/// CHANGES_REQUESTED with no commentary aren't useful in the pane.
pub async fn pr_reviews(repo: &str, number: u64) -> Result<Vec<FetchedComment>> {
    let path = format!("repos/{}/pulls/{}/reviews?per_page=100", repo, number);
    let out = Command::new("gh")
        .args(["api", "--paginate", &path])
        .output()
        .await
        .context("running gh api repos/.../pulls/N/reviews")?;
    if !out.status.success() {
        return Err(anyhow!(
            "gh api reviews failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    #[derive(Deserialize)]
    struct GhReview {
        id: u64,
        user: Option<GhUser>,
        #[serde(default)]
        submitted_at: Option<String>,
        #[serde(default)]
        body: String,
        #[serde(default)]
        state: String,
    }
    #[derive(Deserialize)]
    struct GhUser { login: String }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut all: Vec<FetchedComment> = Vec::new();
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
        let reviews: Vec<GhReview> = match serde_json::from_str(&chunk) {
            Ok(v) => v,
            Err(_) => continue,
        };
        for r in reviews {
            if r.body.trim().is_empty() { continue; }
            let Some(user) = r.user else { continue };
            let when = r.submitted_at.unwrap_or_default();
            let tag = if r.state.is_empty() {
                String::new()
            } else {
                format!("[review · {}] ", r.state.to_ascii_lowercase())
            };
            all.push(FetchedComment {
                id: r.id.to_string(),
                kind: "review_wrapper".to_string(),
                author: user.login,
                created: when,
                body: format!("{tag}{}", r.body),
                is_resolved: false,
                in_reply_to_id: String::new(),
            });
        }
    }
    Ok(all)
}

/// PR body markdown (`gh pr view -R <repo> <num> --json body -q .body`).
pub async fn pr_body(repo: &str, number: u64) -> Result<String> {
    let out = Command::new("gh")
        .args([
            "pr", "view", &number.to_string(),
            "-R", repo,
            "--json", "body",
            "-q", ".body",
        ])
        .output()
        .await
        .context("running gh pr view (body)")?;
    if !out.status.success() {
        return Err(anyhow!(
            "gh pr view (body) failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// PR author's GitHub login (`gh pr view -R <repo> <num> --json author -q .author.login`).
pub async fn pr_author_login(repo: &str, number: u64) -> Result<String> {
    let out = Command::new("gh")
        .args([
            "pr", "view", &number.to_string(),
            "-R", repo,
            "--json", "author",
            "-q", ".author.login",
        ])
        .output()
        .await
        .context("running gh pr view")?;
    if !out.status.success() {
        return Err(anyhow!(
            "gh pr view (author) failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// First name of the GitHub user as set on their public profile, sanitised for
/// use as a git remote name (lowercase alphanumeric + hyphen, no leading or
/// trailing hyphens). Falls back to the login if the user has no name set.
pub async fn user_first_name_remote_safe(login: &str) -> Result<String> {
    let out = Command::new("gh")
        .args([
            "api", &format!("users/{login}"),
            "-q", ".name",
        ])
        .output()
        .await
        .context("running gh api users/<login>")?;
    let raw = if out.status.success() {
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    } else {
        String::new()
    };
    let candidate = if raw.is_empty() {
        login.to_string()
    } else {
        raw.split_whitespace().next().unwrap_or(login).to_string()
    };
    Ok(sanitize_remote_name(&candidate))
}

/// Lowercase, replace runs of non-alphanumeric with `-`, trim leading/trailing
/// `-`. Returns `"contributor"` if the result is empty.
fn sanitize_remote_name(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_dash = false;
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() { "contributor".to_string() } else { trimmed }
}

/// Post an issue-level comment on a PR (`gh pr comment`).
pub async fn post_pr_comment(repo: &str, number: u64, body: &str) -> Result<()> {
    let out = Command::new("gh")
        .args([
            "pr", "comment", &number.to_string(),
            "-R", repo,
            "--body", body,
        ])
        .output()
        .await
        .context("running gh pr comment")?;
    if !out.status.success() {
        return Err(anyhow!(
            "gh pr comment failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// Add a reaction to a PR's top-level body (the PR description). A PR is an
/// issue at the REST level, so reactions go on the issues endpoint. `content`
/// is a GitHub reaction name, e.g. "rocket", "+1", "hooray".
pub async fn add_pr_reaction(repo: &str, number: u64, content: &str) -> Result<()> {
    let out = Command::new("gh")
        .args([
            "api", "-X", "POST",
            "-H", "Accept: application/vnd.github+json",
            &format!("repos/{repo}/issues/{number}/reactions"),
            "-f", &format!("content={content}"),
        ])
        .output()
        .await
        .context("running gh api reactions")?;
    if !out.status.success() {
        return Err(anyhow!(
            "gh api reactions failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// Map of review-comment REST `databaseId` → `isResolved` for every comment
/// in every review thread on the PR. The merge pass uses this to flag
/// `kind = "review"` cached comments. Empty map on any GraphQL failure —
/// caller treats those as unresolved (worst case the user sees a thread
/// they thought was hidden, never the other way around).
pub async fn review_thread_resolution_map(
    repo: &str,
    number: u64,
) -> Result<std::collections::HashMap<String, bool>> {
    let (owner, name) = repo.split_once('/')
        .ok_or_else(|| anyhow!("repo must be `<owner>/<name>`, got: {repo}"))?;
    let query = format!(
        "query {{ repository(owner: \"{owner}\", name: \"{name}\") {{ \
            pullRequest(number: {number}) {{ \
                reviewThreads(first: 100) {{ \
                    nodes {{ isResolved comments(first: 100) {{ nodes {{ databaseId }} }} }} \
                }} \
            }} \
        }} }}",
    );
    let out = Command::new("gh")
        .args(["api", "graphql", "-f", &format!("query={query}")])
        .output()
        .await
        .context("running gh api graphql (resolution map)")?;
    if !out.status.success() {
        return Err(anyhow!(
            "gh api graphql failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    #[derive(Deserialize)]
    struct Resp { data: Data }
    #[derive(Deserialize)]
    struct Data { repository: Repo }
    #[derive(Deserialize)]
    struct Repo { #[serde(rename = "pullRequest")] pull_request: Pr }
    #[derive(Deserialize)]
    struct Pr { #[serde(rename = "reviewThreads")] review_threads: Threads }
    #[derive(Deserialize)]
    struct Threads { nodes: Vec<Thread> }
    #[derive(Deserialize)]
    struct Thread {
        #[serde(rename = "isResolved")] is_resolved: bool,
        comments: ThreadComments,
    }
    #[derive(Deserialize)]
    struct ThreadComments { nodes: Vec<ThreadComment> }
    #[derive(Deserialize)]
    struct ThreadComment { #[serde(rename = "databaseId")] database_id: Option<i64> }
    let resp: Resp = serde_json::from_slice(&out.stdout)
        .context("parsing reviewThreads JSON")?;
    let mut map = std::collections::HashMap::new();
    for t in resp.data.repository.pull_request.review_threads.nodes {
        for c in t.comments.nodes {
            if let Some(id) = c.database_id {
                map.insert(id.to_string(), t.is_resolved);
            }
        }
    }
    Ok(map)
}

/// Mark a review-comment thread as resolved on GitHub (the "Resolve
/// conversation" button). Only works for inline review comments — issue
/// comments and review wrappers have no thread to resolve.
///
/// GitHub doesn't expose thread resolution in REST, so we go through
/// GraphQL: query the PR's `reviewThreads`, find the thread whose comments
/// contain the given REST `databaseId`, then call `resolveReviewThread`
/// with that thread's GraphQL node id.
pub async fn resolve_review_thread(
    repo: &str,
    number: u64,
    comment_id: &str,
) -> Result<()> {
    let (owner, name) = repo.split_once('/')
        .ok_or_else(|| anyhow!("repo must be `<owner>/<name>`, got: {repo}"))?;
    let target_db_id: i64 = comment_id.parse()
        .with_context(|| format!("comment_id must be numeric, got: {comment_id}"))?;
    // 1. List threads. Single query is enough for normal PRs (<100 threads).
    let query = format!(
        "query {{ repository(owner: \"{owner}\", name: \"{name}\") {{ \
            pullRequest(number: {number}) {{ \
                reviewThreads(first: 100) {{ \
                    nodes {{ id isResolved comments(first: 100) {{ nodes {{ databaseId }} }} }} \
                }} \
            }} \
        }} }}",
    );
    let out = Command::new("gh")
        .args(["api", "graphql", "-f", &format!("query={query}")])
        .output()
        .await
        .context("running gh api graphql (list threads)")?;
    if !out.status.success() {
        return Err(anyhow!(
            "gh api graphql failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    #[derive(Deserialize)]
    struct Resp { data: Data }
    #[derive(Deserialize)]
    struct Data { repository: Repo }
    #[derive(Deserialize)]
    struct Repo { #[serde(rename = "pullRequest")] pull_request: Pr }
    #[derive(Deserialize)]
    struct Pr { #[serde(rename = "reviewThreads")] review_threads: Threads }
    #[derive(Deserialize)]
    struct Threads { nodes: Vec<Thread> }
    #[derive(Deserialize)]
    struct Thread {
        id: String,
        #[serde(rename = "isResolved")] is_resolved: bool,
        comments: ThreadComments,
    }
    #[derive(Deserialize)]
    struct ThreadComments { nodes: Vec<ThreadComment> }
    #[derive(Deserialize)]
    struct ThreadComment { #[serde(rename = "databaseId")] database_id: Option<i64> }
    let resp: Resp = serde_json::from_slice(&out.stdout)
        .context("parsing reviewThreads JSON")?;
    let thread = resp.data.repository.pull_request.review_threads.nodes
        .into_iter()
        .find(|t| t.comments.nodes.iter().any(|c| c.database_id == Some(target_db_id)));
    let Some(thread) = thread else {
        return Err(anyhow!(
            "no review thread contains comment {comment_id} (not a review comment?)"
        ));
    };
    if thread.is_resolved {
        return Ok(()); // idempotent — already done
    }
    // 2. Resolve it.
    let mutation = format!(
        "mutation {{ resolveReviewThread(input: {{ threadId: \"{}\" }}) {{ \
            thread {{ isResolved }} \
        }} }}",
        thread.id,
    );
    let out = Command::new("gh")
        .args(["api", "graphql", "-f", &format!("query={mutation}")])
        .output()
        .await
        .context("running gh api graphql (resolve mutation)")?;
    if !out.status.success() {
        return Err(anyhow!(
            "gh api graphql resolve failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// Post a threaded reply to a per-line review comment. Uses the dedicated
/// `/pulls/{n}/comments/{id}/replies` endpoint so the new comment lands
/// inline on the same conversation, not as a fresh top-level review.
pub async fn post_review_comment_reply(
    repo: &str,
    number: u64,
    parent_id: &str,
    body: &str,
) -> Result<()> {
    let path = format!("repos/{}/pulls/{}/comments/{}/replies", repo, number, parent_id);
    let out = Command::new("gh")
        .args([
            "api",
            "--method", "POST",
            "-H", "Accept: application/vnd.github+json",
            &path,
            "-f", &format!("body={body}"),
        ])
        .output()
        .await
        .context("running gh api .../comments/<id>/replies")?;
    if !out.status.success() {
        return Err(anyhow!(
            "gh api reply failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// Most-recent review state submitted by `my_login` on the given PR. Returns
/// `None` when the user hasn't reviewed it yet. Possible values per GitHub:
/// `"APPROVED"`, `"CHANGES_REQUESTED"`, `"COMMENTED"`, `"DISMISSED"`.
pub async fn my_latest_review_state(
    repo: &str,
    number: u64,
    my_login: &str,
) -> Result<Option<String>> {
    let path = format!("repos/{repo}/pulls/{number}/reviews?per_page=100");
    let out = Command::new("gh")
        .args(["api", "--paginate", &path])
        .output()
        .await
        .context("running gh api repos/.../pulls/.../reviews")?;
    if !out.status.success() {
        return Err(anyhow!(
            "gh api reviews failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    #[derive(Deserialize)]
    struct Review {
        user: ReviewUser,
        state: String,
        submitted_at: Option<String>,
    }
    #[derive(Deserialize)]
    struct ReviewUser { login: String }

    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut latest: Option<(String, String)> = None;
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
        let reviews: Vec<Review> = match serde_json::from_str(&chunk) {
            Ok(v) => v,
            Err(_) => continue,
        };
        for r in reviews {
            if !r.user.login.eq_ignore_ascii_case(my_login) { continue; }
            // Track the chronologically last one. submitted_at is RFC3339 so
            // string comparison is fine.
            let when = r.submitted_at.unwrap_or_default();
            match &latest {
                Some((cur_when, _)) if cur_when >= &when => {}
                _ => latest = Some((when, r.state)),
            }
        }
    }
    Ok(latest.map(|(_, s)| s))
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
