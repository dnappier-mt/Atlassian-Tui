use crate::config::ProjectEntry;
use crate::ticket::Ticket;
use anyhow::{anyhow, Context, Result};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

/// Ask the `claude` CLI to pick up to two matching projects for a ticket. Returns up
/// to 2 known project paths in claude's order of relevance. Empty vec means "no
/// match". `Err` only on actual invocation failure.
pub async fn suggest_projects(
    ticket: &Ticket,
    projects: &[ProjectEntry],
) -> Result<Vec<PathBuf>> {
    if projects.is_empty() {
        return Ok(vec![]);
    }
    if which::which("claude").is_err() && tokio::fs::metadata("/usr/local/bin/claude").await.is_err() {
        return Err(anyhow!("`claude` CLI not on PATH"));
    }
    let prompt = build_prompt(ticket, projects);
    let result = tokio::time::timeout(Duration::from_secs(120), run_claude(&prompt))
        .await
        .map_err(|_| anyhow!("claude timed out after 120s"))??;
    let trimmed = result.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("none") {
        return Ok(vec![]);
    }
    let mut picks: Vec<PathBuf> = Vec::new();
    for line in trimmed.lines().filter(|l| !l.trim().is_empty()).take(2) {
        let line = line.trim();
        if line.eq_ignore_ascii_case("none") {
            continue;
        }
        if let Some(p) = match_project(line, projects) {
            if !picks.iter().any(|x| *x == p) {
                picks.push(p);
            }
        }
    }
    Ok(picks)
}

fn match_project(line: &str, projects: &[ProjectEntry]) -> Option<PathBuf> {
    let candidate = PathBuf::from(line);
    if let Some(p) = projects.iter().find(|p| p.path == candidate) {
        return Some(p.path.clone());
    }
    let lower = line.to_ascii_lowercase();
    for p in projects {
        let basename = p.path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        if basename.eq_ignore_ascii_case(line)
            || p.nickname.as_deref().map(|n| n.eq_ignore_ascii_case(line)).unwrap_or(false)
            || p.path.display().to_string().to_ascii_lowercase() == lower
        {
            return Some(p.path.clone());
        }
    }
    None
}

/// Ask Claude Code to propose an implementation for a ticket given access to one or
/// more project directories. Returns Markdown text. Long-running call (`claude -p`
/// with `--add-dir`), so the timeout is generous.
pub async fn propose_implementation(
    ticket: &Ticket,
    project_paths: &[std::path::PathBuf],
) -> Result<String> {
    if which::which("claude").is_err() {
        return Err(anyhow!("`claude` CLI not on PATH"));
    }
    let prompt = build_implementation_prompt(ticket, project_paths);
    let result = tokio::time::timeout(
        Duration::from_secs(600),
        run_claude_with_dirs(&prompt, project_paths),
    )
    .await
    .map_err(|_| anyhow!("claude implementation request timed out after 600s"))??;
    Ok(result.trim().to_string())
}

fn build_implementation_prompt(ticket: &Ticket, project_paths: &[std::path::PathBuf]) -> String {
    let breadcrumb = match (
        ticket.grandparent_summary.as_deref().or(ticket.grandparent_key.as_deref()),
        ticket.parent_summary.as_deref().or(ticket.parent_key.as_deref()),
    ) {
        (Some(g), Some(p)) => format!("{g} > {p}"),
        (Some(g), None) => g.to_string(),
        (None, Some(p)) => p.to_string(),
        _ => "—".into(),
    };
    let mut paths_block = String::new();
    for (i, p) in project_paths.iter().enumerate() {
        paths_block.push_str(&format!("{}. {}\n", i + 1, p.display()));
    }
    format!(
        "You are a senior engineer reviewing a Jira ticket and suggesting how to \
implement it in the linked project(s) below. You have read access to the project \
files via your tools — explore the relevant code to ground your suggestion.\n\
\n\
# Ticket\n\
key: {key}\n\
type: {issue_type}\n\
priority: {priority}\n\
status: {status}\n\
breadcrumb: {breadcrumb}\n\
summary: {summary}\n\
description:\n\
{description}\n\
\n\
# Linked project(s)\n\
{paths}\n\
# Output (Markdown only)\n\
Reply with focused Markdown using these sections (omit any section that doesn't apply):\n\
\n\
## Overview\n\
2-3 sentences on what needs doing.\n\
\n\
## Affected files\n\
A list of likely files with one-line reasons. If you couldn't find the area, say so.\n\
\n\
## Implementation steps\n\
A numbered, actionable checklist.\n\
\n\
## Risks / open questions\n\
Anything ambiguous, blocking, or worth confirming before coding.\n\
\n\
Do not write code blocks unless they're tiny, unavoidable signatures or examples. \
Be concise. Output Markdown only — no preamble, no \"Sure, here's...\".",
        key = ticket.key,
        issue_type = ticket.issue_type.as_deref().unwrap_or("?"),
        priority = ticket.priority.as_deref().unwrap_or("?"),
        status = ticket.status,
        breadcrumb = breadcrumb,
        summary = ticket.summary,
        description = ticket.description.as_deref().unwrap_or("(none)"),
        paths = paths_block,
    )
}

async fn run_claude_with_dirs(
    prompt: &str,
    dirs: &[std::path::PathBuf],
) -> Result<String> {
    let mut cmd = Command::new("claude");
    cmd.arg("-p").arg("--output-format").arg("text");
    // `--add-dir <directories...>` is variadic — without `=` it would swallow our
    // prompt as another directory. Use the `--add-dir=PATH` form (one flag per dir).
    for d in dirs {
        if d.exists() {
            cmd.arg(format!("--add-dir={}", d.display()));
        }
    }
    cmd.arg(prompt);
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let out = cmd.output().await.context("invoking claude CLI for implementation")?;
    if !out.status.success() {
        return Err(anyhow!(
            "claude exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

async fn run_claude(prompt: &str) -> Result<String> {
    let out = Command::new("claude")
        .args(["-p", "--output-format", "text"])
        .arg(prompt)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("invoking claude CLI")?;
    if !out.status.success() {
        return Err(anyhow!(
            "claude exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn build_prompt(ticket: &Ticket, projects: &[ProjectEntry]) -> String {
    let mut toml = String::new();
    for p in projects {
        toml.push_str("[[project]]\n");
        toml.push_str(&format!("path = {:?}\n", p.path.display().to_string()));
        if let Some(nick) = &p.nickname {
            toml.push_str(&format!("nickname = {nick:?}\n"));
        }
        toml.push('\n');
    }
    let breadcrumb = match (
        ticket.grandparent_summary.as_deref().or(ticket.grandparent_key.as_deref()),
        ticket.parent_summary.as_deref().or(ticket.parent_key.as_deref()),
    ) {
        (Some(g), Some(p)) => format!("{g} > {p}"),
        (Some(g), None) => g.to_string(),
        (None, Some(p)) => p.to_string(),
        _ => "—".into(),
    };
    format!(
        "You are matching a Jira ticket to one of the configured local code project \
directories below. Choose the project most likely to contain the code this ticket \
will affect. If no project is a clear match, reply with the literal word \"none\".\n\
\n\
# Configured projects (TOML)\n\
{toml}\n\
# Ticket\n\
key: {key}\n\
type: {issue_type}\n\
status: {status}\n\
breadcrumb: {breadcrumb}\n\
summary: {summary}\n\
description:\n\
{description}\n\
\n\
# Output\n\
Reply with up to TWO lines, each containing the absolute path of a matching project \
from the list above, ordered most-relevant first. If only one project is a clear match, \
reply with just one line. If none match, reply with the literal word \"none\". No \
quotes, no commentary, no numbering, no extra whitespace — just paths separated by \
newlines, or the word \"none\".",
        toml = toml,
        key = ticket.key,
        issue_type = ticket.issue_type.as_deref().unwrap_or("?"),
        status = ticket.status,
        breadcrumb = breadcrumb,
        summary = ticket.summary,
        description = ticket.description.as_deref().unwrap_or("(none)"),
    )
}
