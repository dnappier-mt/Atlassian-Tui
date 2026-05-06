use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GlobalConfig {
    #[serde(default)]
    pub jira: JiraConfig,
    #[serde(default)]
    pub ui: UiConfig,
    #[serde(default)]
    pub notifications: NotificationConfig,
    #[serde(default)]
    pub poll: PollConfig,
    #[serde(default)]
    pub projects: Vec<ProjectEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectEntry {
    pub path: PathBuf,
    #[serde(default)]
    pub nickname: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JiraConfig {
    /// Path to `jira` CLI binary. Defaults to `jira` in PATH.
    #[serde(default)]
    pub binary: Option<String>,
    /// Default JQL filter for "my work" view.
    #[serde(default = "default_my_jql")]
    pub my_jql: String,
}

impl Default for JiraConfig {
    fn default() -> Self {
        Self { binary: None, my_jql: default_my_jql() }
    }
}

fn default_my_jql() -> String {
    // No ORDER BY here — jira-cli's `--paginate` flag rejects it. Sorting is requested
    // separately via `--order-by` / `--reverse` in the search call.
    "assignee = currentUser() AND statusCategory != Done".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UiConfig {
    #[serde(default)]
    pub theme: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationConfig {
    #[serde(default = "yes")]
    pub on_mention: bool,
    #[serde(default = "yes")]
    pub on_assignment: bool,
    #[serde(default)]
    pub tmux_status: bool,
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self { on_mention: true, on_assignment: true, tmux_status: false }
    }
}

fn yes() -> bool { true }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PollConfig {
    #[serde(default = "default_interval")]
    pub interval_secs: u64,
}

impl Default for PollConfig {
    fn default() -> Self { Self { interval_secs: 120 } }
}

fn default_interval() -> u64 { 120 }

/// Per-repo override loaded from `.jui.toml` if present.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RepoConfig {
    /// Restrict ticket list to this project key (e.g. "ENG").
    #[serde(default)]
    pub project: Option<String>,
    /// Override JQL for this repo.
    #[serde(default)]
    pub jql: Option<String>,
}

impl GlobalConfig {
    pub fn load() -> Result<Self> {
        let path = crate::paths::config_file()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save(&self) -> Result<()> {
        crate::paths::ensure_dirs()?;
        let path = crate::paths::config_file()?;
        let raw = toml::to_string_pretty(self)?;
        std::fs::write(&path, raw)?;
        Ok(())
    }

    /// Add a project path (canonicalized) if not already present. Returns true if added.
    pub fn add_project(&mut self, path: PathBuf, nickname: Option<String>) -> bool {
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        if self.projects.iter().any(|p| p.path == path) {
            return false;
        }
        self.projects.push(ProjectEntry { path, nickname });
        true
    }

    pub fn remove_project(&mut self, path: &Path) -> bool {
        let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let before = self.projects.len();
        self.projects.retain(|p| p.path != target);
        self.projects.len() != before
    }
}

impl RepoConfig {
    /// Walks upward from `start` looking for `.jui.toml`. Returns (config, dir) if found.
    pub fn discover(start: &Path) -> Result<Option<(Self, PathBuf)>> {
        let mut cur = Some(start);
        while let Some(dir) = cur {
            let candidate = dir.join(".jui.toml");
            if candidate.exists() {
                let raw = std::fs::read_to_string(&candidate)?;
                let cfg: Self = toml::from_str(&raw)
                    .with_context(|| format!("parsing {}", candidate.display()))?;
                return Ok(Some((cfg, dir.to_path_buf())));
            }
            cur = dir.parent();
        }
        Ok(None)
    }
}
