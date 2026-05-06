use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use tokio::process::Command;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MyselfInfo {
    pub account_id: String,
    pub display_name: String,
    pub email: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserInfo {
    pub account_id: String,
    pub display_name: String,
    pub email: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransitionOption {
    pub id: String,
    pub name: String,
    pub to_status: Option<String>,
}

#[derive(Debug, Clone)]
pub struct JiraApi {
    pub server: String,
    pub login: String,
}

impl JiraApi {
    /// Read jira-cli's config (~/.config/.jira/.config.yml) for server URL + login email.
    pub fn from_jira_cli_config() -> Result<Self> {
        let home = std::env::var("HOME").context("HOME not set")?;
        let path: PathBuf = [&home, ".config", ".jira", ".config.yml"].iter().collect();
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {} (run `jira init` first)", path.display()))?;

        let mut server: Option<String> = None;
        let mut login: Option<String> = None;
        // Hand-parse: jira-cli's config has `server:` and `login:` as top-level/nested
        // string fields. We don't need a real YAML lib for two scalars.
        for line in raw.lines() {
            let l = line.trim();
            if let Some(rest) = l.strip_prefix("server:") {
                server = Some(unquote(rest.trim()));
            } else if let Some(rest) = l.strip_prefix("login:") {
                login = Some(unquote(rest.trim()));
            }
        }
        Ok(Self {
            server: server.context("no `server:` in jira-cli config")?.trim_end_matches('/').to_string(),
            login: login.context("no `login:` in jira-cli config")?,
        })
    }

    /// Fetch the instance's priority levels via REST. Falls back to the standard set
    /// if the call fails so the picker still works.
    pub async fn priorities(&self) -> Result<Vec<String>> {
        let token = std::env::var("JIRA_API_TOKEN").context("JIRA_API_TOKEN not set")?;
        let url = format!("{}/rest/api/3/priority", self.server);
        let out = Command::new("curl")
            .args([
                "-sS", "--fail-with-body",
                "-H", "Accept: application/json",
                "-u", &format!("{}:{}", self.login, token),
                &url,
            ])
            .output()
            .await
            .context("invoking curl")?;
        if !out.status.success() {
            return Ok(default_priorities());
        }
        let v: Value = serde_json::from_slice(&out.stdout)?;
        let arr = v.as_array().cloned().unwrap_or_default();
        let names: Vec<String> = arr
            .into_iter()
            .filter_map(|p| p.get("name").and_then(|x| x.as_str()).map(str::to_string))
            .collect();
        if names.is_empty() {
            Ok(default_priorities())
        } else {
            Ok(names)
        }
    }

    pub async fn list_transitions(&self, key: &str) -> Result<Vec<TransitionOption>> {
        let token = std::env::var("JIRA_API_TOKEN")
            .context("JIRA_API_TOKEN not set; needed for transitions list")?;
        let url = format!("{}/rest/api/3/issue/{}/transitions", self.server, key);
        // -u puts creds on argv (visible to other local processes only). Acceptable
        // for a personal tool — same posture as `jira-cli` itself.
        let out = Command::new("curl")
            .args([
                "-sS", "--fail-with-body",
                "-H", "Accept: application/json",
                "-u", &format!("{}:{}", self.login, token),
                &url,
            ])
            .output()
            .await
            .context("invoking curl")?;
        if !out.status.success() {
            return Err(anyhow!(
                "curl failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let v: Value = serde_json::from_slice(&out.stdout)
            .context("parsing transitions JSON")?;
        let arr = v
            .get("transitions")
            .and_then(|x| x.as_array())
            .ok_or_else(|| anyhow!("no `transitions` array in response"))?;
        Ok(arr
            .iter()
            .filter_map(|t| {
                Some(TransitionOption {
                    id: t.get("id")?.as_str()?.to_string(),
                    name: t.get("name")?.as_str()?.to_string(),
                    to_status: t
                        .get("to")
                        .and_then(|x| x.get("name"))
                        .and_then(|x| x.as_str())
                        .map(str::to_string),
                })
            })
            .collect())
    }

    pub async fn myself(&self) -> Result<MyselfInfo> {
        let token = std::env::var("JIRA_API_TOKEN")
            .context("JIRA_API_TOKEN not set")?;
        let url = format!("{}/rest/api/3/myself", self.server);
        let out = Command::new("curl")
            .args([
                "-sS", "--fail-with-body",
                "-H", "Accept: application/json",
                "-u", &format!("{}:{}", self.login, token),
                &url,
            ])
            .output()
            .await
            .context("invoking curl")?;
        if !out.status.success() {
            return Err(anyhow!(
                "curl /myself failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let v: Value = serde_json::from_slice(&out.stdout)?;
        Ok(MyselfInfo {
            account_id: v.get("accountId").and_then(|x| x.as_str())
                .ok_or_else(|| anyhow!("/myself missing accountId"))?.to_string(),
            display_name: v.get("displayName").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            email: v.get("emailAddress").and_then(|x| x.as_str()).map(str::to_string),
        })
    }

    pub async fn delete_comment(&self, issue_key: &str, comment_id: &str) -> Result<()> {
        let token = std::env::var("JIRA_API_TOKEN")
            .context("JIRA_API_TOKEN not set")?;
        let url = format!("{}/rest/api/3/issue/{}/comment/{}", self.server, issue_key, comment_id);
        let out = Command::new("curl")
            .args([
                "-sS", "--fail-with-body",
                "-X", "DELETE",
                "-u", &format!("{}:{}", self.login, token),
                &url,
            ])
            .output()
            .await
            .context("invoking curl")?;
        if !out.status.success() {
            return Err(anyhow!(
                "curl DELETE failed: {}{}",
                String::from_utf8_lossy(&out.stderr).trim(),
                String::from_utf8_lossy(&out.stdout).trim(),
            ));
        }
        Ok(())
    }

    /// Set issue priority via REST PUT. Tries each known shape — Jira instances vary:
    ///   1. `{"fields": {"priority": {"name": "High"}}}` (standard)
    ///   2. `{"fields": {"priority": "High"}}` (string, some custom configs)
    ///   3. `{"update": {"priority": [{"set": {"name": "High"}}]}}` (Atlassian's
    ///       newer "update" path, more permissive on some sites)
    /// First success wins; if all three fail we return the most informative error.
    pub async fn set_priority(&self, key: &str, priority: &str) -> Result<()> {
        let token = std::env::var("JIRA_API_TOKEN")
            .context("JIRA_API_TOKEN not set; needed to update priority")?;
        let url = format!("{}/rest/api/3/issue/{}", self.server, key);
        let bodies = [
            serde_json::json!({ "fields": { "priority": { "name": priority } } }),
            serde_json::json!({ "fields": { "priority": priority } }),
            serde_json::json!({ "update": { "priority": [{ "set": { "name": priority } }] } }),
        ];
        let mut last_err: Option<String> = None;
        for body in &bodies {
            let body_str = serde_json::to_string(body)?;
            let out = Command::new("curl")
                .args([
                    "-sS", "--fail-with-body",
                    "-X", "PUT",
                    "-H", "Accept: application/json",
                    "-H", "Content-Type: application/json",
                    "-u", &format!("{}:{}", self.login, token),
                    "--data", &body_str,
                    &url,
                ])
                .output()
                .await
                .context("invoking curl")?;
            if out.status.success() {
                return Ok(());
            }
            last_err = Some(format!(
                "shape {body_str}: {}{}",
                String::from_utf8_lossy(&out.stderr).trim(),
                String::from_utf8_lossy(&out.stdout).trim(),
            ));
        }
        Err(anyhow!(
            "all priority PUT shapes rejected — {}",
            last_err.unwrap_or_default()
        ))
    }

    /// Set the original (and optionally remaining) estimate via REST PUT.
    /// Values are Jira-style strings like "8h" or "2d 4h".
    pub async fn set_estimate(
        &self,
        key: &str,
        original: Option<&str>,
        remaining: Option<&str>,
    ) -> Result<()> {
        if original.is_none() && remaining.is_none() {
            return Ok(());
        }
        let token = std::env::var("JIRA_API_TOKEN")
            .context("JIRA_API_TOKEN not set; needed to update estimate")?;
        let url = format!("{}/rest/api/3/issue/{}", self.server, key);
        let mut tt = serde_json::Map::new();
        if let Some(o) = original {
            tt.insert("originalEstimate".into(), serde_json::Value::String(o.into()));
        }
        if let Some(r) = remaining {
            tt.insert("remainingEstimate".into(), serde_json::Value::String(r.into()));
        }
        let body = serde_json::json!({
            "fields": { "timetracking": serde_json::Value::Object(tt) }
        });
        let body_str = serde_json::to_string(&body)?;
        let out = Command::new("curl")
            .args([
                "-sS", "--fail-with-body",
                "-X", "PUT",
                "-H", "Accept: application/json",
                "-H", "Content-Type: application/json",
                "-u", &format!("{}:{}", self.login, token),
                "--data", &body_str,
                &url,
            ])
            .output()
            .await
            .context("invoking curl")?;
        if !out.status.success() {
            return Err(anyhow!(
                "curl PUT {} failed: {}{}",
                url,
                String::from_utf8_lossy(&out.stderr).trim(),
                String::from_utf8_lossy(&out.stdout).trim(),
            ));
        }
        Ok(())
    }

    /// Fetch every active Jira user by paginating `/rest/api/3/users/search`.
    /// Returns at most ~2 000 users in practice; stops when a page is empty.
    pub async fn list_all_users(&self) -> Result<Vec<UserInfo>> {
        let token = std::env::var("JIRA_API_TOKEN").context("JIRA_API_TOKEN not set")?;
        let mut users: Vec<UserInfo> = Vec::new();
        let mut start_at: usize = 0;
        let page_size: usize = 200;
        loop {
            let url = format!(
                "{}/rest/api/3/users/search?maxResults={}&startAt={}",
                self.server, page_size, start_at
            );
            let out = Command::new("curl")
                .args([
                    "-sS", "--fail-with-body",
                    "-H", "Accept: application/json",
                    "-u", &format!("{}:{}", self.login, token),
                    &url,
                ])
                .output()
                .await
                .context("invoking curl")?;
            if !out.status.success() {
                break;
            }
            let arr: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout)
                .unwrap_or_default();
            if arr.is_empty() {
                break;
            }
            for u in &arr {
                // Skip app/bot accounts (accountType = "app" or "atlassian").
                let account_type = u.get("accountType").and_then(|x| x.as_str()).unwrap_or("");
                if account_type == "app" {
                    continue;
                }
                let Some(account_id) = u.get("accountId").and_then(|x| x.as_str()) else { continue };
                let display_name = u.get("displayName").and_then(|x| x.as_str()).unwrap_or("").to_string();
                let email = u.get("emailAddress").and_then(|x| x.as_str()).map(str::to_string);
                users.push(UserInfo {
                    account_id: account_id.to_string(),
                    display_name,
                    email,
                });
            }
            if arr.len() < page_size {
                break;
            }
            start_at += page_size;
        }
        Ok(users)
    }
}  // end impl JiraApi

fn default_priorities() -> Vec<String> {
    ["Highest", "High", "Medium", "Low", "Lowest"].iter().map(|s| s.to_string()).collect()
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if (s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')) {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}
