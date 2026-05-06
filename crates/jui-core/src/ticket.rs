use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Ticket {
    pub key: String,
    pub summary: String,
    pub status: String,
    pub assignee: Option<String>,
    pub reporter: Option<String>,
    #[serde(default)]
    pub priority: Option<String>,
    #[serde(default)]
    pub issue_type: Option<String>,
    #[serde(default)]
    pub updated: Option<String>,
    #[serde(default)]
    pub created: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
    /// Parent issue's key (subtask → task, task → epic in next-gen).
    #[serde(default)]
    pub parent_key: Option<String>,
    /// Parent issue's summary, populated by daemon-side batch resolution.
    #[serde(default)]
    pub parent_summary: Option<String>,
    /// Parent's issue type (e.g. "Story", "Epic").
    #[serde(default)]
    pub parent_issue_type: Option<String>,
    /// Grandparent (typically the Epic when this is a subtask of a story).
    #[serde(default)]
    pub grandparent_key: Option<String>,
    #[serde(default)]
    pub grandparent_summary: Option<String>,
    /// Confirmed local project paths linked to this ticket. Populated by the daemon at
    /// list/detail time via a join on the ticket_projects table; not stored as a column
    /// on the tickets row itself.
    #[serde(default)]
    pub linked_projects: Vec<String>,
    /// Subtasks (children) of this ticket. Populated by `view` requests; empty when
    /// loaded from cache or list.
    #[serde(default)]
    pub subtasks: Vec<SubtaskRef>,
    #[serde(default)]
    pub original_estimate_seconds: Option<i64>,
    #[serde(default)]
    pub remaining_estimate_seconds: Option<i64>,
    #[serde(default)]
    pub time_spent_seconds: Option<i64>,
}

/// Numeric rank for a priority name. Lower = more urgent (sorts first). Unknown
/// strings get rank 99 so they trail the standard set.
pub fn priority_rank(p: Option<&str>) -> u8 {
    match p.unwrap_or("").to_ascii_lowercase().as_str() {
        "highest" | "blocker" => 1,
        "high" | "critical" => 2,
        "medium" | "major" | "normal" => 3,
        "low" | "minor" => 4,
        "lowest" | "trivial" => 5,
        _ => 99,
    }
}

/// Format a Jira ISO-ish timestamp (e.g. "2025-08-11T06:03:25.950-0400") as "YYYY-MM-DD".
/// Returns the input verbatim if it can't be parsed.
pub fn fmt_date(raw: &str) -> String {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(raw) {
        return dt.format("%Y-%m-%d").to_string();
    }
    // Jira sometimes uses "-0400" instead of "-04:00"; try a permissive format.
    if let Ok(dt) = chrono::DateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S%.f%z") {
        return dt.format("%Y-%m-%d").to_string();
    }
    raw.split('T').next().unwrap_or(raw).to_string()
}

/// Format a duration in seconds as a Jira-style string ("2h 30m"). Uses 8 h / day.
pub fn fmt_seconds(secs: i64) -> String {
    if secs <= 0 {
        return "0m".to_string();
    }
    const MIN: i64 = 60;
    const HOUR: i64 = 60 * 60;
    const DAY: i64 = 8 * HOUR;
    let days = secs / DAY;
    let mut rem = secs % DAY;
    let hours = rem / HOUR;
    rem %= HOUR;
    let mins = rem / MIN;
    let mut out = String::new();
    if days > 0 { out.push_str(&format!("{days}d ")); }
    if hours > 0 { out.push_str(&format!("{hours}h ")); }
    if mins > 0 { out.push_str(&format!("{mins}m")); }
    let s = out.trim().to_string();
    if s.is_empty() { "0m".to_string() } else { s }
}

impl Ticket {
    /// Empty placeholder used when we know the key but want to fetch full details
    /// asynchronously. All fields except `key` are defaulted.
    pub fn new_stub() -> Self {
        Self {
            key: String::new(),
            summary: String::new(),
            status: String::new(),
            assignee: None,
            reporter: None,
            priority: None,
            issue_type: None,
            updated: None,
            created: None,
            description: None,
            labels: vec![],
            parent_key: None,
            parent_summary: None,
            parent_issue_type: None,
            grandparent_key: None,
            grandparent_summary: None,
            linked_projects: vec![],
            subtasks: vec![],
            original_estimate_seconds: None,
            remaining_estimate_seconds: None,
            time_spent_seconds: None,
        }
    }

    /// True for statuses we want to push to the bottom and dim — currently exact matches
    /// against "done" category names. Case-insensitive so different sites that use
    /// "RESOLVED" / "resolved" still match.
    pub fn is_inactive(&self) -> bool {
        let s = self.status.to_ascii_lowercase();
        matches!(s.as_str(), "resolved" | "done" | "closed")
    }

    /// Branch slug: "<key>-<summary>" lowercased, non-alphanumerics → '-', collapsed, trimmed,
    /// capped at 60 chars.
    pub fn branch_slug(&self) -> String {
        let raw = format!("{}-{}", self.key, self.summary);
        let mut out = String::with_capacity(raw.len());
        let mut last_dash = false;
        for ch in raw.chars() {
            if ch.is_ascii_alphanumeric() {
                out.push(ch.to_ascii_lowercase());
                last_dash = false;
            } else if !last_dash {
                out.push('-');
                last_dash = true;
            }
        }
        let trimmed = out.trim_matches('-').to_string();
        if trimmed.len() > 60 {
            let cut = trimmed[..60].trim_end_matches('-').to_string();
            cut
        } else {
            trimmed
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SubtaskRef {
    pub key: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub issue_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Comment {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub account_id: Option<String>,
    pub author: String,
    pub created: String,
    pub body: String,
}

/// Marker prepended to reply bodies. Visible to other Jira clients (just shows as a
/// quote block) but lets jui detect/render replies. Format:
///   "> [@Author Name on YYYY-MM-DD]: first line of original\n\n<user reply>"
pub const REPLY_PREFIX: &str = "> [@";

#[derive(Debug, Clone)]
pub struct ParsedReply<'a> {
    pub quoted_author: &'a str,
    pub quoted_excerpt: &'a str,
    pub reply_text: &'a str,
}

/// Detect the reply marker in a comment body and split it into its parts.
/// Returns None if the body doesn't begin with the marker.
pub fn parse_reply<'a>(body: &'a str) -> Option<ParsedReply<'a>> {
    let rest = body.strip_prefix(REPLY_PREFIX)?;
    // rest = "Author Name on YYYY-MM-DD]: excerpt\n\nreply"
    let (header, after) = rest.split_once("]: ")?;
    let quoted_author = header.split(" on ").next().unwrap_or(header);
    // The first newline ends the excerpt; the rest (after a blank line) is the reply.
    let (excerpt, reply) = match after.split_once('\n') {
        Some((e, r)) => (e, r.trim_start_matches('\n')),
        None => (after, ""),
    };
    Some(ParsedReply { quoted_author, quoted_excerpt: excerpt, reply_text: reply })
}

/// Build a reply body with the visible quote marker.
pub fn build_reply_body(parent_author: &str, parent_date: &str, parent_body: &str, reply: &str) -> String {
    let excerpt: String = parent_body
        .split('\n')
        .find(|l| !l.trim().is_empty())
        .map(|l| {
            if l.chars().count() > 120 {
                let cut: String = l.chars().take(117).collect();
                format!("{cut}…")
            } else {
                l.to_string()
            }
        })
        .unwrap_or_default();
    format!(
        "{REPLY_PREFIX}{author} on {date}]: {excerpt}\n\n{reply}",
        author = parent_author,
        date = parent_date,
        excerpt = excerpt,
        reply = reply
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(key: &str, summary: &str) -> Ticket {
        Ticket {
            key: key.into(),
            summary: summary.into(),
            status: "Open".into(),
            assignee: None,
            reporter: None,
            priority: None,
            issue_type: None,
            updated: None,
            created: None,
            description: None,
            labels: vec![],
            parent_key: None,
            parent_summary: None,
            parent_issue_type: None,
            grandparent_key: None,
            grandparent_summary: None,
            linked_projects: vec![],
            subtasks: vec![],
            original_estimate_seconds: None,
            remaining_estimate_seconds: None,
            time_spent_seconds: None,
        }
    }

    #[test]
    fn slug_basic() {
        assert_eq!(t("PROJ-123", "Add login button!").branch_slug(), "proj-123-add-login-button");
    }

    #[test]
    fn slug_collapses_and_trims() {
        assert_eq!(t("X-1", "  Hello   World  ").branch_slug(), "x-1-hello-world");
    }

    #[test]
    fn slug_cap() {
        let s = t("AB-1", &"a".repeat(200)).branch_slug();
        assert!(s.len() <= 60);
        assert!(!s.ends_with('-'));
    }
}
