use crate::ticket::{Comment, Ticket};
use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use std::process::Stdio;
use tokio::process::Command;

#[derive(Debug, Clone)]
pub struct JiraCli {
    pub binary: String,
}

impl JiraCli {
    pub fn new(binary: Option<String>) -> Self {
        // If the user didn't override, auto-detect: prefer `jira` (typical Atlassian
        // install), fall back to `jira-cli` (snap package, common on Ubuntu). Aliases
        // don't propagate to spawned subprocesses, so we resolve to the real binary.
        let resolved = binary.unwrap_or_else(|| {
            if which::which("jira").is_ok() {
                "jira".to_string()
            } else if which::which("jira-cli").is_ok() {
                "jira-cli".to_string()
            } else {
                "jira".to_string() // sensible default for the not-found error message
            }
        });
        Self { binary: resolved }
    }

    async fn run_json(&self, args: &[&str]) -> Result<Value> {
        let out = Command::new(&self.binary)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .with_context(|| format!("invoking {} {:?}", self.binary, args))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            return Err(anyhow!("jira {:?} failed: {}", args, err));
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        serde_json::from_str(stdout.trim())
            .with_context(|| format!("parsing JSON from `jira {:?}`", args))
    }

    async fn run_ok(&self, args: &[&str]) -> Result<String> {
        let out = Command::new(&self.binary)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .with_context(|| format!("invoking {} {:?}", self.binary, args))?;
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        if !out.status.success() {
            return Err(anyhow!("jira {:?} failed: {}", args, stderr.trim()));
        }
        Ok(stdout)
    }

    /// Run a JQL search and return a list of tickets.
    /// Uses `jira issue list -q <jql> --raw`. jira-cli emits a top-level JSON array.
    pub async fn search(&self, jql: &str, limit: u32) -> Result<Vec<Ticket>> {
        let limit_s = limit.to_string();
        let v = self
            .run_json(&[
                "issue", "list",
                "-q", jql,
                "--order-by", "updated",
                "--reverse",
                "--paginate", &limit_s,
                "--raw",
            ])
            .await?;
        parse_search_response(&v)
    }

    pub async fn view(&self, key: &str) -> Result<Ticket> {
        let v = self.run_json(&["issue", "view", key, "--raw"]).await?;
        parse_issue(&v).ok_or_else(|| anyhow!("could not parse issue {key}"))
    }

    pub async fn comments(&self, key: &str) -> Result<Vec<Comment>> {
        let v = self.run_json(&["issue", "view", key, "--comments", "100", "--raw"]).await?;
        Ok(parse_comments(&v))
    }

    pub async fn add_comment(&self, key: &str, body: &str) -> Result<()> {
        self.run_ok(&["issue", "comment", "add", key, body, "--no-input"]).await?;
        Ok(())
    }

    pub async fn transition(&self, key: &str, to: &str) -> Result<()> {
        // `jira issue move` does not accept --no-input; positional <KEY> <STATE> is enough.
        self.run_ok(&["issue", "move", key, to]).await?;
        Ok(())
    }

    pub async fn assign(&self, key: &str, assignee: &str) -> Result<()> {
        self.run_ok(&["issue", "assign", key, assignee, "--no-input"]).await?;
        Ok(())
    }

    pub async fn create(
        &self,
        project: &str,
        issue_type: &str,
        summary: &str,
        body: Option<&str>,
        parent: Option<&str>,
    ) -> Result<String> {
        let mut args: Vec<String> = vec![
            "issue".into(), "create".into(),
            "-p".into(), project.into(),
            "-t".into(), issue_type.into(),
            "-s".into(), summary.into(),
            "--no-input".into(),
        ];
        if let Some(b) = body {
            args.push("-b".into());
            args.push(b.into());
        }
        if let Some(p) = parent {
            args.push("-P".into());
            args.push(p.into());
        }
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let stdout = self.run_ok(&arg_refs).await?;
        // jira-cli prints a confirmation line containing the new key.
        Ok(crate::scm::extract_ticket_key(&stdout).unwrap_or_default())
    }

    pub async fn edit_summary(&self, key: &str, summary: &str) -> Result<()> {
        self.run_ok(&["issue", "edit", key, "-s", summary, "--no-input"]).await?;
        Ok(())
    }

    pub async fn edit_priority(&self, key: &str, priority: &str) -> Result<()> {
        self.run_ok(&["issue", "edit", key, "-y", priority, "--no-input"]).await?;
        Ok(())
    }

    /// Fill in parent_summary, parent_issue_type, grandparent_{key,summary} for any tickets
    /// that have a parent_key. Two batch JQL `key in (...)` searches at most.
    pub async fn resolve_parents(&self, tickets: &mut [Ticket]) -> Result<()> {
        use std::collections::{HashMap, HashSet};

        let parent_keys: HashSet<String> = tickets
            .iter()
            .filter_map(|t| t.parent_key.clone())
            .collect();
        if parent_keys.is_empty() {
            return Ok(());
        }
        let parents = self.batch_fetch(&parent_keys).await.unwrap_or_default();
        let mut grandparent_keys: HashSet<String> = HashSet::new();
        for t in tickets.iter_mut() {
            if let Some(pk) = &t.parent_key {
                if let Some(p) = parents.get(pk) {
                    if t.parent_summary.is_none() {
                        t.parent_summary = Some(p.summary.clone());
                    }
                    if t.parent_issue_type.is_none() {
                        t.parent_issue_type = p.issue_type.clone();
                    }
                    if let Some(gpk) = &p.parent_key {
                        t.grandparent_key = Some(gpk.clone());
                        grandparent_keys.insert(gpk.clone());
                    }
                }
            }
        }
        if !grandparent_keys.is_empty() {
            let grand = self.batch_fetch(&grandparent_keys).await.unwrap_or_default();
            for t in tickets.iter_mut() {
                if let Some(gk) = &t.grandparent_key {
                    if let Some(g) = grand.get(gk) {
                        t.grandparent_summary = Some(g.summary.clone());
                    }
                }
            }
        }
        let _: HashMap<(), ()> = HashMap::new(); // silence unused-import warn for HashMap
        Ok(())
    }

    /// Recursively pull descendants of every ticket in `tickets`, up to `max_depth`
    /// levels including the starting set (so passing `max_depth=5` means: starting set
    /// + 4 rounds of children-of-frontier). One batched JQL call per round per chunk
    /// of ≤40 keys. Tickets already present (by key) are not re-added.
    pub async fn resolve_subtasks(
        &self,
        tickets: &mut Vec<Ticket>,
        max_depth: usize,
    ) -> Result<()> {
        if max_depth <= 1 {
            return Ok(());
        }
        use std::collections::HashSet;
        let mut known: HashSet<String> = tickets.iter().map(|t| t.key.clone()).collect();
        let mut frontier: Vec<String> = tickets.iter().map(|t| t.key.clone()).collect();
        for _ in 0..(max_depth - 1) {
            if frontier.is_empty() {
                break;
            }
            let mut new_round: Vec<Ticket> = Vec::new();
            for chunk in frontier.chunks(40) {
                let jql = format!("parent in ({})", chunk.join(","));
                match self.search(&jql, 100).await {
                    Ok(found) => {
                        for t in found {
                            if known.insert(t.key.clone()) {
                                new_round.push(t);
                            }
                        }
                    }
                    Err(_) => continue,
                }
            }
            if new_round.is_empty() {
                break;
            }
            frontier = new_round.iter().map(|t| t.key.clone()).collect();
            tickets.extend(new_round);
        }
        Ok(())
    }

    async fn batch_fetch(
        &self,
        keys: &std::collections::HashSet<String>,
    ) -> Result<std::collections::HashMap<String, Ticket>> {
        if keys.is_empty() {
            return Ok(Default::default());
        }
        let key_list: Vec<&str> = keys.iter().map(String::as_str).collect();
        let jql = format!("key in ({})", key_list.join(","));
        let limit = (keys.len() as u32).max(1);
        let tickets = self.search(&jql, limit).await?;
        Ok(tickets.into_iter().map(|t| (t.key.clone(), t)).collect())
    }

    pub async fn worklog_add(
        &self,
        key: &str,
        time_spent: &str,
        comment: Option<&str>,
        new_estimate: Option<&str>,
    ) -> Result<()> {
        let mut args: Vec<String> = vec![
            "issue".into(), "worklog".into(), "add".into(),
            key.into(), time_spent.into(), "--no-input".into(),
        ];
        if let Some(c) = comment {
            args.push("--comment".into());
            args.push(c.into());
        }
        if let Some(e) = new_estimate {
            args.push("--new-estimate".into());
            args.push(e.into());
        }
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.run_ok(&arg_refs).await?;
        Ok(())
    }
}

fn parse_search_response(v: &Value) -> Result<Vec<Ticket>> {
    // jira-cli `--raw` emits a top-level array of issues. Older/Server builds may emit
    // the REST search payload `{"issues": [...]}`. Accept both.
    let issues = if let Some(arr) = v.as_array() {
        arr
    } else if let Some(arr) = v.get("issues").and_then(|x| x.as_array()) {
        arr
    } else {
        return Err(anyhow!("unexpected jira-cli response shape (not array or {{issues}})"));
    };
    Ok(issues.iter().filter_map(parse_issue).collect())
}

fn parse_issue(v: &Value) -> Option<Ticket> {
    let key = v.get("key")?.as_str()?.to_string();
    let f = v.get("fields")?;
    let summary = f.get("summary").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let status = f
        .get("status")
        .and_then(|x| x.get("name"))
        .and_then(|x| x.as_str())
        .unwrap_or("Unknown")
        .to_string();
    let assignee = f
        .get("assignee")
        .and_then(|x| x.get("displayName"))
        .and_then(|x| x.as_str())
        .map(str::to_string);
    let reporter = f
        .get("reporter")
        .and_then(|x| x.get("displayName"))
        .and_then(|x| x.as_str())
        .map(str::to_string);
    let priority = f
        .get("priority")
        .and_then(|x| x.get("name"))
        .and_then(|x| x.as_str())
        .map(str::to_string);
    let issue_type = f
        .get("issueType")
        .or_else(|| f.get("issuetype"))
        .and_then(|x| x.get("name"))
        .and_then(|x| x.as_str())
        .map(str::to_string);
    let updated = f.get("updated").and_then(|x| x.as_str()).map(str::to_string);
    let created = f.get("created").and_then(|x| x.as_str()).map(str::to_string);
    let description = f.get("description").and_then(|d| match d {
        Value::String(s) => Some(s.clone()),
        Value::Object(_) => Some(adf_to_text(d)),
        _ => None,
    });
    let labels = f
        .get("labels")
        .and_then(|x| x.as_array())
        .map(|arr| arr.iter().filter_map(|l| l.as_str().map(str::to_string)).collect())
        .unwrap_or_default();

    // Time tracking. `view --raw` returns a `timetracking` object with `*Seconds` fields,
    // and also the flat `time(original)?(estimate|spent)` fields. `list --raw` omits all
    // of them. Pull from whichever is present.
    let tt = f.get("timetracking");
    let take_secs = |path_in_tt: &str, flat: &str| -> Option<i64> {
        tt.and_then(|t| t.get(path_in_tt))
            .and_then(|x| x.as_i64())
            .or_else(|| f.get(flat).and_then(|x| x.as_i64()))
    };
    // Parent — list responses only give `parent.key`. View responses include the parent's
    // summary and issue_type and that parent's own parent. We pull whatever's there.
    let parent = f.get("parent");
    let parent_key = parent.and_then(|p| p.get("key")).and_then(|x| x.as_str()).map(str::to_string);
    let parent_fields = parent.and_then(|p| p.get("fields"));
    let parent_summary = parent_fields
        .and_then(|pf| pf.get("summary"))
        .and_then(|x| x.as_str())
        .map(str::to_string);
    let parent_issue_type = parent_fields
        .and_then(|pf| pf.get("issueType").or_else(|| pf.get("issuetype")))
        .and_then(|x| x.get("name"))
        .and_then(|x| x.as_str())
        .map(str::to_string);

    // Subtasks: typically empty array; on `view` responses each item has key + fields.
    let subtasks: Vec<crate::ticket::SubtaskRef> = f
        .get("Subtasks")
        .or_else(|| f.get("subtasks"))
        .and_then(|x| x.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|s| {
                    let key = s.get("key")?.as_str()?.to_string();
                    let sf = s.get("fields");
                    let summary = sf
                        .and_then(|f| f.get("summary"))
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    let status = sf
                        .and_then(|f| f.get("status"))
                        .and_then(|x| x.get("name"))
                        .and_then(|x| x.as_str())
                        .map(str::to_string);
                    let issue_type = sf
                        .and_then(|f| f.get("issueType").or_else(|| f.get("issuetype")))
                        .and_then(|x| x.get("name"))
                        .and_then(|x| x.as_str())
                        .map(str::to_string);
                    Some(crate::ticket::SubtaskRef { key, summary, status, issue_type })
                })
                .collect()
        })
        .unwrap_or_default();

    let original_estimate_seconds = take_secs("originalEstimateSeconds", "timeoriginalestimate");
    let remaining_estimate_seconds = take_secs("remainingEstimateSeconds", "timeestimate");
    let time_spent_seconds = take_secs("timeSpentSeconds", "timespent");

    Some(Ticket {
        key,
        summary,
        status,
        assignee,
        reporter,
        priority,
        issue_type,
        updated,
        created,
        description,
        labels,
        parent_key,
        parent_summary,
        parent_issue_type,
        grandparent_key: None,
        grandparent_summary: None,
        linked_projects: vec![],
        subtasks,
        original_estimate_seconds,
        remaining_estimate_seconds,
        time_spent_seconds,
    })
}

/// Flatten an Atlassian Document Format node into plain text. Walks `content` recursively
/// and concatenates `text` leaves with paragraph breaks.
fn adf_to_text(node: &Value) -> String {
    let mut out = String::new();
    walk_adf(node, &mut out);
    out.trim().to_string()
}

fn walk_adf(node: &Value, out: &mut String) {
    if let Some(t) = node.get("text").and_then(|x| x.as_str()) {
        out.push_str(t);
    }
    let node_type = node.get("type").and_then(|x| x.as_str()).unwrap_or("");
    if matches!(node_type, "paragraph" | "heading" | "listItem" | "codeBlock") {
        if let Some(arr) = node.get("content").and_then(|x| x.as_array()) {
            for c in arr {
                walk_adf(c, out);
            }
        }
        out.push('\n');
        return;
    }
    if let Some(arr) = node.get("content").and_then(|x| x.as_array()) {
        for c in arr {
            walk_adf(c, out);
        }
    }
}

fn parse_comments(v: &Value) -> Vec<Comment> {
    let arr = v
        .get("fields")
        .and_then(|f| f.get("comment"))
        .and_then(|c| c.get("comments"))
        .and_then(|x| x.as_array());
    let Some(arr) = arr else { return vec![] };
    arr.iter()
        .filter_map(|c| {
            let author_obj = c.get("author");
            Some(Comment {
                id: c.get("id").and_then(|x| x.as_str()).map(str::to_string),
                account_id: author_obj
                    .and_then(|a| a.get("accountId"))
                    .and_then(|x| x.as_str())
                    .map(str::to_string),
                author: author_obj
                    .and_then(|a| a.get("displayName"))
                    .and_then(|x| x.as_str())?
                    .to_string(),
                created: c.get("created").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                body: c.get("body").map(|b| match b {
                    Value::String(s) => s.clone(),
                    Value::Object(_) => adf_to_text(b),
                    _ => String::new(),
                }).unwrap_or_default(),
            })
        })
        .collect()
}
