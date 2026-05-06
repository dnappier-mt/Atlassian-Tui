use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::process::Command;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfluenceSpace {
    pub key: String,
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfluencePage {
    pub id: String,
    pub title: String,
    pub has_children: bool,
}

pub struct ConfluenceApi {
    pub server: String,
    pub login: String,
}

impl ConfluenceApi {
    pub fn from_jira_config() -> Result<Self> {
        let home = std::env::var("HOME").context("HOME not set")?;
        let path: std::path::PathBuf = [&home, ".config", ".jira", ".config.yml"].iter().collect();
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {} (run `jira init` first)", path.display()))?;
        let mut server: Option<String> = None;
        let mut login: Option<String> = None;
        for line in raw.lines() {
            let l = line.trim();
            if let Some(rest) = l.strip_prefix("server:") {
                server = Some(unquote(rest.trim()));
            } else if let Some(rest) = l.strip_prefix("login:") {
                login = Some(unquote(rest.trim()));
            }
        }
        Ok(Self {
            server: server
                .context("no `server:` in jira-cli config")?
                .trim_end_matches('/')
                .to_string(),
            login: login.context("no `login:` in jira-cli config")?,
        })
    }

    fn token() -> Result<String> {
        std::env::var("CONFLUENCE_API_TOKEN")
            .or_else(|_| std::env::var("JIRA_API_TOKEN"))
            .context("CONFLUENCE_API_TOKEN or JIRA_API_TOKEN not set")
    }

    pub async fn list_spaces(&self) -> Result<Vec<ConfluenceSpace>> {
        let token = Self::token()?;
        let url = format!("{}/wiki/rest/api/space?limit=50&type=global", self.server);
        let out = Command::new("curl")
            .args([
                "-sS",
                "--fail-with-body",
                "-H",
                "Accept: application/json",
                "-u",
                &format!("{}:{}", self.login, token),
                &url,
            ])
            .output()
            .await
            .context("invoking curl")?;
        if !out.status.success() {
            return Err(anyhow!(
                "curl failed: {}{}",
                String::from_utf8_lossy(&out.stderr).trim(),
                String::from_utf8_lossy(&out.stdout).trim(),
            ));
        }
        let v: Value = serde_json::from_slice(&out.stdout)?;
        let arr = v
            .get("results")
            .and_then(|x| x.as_array())
            .ok_or_else(|| anyhow!("no `results` in response"))?;
        Ok(arr
            .iter()
            .filter_map(|s| {
                Some(ConfluenceSpace {
                    key: s.get("key")?.as_str()?.to_string(),
                    name: s.get("name")?.as_str()?.to_string(),
                    description: s
                        .get("description")
                        .and_then(|d| d.get("plain"))
                        .and_then(|p| p.get("value"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                })
            })
            .collect())
    }

    pub async fn list_pages(&self, space_key: &str) -> Result<Vec<ConfluencePage>> {
        let token = Self::token()?;
        let url = format!(
            "{}/wiki/rest/api/content?type=page&spaceKey={}&limit=50&expand=children.page",
            self.server, space_key
        );
        self.fetch_pages(&url, &token).await
    }

    pub async fn get_children(&self, page_id: &str) -> Result<Vec<ConfluencePage>> {
        let token = Self::token()?;
        let url = format!(
            "{}/wiki/rest/api/content/{}/child/page?limit=50&expand=children.page",
            self.server, page_id
        );
        self.fetch_pages(&url, &token).await
    }

    async fn fetch_pages(&self, url: &str, token: &str) -> Result<Vec<ConfluencePage>> {
        let out = Command::new("curl")
            .args([
                "-sS",
                "--fail-with-body",
                "-H",
                "Accept: application/json",
                "-u",
                &format!("{}:{}", self.login, token),
                url,
            ])
            .output()
            .await
            .context("invoking curl")?;
        if !out.status.success() {
            return Err(anyhow!(
                "curl failed: {}{}",
                String::from_utf8_lossy(&out.stderr).trim(),
                String::from_utf8_lossy(&out.stdout).trim(),
            ));
        }
        let v: Value = serde_json::from_slice(&out.stdout)?;
        let arr = v
            .get("results")
            .and_then(|x| x.as_array())
            .ok_or_else(|| anyhow!("no `results` in response"))?;
        Ok(arr
            .iter()
            .filter_map(|p| {
                let has_children = p
                    .get("children")
                    .and_then(|c| c.get("page"))
                    .and_then(|pg| pg.get("size"))
                    .and_then(|s| s.as_u64())
                    .unwrap_or(0)
                    > 0;
                Some(ConfluencePage {
                    id: p.get("id")?.as_str()?.to_string(),
                    title: p.get("title")?.as_str()?.to_string(),
                    has_children,
                })
            })
            .collect())
    }

    /// Returns (title, html_body).
    pub async fn get_page_html(&self, page_id: &str) -> Result<(String, String)> {
        let token = Self::token()?;
        let url = format!(
            "{}/wiki/rest/api/content/{}?expand=body.export_view",
            self.server, page_id
        );
        let out = Command::new("curl")
            .args([
                "-sS",
                "--fail-with-body",
                "-H",
                "Accept: application/json",
                "-u",
                &format!("{}:{}", self.login, token),
                &url,
            ])
            .output()
            .await
            .context("invoking curl")?;
        if !out.status.success() {
            return Err(anyhow!(
                "curl failed: {}{}",
                String::from_utf8_lossy(&out.stderr).trim(),
                String::from_utf8_lossy(&out.stdout).trim(),
            ));
        }
        let v: Value = serde_json::from_slice(&out.stdout)?;
        let title = v
            .get("title")
            .and_then(|x| x.as_str())
            .unwrap_or("Untitled")
            .to_string();
        let html = v
            .get("body")
            .and_then(|b| b.get("export_view"))
            .and_then(|e| e.get("value"))
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        Ok((title, html))
    }
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if (s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')) {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}
